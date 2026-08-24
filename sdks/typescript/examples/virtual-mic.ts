import { setTimeout as delay } from "node:timers/promises";

import { MarsClient } from "../src/index.js";

const sampleRate = 48_000;
const chunkFrames = 480;
const client = new MarsClient();
const outcome = await client.setVirtualInputs("com.example.typescript", [{
  id: "mic",
  name: "TypeScript Virtual Mic",
  uid: "com.example.typescript.mic",
  sampleRate,
  channels: 1,
}]);
const mic = outcome.virtualMics[0];
if (mic === undefined) throw new Error("daemon did not return a virtual mic handle");

const writer = mic.openLiveWriter();
const chunk = new Float32Array(chunkFrames);
let phase = 0;

try {
  for (let index = 0; index < 500; index += 1) {
    for (let frame = 0; frame < chunkFrames; frame += 1) {
      chunk[frame] = 0.25 * Math.sin(phase);
      phase = (phase + 2 * Math.PI * 440 / sampleRate) % (2 * Math.PI);
    }
    writer.writeF32InterleavedLive(chunk);
    await delay(10);
  }
  writer.flushSilence();
} finally {
  writer.close();
}
