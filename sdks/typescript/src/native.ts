import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

export interface NativeLiveWriterHandle {
  writeF32InterleavedLive(frames: Float32Array): number;
  clearUnread(): number;
  flushSilence(): void;
  close(): void;
}

interface NativeBinding {
  NativeLiveWriter: new (ensuredJson: string) => NativeLiveWriterHandle;
}

let cachedBinding: NativeBinding | undefined;

export function createNativeLiveWriter(ensuredJson: string): NativeLiveWriterHandle {
  const binding = cachedBinding ?? loadBinding();
  cachedBinding = binding;
  return new binding.NativeLiveWriter(ensuredJson);
}

function loadBinding(): NativeBinding {
  const nativePath = process.env.MARS_SDK_NATIVE_PATH
    ?? fileURLToPath(new URL("../../native/mars_sdk_node.node", import.meta.url));
  try {
    return createRequire(import.meta.url)(nativePath) as NativeBinding;
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    throw new Error(`unable to load the MARS native writer at ${nativePath}: ${detail}`);
  }
}
