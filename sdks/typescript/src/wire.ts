import type {
  AppVirtualInputSpec,
  AppVirtualInputs,
  ApplyResult,
  VirtualInputProducerStatus,
  VirtualMicInfo,
} from "./types.js";
import { MarsClientError } from "./errors.js";

export interface WireAppVirtualInputSpec {
  id: string;
  name: string;
  uid: string;
  sample_rate: number;
  channels: number;
}

export interface WireVirtualInputProducerStatus {
  app_id: string;
  id: string;
  uid: string;
  kind: "external_app";
  state: VirtualInputProducerStatus["state"];
  write_idx: number;
  underrun_count: number;
  attach_count: number;
  generation: number;
}

export interface WireEnsuredVirtualInput {
  uid: string;
  ring_name: string;
  sample_rate: number;
  channels: number;
  capacity_frames: number;
  producer: WireVirtualInputProducerStatus;
}

export function encodeSpec(spec: AppVirtualInputSpec): WireAppVirtualInputSpec {
  return {
    id: spec.id,
    name: spec.name,
    uid: spec.uid,
    sample_rate: spec.sampleRate,
    channels: spec.channels,
  };
}

export function decodeSpec(value: unknown): AppVirtualInputSpec {
  const record = object(value, "virtual input spec");
  return {
    id: string(record.id, "virtual input id"),
    name: string(record.name, "virtual input name"),
    uid: string(record.uid, "virtual input uid"),
    sampleRate: literal(number(record.sample_rate, "sample rate"), 48_000, "sample rate"),
    channels: oneOf(number(record.channels, "channels"), [1, 2] as const, "channels"),
  };
}

export function decodeProducer(value: unknown): VirtualInputProducerStatus {
  const record = object(value, "virtual input producer status");
  return {
    appId: string(record.app_id, "producer app id"),
    id: string(record.id, "producer id"),
    uid: string(record.uid, "producer uid"),
    kind: literal(string(record.kind, "producer kind"), "external_app", "producer kind"),
    state: oneOf(
      string(record.state, "producer state"),
      ["absent", "active", "stale", "underrunning"] as const,
      "producer state",
    ),
    writeIndex: number(record.write_idx, "producer write index"),
    underrunCount: number(record.underrun_count, "producer underrun count"),
    attachCount: number(record.attach_count, "producer attach count"),
    generation: number(record.generation, "producer generation"),
  };
}

export function decodeVirtualMic(value: unknown): {
  info: VirtualMicInfo;
  wire: WireEnsuredVirtualInput;
} {
  const record = object(value, "ensured virtual input");
  const wire: WireEnsuredVirtualInput = {
    uid: string(record.uid, "virtual input uid"),
    ring_name: string(record.ring_name, "ring name"),
    sample_rate: number(record.sample_rate, "sample rate"),
    channels: number(record.channels, "channels"),
    capacity_frames: number(record.capacity_frames, "capacity frames"),
    producer: decodeWireProducer(record.producer),
  };
  return {
    wire,
    info: {
      uid: wire.uid,
      ringName: wire.ring_name,
      sampleRate: literal(wire.sample_rate, 48_000, "sample rate"),
      channels: oneOf(wire.channels, [1, 2] as const, "channels"),
      capacityFrames: wire.capacity_frames,
      producer: decodeProducer(wire.producer),
    },
  };
}

export function decodeAppVirtualInputs(value: unknown): AppVirtualInputs {
  const record = object(value, "app virtual inputs");
  return {
    appId: string(record.app_id, "app id"),
    inputs: array(record.inputs, "virtual inputs").map(decodeSpec),
  };
}

export function decodeApplyResult(value: unknown): ApplyResult {
  const record = object(value, "apply result");
  const plan = object(record.plan, "apply plan");
  return {
    applied: boolean(record.applied, "applied"),
    plan: {
      changes: array(plan.changes, "plan changes").map((change) => {
        const item = object(change, "plan change");
        return {
          kind: string(item.kind, "change kind"),
          target: string(item.target, "change target"),
          details: string(item.details, "change details"),
        };
      }),
      warnings: stringArray(plan.warnings, "plan warnings"),
    },
    warnings: stringArray(record.warnings, "apply warnings"),
    errors: stringArray(record.errors, "apply errors"),
  };
}

export function object(value: unknown, label: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new MarsClientError(`invalid daemon response: ${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function decodeWireProducer(value: unknown): WireVirtualInputProducerStatus {
  const decoded = decodeProducer(value);
  return {
    app_id: decoded.appId,
    id: decoded.id,
    uid: decoded.uid,
    kind: decoded.kind,
    state: decoded.state,
    write_idx: decoded.writeIndex,
    underrun_count: decoded.underrunCount,
    attach_count: decoded.attachCount,
    generation: decoded.generation,
  };
}

function array(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new MarsClientError(`invalid daemon response: ${label} must be an array`);
  }
  return value;
}

function stringArray(value: unknown, label: string): string[] {
  return array(value, label).map((item) => string(item, label));
}

function string(value: unknown, label: string): string {
  if (typeof value !== "string") {
    throw new MarsClientError(`invalid daemon response: ${label} must be a string`);
  }
  return value;
}

function number(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new MarsClientError(`invalid daemon response: ${label} must be a finite number`);
  }
  return value;
}

function boolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") {
    throw new MarsClientError(`invalid daemon response: ${label} must be a boolean`);
  }
  return value;
}

function literal<T extends string | number>(value: string | number, expected: T, label: string): T {
  if (value !== expected) {
    throw new MarsClientError(`invalid daemon response: ${label} must be ${expected}`);
  }
  return expected;
}

function oneOf<T extends readonly (string | number)[]>(
  value: string | number,
  expected: T,
  label: string,
): T[number] {
  if (!expected.includes(value)) {
    throw new MarsClientError(`invalid daemon response: unsupported ${label} ${value}`);
  }
  return value as T[number];
}
