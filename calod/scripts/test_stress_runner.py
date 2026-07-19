import unittest
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).parent))
from stress_runner import summarize_repeated_results


class StressRunnerSummaryTests(unittest.TestCase):
    def test_summarizes_timed_runs_with_medians_and_error_totals(self):
        summary = summarize_repeated_results(
            [
                {
                    "mode": "timed",
                    "operations_total": 100,
                    "errors_total": 1,
                    "ops_per_sec": 10.0,
                    "latency_ms": {"p50": 1.0, "p95": 2.0, "p99": 3.0},
                },
                {
                    "mode": "timed",
                    "operations_total": 120,
                    "errors_total": 0,
                    "ops_per_sec": 12.0,
                    "latency_ms": {"p50": 1.2, "p95": 2.2, "p99": 3.2},
                },
                {
                    "mode": "timed",
                    "operations_total": 110,
                    "errors_total": 2,
                    "ops_per_sec": 11.0,
                    "latency_ms": {"p50": 1.1, "p95": 2.1, "p99": 3.1},
                },
            ]
        )

        self.assertEqual(summary["runs"], 3)
        self.assertEqual(summary["operations_total"], 330)
        self.assertEqual(summary["errors_total"], 3)
        self.assertEqual(summary["ops_per_sec_median"], 11.0)
        self.assertEqual(summary["latency_ms_median"], {"p50": 1.1, "p95": 2.1, "p99": 3.1})


if __name__ == "__main__":
    unittest.main()
