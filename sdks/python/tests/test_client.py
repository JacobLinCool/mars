import asyncio
import json
from pathlib import Path
import tempfile
import unittest

from mars_sdk import (
    AppVirtualInputSpec,
    DaemonError,
    MarsClient,
    ProtocolVersionError,
)


class MarsClientTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="mars-python-sdk-")
        self.socket_path = Path(self.directory.name) / "marsd.sock"

    async def asyncTearDown(self) -> None:
        self.directory.cleanup()

    async def test_set_virtual_inputs_sends_complete_set_and_returns_handles(self) -> None:
        def respond(request):
            self.assertEqual(request["protocol_version"], 3)
            self.assertEqual(request["command"], "set_virtual_inputs")
            self.assertEqual(
                request["payload"],
                {
                    "app_id": "com.example.recorder",
                    "inputs": [
                        {
                            "id": "mic",
                            "name": "Recorder Mic",
                            "uid": "com.example.recorder.mic",
                            "sample_rate": 48_000,
                            "channels": 1,
                        }
                    ],
                },
            )
            return {
                "payload": {
                    "apply": apply_result(applied=True),
                    "ensured_inputs": [ensured_input()],
                }
            }

        async with self.daemon(respond):
            outcome = await MarsClient(self.socket_path).set_virtual_inputs(
                "com.example.recorder",
                [
                    AppVirtualInputSpec(
                        id="mic",
                        name="Recorder Mic",
                        uid="com.example.recorder.mic",
                        sample_rate=48_000,
                        channels=1,
                    )
                ],
            )

        self.assertTrue(outcome.apply.applied)
        self.assertEqual(len(outcome.virtual_mics), 1)
        self.assertEqual(
            outcome.virtual_mics[0].info.ring_name, "mars.vin.test.capability"
        )

    async def test_empty_set_deletes_app_collection(self) -> None:
        def respond(request):
            self.assertEqual(
                request["payload"],
                {"app_id": "com.example.recorder", "inputs": []},
            )
            return {
                "payload": {
                    "apply": apply_result(applied=False),
                    "ensured_inputs": [],
                }
            }

        async with self.daemon(respond):
            outcome = await MarsClient(self.socket_path).set_virtual_inputs(
                "com.example.recorder", []
            )
        self.assertEqual(outcome.virtual_mics, ())

    async def test_get_and_status_decode_daemon_values(self) -> None:
        def respond(request):
            if request["command"] == "get_virtual_inputs":
                return {
                    "payload": {
                        "app_id": "com.example.recorder",
                        "inputs": [
                            {
                                "id": "mic",
                                "name": "Recorder Mic",
                                "uid": "com.example.recorder.mic",
                                "sample_rate": 48_000,
                                "channels": 2,
                            }
                        ],
                    }
                }
            return {"payload": ensured_input()["producer"]}

        async with self.daemon(respond):
            client = MarsClient(self.socket_path)
            declared = await client.get_virtual_inputs("com.example.recorder")
            status = await client.virtual_input_status(
                "com.example.recorder", "mic"
            )

        self.assertEqual(declared.inputs[0].channels, 2)
        self.assertEqual(status.write_idx, 42)
        self.assertEqual(status.kind, "external_app")

    async def test_protocol_mismatch_fails_explicitly(self) -> None:
        async with self.daemon(
            lambda _request: {"protocol_version": 4, "payload": None}
        ):
            with self.assertRaises(ProtocolVersionError):
                await MarsClient(self.socket_path).ping()

    async def test_daemon_error_retains_exit_code(self) -> None:
        async with self.daemon(
            lambda _request: {
                "ok": False,
                "payload": None,
                "error": "uid already exists",
                "exit_code": 3,
            }
        ):
            with self.assertRaises(DaemonError) as raised:
                await MarsClient(self.socket_path).ping()
        self.assertEqual(raised.exception.exit_code, 3)

    def daemon(self, respond):
        return FakeDaemon(self.socket_path, respond)


class FakeDaemon:
    def __init__(self, socket_path, respond):
        self.socket_path = socket_path
        self.respond = respond
        self.server = None

    async def __aenter__(self):
        self.server = await asyncio.start_unix_server(
            self.handle_request, path=self.socket_path
        )
        return self

    async def __aexit__(self, exc_type, exc, traceback):
        self.server.close()
        await self.server.wait_closed()

    async def handle_request(self, reader, writer):
        request = json.loads(await reader.readline())
        override = self.respond(request)
        response = {
            "protocol_version": override.get("protocol_version", 3),
            "request_id": override.get("request_id", request["request_id"]),
            "command": override.get("command", request["command"]),
            "ok": override.get("ok", True),
            "payload": override.get("payload"),
            "error": override.get("error"),
            "exit_code": override.get("exit_code"),
        }
        writer.write(json.dumps(response).encode() + b"\n")
        await writer.drain()
        writer.close()
        await writer.wait_closed()


def apply_result(*, applied):
    return {
        "applied": applied,
        "plan": {"changes": [], "warnings": []},
        "warnings": [],
        "errors": [],
    }


def ensured_input():
    return {
        "uid": "com.example.recorder.mic",
        "ring_name": "mars.vin.test.capability",
        "sample_rate": 48_000,
        "channels": 1,
        "capacity_frames": 4_096,
        "producer": {
            "app_id": "com.example.recorder",
            "id": "mic",
            "uid": "com.example.recorder.mic",
            "kind": "external_app",
            "state": "active",
            "write_idx": 42,
            "underrun_count": 2,
            "attach_count": 1,
            "generation": 1,
        },
    }


if __name__ == "__main__":
    unittest.main()
