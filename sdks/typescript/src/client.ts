import { randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { join } from "node:path";
import { createConnection } from "node:net";

import type {
  AppVirtualInputSpec,
  AppVirtualInputs,
  ApplyResult,
  DaemonStatus,
  VirtualInputProducerStatus,
} from "./types.js";
import { DaemonError, MarsClientError, ProtocolVersionError } from "./errors.js";
import { VirtualMic } from "./virtual-input.js";
import {
  decodeAppVirtualInputs,
  decodeApplyResult,
  decodeProducer,
  decodeVirtualMic,
  encodeSpec,
  object,
} from "./wire.js";
import { PROTOCOL_VERSION } from "./types.js";

const DEFAULT_TIMEOUT_MS = 5_000;
const MAX_RESPONSE_BYTES = 16 * 1024 * 1024;

interface ResponseEnvelope {
  protocol_version: number;
  request_id: string;
  command: string;
  ok: boolean;
  payload: unknown;
  error?: string | null;
  exit_code?: number | null;
}

export interface MarsClientOptions {
  socketPath?: string;
  timeoutMs?: number;
}

export interface SetVirtualInputsOutcome {
  apply: ApplyResult;
  virtualMics: VirtualMic[];
}

export class MarsClient {
  public readonly socketPath: string;
  public readonly timeoutMs: number;

  public constructor(options: MarsClientOptions = {}) {
    this.socketPath = options.socketPath
      ?? join(homedir(), "Library", "Caches", "mars", "marsd.sock");
    this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    if (!Number.isFinite(this.timeoutMs) || this.timeoutMs <= 0) {
      throw new RangeError("timeoutMs must be a positive finite number");
    }
  }

  public async ping(): Promise<void> {
    await this.request("ping", null);
  }

  public async status(): Promise<DaemonStatus> {
    return object(await this.request("status", null), "daemon status");
  }

  public async setVirtualInputs(
    appId: string,
    inputs: AppVirtualInputSpec[],
  ): Promise<SetVirtualInputsOutcome> {
    const payload = await this.request("set_virtual_inputs", {
      app_id: appId,
      inputs: inputs.map(encodeSpec),
    });
    const result = object(payload, "set virtual inputs result");
    return {
      apply: decodeApplyResult(result.apply),
      virtualMics: array(result.ensured_inputs, "ensured virtual inputs").map((item) => {
        const decoded = decodeVirtualMic(item);
        return new VirtualMic(decoded.info, decoded.wire);
      }),
    };
  }

  public async getVirtualInputs(appId: string): Promise<AppVirtualInputs> {
    return decodeAppVirtualInputs(
      await this.request("get_virtual_inputs", { app_id: appId }),
    );
  }

  public async virtualInputStatus(
    appId: string,
    id: string,
  ): Promise<VirtualInputProducerStatus> {
    return decodeProducer(
      await this.request("virtual_input_status", { app_id: appId, id }),
    );
  }

  private request(command: string, payload: unknown): Promise<unknown> {
    const requestId = randomUUID();
    const encoded = `${JSON.stringify({
      protocol_version: PROTOCOL_VERSION,
      request_id: requestId,
      command,
      payload,
    })}\n`;

    return new Promise((resolve, reject) => {
      const socket = createConnection(this.socketPath);
      const chunks: Buffer[] = [];
      let byteLength = 0;
      let settled = false;

      const fail = (error: Error): void => {
        if (!settled) {
          settled = true;
          socket.destroy();
          reject(error);
        }
      };

      socket.setTimeout(this.timeoutMs);
      socket.once("connect", () => socket.write(encoded));
      socket.once("timeout", () => fail(new MarsClientError(
        `request ${command} timed out after ${this.timeoutMs} ms`,
      )));
      socket.once("error", (error) => fail(new MarsClientError(
        `IPC request ${command} failed: ${error.message}`,
      )));
      socket.on("data", (chunk: Buffer) => {
        chunks.push(chunk);
        byteLength += chunk.byteLength;
        if (byteLength > MAX_RESPONSE_BYTES) {
          fail(new MarsClientError("daemon response exceeds 16 MiB"));
          return;
        }
        const response = Buffer.concat(chunks, byteLength);
        const newline = response.indexOf(0x0a);
        if (newline < 0 || settled) {
          return;
        }
        try {
          const envelope = decodeEnvelope(response.subarray(0, newline).toString("utf8"));
          validateEnvelope(envelope, requestId, command);
          settled = true;
          socket.destroy();
          resolve(envelope.payload);
        } catch (error) {
          fail(error instanceof Error ? error : new MarsClientError(String(error)));
        }
      });
      socket.once("end", () => {
        if (!settled) {
          fail(new MarsClientError("daemon closed the connection before sending a response"));
        }
      });
    });
  }
}

function decodeEnvelope(line: string): ResponseEnvelope {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    throw new MarsClientError(`daemon returned invalid JSON: ${detail}`);
  }
  const record = object(parsed, "response envelope");
  if (
    typeof record.protocol_version !== "number"
    || typeof record.request_id !== "string"
    || typeof record.command !== "string"
    || typeof record.ok !== "boolean"
    || !("payload" in record)
  ) {
    throw new MarsClientError("daemon returned an invalid response envelope");
  }
  if (record.error !== undefined && record.error !== null && typeof record.error !== "string") {
    throw new MarsClientError("daemon returned an invalid error field");
  }
  if (
    record.exit_code !== undefined
    && record.exit_code !== null
    && (typeof record.exit_code !== "number" || !Number.isInteger(record.exit_code))
  ) {
    throw new MarsClientError("daemon returned an invalid exit code");
  }
  return record as unknown as ResponseEnvelope;
}

function validateEnvelope(envelope: ResponseEnvelope, requestId: string, command: string): void {
  if (envelope.protocol_version !== PROTOCOL_VERSION) {
    throw new ProtocolVersionError(
      `protocol version mismatch: SDK uses ${PROTOCOL_VERSION}, daemon uses ${envelope.protocol_version}`,
    );
  }
  if (envelope.request_id !== requestId) {
    throw new MarsClientError("daemon response request id does not match the request");
  }
  if (envelope.command !== command) {
    throw new MarsClientError(
      `daemon response command mismatch: expected ${command}, got ${envelope.command}`,
    );
  }
  if (!envelope.ok) {
    throw new DaemonError(
      envelope.error ?? "unknown daemon error",
      envelope.exit_code ?? undefined,
    );
  }
}

function array(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new MarsClientError(`invalid daemon response: ${label} must be an array`);
  }
  return value;
}
