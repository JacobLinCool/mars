#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

METRICS_PATH="target/bench/latest/metrics.json"
PLATFORM="${MARS_BENCH_PLATFORM:-}"
CRITERION_DIR="${MARS_BENCH_CRITERION_DIR:-}"
SKIP_BENCH="0"
BENCH_COMMANDS=()
BENCHMARK_PREFIXES=()

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
    --bench-cmd)
      BENCH_COMMANDS+=("$2")
      shift 2
      ;;
    --benchmark-prefix)
      BENCHMARK_PREFIXES+=("$2")
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ "$SKIP_BENCH" == "0" ]]; then
  RUN_DIR="$ROOT_DIR/target/bench/runs/$(date +%s)"
  if [[ "${#BENCH_COMMANDS[@]}" -eq 0 ]]; then
    CARGO_TARGET_DIR="$RUN_DIR" cargo bench --workspace
  else
    for bench_cmd in "${BENCH_COMMANDS[@]}"; do
      echo "running bench command: $bench_cmd"
      CARGO_TARGET_DIR="$RUN_DIR" bash -lc "$bench_cmd"
    done
  fi
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
if [[ "${#BENCHMARK_PREFIXES[@]}" -gt 0 ]]; then
  for prefix in "${BENCHMARK_PREFIXES[@]}"; do
    VERIFY_ARGS+=(--benchmark-prefix "$prefix")
  done
fi

python3 scripts/bench/collect.py "${COLLECT_ARGS[@]}"
python3 scripts/bench/verify.py "${VERIFY_ARGS[@]}"
