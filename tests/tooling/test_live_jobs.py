"""Offline checks of the job driver's pure oracles: no binary, no provider."""

import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

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
        self.assertEqual(jobs.selected({"ctrl_b", "stop_jobs"}),
                         (("ctrl_b", "tui"), ("stop_jobs", "tui")))

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

    def test_visible_drops_ansi_and_matching_requires_the_fixture_process(self):
        self.assertEqual(jobs.visible(b"\x1b[32m1 process\x1b[0m"), "1 process")
        lines = [
            "python3 hold.py 8 plakjobbudget",
            "/usr/bin/python3 hold.py 8 plakjobbudget",
            "plak agent",
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
            {"kind": "job_terminal", "job_id": "p2", "status": "exited", "stop_reason": None,
             "owner": "root"},
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
            jobs.latency_until([(0.0, {}), (1.5, {"kind": "job_promoted"})], 1.0,
                               lambda event: event.get("kind") == "job_promoted"),
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
        self.assertIn("1000", jobs.judge_budget(
            delta=1.2, finished=False, promoted=True, hint_source="default", hint_ms=1000,
        ))
        self.assertIn("not flushed", jobs.judge_budget(
            delta=1.2, finished=False, promoted=True, hint_source=None, hint_ms=None,
        ))
        failure(self, lambda: jobs.judge_budget(
            delta=0.1, finished=False, promoted=True, hint_source=None, hint_ms=None,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_budget(
            delta=0.1, finished=False, promoted=True, hint_source="default", hint_ms=1000,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_budget(
            delta=1.2, finished=False, promoted=True, hint_source="background", hint_ms=250,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_budget(
            delta=1.2, finished=True, promoted=True, hint_source=None, hint_ms=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_budget(
            delta=9.0, finished=False, promoted=True, hint_source=None, hint_ms=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_budget(
            delta=1.2, finished=False, promoted=True, hint_source="default", hint_ms=30_000,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_budget(
            delta=0.2, finished=False, promoted=True, hint_source="default", hint_ms=250,
        ), live.HARNESS)

    def test_steer_and_background_leave_the_process_running(self):
        self.assertIn("still running", jobs.judge_steer(
            latency=0.4, steered=True, finished=False, still_running=True, acked=True,
        ))
        failure(self, lambda: jobs.judge_steer(
            latency=None, steered=False, finished=False, still_running=False, acked=False,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_steer(
            latency=9.0, steered=True, finished=False, still_running=True, acked=True,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_steer(
            latency=0.4, steered=True, finished=False, still_running=False, acked=True,
        ), live.HARNESS)
        self.assertIn("await", jobs.judge_await(
            latency=0.4, steered=True, finished=False, still_running=True, acked=True,
        ))
        failure(self, lambda: jobs.judge_await(
            latency=0.4, steered=True, finished=True, still_running=False, acked=True,
        ), live.HARNESS)
        self.assertIn("Ctrl+B", jobs.judge_ctrl_b(
            latency=0.4, finished=False, still_running=True, returned=True,
        ))
        failure(self, lambda: jobs.judge_ctrl_b(
            latency=0.4, finished=True, still_running=False, returned=True,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_ctrl_b(
            latency=None, finished=False, still_running=True, returned=False,
        ), live.HARNESS)

    def test_stop_notice_run_child_crash_wake_and_badge(self):
        self.assertIn("woke", jobs.judge_stop(
            confirmed=True, still_running=False, finished=False, woke=True, origin="host_wake",
        ))
        self.assertIn("woke", jobs.judge_stop(
            confirmed=True, still_running=False, finished=False, woke=True, origin=None,
        ))
        failure(self, lambda: jobs.judge_stop(
            confirmed=False, still_running=True, finished=False, woke=False, origin=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_stop(
            confirmed=True, still_running=False, finished=True, woke=True, origin="host_wake",
        ), live.MODEL)
        failure(self, lambda: jobs.judge_stop(
            confirmed=True, still_running=False, finished=False, woke=False, origin=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_stop(
            confirmed=True, still_running=False, finished=False, woke=True, origin="user_prompt",
        ), live.HARNESS)
        self.assertIn("steering boundary", jobs.judge_notice(
            both_started=True, short_finished=True, during_turn=True,
            notice_seen=True, delivery="boundary", steered=True,
        ))
        failure(self, lambda: jobs.judge_notice(
            both_started=True, short_finished=True, during_turn=True,
            notice_seen=True, delivery="idle_wake", steered=True,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_notice(
            both_started=True, short_finished=True, during_turn=True,
            notice_seen=True, delivery="boundary", steered=False,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_notice(
            both_started=False, short_finished=False, during_turn=False,
            notice_seen=False, delivery=None, steered=False,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_run(
            code=5, stderr="plak: agent turn failed: response reached its output bound "
            "before completion\n", finished=False,
        ), live.MODEL)
        with self.assertRaises(live.ScenarioFailure) as caught:
            jobs.judge_run(code=3, stderr="plak: the codex compatibility notice needs an answer "
                           "on stdin and none came\n", finished=False)
        self.assertEqual(caught.exception.outcome, live.HARNESS)
        self.assertIn("`plak models codex`", str(caught.exception))
        self.assertIn("continued", jobs.judge_run(
            code=0, stderr="plak: waiting for p1 (hold); Ctrl-C to stop\n"
            "plak: continuing (turn 2: p1 exited 0)\n", finished=True,
        ))
        failure(self, lambda: jobs.judge_run(
            code=1, stderr="authentication failed", finished=False,
        ), live.PROVIDER)
        failure(self, lambda: jobs.judge_run(
            code=0, stderr="done\n", finished=True,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_run(
            code=0, stderr="plak: waiting for p1\nplak: stopped p1\n", finished=False,
        ), live.HARNESS)
        self.assertIn("refused", jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=True, settled=True, terminal=True, still_running=False,
            stop_reason="model",
        ))
        self.assertIn("refused", jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=True, settled=True, terminal=True, still_running=False,
            stop_reason=None,
        ))
        failure(self, lambda: jobs.judge_child(
            started=True, owner="root", first_code="live_work",
            submitted=True, settled=True, terminal=True, still_running=False,
            stop_reason="model",
        ), live.MODEL)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code=None,
            submitted=False, settled=False, terminal=False, still_running=True,
            stop_reason=None,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_child(
            started=False, owner=None, first_code=None,
            submitted=False, settled=False, terminal=False, still_running=False,
            stop_reason=None,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="ok",
            submitted=True, settled=True, terminal=True, still_running=False,
            stop_reason=None,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=False, settled=True, terminal=True, still_running=False,
            stop_reason="model",
        ), live.MODEL)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=True, settled=True, terminal=False, still_running=True,
            stop_reason=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="submit_stale_token",
            submitted=False, settled=False, terminal=True, still_running=True,
            stop_reason=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=True, settled=True, terminal=True, still_running=False,
            stop_reason="model", detail="HTTP 429 rate limit",
        ), live.PROVIDER)
        self.assertIn("attempt end", jobs.judge_close(
            started=True, finished_early=False, stop_reason="attempt_ended",
            still_running=False,
        ))
        failure(self, lambda: jobs.judge_close(
            started=True, finished_early=False, stop_reason="model", still_running=False,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_close(
            started=True, finished_early=True, stop_reason=None, still_running=False,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_close(
            started=False, finished_early=False, stop_reason=None, still_running=False,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_close(
            started=True, finished_early=False, stop_reason=None, still_running=True,
        ), live.HARNESS)
        receipt = json.dumps({
            "agents": [{
                "agent_id": "c1",
                "status": "failed",
                "attempt": "blocked",
                "blocked": {
                    "account": "openrouter",
                    "quota": "unavailable",
                    "resets_at": 1700000000,
                },
            }],
        })
        self.assertFalse(live.text_is_provider_refusal(receipt))
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=False, settled=True, terminal=True, still_running=False,
            stop_reason="attempt_ended",
            detail=receipt,
        ), live.PROVIDER)
        failure(self, lambda: jobs.judge_child(
            started=True, owner="worker-1", first_code="live_work",
            submitted=False, settled=True, terminal=True, still_running=False,
            stop_reason="attempt_ended",
            detail="child c1 status failed: provider unavailable",
        ), live.PROVIDER)
        self.assertIn("outcome unknown", jobs.judge_crash(unknown=True, warned=True))
        failure(self, lambda: jobs.judge_crash(unknown=False, warned=True), live.HARNESS)
        failure(self, lambda: jobs.judge_crash(unknown=True, warned=False), live.HARNESS)
        self.assertIn("woke", jobs.judge_wake(
            idle_with_job=True, hud=True, second_admission=True,
            origin="host_wake", finished=True,
        ))
        self.assertIn("woke", jobs.judge_wake(
            idle_with_job=True, hud=True, second_admission=True, origin=None, finished=True,
        ))
        failure(self, lambda: jobs.judge_wake(
            idle_with_job=True, hud=True, second_admission=True,
            origin="user_prompt", finished=True,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_wake(
            idle_with_job=False, hud=True, second_admission=False, origin=None, finished=False,
        ), live.MODEL)
        self.assertIn("1 agent", jobs.judge_badge(
            root_idle=True, child_alive=True, agent=True, process=True, promoted=True,
        ))
        failure(self, lambda: jobs.judge_badge(
            root_idle=True, child_alive=True, agent=False, process=True, promoted=True,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_badge(
            root_idle=True, child_alive=True, agent=False, process=False, promoted=None,
        ), live.HARNESS)
        failure(self, lambda: jobs.judge_badge(
            root_idle=True, child_alive=True, agent=False, process=False, promoted=False,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_badge(
            root_idle=True, child_alive=False, agent=True, process=True, promoted=True,
        ), live.MODEL)
        failure(self, lambda: jobs.judge_badge(
            root_idle=False, child_alive=True, agent=True, process=True, promoted=True,
        ), live.MODEL)


class JobEvidenceTests(unittest.TestCase):
    def test_code_mode_uses_the_hint_and_the_outer_cell_boundary(self):
        self.assertTrue(jobs.code_mode(live.ROUTES["codex"]))
        self.assertTrue(jobs.code_mode(live.ROUTES["api-openai"]))
        self.assertFalse(jobs.code_mode(live.ROUTES["grok"]))
        self.assertIn("yield_time_ms 0", jobs.immediate_hint(live.ROUTES["codex"]))
        self.assertIn("run_in_background true", jobs.immediate_hint(live.ROUTES["claude"]))
        self.assertIn("timeout 0", jobs.immediate_hint(live.ROUTES["deepseek"]))
        self.assertIn("block_until_ms 0", jobs.immediate_hint(live.ROUTES["grok"]))
        command = "python3 hold.py 25 token"
        for route, hint, wait in (
            ("codex", "yield_time_ms 0", "write_stdin"),
            ("claude", "run_in_background true", "Read"),
            ("deepseek", "timeout 0", "timeout_ms"),
            ("grok", "block_until_ms 0", "get_command_or_subagent_output"),
        ):
            task = jobs.idle_child_task(live.ROUTES[route], command)
            self.assertIn(hint, task)
            self.assertIn(wait, task)
            self.assertIn(command, task)
            self.assertNotIn("no timeout", task)
            self.assertIn("submit_result", task)
        self.assertIn("outer-cell", jobs.judge_nested_steer(
            promoted=True, steered=True, acked=True, finished=False, still_running=True,
        ))
        failure(self, lambda: jobs.judge_nested_steer(
            promoted=True, steered=True, acked=False, finished=False, still_running=True,
        ), live.HARNESS)
        self.assertIn("Ctrl+B", jobs.judge_nested_ctrl_b(
            promoted=True, sent=True, finished=False, still_running=True,
        ))

    def test_crash_waits_for_a_terminal_exchange(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0001.info").write_text(
                "terminal=pending\nincomplete=true\n")
            self.assertFalse(jobs.exchange_settled(segment))
            (segment / "exchange-0002.info").write_text(
                "terminal=Complete\nincomplete=false\n")
            self.assertTrue(jobs.exchange_settled(segment))

    def test_tokens_differ_per_call_and_only_match_that_fixture(self):
        first, second = jobs.fresh_token("plakjobbudget"), jobs.fresh_token("plakjobbudget")
        self.assertNotEqual(first, second)
        self.assertTrue(first.startswith("plakjobbudget") and len(first) == len("plakjobbudget") + 8)
        lines = [f"python3 hold.py 8 {first}", f"python3 hold.py 8 {second}", "plak agent"]
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
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "[harness]\n" + notice},
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Reply with the single word steered."},
                ]},
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
            steered.write_text(json.dumps({
                "input": [{"type": "message", "content": [{"text": "Reply with the single word steered."}]}],
            }))
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

        judged = dict(both_started=True, short_finished=True, during_turn=True,
                      delivery="boundary", steered=True)
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            # Steer first: the notice and the steer share the steer's request.
            shared = segment / "exchange-0002.request"
            shared.write_text(request("Run both commands.", notice, steer))
            terminal = shared.stat().st_mtime + 5
            self.assertTrue(jobs.notice_on_steer([segment], "p1"))
            self.assertIn("steering boundary", jobs.judge_notice(
                **judged, notice_seen=True,
                notice_in_turn=jobs.notice_in_turn([segment], "p1", terminal)))
            # Notice first: the wait yielded and the notice committed alone; a
            # later steer request that replays only its own suffix lacks it.
            shared.write_text(request("Run both commands.", notice))
            (segment / "exchange-0003.request").write_text(request(steer))
            self.assertFalse(jobs.notice_on_steer([segment], "p1"))
            self.assertTrue(jobs.notice_in_turn([segment], "p1", terminal))
            self.assertIn("Notice yield", jobs.judge_notice(
                **judged, notice_seen=False,
                notice_in_turn=jobs.notice_in_turn([segment], "p1", terminal)))
            # A notice yield needs no steer at all.
            self.assertIn("Notice yield", jobs.judge_notice(
                **{**judged, "steered": False}, notice_seen=False, notice_in_turn=True))
            # A request persisted after the turn's terminal is a later turn.
            self.assertFalse(jobs.notice_in_turn([segment], "p1", shared.stat().st_mtime - 1))
            self.assertFalse(jobs.notice_in_turn([segment], "p1", None))
            self.assertFalse(jobs.notice_in_turn([segment], "p2", terminal))
            # A fast idle wake's request, persisted before the terminal was
            # observed, is still the wake turn's, not this turn's.
            shared.write_text(request("Run both commands.", notice, jobs.WAKE_PROMPT))
            self.assertFalse(jobs.notice_in_turn([segment], "p1", terminal))
            # A response body naming the notice is not a request of the turn.
            shared.write_text(request("Run both commands."))
            (segment / "exchange-0002.body").write_text(notice)
            self.assertFalse(jobs.notice_in_turn([segment], "p1", terminal))
        failure(self, lambda: jobs.judge_notice(
            **judged, notice_seen=False, notice_in_turn=False), live.HARNESS)
        failure(self, lambda: jobs.judge_notice(
            **{**judged, "delivery": "idle_wake"}, notice_seen=False, notice_in_turn=True),
            live.HARNESS)
        failure(self, lambda: jobs.judge_notice(
            **{**judged, "during_turn": False}, notice_seen=False, notice_in_turn=True),
            live.MODEL)

    def test_tui_turn_failed_with_output_limit_is_model(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            log = home / "state" / "k" / "sessions" / "s.events.jsonl"
            log.parent.mkdir(parents=True)
            rows = [
                {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "t1"},
                {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "t1"},
                {"kind": "turn.failed", "disposition": "definite",
                 "msg": "agent turn failed: response reached its output bound before completion"},
            ]
            log.write_text("".join(json.dumps(row) + "\n" for row in rows))
            failure(self, lambda: jobs.classify_tui_turn(home, "s", 0, "ctrl+b"), live.MODEL)
            rows[2]["msg"] = "agent turn failed: protocol"
            log.write_text("".join(json.dumps(row) + "\n" for row in rows))
            failure(self, lambda: jobs.classify_tui_turn(home, "s", 0, "ctrl+b"), live.HARNESS)

    def test_submit_outcomes_read_live_work_then_a_receipt(self):
        refusal = json.dumps({
            "code": "live_work",
            "message": (
                "live work: processes p1 (finish or stop them with your process tools); "
                "finish or stop it before submitting"
            ),
        })
        receipt = json.dumps({"agent_id": "c1", "attempt": "submitted"})
        grok = {
            "input": [
                {"type": "function_call", "name": "run_terminal_command", "call_id": "shell",
                 "arguments": "{}"},
                {"type": "function_call_output", "call_id": "shell", "output": "{\"task_id\":\"p1\"}"},
                {"type": "function_call", "name": "submit_result", "call_id": "s1",
                 "arguments": "{\"turn_token\":1,\"output_json\":\"{}\"}"},
                {"type": "function_call_output", "call_id": "s1", "output": refusal},
                {"type": "function_call", "name": "submit_result", "call_id": "s2",
                 "arguments": "{\"turn_token\":1,\"output_json\":\"{}\"}"},
                {"type": "function_call_output", "call_id": "s2", "output": receipt},
            ],
        }
        claude = {
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "submit_result",
                     "input": {"turn_token": 1, "output_json": "{}"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1",
                     "content": [{"type": "text", "text": refusal}]},
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_2", "name": "submit_result",
                     "input": {"turn_token": 1, "output_json": "{}"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": receipt},
                ]},
            ],
        }
        chat = {
            "messages": [
                {"role": "assistant", "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "submit_result", "arguments": "{}"},
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": refusal},
            ],
        }
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0005.request").write_text(json.dumps({
                "input": grok["input"][:4],
            }))
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
            (alone / "exchange-0001.request").write_text(json.dumps({
                "input": [{
                    "type": "function_call_output", "call_id": "cell",
                    "output": "cell failed: " + refusal,
                }],
            }))
            self.assertEqual(jobs.submit_outcomes([alone]), ["live_work"])
        self.assertEqual(jobs.outcome_code(refusal), "live_work")
        self.assertEqual(jobs.outcome_code(json.dumps({"result": receipt})), "ok")
        events = [
            {"kind": "tree", "nodes": [
                {"agent_id": "root", "attempt": "running"},
                {"agent_id": "c1", "attempt": "running"},
            ]},
            {"kind": "tree", "nodes": [
                {"agent_id": "root"},
                {"agent_id": "c1", "attempt": "completed"},
            ]},
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
        self.assertIn("TaskStop", jobs.stop_hint(live.ROUTES["claude"]))
        self.assertIn("write_stdin", jobs.stop_hint(live.ROUTES["codex"]))
        closed = jobs.close_child_task(live.ROUTES["grok"], "python3 hold.py 40 tok")
        self.assertIn("Do not submit_result", closed)

    def test_cursor_split_hud_is_one_composer_row(self):
        agent = " · \u280b 1 agent".encode()
        process = " · \u2834 1 process".encode()
        raw = b"\x1b[1;1H" + agent + b"\x1b[1;40H" + process
        hud = jobs.latest_hud(raw, 0)
        self.assertIn("1 agent", hud)
        self.assertIn("1 process", hud)
        # A later row that drops the process is the state to judge.
        later = jobs.latest_hud(raw + b"\n" + " · \u2819 1 agent".encode(), len(raw))
        self.assertIn("1 agent", later)
        self.assertNotIn("1 process", later)

    def test_child_wait_does_not_judge_before_start_or_terminal(self):
        token = "plakjobchildabcd"
        started = [{"kind": "job_started", "job_id": "p3", "command": f"python3 hold.py 30 {token}"}]
        self.assertEqual(jobs.child_wait_state([], token, child_settled=False), "wait_started")
        self.assertEqual(jobs.child_wait_state([], token, child_settled=True), "settled_without_job")
        self.assertEqual(jobs.child_wait_state(started, token, child_settled=True), "wait_terminal")
        done = [*started, {"kind": "job_terminal", "job_id": "p3", "status": "stopped",
                           "stop_reason": "attempt_ended", "owner": "worker-1"}]
        self.assertEqual(jobs.child_wait_state(done, token, child_settled=True), "ready")
        # The second wait keeps the id. Evicting job_started must not hide the terminal.
        job_id = "p3"
        terminal = {"kind": "job_terminal", "job_id": job_id, "status": "stopped",
                    "stop_reason": "attempt_ended", "owner": "c1"}
        evicted = [terminal]
        self.assertEqual(jobs.child_wait_state(evicted, token, child_settled=True), "settled_without_job")
        self.assertEqual(jobs.terminal_for(evicted, job_id), terminal)

    def test_hud_bytes_and_idle_come_from_after_the_terminal(self):
        screen = b"the model said 1 agent and 1 process\n" + b"\x1b[32m1 process\x1b[0m"
        mark = len(b"the model said 1 agent and 1 process\n")
        self.assertNotIn("1 agent", jobs.screen_after(screen, mark))
        self.assertIn("1 process", jobs.screen_after(screen, mark))

    def test_latest_hud_keeps_a_paint_from_before_the_terminal(self):
        quote = b"the model said 1 agent and 1 process\n"
        paint = " \u00b7 \u280b 1 agent \u00b7 \u2834 1 process ".encode()
        screen = quote + paint + b"MARK" + b"root idle, no repaint\n"
        mark = len(quote) + len(paint) + len(b"MARK")
        hud = jobs.latest_hud(screen, mark)
        self.assertIn("1 agent", hud)
        self.assertIn("1 process", hud)
        self.assertNotIn("the model said", hud)
        # A quote after the mark is not a repaint.
        quoted = quote + paint + b"MARK" + b"still says 1 agent and 1 process"
        self.assertIn("1 process", jobs.latest_hud(quoted, mark))
        # A later paint that dropped the process is the state to judge.
        later = screen + " \u00b7 \u2819 1 agent ".encode()
        dropped = jobs.latest_hud(later, mark)
        self.assertIn("1 agent", dropped)
        self.assertNotIn("1 process", dropped)
        # wake_while_idle: a process-only border, including ANSI around it.
        process = " \u00b7 \u2834 1 process ".encode()
        ansi = b"before\x1b[32m" + process + b"\x1b[0m"
        self.assertIn("1 process", jobs.latest_hud(ansi, len(b"before")))
        quoted_process = b"quote 1 process\n" + process
        self.assertNotIn("1 agent", jobs.latest_hud(quoted_process, len(quoted_process)))

    def test_unpromoted_child_command_is_model_and_not_a_spawn_quote(self):
        token = "plakjobbadgeabcd"
        command = f"python3 hold.py 25 {token}"
        open_start = [{
            "kind": "tool.started", "name": "run_terminal_command", "call_id": "c1",
            "arguments": command + " block_until_ms 60000",
        }]
        self.assertIs(jobs.command_promoted(open_start, token), False)
        done = [*open_start, {
            "kind": "tool.completed", "name": "run_terminal_command", "call_id": "c1",
        }]
        self.assertIs(jobs.command_promoted(done, token), True)
        self.assertIs(jobs.command_promoted([], token), None)
        spawn = [{
            "kind": "tool.started", "name": "spawn_agent", "exposed_name": "spawn_agent",
            "call_id": "s", "arguments": command,
        }]
        self.assertIs(jobs.command_promoted(spawn, token), None)
        # Canonical name on the row, model-facing name beside it.
        bash = [{
            "kind": "tool.started", "name": "process_session_start",
            "exposed_name": "Bash", "call_id": "b", "subject": command,
        }]
        self.assertIs(jobs.command_promoted(bash, token), False)
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            root = home / "state" / "k" / "sessions" / "root.events.jsonl"
            child = home / "state" / "k" / "sessions" / "child.events.jsonl"
            root.parent.mkdir(parents=True)
            root.write_text(json.dumps({
                "kind": "turn.lifecycle", "phase": "completed", "turn_id": "t",
            }) + "\n")
            child.write_text(json.dumps(bash[0]) + "\n")
            self.assertIs(jobs.command_promoted(jobs.all_session_records(home), token), False)
        idle = [
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "t1"},
            {"kind": "turn.lifecycle", "phase": "completed", "turn_id": "t1"},
        ]
        self.assertTrue(jobs.root_is_idle(idle))
        self.assertFalse(jobs.root_is_idle(idle + [
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "t2"},
        ]))
        self.assertFalse(jobs.root_is_idle([]))
        self.assertFalse(jobs.root_is_idle([
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "t1"},
            {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "t1"},
        ]))

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
        self.assertEqual([event["kind"] for _, event in agent.observed],
                         ["job_promoted", "tool"])
        bare = live.Agent.__new__(live.Agent)
        bare.session_id = None
        bare.children = set()
        bare.note({"kind": "tool", "state": "running"})
        self.assertFalse(hasattr(bare, "observed"))

    def test_tui_failure_uses_retry_telemetry_when_the_log_has_no_status(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            log = home / "state" / "k" / "sessions" / "sess.events.jsonl"
            log.parent.mkdir(parents=True)
            log.write_text(json.dumps({
                "kind": "turn.lifecycle", "turn_id": "t1", "phase": "failed",
            }) + "\n")
            failure(self, lambda: jobs.classify_tui_turn(home, "sess", 0, "stop"), live.HARNESS)
            database = home / "telemetry" / "telemetry-v14.sqlite3"
            database.parent.mkdir()
            connection = sqlite3.connect(database)
            connection.execute(
                "CREATE TABLE model_retries (turn_id TEXT, cause_kind TEXT, "
                "next_attempt INTEGER, max_attempts INTEGER, declined_reason TEXT)"
            )
            connection.execute(
                "INSERT INTO model_retries VALUES ('t1', 'connect', 1, 4, NULL)"
            )
            connection.execute(
                "INSERT INTO model_retries VALUES ('t1', 'http_status', 2, 4, NULL)"
            )
            connection.commit()
            failure(self, lambda: jobs.classify_tui_turn(home, "sess", 0, "stop"), live.HARNESS)
            connection.execute(
                "INSERT INTO model_retries VALUES ('t1', 'http_status', 3, 4, 'deadline')"
            )
            connection.commit()
            failure(self, lambda: jobs.classify_tui_turn(home, "sess", 0, "stop"), live.HARNESS)
            connection.execute(
                "INSERT INTO model_retries VALUES ('t1', 'http_status', 4, 4, NULL)"
            )
            connection.commit()
            failure(self, lambda: jobs.classify_tui_turn(home, "sess", 0, "stop"), live.PROVIDER)
            connection.execute(
                "INSERT INTO model_retries VALUES ('t1', 'http_status', 5, 4, 'budget')"
            )
            connection.commit()
            connection.close()
            failure(self, lambda: jobs.classify_tui_turn(home, "sess", 0, "stop"), live.PROVIDER)
            log.write_text(json.dumps({
                "kind": "turn.lifecycle", "turn_id": "t2", "phase": "failed",
                "model_error_kind": "RateLimited", "http_status": 429,
            }) + "\n")
            failure(self, lambda: jobs.classify_tui_turn(home, "sess", 0, "stop"), live.PROVIDER)


if __name__ == "__main__":
    unittest.main()
