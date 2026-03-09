#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import math
import platform
import sys
from pathlib import Path

POLICY_MEDIAN_THRESHOLD = 0.10
POLICY_P95_THRESHOLD = 0.15
POLICY_RT_P99_MAX_RATIO = 0.75


def detect_platform() -> str:
    if sys.platform == "darwin":
        version = platform.mac_ver()[0]
        major = version.split(".", 1)[0] if version else "unknown"
        return f"macos-{major}"
    if sys.platform.startswith("linux"):
        return "linux"
    return sys.platform


def parse_finite_float(raw: object, context: str) -> float:
    try:
        value = float(raw)
    except (TypeError, ValueError):
        raise SystemExit(f"invalid numeric value for {context}: {raw!r}") from None
    if not math.isfinite(value):
        raise SystemExit(f"non-finite numeric value for {context}: {raw!r}")
    return value


def load_json(path: Path) -> dict | list:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def load_budget_entries(budgets_dir: Path) -> list[dict]:
    entries: list[dict] = []
    for budget_file in sorted(budgets_dir.glob("*.json")):
        payload = load_json(budget_file)
        if isinstance(payload, dict):
            raw_entries = payload.get("budgets")
            if not isinstance(raw_entries, list):
                raise SystemExit(f"budget file missing 'budgets' array: {budget_file}")
        elif isinstance(payload, list):
            raw_entries = payload
        else:
            raise SystemExit(f"invalid budget file shape: {budget_file}")

        for idx, entry in enumerate(raw_entries):
            if not isinstance(entry, dict):
                raise SystemExit(f"budget entry #{idx} in {budget_file} is not an object")
            for required in (
                "benchmark_id",
                "platform",
                "metric",
                "budget_value",
                "regression_threshold",
            ):
                if required not in entry:
                    raise SystemExit(f"budget entry #{idx} in {budget_file} missing '{required}'")
            entries.append(entry)

    if not entries:
        raise SystemExit(f"no budget entries found in {budgets_dir}")

    return entries


def validate_policy(entries: list[dict]) -> list[str]:
    failures: list[str] = []
    for index, entry in enumerate(entries):
        benchmark_id = str(entry["benchmark_id"])
        metric = str(entry["metric"])
        budget = parse_finite_float(entry["budget_value"], f"budget[{index}] {benchmark_id} budget_value")
        threshold = parse_finite_float(
            entry["regression_threshold"],
            f"budget[{index}] {benchmark_id} regression_threshold",
        )

        if metric == "median_ns" and not math.isclose(threshold, POLICY_MEDIAN_THRESHOLD, rel_tol=0.0, abs_tol=1e-12):
            failures.append(
                f"{benchmark_id} median_ns threshold must be {POLICY_MEDIAN_THRESHOLD:.2f}, got {threshold:.4f}"
            )
        if metric == "p95_ns" and not math.isclose(threshold, POLICY_P95_THRESHOLD, rel_tol=0.0, abs_tol=1e-12):
            failures.append(
                f"{benchmark_id} p95_ns threshold must be {POLICY_P95_THRESHOLD:.2f}, got {threshold:.4f}"
            )
        if metric == "rt_cycle_p99_ratio":
            if not math.isclose(budget, POLICY_RT_P99_MAX_RATIO, rel_tol=0.0, abs_tol=1e-12):
                failures.append(
                    f"{benchmark_id} rt_cycle_p99_ratio budget must be {POLICY_RT_P99_MAX_RATIO:.2f}, got {budget:.4f}"
                )
            if not math.isclose(threshold, 0.0, rel_tol=0.0, abs_tol=1e-12):
                failures.append(
                    f"{benchmark_id} rt_cycle_p99_ratio threshold must be 0.0, got {threshold:.4f}"
                )
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify benchmark metrics against committed budgets")
    parser.add_argument("--budgets-dir", default="benches/budgets", type=Path)
    parser.add_argument("--metrics", required=True, type=Path)
    parser.add_argument("--platform", default=detect_platform())
    args = parser.parse_args()

    budget_entries = load_budget_entries(args.budgets_dir)

    policy_failures = validate_policy(budget_entries)
    if policy_failures:
        for failure in policy_failures:
            print(f"policy violation: {failure}")
        return 1

    metrics_payload = load_json(args.metrics)
    if not isinstance(metrics_payload, dict):
        raise SystemExit(f"invalid metrics payload: {args.metrics}")
    metrics_platform = metrics_payload.get("platform")
    if metrics_platform != args.platform:
        raise SystemExit(
            f"metrics platform mismatch: expected {args.platform}, got {metrics_platform}. "
            f"re-run collect with --platform {args.platform}"
        )

    metric_records = metrics_payload.get("records")
    if not isinstance(metric_records, list):
        raise SystemExit(f"metrics payload missing records array: {args.metrics}")

    metrics_map: dict[tuple[str, str], float] = {}
    for index, record in enumerate(metric_records):
        if not isinstance(record, dict):
            continue
        benchmark_id = record.get("benchmark_id")
        metric = record.get("metric")
        value = record.get("value")
        if isinstance(benchmark_id, str) and isinstance(metric, str):
            metrics_map[(benchmark_id, metric)] = parse_finite_float(
                value,
                f"metrics[{index}] {benchmark_id} {metric} value",
            )

    relevant_budgets = [entry for entry in budget_entries if str(entry["platform"]) == args.platform]
    if not relevant_budgets:
        raise SystemExit(f"no budget entries found for platform '{args.platform}'")

    failures: list[str] = []
    checked = 0

    for entry in sorted(
        relevant_budgets,
        key=lambda item: (str(item["benchmark_id"]), str(item["metric"])),
    ):
        benchmark_id = str(entry["benchmark_id"])
        metric = str(entry["metric"])
        baseline = parse_finite_float(entry["budget_value"], f"budget {benchmark_id} {metric} budget_value")
        threshold = parse_finite_float(
            entry["regression_threshold"],
            f"budget {benchmark_id} {metric} regression_threshold",
        )

        observed = metrics_map.get((benchmark_id, metric))
        if observed is None:
            failures.append(f"missing metric '{metric}' for benchmark '{benchmark_id}'")
            continue

        if metric == "rt_cycle_p99_ratio":
            allowed = POLICY_RT_P99_MAX_RATIO
        else:
            allowed = baseline * (1.0 + threshold)
        if not math.isfinite(allowed):
            failures.append(f"invalid computed allowed budget for '{benchmark_id}' '{metric}': {allowed!r}")
            continue

        checked += 1
        if observed > allowed:
            failures.append(
                f"{benchmark_id} {metric} exceeded budget: observed={observed:.4f}, "
                f"allowed={allowed:.4f}, baseline={baseline:.4f}, threshold={threshold:.2%}"
            )

    if failures:
        print(f"benchmark verification failed for {len(failures)} checks (evaluated {checked})")
        for failure in failures:
            print(f"- {failure}")
        return 1

    print(f"benchmark verification passed: evaluated {checked} budget checks on {args.platform}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
