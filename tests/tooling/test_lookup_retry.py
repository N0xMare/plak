"""Startup lookup outcome classification; no binary or provider required."""

import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "live"))
import live


class LookupRetryTests(unittest.TestCase):
    def test_exhausted_lookup_provider_causes(self):
        for cause in ["HTTP 429", "HTTP 500", "HTTP 502", "HTTP 503", "HTTP 504",
                      "transport timeout", "DNS failure", "connect failure",
                      "transport stream failure",
                      "HTTP transport failed in Connect (NotStarted, Io)"]:
            for attempts in ([2, 3] if cause == "transport timeout" else [1, 2, 3] if cause == "HTTP 429" else [3]):
                text = f"plak: model lookup failed after {attempts} attempts: {cause}"
                self.assertIsNotNone(live.provider_reason_from(None, None, text), text)
                with self.assertRaises(live.ScenarioFailure) as caught:
                    live.classify_cli_failure(5, text, "startup")
                self.assertEqual(caught.exception.outcome, live.PROVIDER)
                with self.assertRaises(live.ScenarioFailure) as caught:
                    live.classify_command_error(text, "startup")
                self.assertEqual(caught.exception.outcome, live.PROVIDER)

    def test_injected_lookup_exhaustion_is_harness_until_real_attempts_follow(self):
        text = "model lookup failed after 3 attempts: HTTP 503"
        for faults, outcome in [
            ("host=openrouter.ai,path=/api/v1/models,nth=1,fault=503", live.PROVIDER),
            ("host=openrouter.ai,path=/api/v1/models,fault=503", live.HARNESS),
            ("path=/v1/models,nth=1,fault=503;path=/v1/models,nth=1,fault=503;path=/v1/models,nth=1,fault=503", live.HARNESS),
            ("path=/v1/responses,fault=503", live.PROVIDER),
            ("path=/v1/models,nth=10,fault=503", live.PROVIDER),
        ]:
            with self.assertRaises(live.ScenarioFailure) as caught:
                live.classify_cli_failure(5, text, "lookup", lookup_faults=faults)
            self.assertEqual(caught.exception.outcome, outcome)

    def test_lookup_prefix_faults_and_early_exhaustion_are_harness(self):
        for attempts, cause, faults in [
            (1, "HTTP 429", "host=api.openai.com,path=/v1,nth=1,fault=429,retry-after=120"),
            (3, "HTTP 503", "path=/api,fault=503"),
            (3, "HTTP 503", "path=/backend-api/codex,fault=503"),
            (2, "HTTP 503", None), (1, "connect failure", None),
            (2, "operation deadline", None),
        ]:
            with self.subTest(attempts=attempts, cause=cause, faults=faults):
                text = f"model lookup failed after {attempts} attempts: {cause}"
                with self.assertRaises(live.ScenarioFailure) as caught:
                    live.classify_cli_failure(5, text, "lookup", lookup_faults=faults)
                self.assertEqual(caught.exception.outcome, live.HARNESS)

    def test_fault_plan_reaches_turn_command_and_child_scoring(self):
        text = "model lookup failed after 3 attempts: HTTP 503"
        faults = "path=/v1,fault=503"
        self.assertIsNone(live.provider_reason_from(None, None, text, lookup_faults=faults))
        turn = live.Turn(phase="failed", errors=[text])
        for classify in [lambda: live.classify_incomplete(turn, "turn", lookup_faults=faults),
                         lambda: live.classify_command_error(text, "compact", lookup_faults=faults)]:
            with self.assertRaises(live.ScenarioFailure) as caught:
                classify()
            self.assertEqual(caught.exception.outcome, live.HARNESS)
        self.assertFalse(live.provider_child_text(text, lookup_faults=faults))
        self.assertTrue(live.provider_child_text(text))

    def test_durable_tui_turn_keeps_scenario_fault_plan(self):
        import jobs
        record = {"kind": "turn.lifecycle", "phase": "failed",
                  "message": "model lookup failed after 3 attempts: HTTP 503"}
        with patch.object(live, "session_records", return_value=[record]), \
                patch.object(jobs, "last_retry", return_value=None):
            with self.assertRaises(live.ScenarioFailure) as caught:
                jobs.classify_tui_turn(Path("unused"), "session", 0, "TUI",
                                      lookup_faults="path=/v1,fault=503")
            self.assertEqual(caught.exception.outcome, live.HARNESS)

    def test_one_timeout_attempt_is_not_provider_evidence(self):
        for cause in ["transport timeout", "HTTP transport failed in Resolution (NotStarted, DeadlineExceeded)"]:
            self.assertIsNone(live.lookup_failure_reason(f"model lookup failed after 1 attempts: {cause}"))

    def test_jsonl_startup_exhaustion_scores_provider_and_can_observe_the_cap(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = live.Env(root / "plak", root / "home", root)
            env.capture_segments.append(root / "capture")
            env.environ = Mock(return_value={})
            process = Mock()
            process.poll.return_value = 5

            def start(*args, **kwargs):
                kwargs["stderr"].write("model lookup failed after 2 attempts: transport timeout")
                kwargs["stderr"].flush()
                return process

            agent = live.Agent.__new__(live.Agent)
            with patch.object(live.subprocess, "Popen", side_effect=start), \
                    patch.object(live.threading, "Thread"), \
                    patch.object(live.Agent, "wait_for", return_value={"kind": "eof"}) as wait:
                try:
                    with self.assertRaises(live.ScenarioFailure) as caught:
                        agent.__init__(env, root, "agent", args=[])
                    self.assertEqual(caught.exception.outcome, live.PROVIDER)
                    self.assertGreater(wait.call_args.args[1], 120,
                        "driver must leave room to observe the lookup cap's terminal error")
                finally:
                    agent.stderr.close()
                    agent.transcript.close()

    def test_tui_startup_exhaustion_scores_provider_and_quits(self):
        tui = live.Tui.__new__(live.Tui)
        tui.screen = b"\x1b[31mmodel lookup failed after 3 attempts: HTTP 503\x1b[0m"
        tui.pump = Mock()
        tui.quit = Mock(return_value=0)
        with self.assertRaises(live.ScenarioFailure) as caught:
            tui.wait_until(lambda: False, 1)
        self.assertEqual(caught.exception.outcome, live.PROVIDER)
        tui.quit.assert_called_once()

    def test_lookup_validation_and_unrelated_errors_stay_harness(self):
        for cause in ["invalid catalog", "HTTP 401", "HTTP 403", "HTTP 404", "body bound"]:
            self.assertIsNone(live.provider_reason_from(None, None,
                f"model lookup failed after 3 attempts: {cause}"))
        for text in ["lookup transport failed", "model lookup invalid JSON", "model unavailable"]:
            self.assertIsNone(live.provider_reason_from(None, None, text))


if __name__ == "__main__":
    unittest.main()
