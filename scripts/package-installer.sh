#!/usr/bin/env bash
set -euo pipefail

# Build a signed, native-architecture macOS installer package from the MARS
# runtime payload. Release CI notarizes and staples the resulting flat package.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT_DIR/crates/mars-daemon/Cargo.toml" | head -1)"
ARCH="$(uname -m)"
case "$ARCH" in
  arm64) ;;
  *)
    echo "error: unsupported release architecture: $ARCH" >&2
    exit 1
    ;;
esac

INSTALLER_ID="${MARS_DEVELOPER_ID_INSTALLER:-}"
if [ -z "$INSTALLER_ID" ]; then
  INSTALLER_ID="$(security find-identity -v -p basic 2>/dev/null | grep "Developer ID Installer" | head -1 | sed 's/.*"\(.*\)"/\1/' || true)"
fi
if [ -z "$INSTALLER_ID" ]; then
  echo "error: no Developer ID Installer certificate is available." >&2
  exit 1
fi

MARS_REQUIRE_SIGNING=1 "$ROOT_DIR/scripts/package-runtime.sh"

RUNTIME_DIR="$ROOT_DIR/dist/mars-runtime-$VERSION"
WORK_DIR="$ROOT_DIR/dist/installer-$VERSION-$ARCH"
PAYLOAD_DIR="$WORK_DIR/payload"
PACKAGE_PATH="$ROOT_DIR/dist/mars-$VERSION-$ARCH.pkg"

rm -rf "$WORK_DIR"
mkdir -p \
  "$PAYLOAD_DIR/usr/local/bin" \
  "$PAYLOAD_DIR/usr/local/share/mars" \
  "$PAYLOAD_DIR/Library/Audio/Plug-Ins/HAL"

install -m 0755 "$RUNTIME_DIR/bin/mars" "$PAYLOAD_DIR/usr/local/bin/mars"
install -m 0755 "$RUNTIME_DIR/bin/marsd" "$PAYLOAD_DIR/usr/local/bin/marsd"
install -m 0644 "$RUNTIME_DIR/manifest.json" "$PAYLOAD_DIR/usr/local/share/mars/manifest.json"
install -m 0644 \
  "$RUNTIME_DIR/launchd/com.mars.marsd.plist" \
  "$PAYLOAD_DIR/usr/local/share/mars/com.mars.marsd.plist.in"
cp -R "$RUNTIME_DIR/driver/mars.driver" "$PAYLOAD_DIR/Library/Audio/Plug-Ins/HAL/mars.driver"

rm -f "$PACKAGE_PATH"
pkgbuild \
  --root "$PAYLOAD_DIR" \
  --scripts "$ROOT_DIR/packaging/macos/scripts" \
  --identifier "com.mars.runtime" \
  --version "$VERSION" \
  --install-location "/" \
  --ownership recommended \
  --sign "$INSTALLER_ID" \
  "$PACKAGE_PATH"

pkgutil --check-signature "$PACKAGE_PATH"
echo "Built signed installer: $PACKAGE_PATH"
