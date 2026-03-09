#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

BUDGETS_DIR="$TMP_DIR/budgets"
METRICS_DIR="$TMP_DIR/metrics"
mkdir -p "$BUDGETS_DIR" "$METRICS_DIR"

BUDGETS_PATH="$BUDGETS_DIR/budgets.json"
METRICS_OK="$METRICS_DIR/metrics-ok.json"
METRICS_NAN="$METRICS_DIR/metrics-nan.json"

cat > "$BUDGETS_PATH" <<'JSON'
{
  "budgets": [
    {
      "benchmark_id": "sanity/metric",
      "platform": "macos-15",
      "metric": "median_ns",
      "budget_value": 100.0,
      "regression_threshold": 0.10
    }
  ]
}
JSON

cat > "$METRICS_OK" <<'JSON'
{
  "schema_version": 1,
  "platform": "macos-15",
  "records": [
    {
      "benchmark_id": "sanity/metric",
      "metric": "median_ns",
      "value": 100.0
    }
  ]
}
JSON

cat > "$METRICS_NAN" <<'JSON'
{
  "schema_version": 1,
  "platform": "macos-15",
  "records": [
    {
      "benchmark_id": "sanity/metric",
      "metric": "median_ns",
      "value": NaN
    }
  ]
}
JSON

python3 scripts/bench/verify.py --budgets-dir "$BUDGETS_DIR" --metrics "$METRICS_OK" --platform macos-15 >/dev/null

if python3 scripts/bench/verify.py --budgets-dir "$BUDGETS_DIR" --metrics "$METRICS_NAN" --platform macos-15 >/dev/null 2>&1; then
  echo "expected non-finite metrics to fail verification" >&2
  exit 1
fi

echo "non-finite metric guardrail test passed"
