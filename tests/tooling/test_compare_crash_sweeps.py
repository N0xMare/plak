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
SCRIPT = ROOT / "tools/checks/compare_crash_sweeps.py"
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
