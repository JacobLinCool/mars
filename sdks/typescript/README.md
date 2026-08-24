# MARS TypeScript SDK

Arm64 macOS SDK for applications that own MARS virtual input devices. It uses
IPC protocol v3 for declarations and the same Rust shared-memory writer as the
daemon; JavaScript does not implement the ring protocol.

## Install

```bash
pnpm add @mars-audio/sdk
```

The package requires Apple Silicon macOS, Node.js 20 or newer, and an installed
MARS runtime.

## Build from this repository

```bash
pnpm install --frozen-lockfile
pnpm run build:native
pnpm run build
```

## Declare and write a virtual microphone

```ts
import { MarsClient } from "@mars-audio/sdk";

const client = new MarsClient();
const result = await client.setVirtualInputs("com.example.recorder", [{
  id: "mic",
  name: "Recorder Mic",
  uid: "com.example.recorder.mic",
  sampleRate: 48_000,
  channels: 1,
}]);

const writer = result.virtualMics[0].openLiveWriter();
writer.writeF32InterleavedLive(new Float32Array(480));
writer.flushSilence();
writer.close();
```

`setVirtualInputs` replaces the app's complete collection atomically. Pass an
empty array to delete that app's collection. Declarations survive daemon
restart and `mars clear`; submit the same complete collection again when the
app needs fresh writer handles.

The public control API is `ping`, `status`, `setVirtualInputs`,
`getVirtualInputs`, and read-only `virtualInputStatus`. Inputs are fixed at
48 kHz and support mono or stereo. Each `id` is app-local; each `uid` must be
globally unique.
