import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { DaemonError, MarsClient, ProtocolVersionError } from "../src/index.js";

interface RequestEnvelope {
  protocol_version: number;
  request_id: string;
  command: string;
  payload: unknown;
}

interface ResponseOverrides {
  protocol_version?: number;
  request_id?: string;
  command?: string;
  ok?: boolean;
  payload?: unknown;
  error?: string;
  exit_code?: number;
}

test("setVirtualInputs sends one complete app collection and returns handles", async () => {
  await withDaemon((request) => {
    assert.equal(request.protocol_version, 3);
    assert.equal(request.command, "set_virtual_inputs");
    assert.deepEqual(request.payload, {
      app_id: "com.example.recorder",
      inputs: [{
        id: "mic",
        name: "Recorder Mic",
        uid: "com.example.recorder.mic",
        sample_rate: 48_000,
        channels: 1,
      }],
    });
    return {
      payload: {
        apply: {
          applied: true,
          plan: { changes: [], warnings: [] },
          warnings: [],
          errors: [],
        },
        ensured_inputs: [ensuredInput()],
      },
    };
  }, async (socketPath) => {
    const client = new MarsClient({ socketPath });
    const outcome = await client.setVirtualInputs("com.example.recorder", [{
      id: "mic",
      name: "Recorder Mic",
      uid: "com.example.recorder.mic",
      sampleRate: 48_000,
      channels: 1,
    }]);

    assert.equal(outcome.apply.applied, true);
    assert.equal(outcome.virtualMics.length, 1);
    assert.equal(outcome.virtualMics[0]?.info.ringName, "mars.vin.test.capability");
    assert.equal(outcome.virtualMics[0]?.info.producer.appId, "com.example.recorder");
  });
});

test("empty set is sent as deletion of the app collection", async () => {
  await withDaemon((request) => {
    assert.equal(request.command, "set_virtual_inputs");
    assert.deepEqual(request.payload, { app_id: "com.example.recorder", inputs: [] });
    return {
      payload: {
        apply: {
          applied: false,
          plan: { changes: [], warnings: [] },
          warnings: [],
          errors: [],
        },
        ensured_inputs: [],
      },
    };
  }, async (socketPath) => {
    const outcome = await new MarsClient({ socketPath })
      .setVirtualInputs("com.example.recorder", []);
    assert.deepEqual(outcome.virtualMics, []);
  });
});

test("get and producer status decode snake_case daemon values", async () => {
  let calls = 0;
  await withDaemon((request) => {
    calls += 1;
    if (request.command === "get_virtual_inputs") {
      return {
        payload: {
          app_id: "com.example.recorder",
          inputs: [{
            id: "mic",
            name: "Recorder Mic",
            uid: "com.example.recorder.mic",
            sample_rate: 48_000,
            channels: 2,
          }],
        },
      };
    }
    assert.equal(request.command, "virtual_input_status");
    return { payload: ensuredInput().producer };
  }, async (socketPath) => {
    const client = new MarsClient({ socketPath });
    const declared = await client.getVirtualInputs("com.example.recorder");
    const producer = await client.virtualInputStatus("com.example.recorder", "mic");

    assert.equal(declared.inputs[0]?.sampleRate, 48_000);
    assert.equal(declared.inputs[0]?.channels, 2);
    assert.equal(producer.writeIndex, 42);
    assert.equal(producer.kind, "external_app");
  });
  assert.equal(calls, 2);
});

test("protocol mismatch fails explicitly", async () => {
  await withDaemon(() => ({ protocol_version: 4, payload: null }), async (socketPath) => {
    await assert.rejects(new MarsClient({ socketPath }).ping(), ProtocolVersionError);
  });
});

test("daemon failures retain their exit code", async () => {
  await withDaemon(() => ({
    ok: false,
    payload: null,
    error: "uid already exists",
    exit_code: 3,
  }), async (socketPath) => {
    await assert.rejects(
      new MarsClient({ socketPath }).ping(),
      (error: unknown) => error instanceof DaemonError && error.exitCode === 3,
    );
  });
});

async function withDaemon(
  respond: (request: RequestEnvelope) => ResponseOverrides,
  run: (socketPath: string) => Promise<void>,
): Promise<void> {
  const directory = await mkdtemp(join(tmpdir(), "mars-ts-sdk-"));
  const socketPath = join(directory, "marsd.sock");
  const server = createServer((socket) => {
    let requestBytes = Buffer.alloc(0);
    socket.on("data", (chunk: Buffer) => {
      requestBytes = Buffer.concat([requestBytes, chunk]);
      const newline = requestBytes.indexOf(0x0a);
      if (newline < 0) return;
      const request = JSON.parse(requestBytes.subarray(0, newline).toString("utf8")) as RequestEnvelope;
      const override = respond(request);
      socket.end(`${JSON.stringify({
        protocol_version: override.protocol_version ?? 3,
        request_id: override.request_id ?? request.request_id,
        command: override.command ?? request.command,
        ok: override.ok ?? true,
        payload: override.payload ?? null,
        error: override.error,
        exit_code: override.exit_code,
      })}\n`);
    });
  });

  await listen(server, socketPath);
  try {
    await run(socketPath);
  } finally {
    await close(server);
    await rm(directory, { recursive: true });
  }
}

function listen(server: Server, socketPath: string): Promise<void> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, resolve);
  });
}

function close(server: Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });
}

function ensuredInput() {
  return {
    uid: "com.example.recorder.mic",
    ring_name: "mars.vin.test.capability",
    sample_rate: 48_000,
    channels: 1,
    capacity_frames: 4_096,
    producer: {
      app_id: "com.example.recorder",
      id: "mic",
      uid: "com.example.recorder.mic",
      kind: "external_app",
      state: "active",
      write_idx: 42,
      underrun_count: 2,
      attach_count: 1,
      generation: 1,
    },
  } as const;
}
