#!/usr/bin/env bash
set -euo pipefail

if [[ "$EUID" -eq 0 ]]; then
  echo "error: run ./scripts/install.sh as your normal user (no sudo)."
  echo "The script will prompt for sudo only when installing protected files."
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="/usr/local/bin"
HAL_DIR="/Library/Audio/Plug-Ins/HAL"
LAUNCHD_DIR="$HOME/Library/LaunchAgents"
PLIST_ID="com.mars.marsd"

mkdir -p "$HOME/Library/Logs/mars"
mkdir -p "$HOME/Library/Caches/mars"
mkdir -p "$HOME/Library/Application Support/mars/profiles"

"$ROOT_DIR/scripts/build-driver.sh"

cargo build --release -p mars-cli -p mars-daemon

if [[ -w "$BIN_DIR" ]]; then
  install "$ROOT_DIR/target/release/mars" "$BIN_DIR/mars"
  install "$ROOT_DIR/target/release/marsd" "$BIN_DIR/marsd"
else
  echo "Installing CLI binaries to $BIN_DIR (sudo may prompt)..."
  sudo mkdir -p "$BIN_DIR"
  sudo install "$ROOT_DIR/target/release/mars" "$BIN_DIR/mars"
  sudo install "$ROOT_DIR/target/release/marsd" "$BIN_DIR/marsd"
fi

echo "Installing HAL driver bundle (sudo may prompt)..."
sudo mkdir -p "$HAL_DIR"
sudo rm -rf "$HAL_DIR/mars.driver"
sudo cp -R "$ROOT_DIR/bundles/mars.driver" "$HAL_DIR/mars.driver"

echo "Reloading coreaudiod to pick up HAL driver (sudo may prompt)..."
if ! sudo killall -9 coreaudiod; then
  echo "warning: failed to reload coreaudiod automatically."
  echo "Run manually: sudo killall -9 coreaudiod"
fi

mkdir -p "$LAUNCHD_DIR"
PLIST_TEMPLATE="$ROOT_DIR/launchd/com.mars.marsd.plist"
PLIST_DEST="$LAUNCHD_DIR/com.mars.marsd.plist"

if [[ -e "$PLIST_DEST" && ! -w "$PLIST_DEST" ]]; then
  echo "Removing non-writable launch agent plist (sudo may prompt)..."
  sudo rm -f "$PLIST_DEST"
fi

sed \
  -e "s#__MARS_BIN__#$BIN_DIR#g" \
  -e "s#__HOME__#$HOME#g" \
  "$PLIST_TEMPLATE" > "$PLIST_DEST"

launchctl bootout "gui/$UID/$PLIST_ID" >/dev/null 2>&1 || true
launchctl bootstrap "gui/$UID" "$PLIST_DEST"
launchctl enable "gui/$UID/$PLIST_ID"
launchctl kickstart -k "gui/$UID/$PLIST_ID"

echo "MARS installed. Run: mars doctor"
