# MARS Python SDK

Arm64 macOS SDK for applications that own MARS virtual input devices. The
async control client uses IPC protocol v3 and its native extension delegates
shared-memory writes to the existing Rust SDK.

## Install

```bash
uv add mars-sdk
```

The wheel requires Apple Silicon macOS, CPython 3.11 or newer, and an installed
MARS runtime.

## Build from this repository

```bash
uv sync --locked
```

Run the complete example with `uv run python examples/virtual_mic.py`.

## Declare and write a virtual microphone

```python
from array import array
from mars_sdk import AppVirtualInputSpec, MarsClient

client = MarsClient()
result = await client.set_virtual_inputs(
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

writer = result.virtual_mics[0].open_live_writer()
writer.write_f32_interleaved_live(array("f", [0.0] * 480))
writer.flush_silence()
writer.close()
```

`set_virtual_inputs` atomically replaces the app's complete collection. Pass
an empty list to delete that app's collection. Declarations survive daemon
restart and `mars clear`; submit the same complete collection again when the
app needs fresh writer handles.

The public control API is `ping`, `status`, `set_virtual_inputs`,
`get_virtual_inputs`, and read-only `virtual_input_status`. Inputs are fixed
at 48 kHz and support mono or stereo. Each `id` is app-local; each `uid` must
be globally unique. The Python writer accepts a C-contiguous Float32 buffer
and copies each submitted chunk into native memory before writing it.
