import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";

const binding = createRequire(import.meta.url)("../native/mars_sdk_node.node");

test("arm64 native writer addon loads and rejects an invalid daemon handle", () => {
  assert.equal(typeof binding.NativeLiveWriter, "function");
  assert.throws(
    () => new binding.NativeLiveWriter("{}"),
    /invalid virtual input handle/,
  );
});

test("native writer attaches and writes Float32 frames", () => {
  const writer = new binding.NativeLiveWriter(JSON.stringify(ensuredInput()));
  writer.clearUnread();
  assert.equal(writer.writeF32InterleavedLive(new Float32Array(32)), 32);
  assert.equal(writer.clearUnread(), 32);
  writer.flushSilence();
  writer.close();
  assert.throws(
    () => writer.writeF32InterleavedLive(new Float32Array(1)),
    /live writer is closed/,
  );
});

function ensuredInput() {
  return {
    uid: "com.mars.sdk.test.node",
    ring_name: "mars.vin.test.node.7f52b1c03a9d4e81",
    sample_rate: 48_000,
    channels: 1,
    capacity_frames: 64,
    producer: {
      app_id: "com.mars.sdk.test",
      id: "node",
      uid: "com.mars.sdk.test.node",
      kind: "external_app",
      state: "absent",
      write_idx: 0,
      underrun_count: 0,
      attach_count: 0,
      generation: 0,
    },
  };
}
