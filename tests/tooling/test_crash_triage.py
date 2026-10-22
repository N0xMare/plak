"""Recovery projection diagnostics, never runtime recovery or inference."""

import subprocess
import re
import sys
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests" / "live"))
import crash_triage
import live
import jobs
import long_session
from types import SimpleNamespace


def show(phase, actions):
    return ("Session private-session\nCanonical recovery (root session)\n"
            "  Status         recovery required\n"
            f"  Phase          {phase}\n" + "".join(
                f"  Obligation     operation={op} action={action} request_identity=SECRET\n"
                for op, action in actions))


class CrashTriageTests(unittest.TestCase):
    def test_projection_retains_order_but_not_request_identity_or_children(self):
        text = show("DispatchEvidence", [(4, "ProviderEvidenceRequired"), (1, "FinalizationRequired")])
        text += "\nTopology children (durable snapshot)\n    Phase        EffectDecision\n"
        report = crash_triage.decode_show(text)
        self.assertEqual(report.phase, "DispatchEvidence")
        self.assertEqual(report.obligations, ((4, "ProviderEvidenceRequired"), (1, "FinalizationRequired")))
        self.assertEqual(crash_triage.bucket(report), "unknown")
        detail = crash_triage.detail(report)
        self.assertIn("status=recovery required", detail)
        self.assertIn("phase=DispatchEvidence", detail)
        self.assertNotIn("SECRET", detail)
        self.assertNotIn("private-session", detail)

    def test_covered_quiescent_and_known_uncovered_restore_and_code_cell(self):
        self.assertEqual(crash_triage.bucket(crash_triage.decode_show(
            "Canonical recovery (root session)\n  Status         quiescent at SECRET\n")), "covered")
        for phase, action in [
            ("EffectDecision", "RestoreEffectDecision"),
            ("HostedContinuation", "RestoreToolBatch"),
            ("HostedContinuation", "RestoreServerPause"),
            ("EffectReconciliation", "EvaluationAbandonmentRequired"),
        ]:
            self.assertEqual(crash_triage.bucket(crash_triage.decode_show(show(phase, [(2, action)]))),
                             "known-uncovered")
        for text in ("", show("Finalization", [(1, "FinalizationRequired")]),
                     show("CommitRecovery", [(1, "CommitRecovery")])):
            self.assertEqual(crash_triage.bucket(crash_triage.decode_show(text)), "unknown")

    def test_read_only_commands_use_scenario_environment_and_optional_verdict(self):
        completed = [subprocess.CompletedProcess([], 0, show("Finalization", [(1, "FinalizationRequired")]), ""),
                     subprocess.CompletedProcess([], 1, "root: step=runtime_inspection\n"
                                                 "root: offline=RefusesRecoveryScope recovery_phase=Some(Finalization) evaluator=verified process_budgets=verified projects:3 laws=refused\n", "SECRET")]
        with patch.object(crash_triage.subprocess, "run", side_effect=completed) as run:
            result = crash_triage.inspect(Path("/binary"), Path("/repo"), Path("/home"), "session")
        self.assertEqual(run.call_args_list[0].args[0], ["/binary", "session", "show", "session"])
        self.assertEqual(run.call_args_list[1].args[0], ["/binary", "session", "rehearse", "session"])
        for call in run.call_args_list:
            self.assertEqual(call.kwargs["cwd"], Path("/repo"))
            self.assertEqual(call.kwargs["env"]["PLAK_HOME"], "/home")
            self.assertNotIn("PLAK_TEST_EXCHANGE_CAPTURE_DIR", call.kwargs["env"])
        self.assertIn("bucket=unknown", result)
        self.assertIn("rehearsal=offline=RefusesRecoveryScope", result)
        self.assertIn("process_budgets=verified laws=refused", result)
        self.assertNotIn("SECRET", result)

    def test_rehearsal_verdict_reads_the_root_line_with_or_without_trailing_fields(self):
        line = ("root: offline=OpensQuiescent recovery_phase=None evaluator=refused: recorded_artifact "
                "process_budgets=refused")
        self.assertEqual(crash_triage.rehearsal_verdict(line + " projects:0 laws=valid\n"),
                         "offline=OpensQuiescent recovery_phase=None evaluator=refused: "
                         "recorded_artifact process_budgets=refused laws=valid")
        self.assertEqual(crash_triage.rehearsal_verdict(line + "\n"),
                         "offline=OpensQuiescent recovery_phase=None evaluator=refused: "
                         "recorded_artifact process_budgets=refused")
        self.assertIsNone(crash_triage.rehearsal_verdict(line + " projects:0 laws=valid extra\n"))
        self.assertEqual(crash_triage.rehearsal_verdict("root: refuses: Corrupt\n"), "refuses:Corrupt")

    def test_failed_inspection_is_unknown_and_unsupported_rehearsal_is_silent(self):
        with patch.object(crash_triage.subprocess, "run", return_value=
                          subprocess.CompletedProcess([], 1, "", "SECRET")):
            result = crash_triage.inspect(Path("/binary"), Path("/repo"), Path("/home"), "session")
        self.assertIn("bucket=unknown", result)
        self.assertIn("status=unavailable", result)
        self.assertNotIn("SECRET", result)
        self.assertNotIn("rehearsal=", result)

    def test_reopen_preserves_scoring_and_does_not_triage_unrelated_failures(self):
        for message in ("plak agent did not become ready: {'kind': 'bye'}", "exit code 5"):
            with patch.object(crash_triage, "inspect", return_value="bucket=unknown") as inspect:
                def refused():
                    raise live.ScenarioFailure(live.HARNESS, message)
                with self.assertRaises(live.ScenarioFailure) as caught:
                    crash_triage.reopen(refused, Path("binary"), Path("repo"), Path("home"), "session")
            self.assertEqual(caught.exception.outcome, live.HARNESS)
            self.assertIn("bucket=unknown", str(caught.exception))
            inspect.assert_called_once()
        with patch.object(crash_triage, "inspect") as inspect:
            def failed():
                raise live.ScenarioFailure(live.PROVIDER, "authentication failed")
            with self.assertRaises(live.ScenarioFailure):
                crash_triage.reopen(failed, Path("binary"), Path("repo"), Path("home"), "session")
            inspect.assert_not_called()

    def test_crash_jobs_triages_the_private_home_after_kill(self):
        binary, repo, home = Path("binary"), Path("repo"), Path("home")
        driver = jobs.Jobs(live.ROUTES["grok"], SimpleNamespace(
            binary=binary, capture_segments=[], fixture_seed=None))
        agent = Mock(session_id="session", observed=[])
        def killed(_agent, _prompt, _timeout, tick, _accept_eof):
            tick(None, {"kind": "job_promoted", "job_id": "p1"})
        with patch.object(driver, "prepare", return_value=(repo, home)), \
             patch.object(driver, "open_agent", side_effect=[agent, live.ScenarioFailure(
                 live.HARNESS, "plak agent did not become ready: {'kind': 'bye'}")]), \
             patch.object(jobs, "run_turn", side_effect=killed), \
             patch.object(jobs, "job_id_for", return_value="p1"), \
             patch.object(jobs, "marker", return_value=False), \
             patch.object(jobs, "command_lines", return_value=[]), \
             patch.object(jobs, "matching", return_value=["live job"]), \
             patch.object(jobs, "exchange_settled", return_value=True), \
             patch.object(jobs, "close_agent"), patch.object(jobs, "stop_token"), \
             patch.object(crash_triage, "inspect", return_value="bucket=unknown") as inspect:
            with self.assertRaises(live.ScenarioFailure) as caught:
                driver.crash_jobs()
        self.assertEqual(caught.exception.outcome, live.HARNESS)
        self.assertIn("bucket=unknown", str(caught.exception))
        inspect.assert_called_once_with(binary, repo, home, "session")

    def test_kill_reopen_triages_the_private_home_after_kill(self):
        binary, repo, home = Path("binary"), Path("repo"), Path("home")
        driver = long_session.LongSession(live.ROUTES["grok"], SimpleNamespace(
            binary=binary, fixture_seed=None))
        agent = Mock(session_id="session")
        def killed(prompt, timeout, on_event):
            marker = re.search(r"plak-long-kill-[0-9a-f]+", prompt)[0]
            on_event({"kind": "tool", "state": "running", "subject": marker})
            raise live.ScenarioFailure(live.HARNESS, "plak agent exited during a turn")
        agent.prompt.side_effect = killed
        with patch.object(driver, "repo", return_value=(repo, 0)), \
             patch.object(driver, "home", return_value=home), \
             patch.object(driver, "agent", side_effect=[agent, live.ScenarioFailure(
                 live.HARNESS, "plak agent did not become ready: {'kind': 'bye'}")]), \
             patch.object(driver, "_reap"), \
             patch.object(long_session, "marker_process_running", return_value=True), \
             patch.object(long_session, "close_agent"), \
             patch.object(crash_triage, "inspect", return_value="bucket=unknown") as inspect:
            with self.assertRaises(live.ScenarioFailure) as caught:
                driver.kill_reopen()
        self.assertEqual(caught.exception.outcome, live.HARNESS)
        self.assertIn("bucket=unknown", str(caught.exception))
        inspect.assert_called_once_with(binary, repo, home, "session")
