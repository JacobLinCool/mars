#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo build --release -p mars-hal

BUNDLE_DIR="$ROOT_DIR/bundles/mars.driver/Contents"
mkdir -p "$BUNDLE_DIR/MacOS"
cp "$ROOT_DIR/target/release/libmars_hal.dylib" "$BUNDLE_DIR/MacOS/mars_hal"

# Rust cdylib produces MH_DYLIB, but CoreAudio's DriverHelper loads plugins via
# CFBundleLoadExecutable which requires MH_BUNDLE.  Patch the Mach-O header to
# change the filetype and remove the LC_ID_DYLIB load command (bundles must not
# have one — its presence causes CFBundleLoadExecutable to fail).
python3 -c "
import struct, sys

path = sys.argv[1]
with open(path, 'r+b') as f:
    data = bytearray(f.read())

magic = struct.unpack_from('<I', data, 0)[0]
if magic != 0xFEEDFACF:
    sys.exit('unexpected Mach-O magic')

MH_DYLIB = 6
MH_BUNDLE = 8
LC_ID_DYLIB = 0x0D

# --- 1. Patch filetype: DYLIB -> BUNDLE ---
filetype = struct.unpack_from('<I', data, 12)[0]
if filetype == MH_BUNDLE:
    print('already MH_BUNDLE')
elif filetype == MH_DYLIB:
    struct.pack_into('<I', data, 12, MH_BUNDLE)
    print('patched MH_DYLIB -> MH_BUNDLE')
else:
    sys.exit(f'unexpected filetype {filetype}')

# --- 2. Remove LC_ID_DYLIB load command ---
# Mach-O 64 header is 32 bytes.  Load commands follow immediately after.
ncmds = struct.unpack_from('<I', data, 16)[0]
sizeofcmds = struct.unpack_from('<I', data, 20)[0]
header_size = 32
offset = header_size

found = False
for i in range(ncmds):
    cmd = struct.unpack_from('<I', data, offset)[0]
    cmdsize = struct.unpack_from('<I', data, offset + 4)[0]
    if cmd == LC_ID_DYLIB:
        # Remove by shifting everything after this command forward
        end = offset + cmdsize
        lc_end = header_size + sizeofcmds
        data[offset:lc_end - cmdsize] = data[end:lc_end]
        # Zero-fill the freed space at the end of the load commands area
        data[lc_end - cmdsize:lc_end] = b'\x00' * cmdsize
        # Update header: decrement ncmds, reduce sizeofcmds
        struct.pack_into('<I', data, 16, ncmds - 1)
        struct.pack_into('<I', data, 20, sizeofcmds - cmdsize)
        found = True
        print(f'removed LC_ID_DYLIB ({cmdsize} bytes)')
        break
    offset += cmdsize

if not found:
    print('LC_ID_DYLIB not found (already removed?)')

with open(path, 'wb') as f:
    f.write(data)
" "$BUNDLE_DIR/MacOS/mars_hal"

# Code-sign the bundle with hardened runtime.  macOS 14+ loads HAL plugins
# out-of-process in com.apple.audio.DriverHelper which enforces library
# validation — only "Developer ID Application" certificates pass.
# "Apple Development" certs do NOT satisfy library validation; in that case
# the daemon falls back to stub mode automatically (virtual devices won't
# appear in the system audio list but IPC and profile logic still work).
SIGN_ID="-"  # fallback to adhoc
# Prefer Developer ID Application, then fall back to any available identity.
if DEV_ID=$(security find-identity -v -p codesigning 2>/dev/null | grep "Developer ID Application" | head -1 | sed 's/.*"\(.*\)"/\1/'); then
    if [ -n "$DEV_ID" ]; then
        SIGN_ID="$DEV_ID"
    fi
fi
if [ "$SIGN_ID" = "-" ]; then
    if FOUND_ID=$(security find-identity -v -p codesigning 2>/dev/null | head -1 | sed 's/.*"\(.*\)"/\1/'); then
        if [ -n "$FOUND_ID" ]; then
            SIGN_ID="$FOUND_ID"
        fi
    fi
fi
codesign --force --sign "$SIGN_ID" --deep --options runtime "$ROOT_DIR/bundles/mars.driver"

echo "Built driver bundle at $ROOT_DIR/bundles/mars.driver"
