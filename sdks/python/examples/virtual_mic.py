import asyncio
from array import array
import math

from mars_sdk import AppVirtualInputSpec, MarsClient


async def main() -> None:
    sample_rate = 48_000
    chunk_frames = 480
    client = MarsClient()
    outcome = await client.set_virtual_inputs(
        "com.example.python",
        [
            AppVirtualInputSpec(
                id="mic",
                name="Python Virtual Mic",
                uid="com.example.python.mic",
                sample_rate=sample_rate,
                channels=1,
            )
        ],
    )

    writer = outcome.virtual_mics[0].open_live_writer()
    chunk = array("f", [0.0]) * chunk_frames
    phase = 0.0
    try:
        for _ in range(500):
            for frame in range(chunk_frames):
                chunk[frame] = 0.25 * math.sin(phase)
                phase = (phase + math.tau * 440 / sample_rate) % math.tau
            writer.write_f32_interleaved_live(chunk)
            await asyncio.sleep(0.01)
        writer.flush_silence()
    finally:
        writer.close()


asyncio.run(main())
