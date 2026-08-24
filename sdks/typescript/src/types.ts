export const PROTOCOL_VERSION = 3 as const;

export interface AppVirtualInputSpec {
  id: string;
  name: string;
  uid: string;
  sampleRate: 48000;
  channels: 1 | 2;
}

export type ProducerState = "absent" | "active" | "stale" | "underrunning";

export interface VirtualInputProducerStatus {
  appId: string;
  id: string;
  uid: string;
  kind: "external_app";
  state: ProducerState;
  writeIndex: number;
  underrunCount: number;
  attachCount: number;
  generation: number;
}

export interface ApplyResult {
  applied: boolean;
  plan: {
    changes: Array<{ kind: string; target: string; details: string }>;
    warnings: string[];
  };
  warnings: string[];
  errors: string[];
}

export interface AppVirtualInputs {
  appId: string;
  inputs: AppVirtualInputSpec[];
}

export interface VirtualMicInfo {
  uid: string;
  ringName: string;
  sampleRate: 48000;
  channels: 1 | 2;
  capacityFrames: number;
  producer: VirtualInputProducerStatus;
}

export type DaemonStatus = Record<string, unknown>;
