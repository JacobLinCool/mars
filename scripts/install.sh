#!/usr/bin/env bash
set -euo pipefail

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

install "$ROOT_DIR/target/release/mars" "$BIN_DIR/mars"
install "$ROOT_DIR/target/release/marsd" "$BIN_DIR/marsd"

echo "Installing HAL driver bundle (sudo may prompt)..."
sudo mkdir -p "$HAL_DIR"
sudo rm -rf "$HAL_DIR/mars.driver"
sudo cp -R "$ROOT_DIR/bundles/mars.driver" "$HAL_DIR/mars.driver"

mkdir -p "$LAUNCHD_DIR"
PLIST_TEMPLATE="$ROOT_DIR/launchd/com.mars.marsd.plist"
PLIST_DEST="$LAUNCHD_DIR/com.mars.marsd.plist"
sed \
  -e "s#__MARS_BIN__#$BIN_DIR#g" \
  -e "s#__HOME__#$HOME#g" \
  "$PLIST_TEMPLATE" > "$PLIST_DEST"

launchctl bootout "gui/$UID/$PLIST_ID" >/dev/null 2>&1 || true
launchctl bootstrap "gui/$UID" "$PLIST_DEST"
launchctl enable "gui/$UID/$PLIST_ID"
launchctl kickstart -k "gui/$UID/$PLIST_ID"

echo "MARS installed. Run: mars doctor"
