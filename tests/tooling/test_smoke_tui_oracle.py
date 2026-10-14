"""Offline checks for the live smoke TUI's durable turn oracle."""

import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import live  # noqa: E402
import smoke  # noqa: E402


class SmokeTuiOracleTests(unittest.TestCase):
    def test_terminals_ignore_admissions_and_unrelated_asymmetric_events(self):
        records = [
            {"kind": "turn.assistant", "text": "first answer"},
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "one"},
            {"kind": "tool.completed", "state": "succeeded"},
            {"kind": "turn.lifecycle", "phase": "completed", "turn_id": "one"},
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "two"},
            {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "two",
             "model_error_kind": "RateLimited", "http_status": 429},
            {"kind": "turn.assistant", "text": "late second answer"},
        ]
        self.assertEqual([record["turn_id"] for record in smoke.durable_terminals(records)],
                         ["one", "two"])

    def test_failed_provider_terminal_is_not_classified_as_model(self):
        terminal = {"phase": "failed", "model_error_kind": "RateLimited",
                    "http_status": 429}
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_durable_completed(terminal, "TUI turn")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_completed_terminal_passes(self):
        smoke.require_durable_completed({"phase": "completed"}, "TUI turn")

    def test_unavailable_and_rejected_before_acceptance_are_provider(self):
        for kind, status, text in (
                ("Unavailable", None, "server_is_overloaded"),
                (None, 429, ""),
                ("Protocol", None, "OpenRouter Chat HTTP 429 (rejected-before-acceptance)"),
        ):
            turn = live.Turn(phase="failed", model_error_kind=kind, http_status=status,
                             errors=[text])
            with self.subTest(kind=kind, status=status):
                with self.assertRaises(live.ScenarioFailure) as raised:
                    live.classify_incomplete(turn, "turn")
                self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_a_typed_model_refusal_is_provider_and_keeps_no_provider_prose(self):
        event = {"kind": "model_refusal", "message": "model refused the request",
                 "detail": "stop_reason=refusal;stop_details.type=refusal;"
                           "category=reasoning_extraction;explanation=private-sentinel prose"}
        summary = live.refusal_summary(event)
        self.assertEqual(summary, "stop_reason=refusal;stop_details.type=refusal;"
                                  "category=reasoning_extraction")
        self.assertNotIn("private-sentinel", summary)
        self.assertEqual(live.refusal_summary({"kind": "model_refusal"}), "model refusal")
        refused = live.Turn(phase="refused", refusals=[summary])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(refused, "growth turn")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        self.assertIn("category=reasoning_extraction", str(raised.exception))
        untyped = live.Turn(phase="refused", errors=["unknown command"])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(untyped, "turn")
        self.assertEqual(raised.exception.outcome, live.HARNESS)

    def test_a_durable_refused_terminal_is_provider(self):
        self.assertEqual(live.durable_refusals({"phase": "failed"}), [])
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_durable_completed({"phase": "refused"}, "TUI turn")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_stream_validation_needs_the_captured_protocol_violation(self):
        text = "OpenRouter Chat stream validation failed: usage must be one post-finish terminal-metadata chunk"
        turn = live.Turn(phase="failed", model_error_kind="Protocol", errors=[text])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(turn, "turn")
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0001.body").write_text(
                'data: {"choices":[{"index":0,"delta":{"content":"x"}}],"usage":{"prompt_tokens":1}}\n'
            )
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_incomplete(turn, "turn", segments=[segment])
            self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_injected_429_stays_harness_and_a_real_429_during_reset_is_provider(self):
        limited = live.Turn(phase="failed", model_error_kind="RateLimited", http_status=429)
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_completed(limited, "fault", injected=True, injected_fault="429")
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_completed(limited, "reset", injected=False)
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_real_429s_after_the_single_injected_429_are_provider(self):
        rate_limit = {"cause_kind": "http_status", "cause": "provider rate limited (HTTP 429)"}
        absorbed_once = live.Turn(phase="failed", model_error_kind="RateLimited", http_status=429,
                                  retries=[dict(rate_limit)])
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_completed(absorbed_once, "fault", injected=True, injected_fault="429")
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        exhausted = live.Turn(phase="failed", model_error_kind="RateLimited", http_status=429,
                              retries=[dict(rate_limit) for _ in range(3)])
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_completed(exhausted, "fault", injected=True, injected_fault="429")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_fault_reset_distinguishes_replay_from_a_declined_break_and_a_429(self):
        hello_dir = tempfile.TemporaryDirectory()
        self.addCleanup(hello_dir.cleanup)
        hello = Path(hello_dir.name) / "hello.txt"
        observed = live.Turn(
            phase="failed", errors=["stream interrupted"],
            retries=[{"cause_kind": "stream_interrupted", "output_discarded": False}],
        )
        self.assertIn("declined", smoke.judge_fault_reset(observed, hello, None))
        hello.write_text("hello\n")
        replayed = live.Turn(
            phase="completed",
            retries=[{"cause_kind": "stream_interrupted", "output_discarded": True}],
        )
        self.assertIn("discarded", smoke.judge_fault_reset(replayed, hello, None))
        limited = live.Turn(
            phase="failed", model_error_kind="RateLimited", http_status=429,
            retries=[{"cause_kind": "stream_interrupted", "output_discarded": False}],
        )
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.judge_fault_reset(limited, hello, None)
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        quiet = live.Turn(phase="completed")
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.judge_fault_reset(quiet, hello, None)
        self.assertEqual(raised.exception.outcome, live.HARNESS)

    def test_a_real_http_retry_that_consumed_the_faulted_request_is_provider(self):
        hello_dir = tempfile.TemporaryDirectory()
        self.addCleanup(hello_dir.cleanup)
        hello = Path(hello_dir.name) / "hello.txt"
        hello.write_text("hello\n")
        throttled = live.Turn(
            phase="completed",
            retries=[{"cause_kind": "http_status", "cause": "provider rate limited (HTTP 429)",
                      "output_discarded": False}],
        )
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.judge_fault_reset(throttled, hello, None)
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        self.assertIn("before the reset fired", str(raised.exception))

    def test_cli_exit_text_classifies_provider_refusals(self):
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, "HTTP 429 too many requests", "cli_task")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_command_error("server_is_overloaded", "manual compact")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)


if __name__ == "__main__":
    unittest.main()
