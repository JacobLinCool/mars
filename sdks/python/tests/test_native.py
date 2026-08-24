from array import array
import json
import unittest

from mars_sdk._native import LiveWriter


class NativeWriterTests(unittest.TestCase):
    def test_addon_loads_and_rejects_invalid_daemon_handle(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "invalid virtual input handle"):
            LiveWriter("{}")

    def test_writer_attaches_and_writes_float32_frames(self) -> None:
        writer = LiveWriter(json.dumps(ensured_input()))
        writer.clear_unread()
        self.assertEqual(
            writer.write_f32_interleaved_live(array("f", [0.0] * 32)), 32
        )
        self.assertEqual(writer.clear_unread(), 32)
        writer.flush_silence()
        writer.close()
        with self.assertRaisesRegex(RuntimeError, "live writer is closed"):
            writer.write_f32_interleaved_live(array("f", [0.0]))


def ensured_input():
    return {
        "uid": "com.mars.sdk.test.python",
        "ring_name": "mars.vin.test.python.836fb1d04a29ce75",
        "sample_rate": 48_000,
        "channels": 1,
        "capacity_frames": 64,
        "producer": {
            "app_id": "com.mars.sdk.test",
            "id": "python",
            "uid": "com.mars.sdk.test.python",
            "kind": "external_app",
            "state": "absent",
            "write_idx": 0,
            "underrun_count": 0,
            "attach_count": 0,
            "generation": 0,
        },
    }


if __name__ == "__main__":
    unittest.main()
