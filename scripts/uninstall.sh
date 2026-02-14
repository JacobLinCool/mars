#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="/usr/local/bin"
HAL_DIR="/Library/Audio/Plug-Ins/HAL"
PLIST_ID="com.mars.marsd"
PLIST_PATH="$HOME/Library/LaunchAgents/com.mars.marsd.plist"

launchctl bootout "gui/$UID/$PLIST_ID" >/dev/null 2>&1 || true
rm -f "$PLIST_PATH"

rm -f "$BIN_DIR/mars" "$BIN_DIR/marsd"

echo "Removing HAL driver bundle (sudo may prompt)..."
sudo rm -rf "$HAL_DIR/mars.driver"

rm -rf "$HOME/Library/Caches/mars"

echo "MARS removed"
