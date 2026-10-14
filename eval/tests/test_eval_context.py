"""The invocation binds controls before any model task can start."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "eval"))

from harbor_adapter.context import invocation_context


class InvocationContextTests(unittest.TestCase):
    def make(self, **overrides):
        values = dict(
            arm="plak-openrouter", slice_name="ab", account="openrouter",
            model="deepseek/deepseek-v4-flash", effort="none", arch="amd64",
            attempts=3, concurrency=1, selected_task=None,
            source_commit="f" * 40, source_dirty=False,
            cohort="publication-comparison", repeat_block="1",
        )
        values.update(overrides)
        return invocation_context(**values)

    def test_repeat_and_effort_semantics_are_explicit(self):
        record = self.make()
        self.assertEqual(record["planned_trials"], 24)
        self.assertEqual(record["cohort"], "publication-comparison")
        self.assertEqual(record["repeat_block"], "1")
        self.assertEqual(record["effort_semantics"], "omit effort override")
        self.assertEqual(record["adapter_task_retries"], 0)
        self.assertIn("not assumed identical", record["native_internal_retries"])

    def test_single_task_binding_does_not_claim_whole_slice(self):
        record = self.make(selected_task="terminal-bench/regex-log")
        self.assertEqual(record["planned_trials"], 3)
        self.assertEqual(record["tasks"], ["terminal-bench/regex-log"])

    def test_expanded_slice_is_separate_and_frozen(self):
        record = self.make(slice_name="expanded", attempts=1)
        self.assertEqual(record["planned_trials"], 16)
        self.assertEqual(len(record["tasks"]), 16)
        self.assertEqual(record["dataset"], "terminal-bench/terminal-bench-2-1@6")

    def test_scope_and_invalid_counts_refuse(self):
        for values in ({"selected_task": "unknown"}, {"attempts": 0},
                       {"concurrency": 0}, {"slice_name": "other"}):
            with self.subTest(values=values), self.assertRaises(ValueError):
                self.make(**values)


if __name__ == "__main__":
    unittest.main()
