#!/usr/bin/env bash
set -euo pipefail

LOG_FILE="$HOME/Library/Logs/mars/marsd.log"
mkdir -p "$(dirname "$LOG_FILE")"
touch "$LOG_FILE"
exec tail -f "$LOG_FILE"
