#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

METRICS_PATH="target/bench/latest/metrics.json"
PLATFORM="${MARS_BENCH_PLATFORM:-}"
CRITERION_DIRS=()
if [[ -n "${MARS_BENCH_CRITERION_DIR:-}" ]]; then
  CRITERION_DIRS+=("$MARS_BENCH_CRITERION_DIR")
fi
SKIP_BENCH="0"
ATTEMPTS="1"
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
      CRITERION_DIRS+=("$2")
      shift 2
      ;;
    --attempts)
      ATTEMPTS="$2"
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

if ! [[ "$ATTEMPTS" =~ ^[1-9][0-9]*$ ]]; then
  echo "--attempts must be a positive integer" >&2
  exit 2
fi

if [[ "$SKIP_BENCH" == "0" ]]; then
  RUN_ROOT="$ROOT_DIR/target/bench/runs/$(date +%s)-$$"
  BUILD_DIR="$RUN_ROOT/build"
  for ((attempt = 1; attempt <= ATTEMPTS; attempt++)); do
    rm -rf "$BUILD_DIR/criterion"
    echo "benchmark attempt $attempt/$ATTEMPTS"
    if [[ "${#BENCH_COMMANDS[@]}" -eq 0 ]]; then
      CARGO_TARGET_DIR="$BUILD_DIR" cargo bench --workspace
    else
      for bench_cmd in "${BENCH_COMMANDS[@]}"; do
        echo "running bench command: $bench_cmd"
        CARGO_TARGET_DIR="$BUILD_DIR" bash -lc "$bench_cmd"
      done
    fi
    if [[ ! -d "$BUILD_DIR/criterion" ]]; then
      echo "criterion output missing after benchmark attempt $attempt" >&2
      exit 1
    fi
    ATTEMPT_DIR="$RUN_ROOT/criterion-$attempt"
    mv "$BUILD_DIR/criterion" "$ATTEMPT_DIR"
    CRITERION_DIRS+=("$ATTEMPT_DIR")
  done
elif [[ "$ATTEMPTS" != "1" ]]; then
  echo "--attempts cannot be combined with --skip-bench" >&2
  exit 2
fi

if [[ "${#CRITERION_DIRS[@]}" -eq 0 ]]; then
  CRITERION_DIRS+=("$ROOT_DIR/target/criterion")
fi

COLLECT_ARGS=(--output "$METRICS_PATH")
for criterion_dir in "${CRITERION_DIRS[@]}"; do
  COLLECT_ARGS+=(--criterion-dir "$criterion_dir")
done
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
