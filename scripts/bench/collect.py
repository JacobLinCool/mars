#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import math
import platform
import sys
from datetime import datetime, timezone
from pathlib import Path


def detect_platform() -> str:
    if sys.platform == "darwin":
        version = platform.mac_ver()[0]
        major = version.split(".", 1)[0] if version else "unknown"
        return f"macos-{major}"
    if sys.platform.startswith("linux"):
        return "linux"
    return sys.platform


def quantile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    if q <= 0.0:
        return values[0]
    if q >= 1.0:
        return values[-1]
    idx = (len(values) - 1) * q
    lo = math.floor(idx)
    hi = math.ceil(idx)
    if lo == hi:
        return values[lo]
    return values[lo] + (values[hi] - values[lo]) * (idx - lo)


def parse_frames(full_id: str) -> int | None:
    if not full_id.startswith("engine/render"):
        return None
    last = full_id.rsplit("/", 1)[-1]
    try:
        return int(last)
    except ValueError:
        return None


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def collect_records(criterion_dirs: list[Path], sample_rate_hz: int) -> list[dict]:
    records: dict[tuple[str, str], dict] = {}

    for criterion_dir in criterion_dirs:
        for benchmark_file in sorted(criterion_dir.rglob("new/benchmark.json")):
            bench_dir = benchmark_file.parent
            estimates_path = bench_dir / "estimates.json"
            sample_path = bench_dir / "sample.json"
            if not estimates_path.exists() or not sample_path.exists():
                continue

            benchmark = load_json(benchmark_file)
            estimates = load_json(estimates_path)
            sample = load_json(sample_path)

            benchmark_id = benchmark.get("full_id")
            if not isinstance(benchmark_id, str) or not benchmark_id:
                continue

            iters = sample.get("iters") or []
            times = sample.get("times") or []
            if len(iters) != len(times) or not iters:
                continue

            per_iter_ns = sorted(
                float(total_ns) / float(iter_count)
                for iter_count, total_ns in zip(iters, times)
                if float(iter_count) > 0.0
            )
            if not per_iter_ns:
                continue

            median_ns = float(estimates["median"]["point_estimate"])
            p95_ns = quantile(per_iter_ns, 0.95)
            p99_ns = quantile(per_iter_ns, 0.99)

            for metric, value in (("median_ns", median_ns), ("p95_ns", p95_ns)):
                records[(benchmark_id, metric)] = {
                    "benchmark_id": benchmark_id,
                    "metric": metric,
                    "value": value,
                    "unit": "ns",
                }

            frames = parse_frames(benchmark_id)
            if frames is not None and sample_rate_hz > 0:
                period_ns = (frames * 1_000_000_000.0) / float(sample_rate_hz)
                if period_ns > 0:
                    records[(benchmark_id, "rt_cycle_p99_ratio")] = {
                        "benchmark_id": benchmark_id,
                        "metric": "rt_cycle_p99_ratio",
                        "value": p99_ns / period_ns,
                        "unit": "ratio",
                        "period_frames": frames,
                        "sample_rate_hz": sample_rate_hz,
                    }

    sorted_records = sorted(
        records.values(),
        key=lambda record: (record["benchmark_id"], record["metric"]),
    )
    return sorted_records


def main() -> int:
    parser = argparse.ArgumentParser(description="Collect machine-readable benchmark metrics from Criterion output")
    parser.add_argument(
        "--criterion-dir",
        action="append",
        type=Path,
        required=True,
        help="Criterion output directory (repeatable)",
    )
    parser.add_argument(
        "--output",
        required=True,
        type=Path,
        help="Output JSON path",
    )
    parser.add_argument(
        "--platform",
        default=detect_platform(),
        help="Platform tag to include in output (default: auto-detected)",
    )
    parser.add_argument(
        "--sample-rate-hz",
        type=int,
        default=48_000,
        help="Sample rate used for real-time cycle ratio metrics",
    )

    args = parser.parse_args()

    missing_dirs = [path for path in args.criterion_dir if not path.exists()]
    if missing_dirs:
        missing = ", ".join(str(path) for path in missing_dirs)
        raise SystemExit(f"criterion directory does not exist: {missing}")

    records = collect_records(args.criterion_dir, args.sample_rate_hz)
    payload = {
        "schema_version": 1,
        "platform": args.platform,
        "generated_at": datetime.now(tz=timezone.utc).isoformat(),
        "criterion_dirs": [str(path) for path in args.criterion_dir],
        "records": records,
    }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2)
        fh.write("\n")

    print(
        f"wrote {len(records)} metrics for {len({record['benchmark_id'] for record in records})} benchmarks to {args.output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
