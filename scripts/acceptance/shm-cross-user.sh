#!/bin/bash
# Cross-user shared-memory ring acceptance test (issue #39).
#
# Proves that a ring created by one user can be opened read-write by a
# different user — the same boundary as marsd (logged-in user) vs the HAL
# plug-in (coreaudiod, `_coreaudiod`).
#
# Requirements:
#   - run as an admin user able to sudo
#   - a second local user to act as the peer (default: nobody)
#   - cargo available for both users (the test binary is prebuilt as the
#     invoking user and executed by the peer)
#
# Usage:
#   scripts/acceptance/shm-cross-user.sh [peer-user]
set -euo pipefail

PEER_USER="${1:-nobody}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HELPER_DIR="$(mktemp -d /tmp/mars-shm-xuser.XXXXXX)"
trap 'rm -rf "$HELPER_DIR"' EXIT

echo "==> building shm helper (cargo, current user)"
cat > "$HELPER_DIR/main.rs" <<'EOF'
// Helper: create|write|read a MARS ring by logical name.
// args: <create|write|read> <logical-name>
fn main() {
    let mut args = std::env::args().skip(1);
    let op = args.next().expect("op");
    let name = args.next().expect("logical ring name");
    let spec = mars_shm::RingSpec {
        sample_rate: 48_000,
        channels: 1,
        capacity_frames: 64,
    };
    let registry = mars_shm::RingRegistry::default();
    let ring = registry
        .create_or_open(&name, spec)
        .expect("create_or_open must succeed across users");
    match op.as_str() {
        "create" => {
            println!("created {name}");
        }
        "write" => {
            let frames: Vec<f32> = (0..32).map(|i| i as f32 / 32.0).collect();
            let t = ring.lock().write_interleaved(&frames).expect("write");
            println!("wrote {} frames", t.frames);
        }
        "read" => {
            let mut out = vec![0.0_f32; 32];
            let t = ring.lock().read_interleaved(&mut out).expect("read");
            assert_eq!(t.frames, 32, "expected the peer's 32 frames");
            assert!((out[31] - 31.0 / 32.0).abs() < 1e-6, "payload mismatch");
            println!("read {} frames, payload verified", t.frames);
        }
        other => panic!("unknown op {other}"),
    }
}
EOF

cd "$REPO_ROOT"
cargo build -q -p mars-cli 2>/dev/null || true # warm target dir
HELPER_BIN="$HELPER_DIR/shm-xuser"
cat > "$HELPER_DIR/Cargo.toml" <<EOF
[package]
name = "shm-xuser"
version = "0.0.0"
edition = "2024"

[dependencies]
mars-shm = { path = "$REPO_ROOT/crates/mars-shm" }

[[bin]]
name = "shm-xuser"
path = "main.rs"

[workspace]
EOF
(cd "$HELPER_DIR" && cargo build -q --release)
HELPER_BIN="$HELPER_DIR/target/release/shm-xuser"

RING_NAME="mars.vin.xuser-test.$(uuidgen | tr -d - | cut -c1-16 | tr 'A-Z' 'a-z')"
chmod 755 "$HELPER_DIR" "$HELPER_BIN"

echo "==> creating + writing ring as $(whoami): $RING_NAME"
"$HELPER_BIN" create "$RING_NAME"
"$HELPER_BIN" write "$RING_NAME"

echo "==> reading ring as $PEER_USER"
if sudo -u "$PEER_USER" "$HELPER_BIN" read "$RING_NAME"; then
    echo "PASS: cross-user producer/consumer shared the ring"
else
    echo "FAIL: peer user could not open or read the ring" >&2
    exit 1
fi
