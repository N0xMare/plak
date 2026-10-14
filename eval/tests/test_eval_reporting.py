"""End-to-end offline eval report over a small immutable Harbor archive."""

from __future__ import annotations

import json
import hashlib
import pathlib
import shutil
import sqlite3
from contextlib import closing
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "eval/scripts/summarize.py"


class ReportTests(unittest.TestCase):
    def _trial(self, root, arm, name, *, reward=None, tokens=None, exception=None,
               source="bench/v1", ref="sha256:one", repair_of=None, agent=True):
        trial = root / arm / "run-one" / name
        trial.mkdir(parents=True)
        if agent:
            (trial / "agent").mkdir()
            (trial / "agent/codex.txt").write_text("turn.completed\n")
        result = {
            "task_name": "bench/task-a", "trial_name": name, "source": source,
            "task_id": {"ref": ref}, "task_checksum": "checksum-one",
            "config": {
                "task": {"source": source, "ref": ref},
                "source_trial": repair_of,
                "agent": {"name": "harbor_adapter.vendor_agents:FrozenCodex",
                          "model_name": "openai/gpt-6-luna",
                          "kwargs": {"reasoning_effort": "low"}},
            },
            "agent_result": tokens,
            "agent_setup": {"started_at": "2026-01-01T00:00:00Z", "finished_at": "2026-01-01T00:00:30Z"},
            "agent_execution": {"started_at": "2026-01-01T00:00:30Z", "finished_at": "2026-01-01T00:01:30Z"},
            "verifier": {"started_at": "2026-01-01T00:01:30Z", "finished_at": "2026-01-01T00:01:32Z"},
            "verifier_result": {"rewards": {"reward": reward}} if reward is not None else None,
            "exception_info": {"exception_type": exception} if exception else None,
        }
        (trial / "result.json").write_text(json.dumps(result))
        return trial

    def test_opencode_harbor_output_contracts_keep_corrected_usage_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name, schema, harbor_output, valid_source, complete in (
                ("stock", None, 4, True, True),
                ("old", "opencode.step_finish.part.tokens", 4, True, True),
                ("new", "opencode.step_finish.part.tokens.text_plus_reasoning", 6, True, True),
                ("unknown-schema", "unknown.tokens", 4, True, False),
                ("wrong-source", "opencode.step_finish.part.tokens", 4, False, False),
                ("old-mismatch", "opencode.step_finish.part.tokens", 6, True, False),
                ("new-mismatch", "opencode.step_finish.part.tokens.text_plus_reasoning", 4, True, False),
            ):
                trial = self._trial(root, "opencode-openrouter", f"task-a__{name}",
                                    reward=1, tokens={})
                (trial / "agent/codex.txt").unlink()
                source = trial / "agent/opencode.txt"
                source.write_text(json.dumps({"type": "step_finish", "part": {
                    "id": "one", "sessionID": "session",
                    "tokens": {"input": 10, "output": 4, "reasoning": 2,
                               "cache": {"read": 20, "write": 0}}}}) + "\n")
                result_path = trial / "result.json"
                result = json.loads(result_path.read_text())
                result["config"]["agent"]["name"] = "harbor_adapter.vendor_agents:FrozenOpenCode"
                result["config"]["agent"]["model_name"] = "openrouter/deepseek/deepseek-v4-flash"
                result["agent_result"] = {
                    "n_input_tokens": 30, "n_cache_tokens": 20,
                    "n_output_tokens": harbor_output,
                    "metadata": {"cache_write_tokens": 0, "reasoning_tokens": 2,
                                 "usage_status": "complete", "native_usage": {
                                     "schemaVersion": 1, "harness": "opencode",
                                     "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest()
                                     if valid_source else "0" * 64,
                                     "source_schema": schema}}}
                if schema is None:
                    result["agent_result"]["metadata"].pop("native_usage")
                result_path.write_text(json.dumps(result))
            process = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                     capture_output=True, text=True)
            self.assertEqual(process.returncode, 0, process.stderr)
            report = json.loads(process.stdout)
            self.assertEqual(len(report["trials"]), 7)
            for trial in report["trials"]:
                self.assertEqual(trial["tokens"]["output"], 6)
                self.assertEqual(trial["tokens"]["reasoning"], 2)
                complete = trial["trial"].split("__")[-1] in ("stock", "old", "new")
                self.assertEqual(trial["tokens_complete"], complete)
                self.assertEqual(trial["usage_status"], "complete" if complete else "partial")
                self.assertEqual(trial["cost"]["implied_status"],
                                 "implied" if complete else "unresolved_usage")
                if trial["trial"].endswith(("unknown-schema", "wrong-source")):
                    self.assertIn("native_usage_contract_unverified",
                                  trial["missing_usage_buckets"])

    def test_command_reports_all_attempts_and_preserves_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            complete = {"n_input_tokens": 100, "n_cache_tokens": 0,
                        "n_output_tokens": 20, "cost_usd": 0.5,
                        "metadata": {"cache_write_tokens": 0, "usage_status": "complete"}}
            missing = {"n_input_tokens": 200, "n_cache_tokens": None,
                       "n_output_tokens": 20, "cost_usd": None,
                       "metadata": {"usage_status": "partial"}}
            first = self._trial(root, "arm-a", "task-a__one", reward=1, tokens=complete)
            self._trial(root, "arm-a", "task-a__two", reward=0, tokens=complete)
            self._trial(root, "arm-a", "task-a__three", reward=1, tokens=missing,
                        exception="AgentSetupTimeoutError", agent=False)
            self._trial(root, "arm-b", "task-a__one", reward=0, tokens=complete)
            self._trial(root, "arm-b", "task-a__two", reward=1, tokens=complete)
            original = (first / "result.json").read_bytes()
            report_dir = root / "report"
            command = [sys.executable, str(SCRIPT), "--report", str(report_dir), "--json", str(root)]
            completed = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads((report_dir / "eval-report.json").read_text())
            self.assertEqual(json.loads(completed.stdout)["schemaVersion"], 1)
            self.assertEqual(len(report["trials"]), 5)
            self.assertEqual((first / "result.json").read_bytes(), original)
            a = next(arm for arm in report["arms"] if arm["arm"] == "arm-a")
            self.assertEqual((a["attempts"], a["eligible"], a["passes"]), (3, 2, 1))
            self.assertEqual(a["outcomes"]["setup_infrastructure_error"], 1)
            self.assertIsNone(a["implied_cost_usd"]["complete_total"])
            self.assertEqual(a["implied_cost_usd"]["covered"], 2)
            self.assertIsNone(a["total_attempt_cost_per_pass_usd"])
            self.assertEqual(a["input_tokens"]["observed_total"], 400)
            self.assertEqual(a["agent_execution_sec"]["median"], 60)
            self.assertEqual(len(a["pass_at_1_ci95_wilson"]), 2)
            self.assertEqual(report["pairs"][0]["status"], "mismatched")
            markdown = (report_dir / "eval-report.md").read_text()
            self.assertIn("Total attempt $/pass", markdown)
            self.assertIn("[result](", markdown)
            self.assertIn("setup_infrastructure_error", markdown)
            incomplete = next(t for t in report["trials"] if t["trial"] == "task-a__three")
            self.assertIsNone(incomplete["tokens"]["cache_read"])
            self.assertEqual(incomplete["raw_reward"], 1)
            self.assertIsNone(incomplete["reward"])

    def test_selected_rung_a_requires_asserted_ctrf_for_conditional_rate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 100, "n_cache_tokens": 0,
                      "n_output_tokens": 10, "metadata": {
                          "cache_write_tokens": 0, "usage_status": "complete"}}
            cases = (("bootstrap", 0, None), ("asserted-fail", 0, "failed"),
                     ("asserted-pass", 1, "passed"), ("unverified-pass", 1, None),
                     ("noncompleted", 1, "passed"))
            for name, reward, test_status in cases:
                trial = self._trial(root, "codex", f"regex-log__{name}",
                                    reward=reward, tokens=tokens)
                if name == "noncompleted":
                    (trial / "agent/codex.txt").write_text(
                        '{"type":"error","message":"provider rate limit"}\n')
                result_path = trial / "result.json"
                result = json.loads(result_path.read_text())
                result["task_name"] = "terminal-bench/regex-log"
                result["source"] = "terminal-bench/terminal-bench-2-1"
                result["task_id"]["ref"] = (
                    "sha256:802c16cfd132e6c457529cb864be5a757c1b23b6cadc57f2d01983cb0110292a")
                result["task_checksum"] = (
                    "31dc6115c061b96539a5287090ce41a7a89d3201c291b9b843bd70e416f35c39")
                result["config"]["task"]["source"] = result["source"]
                result["config"]["task"]["ref"] = result["task_id"]["ref"]
                result_path.write_text(json.dumps(result))
                verifier = trial / "verifier"
                verifier.mkdir()
                (verifier / "reward.txt").write_text(str(reward))
                (verifier / "test-stdout.txt").write_text(
                    "bootstrap stopped before pytest" if test_status is None else "pytest ran")
                if test_status:
                    (verifier / "ctrf.json").write_text(json.dumps({"results": {
                        "summary": {"tests": 1, "passed": int(test_status == "passed"),
                                    "failed": int(test_status == "failed"),
                                    "skipped": 0, "pending": 0, "other": 0},
                        "tests": [{"name": "test_regex", "status": test_status}],
                    }}))
            (root / "codex/comparison-context.json").write_text(json.dumps({
                "arm": "codex", "slice": "ab", "dataset": "terminal-bench/terminal-bench-2-1@6",
                "cohort": "selected-eight", "repeat_block": "1", "planned_trials": 5,
                "attempts_per_task": 5, "eval_source_commit": "frozen", "eval_source_dirty": False,
            }))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            rows = {row["trial"].split("__")[-1]: row for row in report["trials"]}
            self.assertEqual(rows["bootstrap"]["outcome"], "verifier_unverified")
            self.assertEqual(rows["bootstrap"]["verifier_evidence"]["status"], "missing_ctrf")
            self.assertEqual(rows["bootstrap"]["raw_reward"], 0)
            self.assertIsNone(rows["bootstrap"]["reward"])
            self.assertIs(rows["bootstrap"]["pass"], False)
            self.assertFalse(rows["bootstrap"]["eligible_pass_rate"])
            self.assertEqual(rows["asserted-fail"]["outcome"], "verifier_fail")
            self.assertEqual(rows["asserted-pass"]["outcome"], "verifier_pass")
            self.assertEqual(rows["asserted-pass"]["verifier_evidence"]["counts"]["passed"], 1)
            self.assertEqual(rows["unverified-pass"]["outcome"], "verifier_unverified")
            self.assertIs(rows["unverified-pass"]["pass"], True)
            self.assertEqual(rows["noncompleted"]["raw_reward"], 1)
            self.assertNotEqual(rows["noncompleted"]["termination"], "completed")
            self.assertIsNone(rows["noncompleted"]["pass"])
            arm = report["arms"][0]
            self.assertEqual((arm["attempts"], arm["eligible"], arm["eligible_passes"],
                              arm["passes"]), (5, 2, 1, 2))
            self.assertEqual(arm["pass_at_1"], 0.5)
            self.assertEqual(arm["operational_pass_per_attempt"], 0.4)

    def test_selected_rung_a_rejects_unasserted_or_mismatched_ctrf(self):
        sys.path.insert(0, str(ROOT / "eval"))
        from harbor_adapter.verifier_evidence import inspect_verifier
        from harbor_adapter.verifier_evidence import TASKS, DATASET
        with tempfile.TemporaryDirectory() as directory:
            trial = pathlib.Path(directory)
            verifier = trial / "verifier"
            verifier.mkdir()
            task = "terminal-bench/regex-log"
            ref, checksum = TASKS[task]
            def inspect(**overrides):
                args = {"dataset": DATASET, "slice_name": "ab", "task": task,
                        "task_ref": ref, "task_checksum": checksum}
                args.update(overrides)
                return inspect_verifier(trial, **args)
            (verifier / "ctrf.json").write_text(json.dumps({"results": {
                "summary": {"tests": 1, "passed": 0, "failed": 0,
                            "skipped": 1, "pending": 0, "other": 0},
                "tests": [{"name": "skipped", "status": "skipped"}],
            }}))
            self.assertEqual(inspect()["status"], "no_assertions")
            self.assertEqual(inspect(task_checksum="wrong")["status"], "task_binding_mismatch")
            (verifier / "ctrf.json").write_text("{invalid")
            self.assertEqual(inspect()["status"], "invalid_ctrf")
            (verifier / "ctrf.json").write_text(json.dumps({"results": {
                "summary": {"tests": 1, "passed": 1, "failed": 0,
                            "skipped": 0, "pending": 0, "other": 0},
                "tests": [{"status": "passed"}],
            }}))
            self.assertEqual(inspect()["status"], "invalid_ctrf")
            self.assertEqual(inspect(dataset="other/grader")["status"], "not_required")
            self.assertEqual(inspect(task="terminal-bench/other-task")["status"], "not_required")
            expanded_task = "terminal-bench/extract-elf"
            expanded_ref, expanded_checksum = TASKS[expanded_task]
            expanded = inspect_verifier(
                trial, dataset=DATASET, slice_name="expanded", task=expanded_task,
                task_ref=expanded_ref, task_checksum=expanded_checksum,
            )
            self.assertEqual(expanded["contract"], "rung-a-tb21-selected16-ctrf")
            self.assertEqual(inspect_verifier(trial / "not-archived", dataset=DATASET,
                                              slice_name="ab", task=task, task_ref=ref,
                                              task_checksum=checksum)["status"], "not_archived")

    def test_harbor_agent_timeout_is_explicit_and_unresolved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trial = self._trial(root, "codex", "task-a__timeout", reward=0,
                                exception="AgentTimeoutError", tokens={
                                    "n_input_tokens": None, "n_cache_tokens": None,
                                    "n_output_tokens": None,
                                    "metadata": {"usage_status": "missing"},
                                })
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            row = json.loads(completed.stdout)["trials"][0]
            self.assertEqual(row["outcome"], "agent_timeout")
            self.assertEqual(row["exception_type"], "AgentTimeoutError")
            self.assertEqual(row["raw_reward"], 0)
            self.assertIsNone(row["reward"])
            self.assertIsNone(row["pass"])
            self.assertFalse(row["eligible_pass_rate"])
            self.assertEqual(row["usage_status"], "partial")
            self.assertEqual(row["cost"]["implied_status"], "unresolved_usage")
            self.assertEqual(row["evidence"]["result"], str(trial / "result.json"))

    def test_pair_mismatch_and_repair_are_explicit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 100, "n_cache_tokens": 0,
                      "n_output_tokens": 10, "metadata": {"cache_write_tokens": 0}}
            self._trial(root, "left", "task-a__one", reward=1, tokens=tokens)
            self._trial(root, "left", "task-a__repair", reward=1, tokens=tokens,
                        repair_of="task-a__one")
            self._trial(root, "right", "task-a__one", reward=1, tokens=tokens,
                        source="bench/v2")
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            pair = report["pairs"][0]
            self.assertEqual(pair["status"], "mismatched")
            self.assertIn("source_or_task_mismatch", pair["reasons"])
            self.assertIn("repairs_present", pair["reasons"])
            self.assertEqual(pair["repair_attempts"]["left"], 1)
            self.assertEqual(len(report["trials"]), 3)

    def test_repair_only_group_cannot_match_as_original_repeat(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 10, "n_cache_tokens": 0,
                      "n_output_tokens": 1, "metadata": {"cache_write_tokens": 0}}
            for arm in ("left", "right"):
                self._trial(root, arm, "task-a__repair", reward=1, tokens=tokens,
                            repair_of="task-a__original")
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            pair = json.loads(completed.stdout)["pairs"][0]
            self.assertEqual(pair["status"], "mismatched")
            self.assertFalse(pair["comparison_eligible"])

    def test_selection_pins_hash_and_context_groups_timestamped_jobs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 100, "n_cache_tokens": 0,
                      "n_output_tokens": 10, "metadata": {"cache_write_tokens": 0}}
            first = self._trial(root, "arm-a-2026-one", "task-a__one", reward=1, tokens=tokens)
            second = self._trial(root, "arm-a-2026-two", "task-a__two", reward=0, tokens=tokens)
            for job in (first.parent, second.parent):
                (job.parent / "comparison-context.json").write_text(json.dumps({
                    "arm": "arm-a", "slice": "ab", "cohort": "cohort-1",
                    "eval_source_commit": "abc", "eval_source_dirty": False,
                    "model": "openai/gpt-6-luna", "effort": "low"}))
            digest = hashlib.sha256((first / "result.json").read_bytes()).hexdigest()
            index = root / "selection.json"
            index.write_text(json.dumps({"schema": "test", "records": [{
                "result_json_sha256": digest, "result_json_path": str(first / "result.json"),
                "arm": "arm-a", "slice": "ab", "source_commit": "abc",
                "task_id": "bench/task-a", "slot": 0}]}))
            command = [sys.executable, str(SCRIPT), "--json", "--selection", str(index), str(root)]
            completed = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(len(report["trials"]), 1)
            self.assertEqual(report["arms"][0]["arm"], "arm-a")
            self.assertEqual(report["selection"]["records"], 1)
            index.write_text(index.read_text().replace(digest, "0" * 64))
            rejected = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(rejected.returncode, 2)
            self.assertIn("hash is missing", rejected.stderr)

    def test_missing_result_with_agent_directory_is_an_attempt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trial = root / "arm" / "run" / "task-a__missing"
            (trial / "agent").mkdir(parents=True)
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(report["arms"][0]["attempts"], 1)
            self.assertEqual(report["trials"][0]["outcome"], "unreadable_result")
            self.assertEqual(report["trials"][0]["result_status"], "missing")

    def test_empty_invocation_writes_planned_unattempted_report(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "comparison-context.json").write_text(json.dumps({
                "arm": "arm-a", "planned_trials": 3, "tasks": ["task-a"],
                "attempts_per_task": 3}))
            output = root / "report"
            result = subprocess.run([sys.executable, str(SCRIPT), "--report", str(output), str(root)],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 1)
            report = json.loads((output / "eval-report.json").read_text())
            self.assertEqual(report["planned_unattempted"], 3)
            self.assertEqual(report["trials"], [])

    def test_selected_readability_ignores_unselected_bad_trial(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 100, "n_cache_tokens": 0,
                      "n_output_tokens": 10, "metadata": {"cache_write_tokens": 0}}
            selected = self._trial(root, "arm", "task-a__good", reward=1, tokens=tokens)
            bad = self._trial(root, "arm", "task-a__bad", reward=0, tokens=tokens)
            (bad / "result.json").write_text("{bad")
            digest = hashlib.sha256((selected / "result.json").read_bytes()).hexdigest()
            index = root / "selection.json"
            index.write_text(json.dumps({"record_count": 1, "records": [{
                "result_json_sha256": digest, "result_json_path": str(selected / "result.json"),
                "arm": "arm", "slice": "ab", "source_commit": "abc",
                "task_id": "bench/task-a", "slot": 0}]}))
            result = subprocess.run([
                sys.executable, str(SCRIPT), "--json", "--require-readable",
                "--expect-trials", "1", "--selection", str(index), str(root),
            ], text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(json.loads(result.stdout)["trials"]), 1)

    def test_three_arm_common_task_has_all_pairwise_latency_differences(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 100, "n_cache_tokens": 0,
                      "n_output_tokens": 10, "metadata": {"cache_write_tokens": 0}}
            for arm, model in (("one", "openai/gpt-6-luna"),
                               ("two", "chatgpt/gpt-6-luna"),
                               ("three", "gpt-6-luna")):
                trial = self._trial(root, arm, "task-a__one", reward=1, tokens=tokens)
                result_path = trial / "result.json"
                result_body = json.loads(result_path.read_text())
                result_body["config"]["agent"]["model_name"] = model
                result_body["config"]["agent"]["kwargs"].pop("reasoning_effort")
                if arm == "one":
                    result_body["config"]["agent"]["kwargs"]["effort"] = "none"
                result_path.write_text(json.dumps(result_body))
                (trial / "agent/vendor-artifact.json").write_text(json.dumps({
                    "schemaVersion": 1, "harness": "codex", "arch": "amd64",
                    "version": "1.0", "binaries": {"codex": {
                        "sha256": ("1" if arm == "one" else "2" if arm == "two" else "3") * 64,
                        "bytes": 100}}}))
                (trial.parent.parent / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "codex",
                    "model": "gpt-6-luna", "effort": "none",
                    "cohort": "c", "repeat_block": "1", "dataset": "bench/v1",
                    "eval_source_commit": "abc", "eval_source_dirty": False,
                    "arch": "amd64", "concurrency": 1, "attempts_per_task": 1,
                    "input_sha256": {"rate_card": "abc"},
                    "timeout_policy": "task-default", "adapter_task_retries": 0,
                    "native_internal_retries": "stock", "effort_semantics": "explicit",
                    "prompt_variant": "default"}))
            result = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            pairs = json.loads(result.stdout)["pairs"]
            self.assertEqual(len(pairs), 1)
            self.assertTrue(pairs[0]["comparison_eligible"])
            self.assertEqual(len(pairs[0]["paired_agent_execution_difference"]), 3)
            self.assertTrue(all(row["artifact"]["binaries"] for row in json.loads(result.stdout)["trials"]))

    def test_comparison_requires_executed_bindings_artifact_identity_and_repeat_slots(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            tokens = {"n_input_tokens": 10, "n_cache_tokens": 0,
                      "n_output_tokens": 1, "metadata": {"cache_write_tokens": 0}}
            for arm, slot in (("left", "one"), ("right", "two")):
                trial = self._trial(root, arm, f"task-a__{slot}", reward=1, tokens=tokens)
                body_path = trial / "result.json"
                body = json.loads(body_path.read_text())
                body["config"]["agent"]["model_name"] = None if arm == "left" else "openai/gpt-6-luna"
                body["config"]["agent"]["kwargs"]["reasoning_effort"] = (
                    "high" if arm == "right" else "low")
                body["config"]["agent"]["kwargs"]["version"] = "1.0"
                body_path.write_text(json.dumps(body))
                (trial / "agent/vendor-artifact.json").write_text(json.dumps({
                    "schemaVersion": 1, "harness": "pi" if arm == "left" else "codex",
                    "arch": "arm64" if arm == "left" else "amd64", "version": "wrong",
                    "binaries": {"codex": {"sha256": "1" * 64, "bytes": 1}}}))
                (trial.parent.parent / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "codex",
                    "model": "openai/gpt-6-luna", "effort": "none", "arch": "amd64",
                    "cohort": "c", "repeat_block": "1", "dataset": "bench/v1",
                    "eval_source_commit": "abc", "eval_source_dirty": False,
                    "concurrency": 1, "attempts_per_task": 1, "input_sha256": {},
                    "timeout_policy": "task-default", "adapter_task_retries": 0,
                    "native_internal_retries": "stock", "effort_semantics": "omit",
                    "prompt_variant": "default"}))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            pair = report["pairs"][0]
            self.assertFalse(pair["comparison_eligible"])
            self.assertNotIn("repeat_slot_mismatch", pair["reasons"])
            self.assertIn("context_result_mismatch", pair["reasons"])
            self.assertIn("artifact_missing_or_varied_within_arm", pair["reasons"])
            self.assertIn("model", report["trials"][0]["context_mismatches"])
            self.assertTrue(all("effort" in row["context_mismatches"] for row in report["trials"]))

    def test_single_attempt_random_suffixes_pair_with_hash_bound_plak_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for arm, suffix in (("plak-codex", "aBcDeFg"), ("pi-codex-oauth", "qRsTuVw")):
                trial = self._trial(root, arm, f"task-a__{suffix}", reward=1, tokens={})
                (trial / "agent/codex.txt").unlink()
                result_path = trial / "result.json"
                result = json.loads(result_path.read_text())
                result["config"]["agent"]["name"] = (
                    "harbor_adapter.agent:PlakAgent" if arm == "plak-codex" else
                    "harbor_adapter.vendor_agents:FrozenPi")
                result["config"]["agent"]["kwargs"] = (
                    {"account": "codex", "effort": "low"} if arm == "plak-codex" else
                    {"thinking": "low", "version": "0.87.1"})
                artifact = {"schemaVersion": 1, "harness": "plak" if arm == "plak-codex" else "pi",
                            "arch": "arm64", "binaries": {"agent": {"sha256": "1" * 64, "bytes": 1}}}
                if arm == "plak-codex":
                    result["agent_result"] = {"metadata": {"artifact": artifact}}
                    result["agent_info"] = {"version": "plak 0.1.0"}
                else:
                    artifact["version"] = "0.87.1"
                    (trial / "agent/pi.txt").write_text("message_end\n")
                    (trial / "agent/vendor-artifact.json").write_text(json.dumps(artifact))
                result_path.write_text(json.dumps(result))
                (trial.parent.parent / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "codex", "model": "openai/gpt-6-luna",
                    "effort": "low", "arch": "arm64", "cohort": "c", "repeat_block": "1",
                    "dataset": "bench/v1", "eval_source_commit": "abc", "eval_source_dirty": False,
                    "concurrency": 1, "attempts_per_task": 1, "input_sha256": {},
                    "timeout_policy": "task-default", "adapter_task_retries": 0,
                    "native_internal_retries": "stock", "effort_semantics": "explicit",
                    "prompt_variant": "default"}))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            pair = json.loads(completed.stdout)["pairs"][0]
            self.assertTrue(pair["comparison_eligible"], pair["reasons"])

    def test_prompt_ablation_allows_only_executed_variant_delta_on_one_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trials = {}
            artifact = {"schemaVersion": 1, "harness": "plak", "arch": "arm64",
                        "binaries": {"plak": {"sha256": "1" * 64, "bytes": 1}}}
            for variant in ("legacy", "parity"):
                arm = f"plak-codex-{variant}"
                trial = self._trial(root, arm, f"task-a__{variant}", reward=1, tokens={})
                trials[variant] = trial
                (trial / "agent/codex.txt").unlink()
                (trial / "agent/plak-exit.txt").write_text("0\n")
                result_path = trial / "result.json"
                result = json.loads(result_path.read_text())
                result["config"]["agent"]["name"] = "harbor_adapter.agent:PlakAgent"
                result["config"]["agent"]["kwargs"] = {
                    "account": "codex", "effort": "low", "prompt_variant": variant,
                }
                result["agent_result"] = {"metadata": {"artifact": artifact}}
                result["agent_info"] = {"version": "plak 0.1.0"}
                result_path.write_text(json.dumps(result))
                (trial.parent.parent / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "codex", "model": "gpt-6-luna",
                    "effort": "low", "arch": "arm64", "cohort": "c", "repeat_block": "1",
                    "dataset": "bench/v1", "eval_source_commit": "abc", "eval_source_dirty": False,
                    "concurrency": 1, "attempts_per_task": 1, "input_sha256": {},
                    "timeout_policy": "task-default", "adapter_task_retries": 0,
                    "native_internal_retries": "stock", "effort_semantics": "explicit",
                    "prompt_variant": variant,
                }))

            def report(*options):
                return subprocess.run([sys.executable, str(SCRIPT), "--json", *options,
                                       str(root)], text=True, capture_output=True)

            ordinary = json.loads(report().stdout)["pairs"][0]
            self.assertFalse(ordinary["comparison_eligible"])
            self.assertIn("control_unverified_or_mismatched:prompt_variant", ordinary["reasons"])
            paired = report("--prompt-ablation")
            self.assertEqual(paired.returncode, 0, paired.stderr)
            self.assertTrue(json.loads(paired.stdout)["pairs"][0]["comparison_eligible"])

            context_path = trials["parity"].parent.parent / "comparison-context.json"
            context = json.loads(context_path.read_text())
            context["eval_source_dirty"] = True
            context_path.write_text(json.dumps(context))
            pair = json.loads(report("--prompt-ablation").stdout)["pairs"][0]
            self.assertFalse(pair["comparison_eligible"])
            self.assertIn("dirty_eval_source", pair["reasons"])
            context["eval_source_dirty"] = False
            context["timeout_policy"] = "different"
            context_path.write_text(json.dumps(context))
            pair = json.loads(report("--prompt-ablation").stdout)["pairs"][0]
            self.assertFalse(pair["comparison_eligible"])
            self.assertIn("control_unverified_or_mismatched:timeout_policy", pair["reasons"])
            context["timeout_policy"] = "task-default"
            context_path.write_text(json.dumps(context))

            result_path = trials["parity"] / "result.json"
            result = json.loads(result_path.read_text())
            result["config"]["agent"]["kwargs"]["prompt_variant"] = "legacy"
            result_path.write_text(json.dumps(result))
            pair = json.loads(report("--prompt-ablation").stdout)["pairs"][0]
            self.assertFalse(pair["comparison_eligible"])
            self.assertIn("context_result_mismatch", pair["reasons"])
            result["config"]["agent"]["kwargs"]["prompt_variant"] = "parity"
            result["agent_result"]["metadata"]["artifact"]["binaries"]["plak"]["sha256"] = "2" * 64
            result_path.write_text(json.dumps(result))
            pair = json.loads(report("--prompt-ablation").stdout)["pairs"][0]
            self.assertFalse(pair["comparison_eligible"])
            self.assertIn("artifact_missing_or_varied_within_arm", pair["reasons"])
            context["arm"] = "plak-grok-parity"
            context_path.write_text(json.dumps(context))
            refused = report("--prompt-ablation")
            self.assertEqual(refused.returncode, 2)
            self.assertIn("requires one legacy/parity Plak arm pair", refused.stderr)

    def test_pmux_report_requires_matching_pin_reported_version_and_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for arm in ("plak-claude", "claude-code"):
                trial = self._trial(root, arm, f"task-a__{arm}", reward=1, tokens={})
                result_path = trial / "result.json"
                result = json.loads(result_path.read_text())
                result["config"]["agent"]["name"] = (
                    "harbor_adapter.pmux_agent:FrozenPmux" if arm == "claude-code" else
                    "harbor_adapter.agent:PlakAgent")
                result["config"]["agent"]["model_name"] = "claude-opus-5-5"
                result["config"]["agent"]["kwargs"] = {"effort": "low", "account": "claude"}
                if arm == "claude-code":
                    (trial / "agent/codex.txt").unlink()
                    (trial / "agent/pmux.txt").write_text("completed\n")
                    result["agent_info"] = {"version": "2.1.280 (Claude Code)"}
                    result["agent_result"] = {"metadata": {"claude_pin": "2.1.280"}}
                    artifact = {"schemaVersion": 1, "harness": "pmux", "arch": "arm64",
                                "version": "2.1.280",
                                "source": {"repo": "pseudomux", "commit": "abc", "dirty": False},
                                "binaries": {name: {"sha256": "1" * 64, "bytes": 1}
                                             for name in ("pmux", "pmuxd", "pmux-rmuxd",
                                                          "pmux-launcher", "pmux-hook",
                                                          "claude-archive", "node-archive")}}
                    (trial / "agent/vendor-artifact.json").write_text(json.dumps(artifact))
                else:
                    (trial / "agent/codex.txt").unlink()
                    (trial / "agent/plak-exit.txt").write_text("0\n")
                    result["agent_result"] = {"metadata": {"artifact": {
                        "schemaVersion": 1, "harness": "plak", "arch": "arm64",
                        "binaries": {"plak": {"sha256": "2" * 64, "bytes": 2}}}}}
                result_path.write_text(json.dumps(result))
                (trial.parent.parent / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "claude", "model": "claude-opus-5-5",
                    "effort": "low", "arch": "arm64", "cohort": "c", "repeat_block": "1",
                    "dataset": "bench/v1", "eval_source_commit": "abc", "eval_source_dirty": False,
                    "concurrency": 1, "attempts_per_task": 1, "input_sha256": {},
                    "timeout_policy": "task-default", "adapter_task_retries": 0,
                    "native_internal_retries": "stock", "effort_semantics": "explicit",
                    "prompt_variant": "default"}))

            def pair():
                completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                           text=True, capture_output=True)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                report = json.loads(completed.stdout)
                self.assertTrue(all(t["artifact"] for t in report["trials"]), report["trials"])
                return report["pairs"][0]

            self.assertTrue(pair()["comparison_eligible"], pair()["reasons"])
            for arm in ("plak-claude", "claude-code"):
                source = root / arm / "run-one" / f"task-a__{arm}"
                other = root / arm / "run-one" / f"task-b__{arm}"
                shutil.copytree(source, other)
                result_path = other / "result.json"
                result = json.loads(result_path.read_text())
                result.update(task_name="bench/task-b", trial_name=other.name,
                              task_id={"ref": "sha256:two"}, task_checksum="checksum-two")
                result["config"]["task"]["ref"] = "sha256:two"
                result_path.write_text(json.dumps(result))
            other_artifact = root / "claude-code/run-one/task-b__claude-code/agent/vendor-artifact.json"
            changed = json.loads(other_artifact.read_text())
            changed["binaries"]["claude-archive"]["sha256"] = "9" * 64
            other_artifact.write_text(json.dumps(changed))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(len(json.loads(completed.stdout)["pairs"]), 2)
            for entry in json.loads(completed.stdout)["pairs"]:
                self.assertIn("artifact_missing_or_varied_within_arm", entry["reasons"])
            changed["binaries"]["claude-archive"]["sha256"] = "1" * 64
            other_artifact.write_text(json.dumps(changed))
            pmux_trial = root / "claude-code/run-one/task-a__claude-code"
            result_path = pmux_trial / "result.json"
            result = json.loads(result_path.read_text())
            result["agent_result"]["metadata"]["claude_pin"] = "2.1.279"
            result_path.write_text(json.dumps(result))
            self.assertIn("artifact_missing_or_varied_within_arm", pair()["reasons"])
            result["agent_result"]["metadata"]["claude_pin"] = "2.1.280"
            result_path.write_text(json.dumps(result))
            artifact_path = pmux_trial / "agent/vendor-artifact.json"
            artifact = json.loads(artifact_path.read_text())
            artifact["binaries"].pop("claude-archive")
            artifact_path.write_text(json.dumps(artifact))
            self.assertIn("artifact_missing_or_varied_within_arm", pair()["reasons"])
            artifact["binaries"]["claude-archive"] = {"sha256": "3" * 64, "bytes": 3}
            artifact["source"]["dirty"] = True
            artifact_path.write_text(json.dumps(artifact))
            self.assertIn("artifact_missing_or_varied_within_arm", pair()["reasons"])

    def test_repeats_without_explicit_shared_slots_remain_unverified(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for arm in ("left", "right"):
                for suffix in ("random-one", "random-two"):
                    trial = self._trial(root, arm, f"task-a__{arm}-{suffix}", reward=1, tokens={})
                    (trial / "agent/vendor-artifact.json").write_text(json.dumps({
                        "schemaVersion": 1, "harness": "codex", "arch": "arm64",
                        "version": "0.155.1", "binaries": {"codex": {"sha256": "1" * 64, "bytes": 1}}}))
                    body_path = trial / "result.json"
                    body = json.loads(body_path.read_text())
                    body["config"]["agent"]["kwargs"]["version"] = "0.155.1"
                    body_path.write_text(json.dumps(body))
                (root / arm / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "codex", "model": "openai/gpt-6-luna",
                    "effort": "low", "arch": "arm64", "cohort": "c", "repeat_block": "1",
                    "dataset": "bench/v1", "eval_source_commit": "abc", "eval_source_dirty": False,
                    "concurrency": 1, "attempts_per_task": 2, "input_sha256": {},
                    "timeout_policy": "task-default", "adapter_task_retries": 0,
                    "native_internal_retries": "stock", "effort_semantics": "explicit",
                    "prompt_variant": "default"}))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            pair = json.loads(completed.stdout)["pairs"][0]
            self.assertFalse(pair["comparison_eligible"])
            self.assertIn("repeat_slot_unverified", pair["reasons"])

    def test_selection_repeats_require_same_explicit_slots(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            records = []
            for arm, slots in (("left", (0, 1)), ("right", (0, 2))):
                for index, slot in enumerate(slots):
                    trial = self._trial(root, arm, f"task-a__{arm}-{index}", reward=1, tokens={})
                    result_path = trial / "result.json"
                    records.append({"result_json_sha256": hashlib.sha256(result_path.read_bytes()).hexdigest(),
                                    "result_json_path": str(result_path), "arm": arm, "slice": "ab",
                                    "source_commit": "abc", "task_id": "bench/task-a", "slot": slot})
                (root / arm / "comparison-context.json").write_text(json.dumps({
                    "arm": arm, "slice": "ab", "account": "codex", "model": "openai/gpt-6-luna",
                    "effort": "low", "cohort": "c", "repeat_block": "1"}))
            index_path = root / "selection.json"
            index_path.write_text(json.dumps({"records": records}))
            command = [sys.executable, str(SCRIPT), "--json", "--selection", str(index_path), str(root)]
            completed = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("repeat_slot_mismatch", json.loads(completed.stdout)["pairs"][0]["reasons"])
            records[0]["slot"] = []
            index_path.write_text(json.dumps({"records": records}))
            malformed = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(malformed.returncode, 2)
            self.assertIn("selection slot", malformed.stderr)

    def test_repaired_result_is_diagnostic_but_not_score_eligible(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trial = self._trial(root, "codex", "task-a__one", reward=1, tokens={})
            path = trial / "result.json"
            body = json.loads(path.read_text())
            body["scrubbed_by_harbor"] = True
            path.write_text(json.dumps(body).replace("true", "[REDACTED]"))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            row = json.loads(completed.stdout)["trials"][0]
            self.assertEqual(row["result_status"], "repaired")
            self.assertEqual(row["outcome"], "verifier_pass")
            self.assertFalse(row["eligible_pass_rate"])
            self.assertIsNone(row["pass"])
            self.assertEqual(row["raw_reward"], 1)
            self.assertIsNone(row["reward"])

    def test_pi_codex_result_binds_executed_route_and_thinking(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trial = self._trial(root, "pi-codex-oauth", "task-a__one", reward=1, tokens={})
            (trial / "agent/codex.txt").unlink()
            (trial / "agent/pi.txt").write_text("message_end\n")
            result_path = trial / "result.json"
            result = json.loads(result_path.read_text())
            result["config"]["agent"]["name"] = "harbor_adapter.vendor_agents:FrozenPi"
            result["config"]["agent"]["kwargs"] = {"thinking": "low", "version": "0.87.1"}
            result_path.write_text(json.dumps(result))
            context = {"arm": "pi-codex-oauth", "model": "openai/gpt-6-luna",
                       "account": "codex", "effort": "low"}
            (trial.parent.parent / "comparison-context.json").write_text(json.dumps(context))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            row = json.loads(completed.stdout)["trials"][0]
            self.assertEqual(row["harness"], "pi")
            self.assertEqual(row["effort"], "low")
            self.assertEqual(row["context_mismatches"], [])
            result["config"]["agent"]["model_name"] = "openrouter/gpt-6-luna"
            result_path.write_text(json.dumps(result))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("account_or_route", json.loads(completed.stdout)["trials"][0]["context_mismatches"])

    def test_native_trajectory_recovers_cache_write_without_result_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trial = self._trial(root, "codex", "task-a__one", reward=1, tokens={
                "n_input_tokens": 100, "n_cache_tokens": 20,
                "n_output_tokens": 10, "cost_usd": None})
            (trial / "agent/trajectory.json").write_text(json.dumps({"schema_version": "ATIF-v1.7", "final_metrics": {
                "total_prompt_tokens": 100, "total_cached_tokens": 20,
                "total_completion_tokens": 10,
                "extra": {"total_cache_write_input_tokens": 5}}}))
            result = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            row = report["trials"][0]
            self.assertEqual(row["tokens"]["cache_write"], 5)
            self.assertEqual(row["usage_status"], "complete")
            self.assertEqual(row["native_usage"]["source_schema"], "ATIF-v1.7.final_metrics")
            self.assertIsNotNone(row["cost"]["implied_usd"])

    def test_pi_legacy_and_normalized_harbor_inputs_both_bind(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            events = [
                {"type": "message_end", "message": {"role": "assistant", "usage": {
                    "input": 10, "cacheRead": 20, "cacheWrite": 5, "output": 4}}},
                {"type": "agent_end"},
            ]
            for name, harbor_input, normalized in (("old", 30, False), ("new", 35, True)):
                trial = self._trial(root, f"pi-{name}", f"task-a__{name}", reward=1,
                    tokens={"n_input_tokens": harbor_input, "n_cache_tokens": 20,
                            "n_output_tokens": 4, "metadata": {"usage_status": "complete"}})
                (trial / "agent/codex.txt").unlink()
                native_path = trial / "agent/pi.txt"
                native_path.write_text("\n".join(json.dumps(e) for e in events) + "\n")
                body = json.loads((trial / "result.json").read_text())
                body["config"]["agent"]["name"] = "harbor_adapter.vendor_agents:FrozenPi"
                if normalized:
                    body["agent_result"]["metadata"]["native_usage"] = {
                        "schemaVersion": 1, "harness": "pi",
                        "source_sha256": hashlib.sha256(native_path.read_bytes()).hexdigest()}
                (trial / "result.json").write_text(json.dumps(body))
            result = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = json.loads(result.stdout)["trials"]
            self.assertEqual(len(rows), 2)
            for row in rows:
                self.assertFalse(row["native_harbor_disagreement"])
                self.assertEqual(row["tokens"]["input"], 35)
                self.assertIsNotNone(row["cost"]["implied_usd"])

    def test_plak_archive_precedence_and_result_disagreement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name, raw_input, db_mode in (("fresh", 100, "absent"),
                                             ("unverified", 100, "absent"),
                                             ("legacy", 100, "match"),
                                             ("conflict", 99, "match"),
                                             ("ambiguous", 100, "ambiguous")):
                trial = self._trial(root, name, f"task-a__{name}", reward=1, tokens={
                    "n_input_tokens": raw_input, "n_cache_tokens": 20,
                    "n_output_tokens": 10, "metadata": {"usage_status": "complete",
                                                          "cache_write_tokens": 30,
                                                          **({"usage_scope": "turns_plus_compactions_v1"}
                                                             if name == "fresh" else {})}})
                (trial / "agent/codex.txt").unlink()
                (trial / "agent/plak-exit.txt").write_text("0\n")
                result_path = trial / "result.json"
                body = json.loads(result_path.read_text())
                body["config"]["agent"]["name"] = "harbor_adapter.agent:PlakAgent"
                result_path.write_text(json.dumps(body))
                if db_mode == "absent":
                    continue
                home = trial / "agent/plak-home"
                product = home / "state/product.sqlite3"
                telemetry = home / "telemetry/telemetry.sqlite3"
                product.parent.mkdir(parents=True)
                telemetry.parent.mkdir(parents=True)
                with closing(sqlite3.connect(product)) as db, db:
                    db.execute("CREATE TABLE sessions (id TEXT, locator TEXT)")
                    db.execute("CREATE TABLE workflows (session_id TEXT, kind TEXT)")
                    db.execute("INSERT INTO sessions VALUES ('product-id', 'runtime-id/main')")
                    if db_mode == "ambiguous":
                        db.execute("INSERT INTO sessions VALUES ('other', 'other/main')")
                db.close()
                with closing(sqlite3.connect(telemetry)) as db, db:
                    db.execute("PRAGMA user_version = 7")
                    db.execute("CREATE TABLE turns (session_id TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cost_micros INTEGER, cost_status TEXT, cache_write_tokens INTEGER, turn_id TEXT, compactions INTEGER)")
                    db.execute("CREATE TABLE compactions (session_id TEXT, outcome TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cache_write_tokens INTEGER, turn_id TEXT)")
                    db.execute("INSERT INTO turns VALUES ('runtime-id', 100, 20, 10, 2, NULL, 'unknown', 30, 'turn-id', 0)")
                db.close()
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            rows = {row["trial"].split("__")[1]: row for row in report["trials"]}
            self.assertEqual(rows["fresh"]["usage_status"], "complete")
            self.assertIsNotNone(rows["fresh"]["cost"]["implied_usd"])
            self.assertEqual(rows["unverified"]["usage_status"], "partial")
            self.assertIn("compaction_scope_unverified", rows["unverified"]["missing_usage_buckets"])
            self.assertEqual(rows["legacy"]["archived_plak_usage"]["status"], "complete")
            self.assertFalse(rows["legacy"]["archived_plak_harbor_disagreement"])
            self.assertEqual(rows["conflict"]["usage_status"], "partial")
            self.assertTrue(rows["conflict"]["archived_plak_harbor_disagreement"])
            self.assertIsNone(rows["conflict"]["cost"]["implied_usd"])
            self.assertEqual(rows["ambiguous"]["archived_plak_usage"]["status"], "ambiguous_session")
            self.assertEqual(rows["ambiguous"]["usage_status"], "partial")

    def test_plak_compactions_keep_observed_tokens_and_unknown_total_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name, normalized in (("historical", False), ("normalized", True),
                                     ("schema8", True), ("turn-missing", True),
                                     ("failed-compact", False), ("missing-row", False)):
                trial = self._trial(root, name, f"task-a__{name}", reward=1, tokens={
                    "n_input_tokens": 107 if normalized else 100,
                    "n_cache_tokens": 22 if normalized else 20,
                    "n_output_tokens": 13 if normalized else 10,
                    "cost_usd": .0012 if name == "schema8" else None if normalized else .001,
                    "metadata": {"usage_status": "complete", "cache_write_tokens": 30,
                                 "cost_status": "reported" if name == "schema8" else "partial",
                                 **({"usage_scope": "turns_plus_compactions_v1"} if normalized else {})},
                })
                (trial / "agent/codex.txt").unlink()
                (trial / "agent/plak-exit.txt").write_text("0\n")
                result_path = trial / "result.json"
                body = json.loads(result_path.read_text())
                body["config"]["agent"]["name"] = "harbor_adapter.agent:PlakAgent"
                result_path.write_text(json.dumps(body))
                home = trial / "agent/plak-home"
                product = home / "state/product.sqlite3"
                telemetry = home / "telemetry/telemetry.sqlite3"
                product.parent.mkdir(parents=True)
                telemetry.parent.mkdir(parents=True)
                with closing(sqlite3.connect(product)) as db, db:
                    db.execute("CREATE TABLE sessions (id TEXT, locator TEXT)")
                    db.execute("CREATE TABLE workflows (session_id TEXT, kind TEXT)")
                    db.execute("INSERT INTO sessions VALUES ('product-id', 'runtime-id/main')")
                with closing(sqlite3.connect(telemetry)) as db, db:
                    schema8 = name in ("schema8", "turn-missing")
                    db.execute(f"PRAGMA user_version = {8 if schema8 else 7}")
                    db.execute("CREATE TABLE turns (session_id TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cost_micros INTEGER, cost_status TEXT, cache_write_tokens INTEGER, turn_id TEXT, compactions INTEGER)")
                    cost_columns = ", cost_micros INTEGER, cost_status TEXT" if schema8 else ""
                    db.execute("CREATE TABLE compactions (session_id TEXT, outcome TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cache_write_tokens INTEGER, turn_id TEXT" + cost_columns + ")")
                    turn_cost = "NULL, 'unknown'" if name == "turn-missing" else "1000, 'reported'"
                    expected_compactions = 2 if name == "failed-compact" else 1
                    db.execute(f"INSERT INTO turns VALUES ('runtime-id', 100, 20, 10, 2, {turn_cost}, 30, 'turn-id', {expected_compactions})")
                    if name != "missing-row":
                        db.execute("INSERT INTO compactions VALUES ('runtime-id', 'committed', 7, 2, 3, 1, 0, 'turn-id'" + (", 200, 'reported'" if schema8 else "") + ")")
                    if name == "failed-compact":
                        db.execute("INSERT INTO compactions VALUES ('runtime-id', 'failed', NULL, NULL, NULL, NULL, NULL, 'turn-id')")
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            rows = {row["trial"].split("__")[1]: row for row in report["trials"]}
            for name in ("historical", "normalized"):
                row = rows[name]
                self.assertEqual(row["tokens"]["input"], 107)
                self.assertEqual(row["tokens"]["cache_read"], 22)
                self.assertEqual(row["tokens"]["output"], 13)
                self.assertFalse(row["archived_plak_harbor_disagreement"])
                self.assertTrue(row["tokens_complete"])
                self.assertIsNotNone(row["cost"]["implied_usd"])
                self.assertEqual(row["cost"]["reported_usd"], .001)
                self.assertEqual(row["cost"]["reported_status"], "turn_subtotal_excludes_compactions")
            schema8 = rows["schema8"]
            self.assertFalse(schema8["archived_plak_harbor_disagreement"])
            self.assertEqual(schema8["cost"]["reported_usd"], .0012)
            self.assertEqual(schema8["cost"]["reported_status"], "reported")
            turn_missing = rows["turn-missing"]
            self.assertTrue(turn_missing["tokens_complete"])
            self.assertEqual(turn_missing["cost"]["reported_usd"], .0002)
            self.assertEqual(turn_missing["cost"]["reported_status"],
                             "observed_subtotal_incomplete_reported_cost")
            missing_row = rows["missing-row"]
            self.assertFalse(missing_row["tokens_complete"])
            self.assertEqual(missing_row["cost"]["reported_usd"], .001)
            self.assertEqual(missing_row["cost"]["reported_status"],
                             "observed_subtotal_unverified_compaction_scope")
            arms = {arm["arm"].split("|")[0]: arm for arm in report["arms"]}
            self.assertIsNone(arms["historical"]["reported_cost_usd"]["complete_total"])
            self.assertEqual(arms["historical"]["reported_cost_usd"]["observed_total"], .001)
            self.assertEqual(arms["schema8"]["reported_cost_usd"]["complete_total"], .0012)
            failed = rows["failed-compact"]
            self.assertEqual(failed["tokens"]["input"], 107)
            self.assertFalse(failed["tokens_complete"])
            self.assertIsNone(failed["cost"]["implied_usd"])
            self.assertIn("input", failed["missing_usage_buckets"])

    def test_pmux_historical_remainder_and_modern_total_both_bind(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            stdout = json.dumps({"usage": {"combined": {
                "input_tokens": 10, "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 5, "output_tokens": 4}}})
            for name, harbor_input, modern in (("old", 10, False),
                                               ("archived-total", 35, False),
                                               ("new", 35, True)):
                trial = self._trial(root, f"claude-{name}", f"task-a__{name}", reward=1,
                    tokens={"n_input_tokens": harbor_input, "n_cache_tokens": 20,
                            "n_output_tokens": 4,
                            "metadata": {"usage_status": "complete", "cache_write_tokens": 5}
                            if modern else {"usage_resolved": True}})
                (trial / "agent/codex.txt").unlink()
                (trial / "agent/pmux-stdout.txt").write_text(stdout)
                body = json.loads((trial / "result.json").read_text())
                body["config"]["agent"]["name"] = "harbor_adapter.pmux_agent:FrozenPmux"
                (trial / "result.json").write_text(json.dumps(body))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            rows = json.loads(completed.stdout)["trials"]
            self.assertEqual(len(rows), 3)
            for row in rows:
                self.assertEqual(row["tokens"]["input"], 35)
                self.assertEqual(row["tokens"]["cache_write"], 5)
                self.assertEqual(row["usage_status"], "complete")
                self.assertFalse(row["native_harbor_disagreement"])
                self.assertIsNotNone(row["cost"]["implied_usd"])

    def test_failed_turn_keeps_observed_tokens_but_not_complete_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            trial = self._trial(root, "codex", "task-a__failed", reward=0,
                exception="NonZeroAgentExitCodeError",
                tokens={"n_input_tokens": 100, "n_cache_tokens": 20,
                        "n_output_tokens": 10,
                        "metadata": {"usage_status": "complete", "cache_write_tokens": 0}})
            (trial / "agent/trajectory.json").write_text(json.dumps({
                "schema_version": "ATIF-v1.7", "final_metrics": {
                    "total_prompt_tokens": 100, "total_cached_tokens": 20,
                    "total_completion_tokens": 10,
                    "extra": {"total_cache_write_input_tokens": 0}}}))
            completed = subprocess.run([sys.executable, str(SCRIPT), "--json", str(root)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            row = json.loads(completed.stdout)["trials"][0]
            self.assertEqual(row["tokens"]["input"], 100)
            self.assertEqual(row["usage_status"], "partial")
            self.assertIn("request_completion_unverified", row["missing_usage_buckets"])
            self.assertIsNone(row["cost"]["implied_usd"])


if __name__ == "__main__":
    unittest.main()
