#!/usr/bin/env bash
set -euo pipefail

# Notarize the signed mars.driver bundle through a ZIP submission, then staple
# and validate the ticket on the original bundle before it is packaged.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DRIVER_BUNDLE="$ROOT_DIR/bundles/mars.driver"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT_DIR/crates/mars-daemon/Cargo.toml" | head -1)"
ARCH="$(uname -m)"

for name in NOTARY_KEY_PATH NOTARY_KEY_ID NOTARY_ISSUER_ID; do
  if [ -z "${!name:-}" ]; then
    echo "error: missing required notarization credential: $name" >&2
    exit 1
  fi
done

if [ ! -f "$NOTARY_KEY_PATH" ]; then
  echo "error: notarization API key does not exist: $NOTARY_KEY_PATH" >&2
  exit 1
fi
if [ ! -d "$DRIVER_BUNDLE" ]; then
  echo "error: driver bundle does not exist: $DRIVER_BUNDLE" >&2
  exit 1
fi
if [ -z "$VERSION" ]; then
  echo "error: failed to determine the driver version." >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "$DRIVER_BUNDLE"

mkdir -p "$ROOT_DIR/dist"
RESULT="$ROOT_DIR/dist/mars-$VERSION-$ARCH.driver.notary-result.json"
LOG="$ROOT_DIR/dist/mars-$VERSION-$ARCH.driver.notary-log.json"
TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/mars-driver-notary.XXXXXX")"
ARCHIVE="$TEMP_DIR/mars.driver.zip"
trap 'rm -rf "$TEMP_DIR"' EXIT

ditto -c -k --keepParent "$DRIVER_BUNDLE" "$ARCHIVE"
xcrun notarytool submit "$ARCHIVE" \
  --key "$NOTARY_KEY_PATH" \
  --key-id "$NOTARY_KEY_ID" \
  --issuer "$NOTARY_ISSUER_ID" \
  --wait \
  --output-format json > "$RESULT"
cat "$RESULT"

SUBMISSION_ID="$(jq -er '.id | select(type == "string" and length > 0)' "$RESULT")"
xcrun notarytool log "$SUBMISSION_ID" \
  --key "$NOTARY_KEY_PATH" \
  --key-id "$NOTARY_KEY_ID" \
  --issuer "$NOTARY_ISSUER_ID" \
  "$LOG"

jq -e '.status == "Accepted"' "$RESULT"
jq -e '([.issues[]? | select(.severity == "error")] | length) == 0' "$LOG"
xcrun stapler staple "$DRIVER_BUNDLE"
xcrun stapler validate "$DRIVER_BUNDLE"
codesign --verify --deep --strict --verbose=2 "$DRIVER_BUNDLE"

echo "Notarized and stapled driver bundle: $DRIVER_BUNDLE"
echo "Notarization result: $RESULT"
echo "Notarization log: $LOG"
