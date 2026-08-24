from __future__ import annotations

import asyncio
import json
import math
from pathlib import Path
from typing import Any, Literal, cast
from uuid import uuid4

from .errors import DaemonError, MarsClientError, ProtocolVersionError
from .models import (
    AppVirtualInputSpec,
    AppVirtualInputs,
    ApplyPlan,
    ApplyResult,
    PlanChange,
    ProducerState,
    SetVirtualInputsOutcome,
    VirtualMicInfo,
    VirtualInputProducerStatus,
)
from .virtual_input import VirtualMic

PROTOCOL_VERSION = 3
MAX_RESPONSE_BYTES = 16 * 1024 * 1024


class MarsClient:
    def __init__(self, socket_path: Path | None = None, timeout: float = 5.0) -> None:
        self.socket_path = socket_path or (
            Path.home() / "Library" / "Caches" / "mars" / "marsd.sock"
        )
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be a positive finite number")
        self.timeout = timeout

    async def ping(self) -> None:
        await self._request("ping", None)

    async def status(self) -> dict[str, Any]:
        return _object(await self._request("status", None), "daemon status")

    async def set_virtual_inputs(
        self, app_id: str, inputs: list[AppVirtualInputSpec]
    ) -> SetVirtualInputsOutcome:
        payload = await self._request(
            "set_virtual_inputs",
            {
                "app_id": app_id,
                "inputs": [_encode_spec(spec) for spec in inputs],
            },
        )
        result = _object(payload, "set virtual inputs result")
        ensured_inputs = _list(result.get("ensured_inputs"), "ensured virtual inputs")
        return SetVirtualInputsOutcome(
            apply=_decode_apply_result(result.get("apply")),
            virtual_mics=tuple(_decode_virtual_mic(item) for item in ensured_inputs),
        )

    async def get_virtual_inputs(self, app_id: str) -> AppVirtualInputs:
        result = _object(
            await self._request("get_virtual_inputs", {"app_id": app_id}),
            "app virtual inputs",
        )
        return AppVirtualInputs(
            app_id=_string(result.get("app_id"), "app id"),
            inputs=tuple(
                _decode_spec(item)
                for item in _list(result.get("inputs"), "virtual inputs")
            ),
        )

    async def virtual_input_status(
        self, app_id: str, id: str
    ) -> VirtualInputProducerStatus:
        return _decode_producer(
            await self._request(
                "virtual_input_status", {"app_id": app_id, "id": id}
            )
        )

    async def _request(self, command: str, payload: Any) -> Any:
        request_id = str(uuid4())
        envelope = {
            "protocol_version": PROTOCOL_VERSION,
            "request_id": request_id,
            "command": command,
            "payload": payload,
        }
        writer: asyncio.StreamWriter | None = None
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_unix_connection(
                    self.socket_path, limit=MAX_RESPONSE_BYTES + 1
                ),
                self.timeout,
            )
            writer.write(
                json.dumps(envelope, separators=(",", ":")).encode("utf-8") + b"\n"
            )
            await asyncio.wait_for(writer.drain(), self.timeout)
            line = await asyncio.wait_for(reader.readline(), self.timeout)
        except TimeoutError as error:
            raise MarsClientError(
                f"request {command} timed out after {self.timeout:g} seconds"
            ) from error
        except OSError as error:
            raise MarsClientError(f"IPC request {command} failed: {error}") from error
        except ValueError as error:
            raise MarsClientError("daemon response exceeds 16 MiB") from error
        finally:
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass

        if not line:
            raise MarsClientError(
                "daemon closed the connection before sending a response"
            )
        if len(line) > MAX_RESPONSE_BYTES:
            raise MarsClientError("daemon response exceeds 16 MiB")
        try:
            response = _object(json.loads(line), "response envelope")
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise MarsClientError(f"daemon returned invalid JSON: {error}") from error

        version = response.get("protocol_version")
        if version != PROTOCOL_VERSION:
            raise ProtocolVersionError(
                f"protocol version mismatch: SDK uses {PROTOCOL_VERSION}, daemon uses {version}"
            )
        if response.get("request_id") != request_id:
            raise MarsClientError("daemon response request id does not match the request")
        if response.get("command") != command:
            raise MarsClientError(
                "daemon response command mismatch: "
                f"expected {command}, got {response.get('command')}"
            )
        if response.get("ok") is not True:
            exit_code = response.get("exit_code")
            raise DaemonError(
                _string(response.get("error", "unknown daemon error"), "daemon error"),
                exit_code=exit_code if type(exit_code) is int else None,
            )
        if "payload" not in response:
            raise MarsClientError("daemon response envelope has no payload")
        return response["payload"]


def _encode_spec(spec: AppVirtualInputSpec) -> dict[str, Any]:
    return {
        "id": spec.id,
        "name": spec.name,
        "uid": spec.uid,
        "sample_rate": spec.sample_rate,
        "channels": spec.channels,
    }


def _decode_spec(value: Any) -> AppVirtualInputSpec:
    item = _object(value, "virtual input spec")
    sample_rate = _int(item.get("sample_rate"), "sample rate")
    channels = _int(item.get("channels"), "channels")
    if sample_rate != 48_000:
        raise MarsClientError(f"unsupported sample rate in daemon response: {sample_rate}")
    if channels not in (1, 2):
        raise MarsClientError(f"unsupported channel count in daemon response: {channels}")
    return AppVirtualInputSpec(
        id=_string(item.get("id"), "virtual input id"),
        name=_string(item.get("name"), "virtual input name"),
        uid=_string(item.get("uid"), "virtual input uid"),
        sample_rate=cast(Literal[48000], sample_rate),
        channels=cast(Literal[1, 2], channels),
    )


def _decode_producer(value: Any) -> VirtualInputProducerStatus:
    item = _object(value, "virtual input producer status")
    kind = _string(item.get("kind"), "producer kind")
    state = _string(item.get("state"), "producer state")
    if kind != "external_app":
        raise MarsClientError(f"unsupported producer kind in daemon response: {kind}")
    if state not in ("absent", "active", "stale", "underrunning"):
        raise MarsClientError(f"unsupported producer state in daemon response: {state}")
    return VirtualInputProducerStatus(
        app_id=_string(item.get("app_id"), "producer app id"),
        id=_string(item.get("id"), "producer id"),
        uid=_string(item.get("uid"), "producer uid"),
        kind="external_app",
        state=cast(ProducerState, state),
        write_idx=_int(item.get("write_idx"), "producer write index"),
        underrun_count=_int(item.get("underrun_count"), "producer underrun count"),
        attach_count=_int(item.get("attach_count"), "producer attach count"),
        generation=_int(item.get("generation"), "producer generation"),
    )


def _decode_virtual_mic(value: Any) -> VirtualMic:
    item = _object(value, "ensured virtual input")
    sample_rate = _int(item.get("sample_rate"), "sample rate")
    channels = _int(item.get("channels"), "channels")
    if sample_rate != 48_000:
        raise MarsClientError(f"unsupported sample rate in daemon response: {sample_rate}")
    if channels not in (1, 2):
        raise MarsClientError(f"unsupported channel count in daemon response: {channels}")
    info = VirtualMicInfo(
        uid=_string(item.get("uid"), "virtual input uid"),
        ring_name=_string(item.get("ring_name"), "ring name"),
        sample_rate=cast(Literal[48000], sample_rate),
        channels=cast(Literal[1, 2], channels),
        capacity_frames=_int(item.get("capacity_frames"), "capacity frames"),
        producer=_decode_producer(item.get("producer")),
    )
    return VirtualMic(info, item)


def _decode_apply_result(value: Any) -> ApplyResult:
    item = _object(value, "apply result")
    plan_item = _object(item.get("plan"), "apply plan")
    return ApplyResult(
        applied=_bool(item.get("applied"), "applied"),
        plan=ApplyPlan(
            changes=tuple(
                _decode_plan_change(change)
                for change in _list(plan_item.get("changes"), "plan changes")
            ),
            warnings=_strings(plan_item.get("warnings"), "plan warnings"),
        ),
        warnings=_strings(item.get("warnings"), "apply warnings"),
        errors=_strings(item.get("errors"), "apply errors"),
    )


def _decode_plan_change(value: Any) -> PlanChange:
    item = _object(value, "plan change")
    return PlanChange(
        kind=_string(item.get("kind"), "change kind"),
        target=_string(item.get("target"), "change target"),
        details=_string(item.get("details"), "change details"),
    )


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise MarsClientError(f"invalid daemon response: {label} must be an object")
    return value


def _list(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        raise MarsClientError(f"invalid daemon response: {label} must be an array")
    return value


def _string(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise MarsClientError(f"invalid daemon response: {label} must be a string")
    return value


def _int(value: Any, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool):
        raise MarsClientError(f"invalid daemon response: {label} must be an integer")
    return value


def _bool(value: Any, label: str) -> bool:
    if not isinstance(value, bool):
        raise MarsClientError(f"invalid daemon response: {label} must be a boolean")
    return value


def _strings(value: Any, label: str) -> tuple[str, ...]:
    return tuple(_string(item, label) for item in _list(value, label))
