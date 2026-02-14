#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export MARS_ALLOW_DRIVER_STUB=1
cargo run -p mars-daemon --bin marsd -- --serve
