# TypeScript and Python SDKs

Both language SDKs expose the same app-owned virtual-input contract as
`mars-sdk`: one app atomically replaces its complete declaration, receives
daemon-issued virtual-microphone handles, and attaches one live writer per
input.

The control plane is implemented with each language's native Unix-socket and
JSON support. The data plane is implemented by small native bindings that
delegate to the Rust `mars-sdk::LiveWriter`; TypeScript and Python never
reimplement the shared-memory ring protocol or capability naming.

The SDKs live in `sdks/typescript` and `sdks/python`. Both are package-ready
for arm64 macOS and intentionally expose only:

- daemon `ping` and `status`;
- complete-set `setVirtualInputs` / `set_virtual_inputs`;
- declarative `getVirtualInputs` / `get_virtual_inputs`;
- read-only producer status; and
- a live Float32 interleaved writer returned from a daemon-issued handle.

The TypeScript surface uses camelCase and accepts `Float32Array`. The Python
surface uses snake_case, is async, and accepts C-contiguous Float32 buffers
such as `array("f")`. See each package README and its `examples` directory for
a complete producer.

IPC protocol v3, 48 kHz, mono/stereo, stable globally unique UIDs, and
complete-set replacement are public contract. Protocol mismatches and invalid
responses fail explicitly; neither SDK contains a legacy protocol or a
JavaScript/Python ring fallback.
