"""Equal ordinals are comparable only with equal pre-cut evidence."""

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools/dev/compare_crash_sweeps.py"
SPEC = importlib.util.spec_from_file_location("compare_crash_sweeps", SCRIPT)
COMPARE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COMPARE)


def row(state="prepared", outcome="ABANDONED", repeat=1):
    return {"shape": "capacity_overflow", "store": "product", "store_ordinal": 5,
            "repeat": repeat, "outcome": outcome, "invariants": True,
            "before": {"ledger": [{"operation": 1, "kind": "model", "state": state}]},
            "global_order": [
                {"store": "runtime", "store_ordinal": 1, "ordinal": 1,
                 "label": "prepare", "tables": []},
                {"store": "product", "store_ordinal": 5, "ordinal": 2,
                 "label": "projection", "tables": ["sessions"]}]}


class CompareCrashSweepsTests(unittest.TestCase):
    def test_different_ledger_at_same_cut_is_floating_not_a_regression(self):
        report = COMPARE.compare([row()], [row("dispatched", "COMPLETED_ONCE")])
        self.assertEqual(report["counts"]["floating"], 2)
        self.assertFalse(report["changed"])

    def test_different_other_store_prefix_is_floating_even_with_equal_ledger(self):
        tip = row()
        tip["global_order"][0]["label"] = "dispatch"
        report = COMPARE.compare([row()], [tip])
        self.assertEqual(report["counts"]["floating"], 2)
        self.assertFalse(report["matching"])

    def test_cross_store_interleaving_and_random_ids_do_not_change_inputs(self):
        tip = row(repeat=9)
        tip["global_order"].reverse()
        tip["global_order"][0]["ordinal"] = 1
        tip["global_order"][1]["ordinal"] = 2
        tip["before"]["head"] = "random-new-checkpoint"
        report = COMPARE.compare([row()], [tip])
        self.assertEqual(report["counts"]["matching"], 1)
        self.assertFalse(report["floating"])

    def test_equal_inputs_with_changed_outcome_or_invariant_are_genuine(self):
        for tip in (row(outcome="COMPLETED_ONCE"), dict(row(), invariants=False)):
            report = COMPARE.compare([row()], [tip])
            self.assertEqual(report["counts"]["changed"], 1)
            self.assertFalse(report["floating"])

    def test_repeated_samples_match_by_inputs_then_outcomes_not_repeat_number(self):
        base = [row(repeat=1), row(outcome="COMPLETED_ONCE", repeat=2)]
        tip = [row(outcome="COMPLETED_ONCE", repeat=1), row(repeat=2)]
        report = COMPARE.compare(base, tip)
        self.assertEqual(report["counts"]["matching"], 2)
        self.assertFalse(report["changed"])
        report = COMPARE.compare(base, tip[:1])
        self.assertEqual(report["counts"]["matching"], 1)
        self.assertEqual(report["counts"]["unpaired"], 1)
        self.assertFalse(report["floating"])

    def test_missing_cuts_and_missing_input_evidence_are_not_matches(self):
        tip = dict(row(), store_ordinal=6)
        report = COMPARE.compare([row()], [tip])
        self.assertEqual(report["counts"]["only_left"], 1)
        self.assertEqual(report["counts"]["only_right"], 1)
        pilot = {key: value for key, value in row().items()
                 if key not in ("global_order", "before")}
        report = COMPARE.compare([pilot], [copy.deepcopy(pilot)])
        self.assertEqual(report["counts"]["uncomparable"], 2)
        self.assertFalse(report["matching"])

    def test_ledger_only_artifacts_can_be_compared(self):
        base = row()
        del base["global_order"]
        self.assertEqual(COMPARE.compare([base], [base])["counts"]["matching"], 1)

    def test_bad_tip_rows_regress_even_with_different_or_missing_inputs(self):
        extra_commit = row(outcome="UNCLASSIFIED")
        extra_commit["global_order"].append(
            {"store": "tree", "store_ordinal": 1, "label": "cursor", "tables": []})
        extra_commit["invariants"] = False
        pilot = dict(row(outcome="DIVERGENT"), store="pilot", store_ordinal=0)
        refused = dict(row("dispatched", "REFUSED: recovery"), invariants=False)
        no_evidence = dict(row(), invariants=False)
        del no_evidence["global_order"]
        del no_evidence["before"]
        for tip in (extra_commit, pilot, refused, no_evidence):
            with self.subTest(tip=tip), tempfile.TemporaryDirectory() as directory:
                report = COMPARE.compare([row()], [tip])
                self.assertEqual(report["counts"]["regressed"], 1)
                self.assertFalse(report["regressed"][0]["baseline_failed"])
                base_path, tip_path = (Path(directory) / name for name in ("base", "tip"))
                base_path.write_text(json.dumps(row()) + "\n")
                tip_path.write_text(json.dumps(tip) + "\n")
                result = subprocess.run(
                    [sys.executable, str(SCRIPT), str(base_path), str(tip_path), "--json"],
                    capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(json.loads(result.stdout)["counts"]["regressed"], 1)

    def test_preexisting_failure_is_reported_without_a_new_regression_exit(self):
        base = dict(row(), invariants=False)
        tip = dict(row("dispatched"), invariants=False)
        report = COMPARE.compare([base], [tip])
        self.assertEqual(report["counts"]["regressed"], 1)
        self.assertTrue(report["regressed"][0]["baseline_failed"])
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / name for name in ("base", "tip")]
            for path, sample in zip(paths, (base, tip)):
                path.write_text(json.dumps(sample) + "\n")
            result = subprocess.run([sys.executable, str(SCRIPT), *map(str, paths)],
                                    capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("regressed=1", result.stdout)

    def test_bad_outcomes_are_findings_even_with_true_invariants(self):
        for state in ("DIVERGENT", "UNCLASSIFIED", "REFUSED", "REFUSED_STORE"):
            with self.subTest(outcome=state):
                report = COMPARE.compare([], [row(outcome=state)])
                self.assertEqual(report["counts"]["regressed"], 1)
        for invariant in (None, False, 1):
            with self.subTest(invariants=invariant):
                report = COMPARE.compare([], [dict(row(), invariants=invariant)])
                self.assertEqual(report["counts"]["regressed"], 1)

    def test_floating_failures_cannot_hide_increasing_counts_or_new_kinds(self):
        base = [dict(row(), invariants=False)]
        duplicate = dict(row("dispatched"), invariants=False)
        for tip in ([duplicate, dict(duplicate, repeat=2)],
                    [row("dispatched", "DIVERGENT")],
                    [dict(row("dispatched", "REFUSED_STORE"), invariants=False)]):
            with self.subTest(tip=tip), tempfile.TemporaryDirectory() as directory:
                report = COMPARE.compare(base, tip)
                self.assertFalse(report["changed"])
                paths = [Path(directory) / name for name in ("base", "tip")]
                for path, samples in zip(paths, (base, tip)):
                    path.write_text("".join(json.dumps(sample) + "\n" for sample in samples))
                result = subprocess.run([sys.executable, str(SCRIPT), *map(str, paths)],
                                        capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertTrue(any(not r["baseline_failed"] for r in report["regressed"]))

    def test_fewer_same_kind_failures_are_not_a_new_regression(self):
        base = [dict(row(repeat=i), invariants=False) for i in range(2)]
        tip = [dict(row("dispatched"), invariants=False)]
        report = COMPARE.compare(base, tip)
        self.assertEqual(report["counts"]["regressed"], 1)
        self.assertTrue(report["regressed"][0]["baseline_failed"])
        self.assertFalse(report["changed"])

    def test_cli_exit_status_distinguishes_real_changes_and_malformed_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            base, tip = (Path(directory) / name for name in ("base.jsonl", "tip.jsonl"))
            base.write_text(json.dumps(row()) + "\n")
            for content, expected in ((row("dispatched"), 0),
                                      (row(outcome="COMPLETED_ONCE"), 1), ({}, 2)):
                tip.write_text(json.dumps(content) + "\n")
                result = subprocess.run([sys.executable, str(SCRIPT), str(base), str(tip),
                                         "--json"], capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, expected, result.stderr)
                if expected != 2:
                    self.assertIn("counts", json.loads(result.stdout))


if __name__ == "__main__":
    unittest.main()
