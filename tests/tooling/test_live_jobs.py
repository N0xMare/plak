"""Offline checks of the job driver's pure oracles: no binary, no provider."""

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import jobs  # noqa: E402
import live  # noqa: E402


def failure(test, call, outcome):
    with test.assertRaises(live.ScenarioFailure) as caught:
        call()
    test.assertEqual(caught.exception.outcome, outcome)


class JobSelectionTests(unittest.TestCase):
    def test_selected_is_the_whole_set_or_a_named_subset(self):
        self.assertEqual(jobs.selected(set()), jobs.SCENARIOS)
        self.assertEqual(jobs.selected({"promotion_budget"}), (("promotion_budget", "jsonl"),))

    def test_unknown_scenario_is_a_usage_error(self):
        with self.assertRaises(ValueError):
            jobs.selected({"nope"})

    def test_pin_jobs_appends_the_budget_read_at_session_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / "home"
            live.write_home(home, live.ROUTES["grok"], Path(directory))
            jobs.pin_jobs(home, 1)
            text = (home / "config.toml").read_text()
            self.assertIn("\n[jobs]\nforeground_seconds = 1\n", text)
            failure(self, lambda: jobs.pin_jobs(home, 45), live.HARNESS)
        failure(self, lambda: jobs.pin_jobs(Path("unused"), 0), live.HARNESS)
        failure(self, lambda: jobs.pin_jobs(Path("unused"), 601), live.HARNESS)

    def test_matching_requires_the_fixture_process(self):
        lines = [
            "python3 hold.py 8 plakjobbudget",
            "/usr/bin/python3 hold.py 8 plakjobbudget",
            "plakd agent",
            "python3 other.py plakjobbudget",
        ]
        self.assertEqual(jobs.matching(lines, "plakjobbudget"), lines[:2])

    def test_note_keeps_job_events_a_turn_waiter_does_not_return(self):
        agent = live.Agent.__new__(live.Agent)
        agent.session_id = None
        agent.children = set()
        agent.observed = []
        agent.note({"kind": "job_promoted", "job_id": "p1", "owner": "root"})
        self.assertEqual(agent.observed[0][1]["job_id"], "p1")

        bare = live.Agent.__new__(live.Agent)
        bare.session_id = None
        bare.children = set()
        bare.note({"kind": "hello", "session_id": "sess"})
        self.assertEqual(bare.session_id, "sess")
        self.assertFalse(hasattr(bare, "observed"))

    def test_job_identity_and_notice_order_come_from_the_event_log(self):
        events = [
            {"kind": "job_started", "job_id": "p2", "command": "python3 hold.py 8 plakjobbudget"},
            {"kind": "job_promoted", "job_id": "p2"},
            {"kind": "turn_lifecycle", "phase": "admitted"},
            {
                "kind": "job_terminal",
                "job_id": "p2",
                "status": "exited",
                "stop_reason": None,
                "owner": "root",
            },
            {"kind": "turn_lifecycle", "phase": "completed"},
        ]
        self.assertEqual(jobs.job_id_for(events, "plakjobbudget"), "p2")
        self.assertIsNone(jobs.job_id_for(events, "other"))
        self.assertTrue(jobs.exit_before_turn_terminal(events, "p2"))
        late = list(reversed(events))
        self.assertFalse(jobs.exit_before_turn_terminal(late, "p2"))
        self.assertEqual(jobs.terminal_for(events, "p2")["status"], "exited")
        self.assertIsNone(jobs.latency_until([], 1.0, lambda _event: True))
        self.assertEqual(
            jobs.latency_until(
                [(0.0, {}), (1.5, {"kind": "job_promoted"})],
                1.0,
                lambda event: event.get("kind") == "job_promoted",
            ),
            0.5,
        )

    def test_capture_has_reads_request_and_body_only(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0001.txt").write_text("Plak stopped unexpectedly")
            self.assertFalse(jobs.capture_has([segment], "Plak stopped unexpectedly"))
            (segment / "exchange-0001.request").write_text(
                "Plak stopped unexpectedly while this command was running."
            )
            self.assertTrue(jobs.capture_has([segment], jobs.WARNING))


class JobOracleTests(unittest.TestCase):
    def test_budget_requires_the_wait_not_only_the_configured_hint(self):
        self.assertIn(
            "1000",
            jobs.judge_budget(
                delta=1.2,
                finished=False,
                promoted=True,
                hint_source="default",
                hint_ms=1000,
            ),
        )
        self.assertIn(
            "not flushed",
            jobs.judge_budget(
                delta=1.2,
                finished=False,
                promoted=True,
                hint_source=None,
                hint_ms=None,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=0.1,
                finished=False,
                promoted=True,
                hint_source=None,
                hint_ms=None,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=0.1,
                finished=False,
                promoted=True,
                hint_source="default",
                hint_ms=1000,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=1.2,
                finished=False,
                promoted=True,
                hint_source="background",
                hint_ms=250,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=1.2,
                finished=True,
                promoted=True,
                hint_source=None,
                hint_ms=None,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=9.0,
                finished=False,
                promoted=True,
                hint_source=None,
                hint_ms=None,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=1.2,
                finished=False,
                promoted=True,
                hint_source="default",
                hint_ms=30_000,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_budget(
                delta=0.2,
                finished=False,
                promoted=True,
                hint_source="default",
                hint_ms=250,
            ),
            live.HARNESS,
        )

    def test_steer_and_await_leave_the_process_running(self):
        self.assertIn(
            "still running",
            jobs.judge_steer(
                latency=0.4,
                steered=True,
                finished=False,
                still_running=True,
                acked=True,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_steer(
                latency=None,
                steered=False,
                finished=False,
                still_running=False,
                acked=False,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_steer(
                latency=9.0,
                steered=True,
                finished=False,
                still_running=True,
                acked=True,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_steer(
                latency=0.4,
                steered=True,
                finished=False,
                still_running=False,
                acked=True,
            ),
            live.HARNESS,
        )
        self.assertIn(
            "await",
            jobs.judge_await(
                latency=0.4,
                steered=True,
                finished=False,
                still_running=True,
                acked=True,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_await(
                latency=0.4,
                steered=True,
                finished=True,
                still_running=False,
                acked=True,
            ),
            live.HARNESS,
        )

    def test_notice_run_child_and_crash(self):
        self.assertIn(
            "steering boundary",
            jobs.judge_notice(
                both_started=True,
                short_finished=True,
                during_turn=True,
                notice_seen=True,
                delivery="boundary",
                steered=True,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_notice(
                both_started=True,
                short_finished=True,
                during_turn=True,
                notice_seen=True,
                delivery="idle_wake",
                steered=True,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_notice(
                both_started=True,
                short_finished=True,
                during_turn=True,
                notice_seen=True,
                delivery="boundary",
                steered=False,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_notice(
                both_started=False,
                short_finished=False,
                during_turn=False,
                notice_seen=False,
                delivery=None,
                steered=False,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_run(
                code=5,
                stderr="plakd: agent turn failed: response reached its output bound "
                "before completion\n",
                finished=False,
            ),
            live.MODEL,
        )
        with self.assertRaises(live.ScenarioFailure) as caught:
            jobs.judge_run(
                code=3,
                stderr="plakd: the codex compatibility notice needs an answer "
                "on stdin and none came\n",
                finished=False,
            )
        self.assertEqual(caught.exception.outcome, live.HARNESS)
        self.assertIn("`plakd models codex`", str(caught.exception))
        self.assertIn(
            "continued",
            jobs.judge_run(
                code=0,
                stderr="plakd: waiting for p1 (hold); Ctrl-C to stop\n"
                "plakd: continuing (turn 2: p1 exited 0)\n",
                finished=True,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_run(
                code=1,
                stderr="authentication failed",
                finished=False,
            ),
            live.PROVIDER,
        )
        failure(
            self,
            lambda: jobs.judge_run(
                code=0,
                stderr="done\n",
                finished=True,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_run(
                code=0,
                stderr="plakd: waiting for p1\nplakd: stopped p1\n",
                finished=False,
            ),
            live.HARNESS,
        )
        self.assertIn(
            "refused",
            jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=True,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason="model",
            ),
        )
        self.assertIn(
            "refused",
            jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=True,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason=None,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="root",
                first_code="live_work",
                submitted=True,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason="model",
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code=None,
                submitted=False,
                settled=False,
                terminal=False,
                still_running=True,
                stop_reason=None,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=False,
                owner=None,
                first_code=None,
                submitted=False,
                settled=False,
                terminal=False,
                still_running=False,
                stop_reason=None,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="ok",
                submitted=True,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason=None,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=False,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason="model",
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=True,
                settled=True,
                terminal=False,
                still_running=True,
                stop_reason=None,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="submit_stale_token",
                submitted=False,
                settled=False,
                terminal=True,
                still_running=True,
                stop_reason=None,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=True,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason="model",
                detail="HTTP 429 rate limit",
            ),
            live.PROVIDER,
        )
        self.assertIn(
            "attempt end",
            jobs.judge_close(
                started=True,
                finished_early=False,
                stop_reason="attempt_ended",
                still_running=False,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_close(
                started=True,
                finished_early=False,
                stop_reason="model",
                still_running=False,
            ),
            live.MODEL,
        )
        # A command that finished before close: model only for a job the
        # worker never left promoted; the driver's own timing and a close that
        # did not stop the job are harness.
        failure(
            self,
            lambda: jobs.judge_close(
                started=True,
                finished_early=True,
                stop_reason=None,
                still_running=False,
                promoted=False,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_close(
                started=True,
                finished_early=True,
                stop_reason=None,
                still_running=False,
                sent=False,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_close(
                started=True,
                finished_early=True,
                stop_reason=None,
                still_running=False,
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_close(
                started=False,
                finished_early=False,
                stop_reason=None,
                still_running=False,
            ),
            live.MODEL,
        )
        failure(
            self,
            lambda: jobs.judge_close(
                started=True,
                finished_early=False,
                stop_reason=None,
                still_running=True,
            ),
            live.HARNESS,
        )
        receipt = json.dumps(
            {
                "agents": [
                    {
                        "agent_id": "c1",
                        "status": "failed",
                        "attempt": "blocked",
                        "blocked": {
                            "account": "openrouter",
                            "quota": "unavailable",
                            "resets_at": 1700000000,
                        },
                    }
                ],
            }
        )
        self.assertFalse(live.text_is_provider_refusal(receipt))
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=False,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason="attempt_ended",
                detail=receipt,
            ),
            live.PROVIDER,
        )
        failure(
            self,
            lambda: jobs.judge_child(
                started=True,
                owner="worker-1",
                first_code="live_work",
                submitted=False,
                settled=True,
                terminal=True,
                still_running=False,
                stop_reason="attempt_ended",
                detail="child c1 status failed: provider unavailable",
            ),
            live.PROVIDER,
        )
        self.assertIn("outcome unknown", jobs.judge_crash(unknown=True, warned=True))
        failure(self, lambda: jobs.judge_crash(unknown=False, warned=True), live.HARNESS)
        failure(self, lambda: jobs.judge_crash(unknown=True, warned=False), live.HARNESS)


class JobEvidenceTests(unittest.TestCase):
    def test_code_mode_uses_the_hint_and_the_outer_cell_boundary(self):
        self.assertTrue(jobs.code_mode(live.ROUTES["codex"]))
        self.assertTrue(jobs.code_mode(live.ROUTES["api-openai"]))
        self.assertFalse(jobs.code_mode(live.ROUTES["grok"]))
        self.assertIn("yield_time_ms 0", jobs.immediate_hint(live.ROUTES["codex"]))
        self.assertIn(
            "run_in_background true", jobs.immediate_hint(live.ROUTES["claude-sonnet55-low"])
        )
        self.assertIn("timeout 0", jobs.immediate_hint(live.ROUTES["deepseek"]))
        self.assertIn("block_until_ms 0", jobs.immediate_hint(live.ROUTES["grok"]))
        self.assertIn(
            "steer applied with the job still running",
            jobs.judge_nested_steer(
                promoted=True,
                steered=True,
                acked=True,
                finished=False,
                still_running=True,
            ),
        )
        failure(
            self,
            lambda: jobs.judge_nested_steer(
                promoted=True,
                steered=True,
                acked=False,
                finished=False,
                still_running=True,
            ),
            live.HARNESS,
        )

    def test_crash_waits_for_a_terminal_exchange(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0001.info").write_text("terminal=pending\nincomplete=true\n")
            self.assertFalse(jobs.exchange_settled(segment))
            (segment / "exchange-0002.info").write_text("terminal=Complete\nincomplete=false\n")
            self.assertTrue(jobs.exchange_settled(segment))

    def test_tokens_differ_per_call_and_only_match_that_fixture(self):
        first, second = jobs.fresh_token("plakjobbudget"), jobs.fresh_token("plakjobbudget")
        self.assertNotEqual(first, second)
        self.assertTrue(
            first.startswith("plakjobbudget") and len(first) == len("plakjobbudget") + 8
        )
        lines = [f"python3 hold.py 8 {first}", f"python3 hold.py 8 {second}", "plakd agent"]
        self.assertEqual(jobs.matching(lines, first), [lines[0]])
        framework = f"/Library/Frameworks/Python.framework/Versions/3.13/Resources/Python.app/Contents/MacOS/Python hold.py 8 {first}"
        self.assertEqual(jobs.matching([framework, f"echo hold.py {first}"], first), [framework])

    def test_notice_id_is_not_satisfied_by_the_prompt_token(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0001.request").write_text(
                "Run python3 short.py plakjobshortabcd and do not stop it.\n"
            )
            self.assertFalse(jobs.notice_for([segment], "p7"))
            (segment / "exchange-0001.body").write_text(
                '<process_notice id="p7" status="exited">\ncommand: python3 short.py plakjobshortabcd\n'
            )
            self.assertTrue(jobs.notice_for([segment], "p7"))
            self.assertFalse(jobs.notice_for([segment], "p8"))

    def test_notice_on_steer_parses_escaped_request_json(self):
        notice = '<process_notice id="p1" status="exited">\ncommand: python3 short.py tok\n'
        request = {
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "[harness]\n" + notice},
                    ],
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Reply with the single word steered."},
                    ],
                },
            ],
        }
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            path = segment / "exchange-0003.request"
            path.write_text(json.dumps(request))
            raw = path.read_text()
            self.assertNotIn('<process_notice id="p1"', raw)
            self.assertIn('\\"p1\\"', raw)
            self.assertTrue(jobs.notice_for([segment], "p1"))
            self.assertTrue(jobs.notice_on_steer([segment], "p1"))
            self.assertFalse(jobs.notice_on_steer([segment], "p2"))
            # The steer in a different exchange is not this boundary.
            steered = segment / "exchange-0004.request"
            steered.write_text(
                json.dumps(
                    {
                        "input": [
                            {
                                "type": "message",
                                "content": [{"text": "Reply with the single word steered."}],
                            }
                        ],
                    }
                )
            )
            only_notice = {
                "input": [{"type": "message", "content": [{"text": "[harness]\n" + notice}]}],
            }
            path.write_text(json.dumps(only_notice))
            self.assertFalse(jobs.notice_on_steer([segment], "p1"))

    def test_wake_prompt_is_the_products(self):
        source = (ROOT / "crates/hosting/hosted/src/runtime/host_loop/wake.rs").read_text()
        self.assertIn(f'const WAKE_PROMPT: &str = "{jobs.WAKE_PROMPT}";', source)

    def test_notice_passes_at_the_steer_or_at_a_notice_yield_of_the_same_turn(self):
        """Both orderings R1 allows: steer first (one commit) or notice first."""
        notice = '<process_notice id="p1" status="exited">\ncommand: python3 short.py tok\n'
        steer = "Reply with the single word steered."

        def request(*texts):
            return json.dumps({"messages": [{"role": "user", "content": text} for text in texts]})

        judged = dict(
            both_started=True,
            short_finished=True,
            during_turn=True,
            delivery="boundary",
            steered=True,
        )
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            # Steer first: the notice and the steer share the steer's request.
            shared = segment / "exchange-0002.request"
            shared.write_text(request("Run both commands.", notice, steer))
            terminal = shared.stat().st_mtime + 5
            self.assertTrue(jobs.notice_on_steer([segment], "p1"))
            self.assertIn(
                "steering boundary",
                jobs.judge_notice(
                    **judged,
                    notice_seen=True,
                    notice_in_turn=jobs.notice_in_turn([segment], "p1", terminal),
                ),
            )
            # Notice first: the wait yielded and the notice committed alone; a
            # later steer request that replays only its own suffix lacks it.
            shared.write_text(request("Run both commands.", notice))
            (segment / "exchange-0003.request").write_text(request(steer))
            self.assertFalse(jobs.notice_on_steer([segment], "p1"))
            self.assertTrue(jobs.notice_in_turn([segment], "p1", terminal))
            self.assertIn(
                "Notice yield",
                jobs.judge_notice(
                    **judged,
                    notice_seen=False,
                    notice_in_turn=jobs.notice_in_turn([segment], "p1", terminal),
                ),
            )
            # A notice yield needs no steer at all.
            self.assertIn(
                "Notice yield",
                jobs.judge_notice(
                    **{**judged, "steered": False}, notice_seen=False, notice_in_turn=True
                ),
            )
            # A request persisted after the turn's terminal is a later turn.
            self.assertFalse(jobs.notice_in_turn([segment], "p1", shared.stat().st_mtime - 1))
            self.assertFalse(jobs.notice_in_turn([segment], "p1", None))
            self.assertFalse(jobs.notice_in_turn([segment], "p2", terminal))
            # A fast idle wake's request, persisted before the terminal was
            # observed, is still the wake turn's, not this turn's.
            shared.write_text(request("Run both commands.", notice, jobs.WAKE_PROMPT))
            self.assertFalse(jobs.notice_in_turn([segment], "p1", terminal))
            # An earlier idle wake in the replayed history does not disqualify a
            # later in-turn delivery: only the newest user input decides.
            shared.write_text(request(jobs.WAKE_PROMPT, "Run both commands.", notice))
            self.assertTrue(jobs.notice_in_turn([segment], "p1", terminal))
            responses = {
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": jobs.WAKE_PROMPT}],
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": notice}],
                    },
                    {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                ]
            }
            shared.write_text(json.dumps(responses))
            self.assertTrue(jobs.notice_in_turn([segment], "p1", terminal))
            responses["input"].append(
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": jobs.WAKE_PROMPT}],
                }
            )
            shared.write_text(json.dumps(responses))
            self.assertFalse(jobs.notice_in_turn([segment], "p1", terminal))
            # A response body naming the notice is not a request of the turn.
            shared.write_text(request("Run both commands."))
            (segment / "exchange-0002.body").write_text(notice)
            self.assertFalse(jobs.notice_in_turn([segment], "p1", terminal))
        failure(
            self,
            lambda: jobs.judge_notice(**judged, notice_seen=False, notice_in_turn=False),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_notice(
                **{**judged, "delivery": "idle_wake"}, notice_seen=False, notice_in_turn=True
            ),
            live.HARNESS,
        )
        failure(
            self,
            lambda: jobs.judge_notice(
                **{**judged, "during_turn": False}, notice_seen=False, notice_in_turn=True
            ),
            live.MODEL,
        )

    def test_submit_outcomes_read_live_work_then_a_receipt(self):
        refusal = json.dumps(
            {
                "code": "live_work",
                "message": (
                    "live work: processes p1 (finish or stop them with your process tools); "
                    "finish or stop it before submitting"
                ),
            }
        )
        receipt = json.dumps({"agent_id": "c1", "attempt": "submitted"})
        grok = {
            "input": [
                {
                    "type": "function_call",
                    "name": "run_terminal_command",
                    "call_id": "shell",
                    "arguments": "{}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "shell",
                    "output": "{\"task_id\":\"p1\"}",
                },
                {
                    "type": "function_call",
                    "name": "submit_result",
                    "call_id": "s1",
                    "arguments": "{\"turn_token\":1,\"output_json\":\"{}\"}",
                },
                {"type": "function_call_output", "call_id": "s1", "output": refusal},
                {
                    "type": "function_call",
                    "name": "submit_result",
                    "call_id": "s2",
                    "arguments": "{\"turn_token\":1,\"output_json\":\"{}\"}",
                },
                {"type": "function_call_output", "call_id": "s2", "output": receipt},
            ],
        }
        claude = {
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "submit_result",
                            "input": {"turn_token": 1, "output_json": "{}"},
                        },
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": [{"type": "text", "text": refusal}],
                        },
                    ],
                },
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_2",
                            "name": "submit_result",
                            "input": {"turn_token": 1, "output_json": "{}"},
                        },
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_2", "content": receipt},
                    ],
                },
            ],
        }
        chat = {
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "submit_result", "arguments": "{}"},
                        }
                    ],
                },
                {"role": "tool", "tool_call_id": "call_1", "content": refusal},
            ],
        }
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0005.request").write_text(
                json.dumps(
                    {
                        "input": grok["input"][:4],
                    }
                )
            )
            (segment / "exchange-0006.request").write_text(json.dumps(grok))
            self.assertEqual(jobs.submit_outcomes([segment]), ["live_work", "ok"])
            wrapped = segment / "exchange-0007.request"
            wrapped.write_text(json.dumps(claude))
            # The longer history wins, including a different wire shape alone.
            alone = Path(directory) / "other"
            alone.mkdir()
            (alone / "exchange-0001.request").write_text(json.dumps(claude))
            self.assertEqual(jobs.submit_outcomes([alone]), ["live_work", "ok"])
            (alone / "exchange-0001.request").write_text(json.dumps(chat))
            self.assertEqual(jobs.submit_outcomes([alone]), ["live_work"])
            # A code cell returns the refusal as text, not as a submit_result pair.
            (alone / "exchange-0001.request").write_text(
                json.dumps(
                    {
                        "input": [
                            {
                                "type": "function_call_output",
                                "call_id": "cell",
                                "output": "cell failed: " + refusal,
                            }
                        ],
                    }
                )
            )
            self.assertEqual(jobs.submit_outcomes([alone]), ["live_work"])
        self.assertEqual(jobs.outcome_code(refusal), "live_work")
        self.assertEqual(jobs.outcome_code(json.dumps({"result": receipt})), "ok")
        events = [
            {
                "kind": "tree",
                "nodes": [
                    {"agent_id": "root", "attempt": "running"},
                    {"agent_id": "c1", "attempt": "running"},
                ],
            },
            {
                "kind": "tree",
                "nodes": [
                    {"agent_id": "root"},
                    {"agent_id": "c1", "attempt": "completed"},
                ],
            },
        ]
        self.assertEqual(jobs.spawned_children(events, "root"), ["c1"])
        self.assertEqual(jobs.child_attempt(events, "c1"), "completed")
        self.assertIn("submitted", jobs.SUBMITTED_ATTEMPTS)
        self.assertIn("completed", jobs.SETTLED_ATTEMPTS)
        self.assertNotIn("submitted", jobs.SETTLED_ATTEMPTS)
        task = jobs.settle_child_task(live.ROUTES["grok"], "python3 hold.py 30 tok")
        self.assertIn("kill_command_or_subagent", task)
        self.assertIn("submit_result once", task)
        self.assertNotIn("live_work", task)
        self.assertIn("TaskStop", jobs.stop_hint(live.ROUTES["claude-sonnet55-low"]))
        self.assertIn("write_stdin", jobs.stop_hint(live.ROUTES["codex"]))
        closed = jobs.close_child_task(live.ROUTES["grok"], "python3 hold.py 40 tok")
        self.assertIn("Do not submit_result", closed)
        # Neither half asks a root to disobey its wait-for-children agreement.
        for prompt in (task, closed):
            self.assertIn("you may wait for it with wait_agent", prompt)
            self.assertNotIn("without waiting for it", prompt)

    def test_child_wait_does_not_judge_before_start_or_terminal(self):
        token = "plakjobchildabcd"
        started = [
            {"kind": "job_started", "job_id": "p3", "command": f"python3 hold.py 30 {token}"}
        ]
        self.assertEqual(jobs.child_wait_state([], token, child_settled=False), "wait_started")
        self.assertEqual(
            jobs.child_wait_state([], token, child_settled=True), "settled_without_job"
        )
        self.assertEqual(jobs.child_wait_state(started, token, child_settled=True), "wait_terminal")
        done = [
            *started,
            {
                "kind": "job_terminal",
                "job_id": "p3",
                "status": "stopped",
                "stop_reason": "attempt_ended",
                "owner": "worker-1",
            },
        ]
        self.assertEqual(jobs.child_wait_state(done, token, child_settled=True), "ready")
        # The second wait keeps the id. Evicting job_started must not hide the terminal.
        job_id = "p3"
        terminal = {
            "kind": "job_terminal",
            "job_id": job_id,
            "status": "stopped",
            "stop_reason": "attempt_ended",
            "owner": "c1",
        }
        evicted = [terminal]
        self.assertEqual(
            jobs.child_wait_state(evicted, token, child_settled=True), "settled_without_job"
        )
        self.assertEqual(jobs.terminal_for(evicted, job_id), terminal)

    def test_observed_log_is_opt_in_and_drops_streamed_text(self):
        import inspect
        from collections import deque

        parameter = inspect.signature(live.Agent.__init__).parameters["observe"]
        self.assertIs(parameter.default, False)
        agent = live.Agent.__new__(live.Agent)
        agent.session_id = None
        agent.children = set()
        agent.observed = deque(maxlen=2)
        agent.note({"kind": "job_started", "job_id": "p1", "command": "hold"})
        agent.note({"kind": "assistant", "text": "x" * 10000})
        agent.note({"kind": "reasoning", "text": "y" * 10000})
        agent.note({"kind": "child_live", "op": {"kind": "assistant", "text": "z" * 10000}})
        agent.note({"kind": "child_live", "op": {"kind": "reasoning", "text": "w"}})
        agent.note({"kind": "child_live", "text": "bare child delta"})
        agent.note({"kind": "job_promoted", "job_id": "p1"})
        agent.note({"kind": "tool", "state": "running"})
        self.assertEqual([event["kind"] for _, event in agent.observed], ["job_promoted", "tool"])
        bare = live.Agent.__new__(live.Agent)
        bare.session_id = None
        bare.children = set()
        bare.note({"kind": "tool", "state": "running"})
        self.assertFalse(hasattr(bare, "observed"))


if __name__ == "__main__":
    unittest.main()


class ChildSettleCloseReplayTests(unittest.TestCase):
    """The close half of `child_settle`, replayed from the recorded root shapes."""

    CLOSE = "plakjobclose"

    def play(self, during_turn, after_turn):
        """`during_turn` events arrive inside the root turn, `after_turn` after it.

        The job stops at attempt end once close_node is sent and runs to its end
        otherwise; `finish` in a list marks the hold ending on its own there.
        """
        import collections
        from unittest.mock import Mock, patch

        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            driver = jobs.Jobs(live.ROUTES["codex-sol"], Mock(fixture_seed=1, capture_segments=[]))
            driver.prepare = Mock(return_value=(repo, repo / "home"))
            agent = Mock(session_id="root", observed=collections.deque())
            sent = []
            state = {"stopped": False, "finished": False}

            def send(kind, **fields):
                sent.append((kind, fields))
                if kind == "close_node" and not state["finished"]:
                    state["stopped"] = True
                    agent.observed.append(
                        (
                            0.0,
                            {
                                "kind": "job_terminal",
                                "job_id": "p1",
                                "owner": "o2",
                                "status": "stopped",
                                "stop_reason": "attempt_ended",
                                "output": "",
                            },
                        )
                    )
                return "1"

            agent.send.side_effect = send
            driver.open_agent = Mock(return_value=agent)
            pending_after = list(after_turn)
            calls = {"n": 0}

            def feed(event):
                if event == "finish":
                    if not state["stopped"]:
                        state["finished"] = True
                        (repo / "job-finished").write_text("1\n")
                        agent.observed.append(
                            (
                                0.0,
                                {
                                    "kind": "job_terminal",
                                    "job_id": "p1",
                                    "owner": "o2",
                                    "status": "exited",
                                    "output": "",
                                },
                            )
                        )
                    return
                agent.observed.append((0.0, event))

            def run_turn(_agent, _text, _timeout, tick, _accept_eof):
                calls["n"] += 1
                if calls["n"] == 2:
                    for event in during_turn:
                        feed(event)
                        if tick:
                            tick(None, event)
                return live.Turn(phase="completed")

            def drain(_agent, _seconds):
                if pending_after:
                    feed(pending_after.pop(0))

            def matching(_lines, token):
                running = (
                    not state["stopped"]
                    and not state["finished"]
                    and any(
                        event.get("kind") == "job_started" and token in event.get("command", "")
                        for _, event in agent.observed
                    )
                )
                return ["hold"] if running else []

            with (
                patch.object(jobs, "run_turn", run_turn),
                patch.object(jobs, "drain", drain),
                patch.object(jobs, "matching", side_effect=matching),
                patch.object(jobs, "command_lines", return_value=[]),
                patch.object(jobs, "classify_turn"),
                patch.object(jobs, "require_script"),
                patch.object(jobs, "judge_child", return_value="lived"),
                patch.object(jobs, "submit_outcomes", return_value=["live_work", "ok"]),
                patch.object(jobs, "child_has_settled", return_value=True),
                patch.object(jobs, "close_agent"),
                patch.object(jobs, "stop_token"),
                patch.object(jobs, "CHILD_WAIT_S", 0.5),
                patch.object(driver, "fresh_token", side_effect=["plakjobchild", self.CLOSE]),
            ):
                try:
                    return driver.child_settle(), sent
                except live.ScenarioFailure as caught:
                    return caught, sent

    TREE = {
        "kind": "tree",
        "nodes": [
            {"agent_id": "root", "depth": 0},
            {
                "agent_id": "c2",
                "depth": 1,
                "prompt": "run exactly: python3 hold.py 40 plakjobclose",
            },
        ],
    }
    TWO = {
        "kind": "tree",
        "nodes": [
            {"agent_id": "root", "depth": 0},
            {
                "agent_id": "c2",
                "depth": 1,
                "prompt": "run exactly: python3 hold.py 40 plakjobclose",
            },
            {
                "agent_id": "c3",
                "depth": 1,
                "prompt": "run exactly: python3 hold.py 40 plakjobclose",
            },
        ],
    }
    STARTED = {
        "kind": "job_started",
        "job_id": "p1",
        "owner": "o2",
        "command": "python3 hold.py 40 plakjobclose",
    }
    PROMOTED = {"kind": "job_promoted", "job_id": "p1", "owner": "o2"}
    WAIT = {"kind": "tool", "name": "wait_agent", "state": "running", "call_id": "w"}

    def test_a_root_parked_in_wait_agent_has_its_worker_closed_during_the_turn(self):
        # The recorded gpt-6.1-sol shape: the root waits on the worker past the
        # 40 s hold; the driver closes it while that wait runs.
        result, sent = self.play([self.TREE, self.STARTED, self.PROMOTED, self.WAIT, "finish"], [])
        self.assertEqual(result, "lived; close stopped the child job at attempt end")
        self.assertEqual(sent, [("close_node", {"agent_id": "c2"})])

    def test_a_root_that_says_done_first_has_its_worker_closed_after_the_turn(self):
        # The recorded gpt-6-luna shape: the root replies done right after the
        # spawn; the job events arrive after its turn ends.
        result, sent = self.play([self.TREE], [self.STARTED, self.PROMOTED])
        self.assertEqual(result, "lived; close stopped the child job at attempt end")
        self.assertEqual(sent, [("close_node", {"agent_id": "c2"})])

    def test_a_job_that_finished_before_the_driver_could_close_is_not_the_model(self):
        # The job ends before the driver ever sees the worker in a tree.
        result, sent = self.play([self.STARTED, self.PROMOTED, "finish"], [self.TREE])
        self.assertEqual(sent, [])
        self.assertEqual(
            (result.outcome, str(result)),
            (live.HARNESS, "the driver did not close the child while its job ran"),
        )

    def test_a_start_that_was_never_promoted_is_not_closed_and_is_the_model(self):
        # A foreground start: close would cancel it, so the driver waits for
        # promotion that never comes; the job then finishes on its own.
        result, sent = self.play([self.TREE, self.STARTED, "finish"], [])
        self.assertEqual(sent, [])
        self.assertEqual(
            (result.outcome, str(result)),
            (
                live.MODEL,
                "the child ran the command in the foreground instead of leaving a "
                "promoted job for close",
            ),
        )

    def test_the_close_goes_to_the_worker_the_tree_maps_to_the_job(self):
        result, sent = self.play([self.STARTED, self.TREE, self.PROMOTED, "finish"], [])
        self.assertEqual(sent, [("close_node", {"agent_id": "c2"})])
        self.assertEqual(result, "lived; close stopped the child job at attempt end")
        # Job owners are runtime ids, not tree agent ids: the tree maps them.
        self.assertEqual(
            jobs.close_target([self.TREE, self.STARTED], "root", set(), self.CLOSE), ("c2", None)
        )

    def test_more_than_one_new_worker_is_the_model_and_nobody_is_closed(self):
        result, sent = self.play([self.TWO, self.STARTED, self.PROMOTED, "finish"], [])
        self.assertEqual(sent, [])
        self.assertEqual(
            (result.outcome, str(result)), (live.MODEL, "the root spawned more than one worker")
        )
        other = {"kind": "tree", "nodes": [{"agent_id": "c2", "depth": 1, "prompt": "run ls"}]}
        self.assertEqual(
            jobs.close_target([other], "root", set(), self.CLOSE),
            (None, "the spawned worker's prompt does not carry the command"),
        )


class NestedSteerAwaitTests(unittest.TestCase):
    """The Code Mode await steer is judged at the moment it is applied."""

    def replay(self, events, *, finished_after: int | None = None):
        """Feed timed `plakd agent` events through the scenario; the job ends after event N.

        Each event is `(seconds, event)` on the driver's monotonic clock.
        """
        driver = jobs.Jobs(live.ROUTES["codex"], Mock(fixture_seed=1))
        repo = Path("repo")
        driver.prepare = Mock(return_value=(repo, Path("home")))
        sent = []
        agent = Mock(observed=[], capture_dir=None)
        agent.send.side_effect = lambda kind, **fields: sent.append(fields) or "s"
        driver.open_agent = Mock(return_value=agent)
        seen = {"n": 0, "t": 0.0}

        def finished(_repo, name):
            return (
                name == "job-finished" and finished_after is not None and seen["n"] > finished_after
            )

        def run_turn(_agent, text, _timeout, tick, _accept_eof):
            self.prompt = text
            for index, (at, event) in enumerate(events):
                seen["n"], seen["t"] = index, at
                agent.observed.append((at, event))
                tick(None, event)
            return live.Turn(phase="completed")

        with (
            patch.object(jobs, "run_turn", run_turn),
            patch.object(jobs, "marker", side_effect=finished),
            patch.object(
                jobs,
                "matching",
                side_effect=lambda lines, token: [] if finished(repo, "job-finished") else ["hold"],
            ),
            patch.object(jobs, "command_lines", return_value=[]),
            patch.object(jobs, "require_script"),
            patch.object(jobs, "close_agent"),
            patch.object(jobs, "stop_token"),
            patch.object(jobs, "job_id_for", return_value="p1"),
            patch.object(jobs.time, "monotonic", side_effect=lambda: seen["t"]),
        ):
            try:
                return driver.steer_await(), sent
            except live.ScenarioFailure as failure:
                return failure, sent

    CELL = {"kind": "tool", "name": "exec", "call_id": "cell", "state": "running"}
    START = {
        "kind": "tool",
        "name": "process_session_start",
        "call_id": "start",
        "parent_call_id": "cell",
        "state": "running",
    }
    STARTED = {**START, "state": "done"}
    PROMOTED = {"kind": "job_promoted", "job_id": "p1"}
    WRITE = {
        "kind": "tool",
        "name": "process_session_write",
        "call_id": "write",
        "parent_call_id": "cell",
        "state": "running",
    }
    YIELDED = {**WRITE, "state": "done"}
    APPLIED = {"kind": "steer_ack", "state": "applied"}
    QUEUED = {"kind": "steer_ack", "state": "queued", "position": 1}
    CELL_WAIT = {
        "kind": "tool",
        "name": "wait",
        "call_id": "wait",
        "parent_call_id": "cell",
        "state": "running",
    }
    CELL_WAITED = {**CELL_WAIT, "state": "done"}
    CELL_DONE = {**CELL, "state": "done"}

    def timed(self, *events, step=0.1):
        return [(index * step, event) for index, event in enumerate(events)]

    def test_the_prompt_asks_for_one_nested_wait_and_to_follow_a_new_instruction(self):
        self.replay([])
        self.assertIn("one write_stdin call", self.prompt)
        self.assertIn(
            "If a new instruction arrives, follow it immediately without waiting on the cell.",
            self.prompt,
        )
        self.assertNotIn("must not yield", self.prompt)

    def test_a_steer_applied_while_the_job_runs_passes_even_if_the_job_ends_later(self):
        result, sent = self.replay(
            self.timed(
                self.CELL,
                self.START,
                self.STARTED,
                self.PROMOTED,
                self.WRITE,
                self.QUEUED,
                self.YIELDED,
                self.APPLIED,
                self.CELL_DONE,
            ),
            finished_after=7,
        )
        self.assertEqual(
            result,
            "nested wait yielded on the steer; applied at the outer-cell "
            "boundary in 0.3 s; job still running",
        )
        self.assertEqual(sent, [{"text": jobs.STEER_TEXT, "steer": True}])

    def test_the_recorded_shape_waits_on_the_cell_after_the_steer(self):
        result, _sent = self.replay(
            self.timed(
                self.CELL,
                self.START,
                self.STARTED,
                self.PROMOTED,
                self.WRITE,
                self.YIELDED,
                self.APPLIED,
                self.CELL_WAIT,
                self.CELL_DONE,
            ),
            finished_after=7,
        )
        self.assertEqual(
            (result.outcome, str(result)),
            (
                live.MODEL,
                "model waited on the cell after the steer was applied instead of "
                "following the steer",
            ),
        )

    def test_only_a_cell_wait_first_seen_after_the_applied_steer_counts(self):
        # A wait started before the steer and finished after it is not the miss.
        result, _sent = self.replay(
            self.timed(
                self.CELL,
                self.START,
                self.STARTED,
                self.PROMOTED,
                self.CELL_WAIT,
                self.WRITE,
                self.YIELDED,
                self.APPLIED,
                self.CELL_WAITED,
                self.CELL_DONE,
            ),
            finished_after=8,
        )
        self.assertIsInstance(result, str)

    def test_a_start_alone_is_not_a_nested_wait(self):
        result, sent = self.replay(
            self.timed(self.CELL, self.START, self.PROMOTED, self.STARTED, self.CELL_DONE)
        )
        self.assertEqual(sent, [])
        self.assertEqual(
            (result.outcome, str(result)),
            (live.MODEL, "no nested wait was in flight inside the cell while the job ran"),
        )

    def test_a_cell_that_already_completed_is_not_an_outer_cell_steer(self):
        result, sent = self.replay(
            self.timed(self.CELL, self.START, self.STARTED, self.PROMOTED, self.CELL_DONE)
        )
        self.assertEqual(sent, [])
        self.assertEqual(result.outcome, live.MODEL)

    def test_latency_and_the_job_state_at_the_applied_steer_decide(self):
        events = [
            (0.0, self.CELL),
            (0.1, self.START),
            (0.2, self.STARTED),
            (0.3, self.PROMOTED),
            (0.4, self.WRITE),
            (0.5, self.YIELDED),
        ]
        result, _ = self.replay([*events, (9.5, self.APPLIED)])
        self.assertEqual(
            (result.outcome, str(result)),
            (live.HARNESS, "steer was applied 9.1 s after it was sent, past 8 s"),
        )
        # The nested wait yielded while the job ran; the job then ended on its
        # own before the outer cell applied the steer: a late nested wait.
        result, _ = self.replay([*events, (2.0, self.APPLIED)], finished_after=5)
        self.assertEqual(
            (result.outcome, str(result)),
            (
                live.MODEL,
                "job finished within 1.6 s of the steer, after the nested wait yielded: "
                "the nested wait began too late",
            ),
        )
        # The nested wait returned only once the job had finished: never yielded.
        result, _ = self.replay([*events, (2.0, self.APPLIED)], finished_after=4)
        self.assertEqual(
            (result.outcome, str(result)),
            (
                live.HARNESS,
                "the nested wait returned only when the job finished: it was not "
                "yielded on the steer",
            ),
        )
        result, _ = self.replay([*events, (0.6, self.QUEUED)])
        self.assertEqual(
            (result.outcome, str(result)),
            (live.HARNESS, "steer was not applied at the outer-cell boundary"),
        )
        # A steer sent with under the 8 s bar of the 25 s hold left is late,
        # whatever the product did with the nested wait.
        with patch.object(jobs, "started_wall", side_effect=lambda _repo: jobs.time.time() - 20.0):
            result, _ = self.replay([*events, (2.0, self.APPLIED)], finished_after=4)
        self.assertEqual(result.outcome, live.MODEL)
        self.assertRegex(
            str(result),
            r"^the steer went out with [45]\.\d s of the hold left, "
            r"under the 8 s bar",
        )

    def test_the_promotion_prompt_names_the_only_code_mode_argument(self):
        codex = jobs.promotion_prompt(live.ROUTES["codex"], "python3 hold.py 8 t")
        self.assertIn(
            "Call exec_command with only the cmd argument: do not pass "
            "yield_time_ms or any other argument.",
            codex,
        )
        grok = jobs.promotion_prompt(live.ROUTES["grok"], "python3 hold.py 8 t")
        self.assertIn("Do not pass a timeout, yield, block_until, or background flag.", grok)
        self.assertNotIn("exec_command", grok)


class SteerCommandSnapshotTests(unittest.TestCase):
    """The Code Mode command steer is judged when the steer is applied."""

    PROMOTED = {"kind": "job_promoted", "job_id": "p1"}
    APPLIED = {"kind": "steer_ack", "state": "applied"}
    DONE = {"kind": "turn_lifecycle", "phase": "completed"}

    def replay(self, events, finished_after=None):
        driver = jobs.Jobs(live.ROUTES["codex"], Mock(fixture_seed=1))
        driver.prepare = Mock(return_value=(Path("repo"), Path("home")))
        agent = Mock(observed=[], capture_dir=None)
        driver.open_agent = Mock(return_value=agent)
        seen = {"n": 0}

        def finished():
            return finished_after is not None and seen["n"] > finished_after

        def run_turn(_agent, _text, _timeout, tick, _accept_eof):
            for index, event in enumerate(events):
                seen["n"] = index
                agent.observed.append((0.0, event))
                tick(None, event)
            seen["n"] = len(events)
            return live.Turn(phase="completed")

        with (
            patch.object(jobs, "run_turn", run_turn),
            patch.object(
                jobs,
                "marker",
                side_effect=lambda _repo, name: name == "job-finished" and finished(),
            ),
            patch.object(jobs, "matching", side_effect=lambda *_: [] if finished() else ["x"]),
            patch.object(jobs, "command_lines", return_value=[]),
            patch.object(jobs, "require_script"),
            patch.object(jobs, "close_agent"),
            patch.object(jobs, "stop_token"),
            patch.object(jobs, "job_id_for", return_value="p1"),
        ):
            try:
                return driver.steer_command()
            except live.ScenarioFailure as failure:
                return failure

    def test_a_job_that_ends_after_the_applied_steer_still_passes(self):
        self.assertEqual(
            self.replay([self.PROMOTED, self.APPLIED, self.DONE], finished_after=1),
            "hint promoted the nested command; steer applied with the job still running",
        )

    def test_the_state_at_the_applied_steer_decides(self):
        failure = self.replay(
            [self.PROMOTED, {"kind": "status"}, self.APPLIED, self.DONE], finished_after=1
        )
        self.assertEqual(
            (failure.outcome, str(failure)),
            (live.MODEL, "command finished before the steer was applied"),
        )
        failure = self.replay([self.PROMOTED, self.DONE])
        self.assertEqual(
            (failure.outcome, str(failure)),
            (live.HARNESS, "steer was not applied at the outer-cell boundary"),
        )
        # A steer only queued, never applied, is the product's: steer_command
        # now requires `applied`, not any `steer_ack`.
        failure = self.replay([self.PROMOTED, {"kind": "steer_ack", "state": "queued"}, self.DONE])
        self.assertEqual(
            (failure.outcome, str(failure)),
            (live.HARNESS, "steer was not applied at the outer-cell boundary"),
        )


class JobsTurnTimeoutTests(unittest.TestCase):
    def test_a_jobs_turn_without_a_terminal_uses_the_budget_accounting(self):
        agent = live.Agent.__new__(live.Agent)
        agent.events = __import__("queue").Queue()
        agent.env, agent.capture_dir, agent.lookup_faults = None, None, None
        with (
            patch.object(agent, "send", return_value="1"),
            patch.object(live, "timeout_failure", wraps=live.timeout_failure) as accounted,
        ):
            with self.assertRaises(live.ScenarioFailure) as raised:
                jobs.run_turn(agent, "hi", 0.05, None, lambda: False)
        self.assertEqual(
            (raised.exception.outcome, str(raised.exception)),
            (live.HARNESS, "no turn terminal within 0s"),
        )
        self.assertEqual(accounted.call_args.kwargs["timeout"], 0.05)
