from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("collect.py")
SPEC = importlib.util.spec_from_file_location("mars_bench_collect", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
COLLECT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COLLECT)


class CollectMultiRunTests(unittest.TestCase):
    def write_run(self, root: Path, median: float, samples: list[float]) -> Path:
        bench = root / "engine" / "render_matrix" / "case" / "256" / "new"
        bench.mkdir(parents=True)
        (bench / "benchmark.json").write_text(
            json.dumps({"full_id": "engine/render_matrix/case/256"}),
            encoding="utf-8",
        )
        (bench / "estimates.json").write_text(
            json.dumps({"median": {"point_estimate": median}}),
            encoding="utf-8",
        )
        (bench / "sample.json").write_text(
            json.dumps({"iters": [1] * len(samples), "times": samples}),
            encoding="utf-8",
        )
        return root

    def test_multiple_runs_keep_the_lowest_value_for_each_metric(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            slow = self.write_run(root / "slow", 120.0, [100.0, 120.0, 140.0])
            fast = self.write_run(root / "fast", 80.0, [70.0, 80.0, 90.0])

            records = COLLECT.collect_records([slow, fast], 48_000)
            values = {record["metric"]: record["value"] for record in records}

            self.assertEqual(values["median_ns"], 80.0)
            self.assertLess(values["p95_ns"], 100.0)
            self.assertLess(values["rt_cycle_p99_ratio"], 0.001)


if __name__ == "__main__":
    unittest.main()
