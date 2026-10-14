"""Unit tests for the offline performance smoke's regression rule."""

from __future__ import annotations

import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "check_perf_smoke", ROOT / "tools" / "checks" / "check_perf_smoke.py"
)
assert SPEC and SPEC.loader
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)


class RegressionRuleTests(unittest.TestCase):
    def test_a_duration_within_tolerance_is_not_a_regression(self):
        self.assertIsNone(CHECKER.regression("cold_start_to_ready_ms", 140.0, 100.0, 0.5))

    def test_a_duration_beyond_tolerance_is_a_regression(self):
        message = CHECKER.regression("cold_start_to_ready_ms", 160.0, 100.0, 0.5)
        self.assertIsNotNone(message)
        self.assertIn("cold_start_to_ready_ms", message)

    def test_a_faster_duration_is_never_a_regression(self):
        self.assertIsNone(CHECKER.regression("cold_start_to_ready_ms", 1.0, 100.0, 0.5))

    def test_a_throughput_drop_beyond_tolerance_is_a_regression(self):
        self.assertIn(
            "jsonl_bulk_throughput_mb_per_s",
            CHECKER.regression("jsonl_bulk_throughput_mb_per_s", 0.4, 1.0, 0.5),
        )

    def test_a_higher_throughput_is_never_a_regression(self):
        self.assertIsNone(
            CHECKER.regression("jsonl_bulk_throughput_mb_per_s", 4.0, 1.0, 0.5)
        )


class BaselineTests(unittest.TestCase):
    def test_the_committed_baseline_is_a_versioned_document(self):
        document = json.loads(CHECKER.BASELINE.read_text())
        self.assertEqual(document["version"], 1)
        self.assertGreater(float(document["tolerance"]), 0.0)
        self.assertTrue(document["machines"], "the baseline names no machine")
        for machine, metrics in document["machines"].items():
            self.assertRegex(machine, r"^[0-9a-f]{12}$")
            for name, row in metrics.items():
                self.assertIn("value", row, name)
                self.assertIn("unit", row, name)

    def test_an_unseeded_machine_reports_and_passes(self):
        exit_code = self.run_check(machine="ffffffffffff", seeded={})
        self.assertEqual(exit_code, 0)

    def test_a_seeded_machine_that_regressed_on_a_durable_metric_fails(self):
        exit_code = self.run_check(
            machine="aaaaaaaaaaaa",
            seeded={"aaaaaaaaaaaa": {"growth_turn_peak_rss_mb": {"value": 1.0, "unit": "mb"}}},
            value=100.0,
        )
        self.assertEqual(exit_code, 1)
        self.assertIn("perf smoke regression", self.printed)

    def test_each_gated_metric_past_tolerance_fails(self):
        """Every gated metric must actually gate, not just the first two."""
        for metric, unit, seed, value in (
            ("cli_two_tool_turn_ms", "ms", 380.0, 9_000.0),
            ("growth_turn_peak_rss_mb", "mb", 54.8, 500.0),
            ("jsonl_bulk_throughput_mb_per_s", "mb_per_s", 1.0, 0.001),
            ("tui_transcript_20k_layout_ms", "ms", 62.6, 900.0),
        ):
            with self.subTest(metric=metric):
                exit_code = self.run_check(
                    machine="aaaaaaaaaaaa",
                    seeded={"aaaaaaaaaaaa": {metric: {"value": seed, "unit": unit}}},
                    value=value,
                    metric=metric,
                    unit=unit,
                )
                self.assertEqual(exit_code, 1)
                self.assertIn("perf smoke regression", self.printed)

    def test_a_seeded_machine_within_tolerance_passes(self):
        exit_code = self.run_check(
            machine="aaaaaaaaaaaa",
            seeded={"aaaaaaaaaaaa": {"growth_turn_peak_rss_mb": {"value": 100.0, "unit": "mb"}}},
            value=120.0,
        )
        self.assertEqual(exit_code, 0)

    def test_the_gated_set_is_the_four_measured_stable_metrics(self):
        """The set follows the idle-host spread table, not instinct."""
        self.assertEqual(
            CHECKER.GATED_METRICS,
            {
                "cli_two_tool_turn_ms",
                "growth_turn_peak_rss_mb",
                "jsonl_bulk_throughput_mb_per_s",
                "tui_transcript_20k_layout_ms",
            },
        )

    def test_the_two_noisy_metrics_never_read_as_a_regression(self):
        """These two swing on an idle host; a run must not read as a failure."""
        for metric, unit, seed, value in (
            ("cold_start_to_ready_ms", "ms", 100.0, 10_000.0),
            ("jsonl_tool_round_trip_ms", "ms", 14.0, 900.0),
        ):
            with self.subTest(metric=metric):
                exit_code = self.run_check(
                    machine="aaaaaaaaaaaa",
                    seeded={"aaaaaaaaaaaa": {metric: {"value": seed, "unit": unit}}},
                    value=value,
                    metric=metric,
                    unit=unit,
                )
                self.assertEqual(exit_code, 0)
                self.assertIn("informational", self.printed)
                self.assertNotIn("regression", self.printed)

    def test_a_seeded_metric_that_was_not_measured_is_not_a_regression(self):
        """A missing measurement must read as missing, never as a slowdown."""
        with tempfile.TemporaryDirectory() as directory:
            measurements = Path(directory) / "m.jsonl"
            measurements.write_text(
                json.dumps(
                    {
                        "metric": "cold_start_to_ready_ms",
                        "value": 100.0,
                        "unit": "ms",
                        "machine": "aaaaaaaaaaaa",
                    }
                )
                + "\n"
            )
            baseline = Path(directory) / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "tolerance": 0.5,
                        "machines": {
                            "aaaaaaaaaaaa": {
                                "cold_start_to_ready_ms": {"value": 100.0, "unit": "ms"},
                                "growth_turn_peak_rss_mb": {"value": 50.0, "unit": "mb"},
                            }
                        },
                    }
                )
            )
            argv, stdout = sys.argv, sys.stdout
            sys.argv = [
                "check_perf_smoke.py",
                str(measurements),
                "--baseline",
                str(baseline),
            ]
            sys.stdout = io.StringIO()
            try:
                code = CHECKER.main()
                printed = sys.stdout.getvalue()
            finally:
                sys.argv, sys.stdout = argv, stdout
        self.assertEqual(code, 0)
        self.assertIn("growth_turn_peak_rss_mb: no measurement", printed)
        self.assertNotIn("regression", printed)

    def run_check(
        self, machine, seeded, value=1.0, metric="growth_turn_peak_rss_mb", unit="mb"
    ):
        """Run the checker over one measurement and return its exit code."""
        with tempfile.TemporaryDirectory() as directory:
            measurements = Path(directory) / "m.jsonl"
            measurements.write_text(
                json.dumps(
                    {
                        "metric": metric,
                        "value": value,
                        "unit": unit,
                        "machine": machine,
                    }
                )
                + "\n"
            )
            baseline = Path(directory) / "baseline.json"
            baseline.write_text(
                json.dumps({"version": 1, "tolerance": 0.5, "machines": seeded})
            )
            argv = sys.argv
            stdout = sys.stdout
            sys.argv = [
                "check_perf_smoke.py",
                str(measurements),
                "--baseline",
                str(baseline),
            ]
            sys.stdout = io.StringIO()
            try:
                code = CHECKER.main()
                self.printed = sys.stdout.getvalue()
                return code
            finally:
                sys.argv = argv
                sys.stdout = stdout


if __name__ == "__main__":
    unittest.main()
