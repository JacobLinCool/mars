#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

METRICS_PATH="target/bench/latest/metrics.json"
PLATFORM="${MARS_BENCH_PLATFORM:-}"
CRITERION_DIR="${MARS_BENCH_CRITERION_DIR:-}"
SKIP_BENCH="0"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --metrics)
      METRICS_PATH="$2"
      shift 2
      ;;
    --platform)
      PLATFORM="$2"
      shift 2
      ;;
    --criterion-dir)
      CRITERION_DIR="$2"
      shift 2
      ;;
    --skip-bench)
      SKIP_BENCH="1"
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ "$SKIP_BENCH" == "0" ]]; then
  RUN_DIR="$ROOT_DIR/target/bench/runs/$(date +%s)"
  CARGO_TARGET_DIR="$RUN_DIR" cargo bench --workspace
  CRITERION_DIR="$RUN_DIR/criterion"
fi

if [[ -z "$CRITERION_DIR" ]]; then
  CRITERION_DIR="$ROOT_DIR/target/criterion"
fi

COLLECT_ARGS=(--criterion-dir "$CRITERION_DIR" --output "$METRICS_PATH")
VERIFY_ARGS=(--budgets-dir benches/budgets --metrics "$METRICS_PATH")
if [[ -n "$PLATFORM" ]]; then
  COLLECT_ARGS+=(--platform "$PLATFORM")
  VERIFY_ARGS+=(--platform "$PLATFORM")
fi

python3 scripts/bench/collect.py "${COLLECT_ARGS[@]}"
python3 scripts/bench/verify.py "${VERIFY_ARGS[@]}"
