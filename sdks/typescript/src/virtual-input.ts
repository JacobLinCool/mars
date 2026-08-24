import type { VirtualMicInfo } from "./types.js";
import { createNativeLiveWriter, type NativeLiveWriterHandle } from "./native.js";
import type { WireEnsuredVirtualInput } from "./wire.js";

export class LiveWriter {
  private closed = false;

  /** @internal */
  public constructor(private readonly native: NativeLiveWriterHandle) {}

  public writeF32InterleavedLive(frames: Float32Array): number {
    this.assertOpen();
    return this.native.writeF32InterleavedLive(frames);
  }

  public clearUnread(): number {
    this.assertOpen();
    return this.native.clearUnread();
  }

  public flushSilence(): void {
    this.assertOpen();
    this.native.flushSilence();
  }

  public close(): void {
    if (!this.closed) {
      this.native.close();
      this.closed = true;
    }
  }

  private assertOpen(): void {
    if (this.closed) {
      throw new Error("live writer is closed");
    }
  }
}

export class VirtualMic {
  /** @internal */
  public constructor(
    public readonly info: VirtualMicInfo,
    private readonly wireInfo: WireEnsuredVirtualInput,
  ) {}

  public openLiveWriter(): LiveWriter {
    return new LiveWriter(createNativeLiveWriter(JSON.stringify(this.wireInfo)));
  }
}
