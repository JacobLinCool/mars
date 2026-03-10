# MARS Performance Gates (macOS 15+)

This repository uses hard, merge-blocking benchmark gates on `macos-15`.

## Gate policy

- `median_ns`: fail on regression over `10%`
- `p95_ns`: fail on regression over `15%`
- `rt_cycle_p99_ratio`: fail when over `0.75` of callback period
- non-finite budget or metric values (`NaN`, `inf`) are rejected

Budgets are committed at:

- `benches/budgets/macos-15.json`

## CI jobs

`Benchmark Budget Gate` workflow runs:

- matrix engine gate
- DSP chain/block gate
- capture gate
- sink gate

Each job uploads `target/bench/latest/metrics.json` as an artifact for triage.

## Local reproduction

Run from repository root.

### Full gate (all budgets, slow)

```bash
scripts/bench/verify.sh --platform macos-15
```

### Matrix gate

```bash
scripts/bench/verify.sh --platform macos-15 \
  --bench-cmd "cargo bench -p mars-engine --bench engine -- engine/render_matrix" \
  --benchmark-prefix "engine/render_matrix/"
```

### DSP gate

```bash
scripts/bench/verify.sh --platform macos-15 \
  --bench-cmd "cargo bench -p mars-engine --bench engine -- engine/render_chain_length" \
  --bench-cmd "cargo bench -p mars-engine --bench engine -- engine/render_param_updates" \
  --bench-cmd "cargo bench -p mars-engine --bench engine -- engine/render_processor_block" \
  --bench-cmd "cargo bench -p mars-engine --bench engine -- engine/render_processor_chain_kind" \
  --bench-cmd "cargo bench -p mars-engine --bench engine -- engine/render_timeshift_depth" \
  --benchmark-prefix "engine/render_chain_length/" \
  --benchmark-prefix "engine/render_param_updates/" \
  --benchmark-prefix "engine/render_processor_block/" \
  --benchmark-prefix "engine/render_processor_chain_kind/" \
  --benchmark-prefix "engine/render_timeshift_depth/"
```

### Capture gate

```bash
scripts/bench/verify.sh --platform macos-15 \
  --bench-cmd "cargo bench -p mars-daemon --bench capture_runtime" \
  --bench-cmd "cargo bench -p mars-daemon --bench capture_render" \
  --benchmark-prefix "daemon/capture/" \
  --benchmark-prefix "daemon/capture_render/"
```

### Sink gate

```bash
scripts/bench/verify.sh --platform macos-15 \
  --bench-cmd "cargo bench -p mars-daemon --bench sink_runtime" \
  --benchmark-prefix "daemon/sink/"
```
