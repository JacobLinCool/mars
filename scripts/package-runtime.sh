#!/usr/bin/env bash
set -euo pipefail

# Builds a versioned, relocatable MARS runtime package:
#   dist/mars-runtime-<version>.tar.gz
# containing:
#   manifest.json   # { version, min_macos, protocol_version, files: [...] }
#   bin/mars  bin/marsd
#   launchd/com.mars.marsd.plist
#   driver/mars.driver/
#
# Install it on a target machine with: mars runtime install --package <tar.gz>

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# The workspace Cargo.toml does not define a shared version; the daemon crate
# version is the runtime version (it is what `marsd` and `mars doctor` report).
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT_DIR/crates/mars-daemon/Cargo.toml" | head -1)"
PROTOCOL_VERSION="$(sed -n 's/^pub const PROTOCOL_VERSION: u16 = \([0-9][0-9]*\);$/\1/p' "$ROOT_DIR/crates/mars-ipc/src/lib.rs" | head -1)"
MIN_MACOS="${MARS_MIN_MACOS:-15.0}"

if [[ -z "$VERSION" || -z "$PROTOCOL_VERSION" ]]; then
  echo "error: failed to determine runtime version ($VERSION) or protocol version ($PROTOCOL_VERSION)." >&2
  exit 1
fi

"$ROOT_DIR/scripts/build-driver.sh"

cargo build --release -p mars-cli -p mars-daemon

STAGE_DIR="$ROOT_DIR/dist/mars-runtime-$VERSION"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/launchd" "$STAGE_DIR/driver"

install -m 0755 "$ROOT_DIR/target/release/mars" "$STAGE_DIR/bin/mars"
install -m 0755 "$ROOT_DIR/target/release/marsd" "$STAGE_DIR/bin/marsd"
cp "$ROOT_DIR/launchd/com.mars.marsd.plist" "$STAGE_DIR/launchd/com.mars.marsd.plist"
cp -R "$ROOT_DIR/bundles/mars.driver" "$STAGE_DIR/driver/mars.driver"

# Sign the CLI binaries with the same Developer ID policy as the driver bundle
# (build-driver.sh already signed the bundle). Unsigned packages stay unsigned
# and must be installed with --allow-unsigned (development only).
DEV_ID="$(security find-identity -v -p codesigning 2>/dev/null | grep "Developer ID Application" | head -1 | sed 's/.*"\(.*\)"/\1/' || true)"
if [ -n "$DEV_ID" ]; then
  codesign --force --sign "$DEV_ID" --options runtime "$STAGE_DIR/bin/mars"
  codesign --force --sign "$DEV_ID" --options runtime "$STAGE_DIR/bin/marsd"
else
  echo "warning: no Developer ID Application certificate found; packaging unsigned binaries." >&2
  echo "warning: install will require --allow-unsigned (development only)." >&2
fi

# Generate manifest.json: sha256 for every file, codesign authority for the
# binaries and the driver bundle executable (read back from the artifacts —
# never invented).
python3 - "$STAGE_DIR" "$VERSION" "$MIN_MACOS" "$PROTOCOL_VERSION" <<'PY'
import hashlib
import json
import os
import subprocess
import sys

stage_dir, version, min_macos, protocol_version = sys.argv[1:5]

SIGNED_PATHS = {
    "bin/mars": "bin/mars",
    "bin/marsd": "bin/marsd",
    # codesign verifies bundles at the bundle root.
    "driver/mars.driver/Contents/MacOS/mars_hal": "driver/mars.driver",
}


def codesign_authority(target):
    result = subprocess.run(
        ["/usr/bin/codesign", "-d", "--verbose=2", os.path.join(stage_dir, target)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    for line in result.stderr.splitlines():
        if line.startswith("Authority="):
            return line[len("Authority="):].strip()
    return None


files = []
for root, _dirs, names in os.walk(stage_dir):
    for name in sorted(names):
        full = os.path.join(root, name)
        rel = os.path.relpath(full, stage_dir)
        if rel == "manifest.json" or name == ".DS_Store":
            continue
        digest = hashlib.sha256()
        with open(full, "rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        entry = {"path": rel, "sha256": digest.hexdigest(), "codesign_id": None}
        if rel in SIGNED_PATHS:
            entry["codesign_id"] = codesign_authority(SIGNED_PATHS[rel])
        files.append(entry)

files.sort(key=lambda entry: entry["path"])
manifest = {
    "version": version,
    "min_macos": min_macos,
    "protocol_version": int(protocol_version),
    "files": files,
}
with open(os.path.join(stage_dir, "manifest.json"), "w") as handle:
    json.dump(manifest, handle, indent=2)
    handle.write("\n")
print(f"wrote manifest.json with {len(files)} files")
PY

ARCHIVE="$ROOT_DIR/dist/mars-runtime-$VERSION.tar.gz"
rm -f "$ARCHIVE"
tar -czf "$ARCHIVE" -C "$STAGE_DIR" manifest.json bin launchd driver

echo "Built runtime package: $ARCHIVE"
