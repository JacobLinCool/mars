#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo build --release -p mars-hal

BUNDLE_DIR="$ROOT_DIR/bundles/mars.driver/Contents"
mkdir -p "$BUNDLE_DIR/MacOS"
cp "$ROOT_DIR/target/release/libmars_hal.dylib" "$BUNDLE_DIR/MacOS/mars_hal"

echo "Built driver bundle at $ROOT_DIR/bundles/mars.driver"
