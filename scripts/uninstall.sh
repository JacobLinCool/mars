#!/usr/bin/env bash
set -euo pipefail

if [[ "$EUID" -eq 0 ]]; then
  echo "error: run ./scripts/uninstall.sh as your normal user (no sudo)."
  echo "The script will prompt for sudo only when removing protected files."
  exit 1
fi

BIN_DIR="/usr/local/bin"
HAL_DIR="/Library/Audio/Plug-Ins/HAL"
PLIST_ID="com.mars.marsd"
PLIST_PATH="$HOME/Library/LaunchAgents/com.mars.marsd.plist"

launchctl bootout "gui/$UID/$PLIST_ID" >/dev/null 2>&1 || true
if [[ -e "$PLIST_PATH" && ! -w "$PLIST_PATH" ]]; then
  echo "Removing non-writable launch agent plist (sudo may prompt)..."
  sudo rm -f "$PLIST_PATH"
else
  rm -f "$PLIST_PATH"
fi

if [[ -w "$BIN_DIR" ]]; then
  rm -f "$BIN_DIR/mars" "$BIN_DIR/marsd"
else
  echo "Removing CLI binaries from $BIN_DIR (sudo may prompt)..."
  sudo rm -f "$BIN_DIR/mars" "$BIN_DIR/marsd"
fi

echo "Removing HAL driver bundle (sudo may prompt)..."
sudo rm -rf "$HAL_DIR/mars.driver"

echo "Reloading coreaudiod to unload HAL driver (sudo may prompt)..."
if ! sudo killall -9 coreaudiod; then
  echo "warning: failed to reload coreaudiod automatically."
  echo "Run manually: sudo killall -9 coreaudiod"
fi

rm -rf "$HOME/Library/Caches/mars"

echo "MARS removed"
