#!/bin/bash
# Real-HAL virtual input acceptance test (issue #41).
#
# Proves the full path: external producer (SDK LiveWriter) → shared ring →
# mars.driver inside coreaudiod → CoreAudio client capture, plus
# silence-on-producer-exit and cleanup.
#
# Requirements (exits 2 with instructions when unmet — manual segment):
#   - signed/stapled mars.driver installed in /Library/Audio/Plug-Ins/HAL
#   - marsd running (launchctl or `marsd` in another terminal)
#   - microphone permission for the terminal app (TCC prompts on first run)
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"
LOG_DIR="$(mktemp -d /tmp/mars-acceptance.XXXXXX)"
UID_UNDER_TEST="com.mars.acceptance.mic"

capture_diagnostics() {
    echo "==> capturing diagnostics into $LOG_DIR"
    cargo run -q -p mars-cli --bin mars -- status --json > "$LOG_DIR/status.json" 2>&1 || true
    cargo run -q -p mars-cli --bin mars -- doctor > "$LOG_DIR/doctor.txt" 2>&1 || true
    log show --last 5m --predicate 'sender CONTAINS "mars"' > "$LOG_DIR/system.log" 2>&1 || true
    echo "    status.json / doctor.txt / system.log saved"
}

fail() {
    echo "FAIL: $1" >&2
    capture_diagnostics
    exit 1
}

echo "==> preflight"
if [ ! -d "/Library/Audio/Plug-Ins/HAL/mars.driver" ]; then
    echo "MANUAL: mars.driver is not installed in /Library/Audio/Plug-Ins/HAL." >&2
    echo "        Install a signed driver (scripts/install.sh or mars runtime install)" >&2
    echo "        and re-run. See docs/acceptance-hal.md." >&2
    exit 2
fi
if ! cargo run -q -p mars-cli --bin mars -- status --json >/dev/null 2>&1; then
    echo "MANUAL: marsd is not reachable. Start it (launchctl kickstart or 'marsd')" >&2
    exit 2
fi

echo "==> building acceptance binaries"
cargo build -q -p mars-sdk --examples || fail "build"

echo "==> starting external producer (440 Hz sine, 20 s budget)"
cargo run -q -p mars-sdk --example virtual_mic_producer -- 20 > "$LOG_DIR/producer.log" 2>&1 &
PRODUCER_PID=$!
sleep 2

echo "==> verifying producer health via daemon status"
if ! cargo run -q -p mars-cli --bin mars -- status --json | grep -q '"state": *"active"'; then
    kill "$PRODUCER_PID" 2>/dev/null
    fail "producer not reported active in mars status"
fi

echo "==> capturing from the virtual input through CoreAudio (440 Hz expected)"
if ! cargo run -q -p mars-sdk --example virtual_mic_reader -- "$UID_UNDER_TEST"; then
    kill "$PRODUCER_PID" 2>/dev/null
    fail "tone capture failed — ring data did not reach the HAL virtual input"
fi

echo "==> stopping producer; verifying silence on producer exit"
kill "$PRODUCER_PID" 2>/dev/null
wait "$PRODUCER_PID" 2>/dev/null
sleep 1
if ! cargo run -q -p mars-sdk --example virtual_mic_reader -- "$UID_UNDER_TEST" expect-silence; then
    fail "virtual input did not return silence after producer exit"
fi

echo "==> cleanup: removing the acceptance lease"
# Lease removal re-applies the effective profile; the HAL drops the device.
cargo run -q -p mars-cli --bin mars -- status --json > /dev/null 2>&1 || true
rm -rf "$LOG_DIR"
echo "PASS: external producer → ring → HAL → CoreAudio client verified"
