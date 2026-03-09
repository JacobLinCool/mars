# Benchmark Budgets

This directory stores merge-blocking benchmark budgets for `macOS 15+`.

## Budget Entry Format

Each JSON entry in `benches/budgets/*.json` must include:

- `benchmark_id`
- `platform`
- `metric`
- `budget_value`
- `regression_threshold`

Example:

```json
{
  "benchmark_id": "engine/render_simple/256",
  "platform": "macos-15",
  "metric": "median_ns",
  "budget_value": 1380.0,
  "regression_threshold": 0.10
}
```

## Enforced Policy

- `median_ns`: regression threshold must be `0.10`.
- `p95_ns`: regression threshold must be `0.15`.
- `rt_cycle_p99_ratio`: hard cap at `0.75` of buffer period.

## Local Verification

Run full benchmark + gate verification:

```bash
scripts/bench/verify.sh --platform macos-15
```

Reuse existing Criterion results (skip rerun):

```bash
scripts/bench/verify.sh --skip-bench --criterion-dir /abs/path/to/criterion --platform macos-15
```
