"""Offline regressions for the qualification drivers; no provider or binary."""

import io
import itertools
import json
import os
import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import jobs  # noqa: E402
import live  # noqa: E402
import long_session  # noqa: E402
import qualification  # noqa: E402
import smoke  # noqa: E402


class QualificationFixTests(unittest.TestCase):
    def test_fault_reset_does_not_pass_a_capture_proven_model_argument_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, self.chat_arguments("{"))
            turn = live.Turn(
                phase="failed", model_error_kind="Protocol",
                errors=["OpenRouter Chat stream validation failed: "
                        "tool arguments are malformed or ambiguous", "stream reset observed"],
            )
            with self.assertRaises(live.ScenarioFailure) as raised:
                smoke.judge_fault_reset(turn, segment / "hello.txt", [segment])
            self.assertEqual(raised.exception.outcome, live.MODEL)

    def test_smoke_compaction_accepts_context_limit_then_capacity_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            driver, agent = self.compaction_driver(Path(directory))
            driver.route = live.ROUTES["or-qwen38"]
            agent.prompt.side_effect = [
                live.Turn(phase="failed", model_error_kind="ContextLimit"),
                live.Turn(phase="completed"),
                live.Turn(phase="completed", text="ZEBRA-42"),
            ]
            rows = [
                {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "first"},
                {"kind": "compact.started", "reason": "capacity"},
                {"kind": "compact.started", "reason": "manual"},
                {"kind": "compact.finished", "outcome": "committed"},
            ]
            with patch.object(long_session, "poll_records", return_value=rows):
                self.assertEqual(driver.compaction(), "capacity recovery compaction, context carried")
            self.assertEqual(agent.prompt.call_count, 3)
            agent.command.assert_not_called()

    def test_smoke_capacity_remedy_rejects_large_windows_and_reordered_logs(self):
        failed = {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "first"}
        started = {"kind": "compact.started", "reason": "capacity"}
        committed = {"kind": "compact.finished", "outcome": "committed"}
        with tempfile.TemporaryDirectory() as directory:
            for route, rows in (
                    ("codex", [failed, started, committed]),
                    ("grok", [failed, started, committed]),
                    ("or-qwen38", [started, failed, committed]),
                    ("or-qwen38", [started, committed]),
                    ("deepseek", [dict(failed, phase="completed"), started, committed])):
                with self.subTest(route=route, rows=rows):
                    driver, agent = self.compaction_driver(Path(directory))
                    driver.route = live.ROUTES[route]
                    agent.prompt.side_effect = [
                        live.Turn(phase="failed", model_error_kind="ContextLimit"),
                        live.Turn(phase="completed"),
                        live.Turn(phase="completed", text="ZEBRA-42"),
                    ]
                    with patch.object(long_session, "poll_records", return_value=rows):
                        with self.assertRaises(live.ScenarioFailure) as raised:
                            driver.compaction()
                    self.assertEqual(raised.exception.outcome, live.HARNESS)
                    self.assertEqual(agent.prompt.call_count, 1)

    def test_smoke_small_known_window_accepts_ordered_capacity_recovery(self):
        with tempfile.TemporaryDirectory() as directory:
            driver, agent = self.compaction_driver(Path(directory))
            driver.route = live.ROUTES["deepseek"]
            agent.prompt.side_effect = [
                live.Turn(phase="failed", model_error_kind="ContextLimit"),
                live.Turn(phase="completed"),
                live.Turn(phase="completed", text="ZEBRA-42"),
            ]
            rows = [
                {"kind": "turn.lifecycle", "phase": "admitted"},
                {"kind": "turn.lifecycle", "phase": "failed"},
                {"kind": "compact.started", "reason": "capacity"},
                {"kind": "compact.finished", "outcome": "committed"},
            ]
            with patch.object(long_session, "poll_records", return_value=rows):
                self.assertEqual(driver.compaction(), "capacity recovery compaction, context carried")

    def test_smoke_context_limit_without_capacity_commit_stays_harness(self):
        with tempfile.TemporaryDirectory() as directory:
            for rows in ([], [{"kind": "compact.finished", "outcome": "committed"}],
                         [{"kind": "compact.started", "reason": "capacity"},
                          {"kind": "compact.finished", "outcome": "failed"}]):
                driver, agent = self.compaction_driver(Path(directory))
                agent.prompt.return_value = live.Turn(phase="failed", model_error_kind="ContextLimit")
                with patch.object(long_session, "poll_records", return_value=rows):
                    with self.assertRaises(live.ScenarioFailure) as raised:
                        driver.compaction()
                self.assertEqual(raised.exception.outcome, live.HARNESS)
                self.assertEqual(agent.prompt.call_count, 1)

    def write_capture(self, segment, events, exchange=1):
        body = "".join(f"data: {json.dumps(event)}\n\n" for event in events).encode()
        name = f"exchange-{exchange:04}"
        (segment / f"{name}.request").write_bytes(b"synthetic request")
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={exchange}\nstatus=Some(200)\nconnection=Some(Fresh)\n"
            f"terminal=Complete\nrecorded_bytes={len(body)}\nincomplete=false\n")

    def chat_arguments(self, arguments):
        return [
            {"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": arguments}}]}}]},
            {"choices": [{"index": 0, "finish_reason": "tool_calls"}]},
        ]

    def messages_arguments(self, arguments):
        return [
            {"type": "content_block_start", "index": 0,
             "content_block": {"type": "tool_use", "input": {}}},
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "input_json_delta", "partial_json": arguments}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            {"type": "message_stop"},
        ]

    def test_messages_duplicate_keys_are_valid_but_non_json_constants_are_not(self):
        message = "Messages stream validation failed: terminal Messages tool input was not complete valid JSON"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for arguments, outcome in (('{"x":1,"x":2}', live.HARNESS),
                                       ('{"x":NaN}', live.MODEL),
                                       ('{"x":Infinity}', live.MODEL),
                                       ("[]", live.MODEL)):
                with self.subTest(arguments=arguments):
                    self.write_capture(segment, self.messages_arguments(arguments))
                    self.assertEqual(live.stream_validation_outcome(message, [segment]), outcome)

    def test_argument_failure_markers_require_their_own_protocol_and_full_prefix(self):
        chat = "Chat stream validation failed: tool arguments are malformed or ambiguous"
        messages = "Messages stream validation failed: terminal Messages tool input was not complete valid JSON"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for marker, matching, mismatched in (
                    (chat, self.chat_arguments("{"), self.messages_arguments("{")),
                    (messages, self.messages_arguments("{"), self.chat_arguments("{"))):
                with self.subTest(marker=marker):
                    self.write_capture(segment, mismatched)
                    self.assertEqual(live.stream_validation_outcome(marker, [segment]), live.HARNESS)
                    self.write_capture(segment, matching)
                    self.assertEqual(live.stream_validation_outcome(marker, [segment]), live.MODEL)
                    bare = marker.split("stream validation failed: ", 1)[1]
                    self.assertIsNone(live.stream_validation_outcome(bare, [segment]))

    def test_other_provider_argument_failures_without_stream_prefix_stay_harness(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for diagnostic, events in (
                    ("OpenAI tool arguments are malformed or ambiguous", self.chat_arguments("{")),
                    ("Anthropic terminal Messages tool input was not complete valid JSON",
                     self.messages_arguments("{"))):
                with self.subTest(diagnostic=diagnostic):
                    self.write_capture(segment, events)
                    turn = live.Turn(phase="failed", model_error_kind="Protocol", errors=[diagnostic])
                    with self.assertRaises(live.ScenarioFailure) as raised:
                        live.classify_incomplete(turn, "turn", segments=[segment])
                    self.assertEqual(raised.exception.outcome, live.HARNESS)

    def test_invalid_argument_evidence_is_process_and_last_exchange_scoped(self):
        message = "Chat stream validation failed: tool arguments are malformed or ambiguous"
        with tempfile.TemporaryDirectory() as directory:
            earlier = Path(directory) / "earlier"
            failing = Path(directory) / "failing"
            earlier.mkdir()
            failing.mkdir()
            self.write_capture(earlier, self.chat_arguments("{"))
            self.write_capture(failing, self.chat_arguments("{}"))
            self.assertEqual(live.stream_validation_outcome(message, [earlier, failing]), live.HARNESS)
            self.write_capture(failing, self.chat_arguments("{"))
            self.write_capture(failing, self.chat_arguments("{}"), exchange=2)
            self.assertEqual(live.stream_validation_outcome(message, [failing]), live.HARNESS)
            self.write_capture(failing, self.chat_arguments("{"), exchange=3)
            self.assertEqual(live.stream_validation_outcome(message, [failing]), live.MODEL)

    def test_invalid_argument_evidence_orders_exchanges_by_number(self):
        message = "OpenRouter Chat stream validation failed: tool arguments are malformed or ambiguous"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, self.chat_arguments("{"), exchange=9999)
            self.write_capture(segment, self.chat_arguments("{}"), exchange=10000)
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.HARNESS)
            self.write_capture(segment, self.chat_arguments("{"), exchange=10000)
            info = segment / "exchange-10000.info"
            info.write_text(info.read_text().replace("terminal=Complete", "terminal=ProtocolComplete"))
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.MODEL)

    def test_invalid_argument_evidence_requires_the_highest_exchange_to_be_finished(self):
        message = "OpenRouter Chat stream validation failed: tool arguments are malformed or ambiguous"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, self.chat_arguments("{"))
            for terminal, outcome in (("ClosedEarly", live.MODEL), ("Abandoned", live.HARNESS)):
                with self.subTest(terminal=terminal):
                    self.write_capture(segment, self.chat_arguments("{"), exchange=2)
                    info = segment / "exchange-0002.info"
                    info.write_text(info.read_text().replace("terminal=Complete", f"terminal={terminal}"))
                    self.assertEqual(live.stream_validation_outcome(message, [segment]), outcome)

    def test_a_fail_closed_early_close_needs_the_finish_in_the_body(self):
        # The kernel rejects the finished arguments and closes before end of
        # body: the captured finish proves the model ended them unterminated.
        chat = "OpenRouter Chat stream validation failed: tool arguments are malformed or ambiguous"
        messages = "Messages stream validation failed: terminal Messages tool input was not complete valid JSON"
        unterminated = '{"path": "repo. cwd: repo. cwd: repo'
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for marker, finished, unfinished in (
                    (chat, self.chat_arguments(unterminated), self.chat_arguments(unterminated)[:1]),
                    (messages, self.messages_arguments(unterminated),
                     self.messages_arguments(unterminated)[:2])):
                for events, outcome in ((finished, live.MODEL), (unfinished, live.HARNESS)):
                    with self.subTest(marker=marker, events=len(events)):
                        self.write_capture(segment, events)
                        info = segment / "exchange-0001.info"
                        info.write_text(info.read_text().replace("terminal=Complete", "terminal=ClosedEarly"))
                        self.assertEqual(live.stream_validation_outcome(marker, [segment]), outcome)

    def test_subagents_scores_only_its_root_process_capture(self):
        message = "Chat stream validation failed: tool arguments are malformed or ambiguous"
        with tempfile.TemporaryDirectory() as directory:
            earlier = Path(directory) / "earlier"
            root = Path(directory) / "root"
            earlier.mkdir()
            root.mkdir()
            self.write_capture(earlier, self.chat_arguments("{"))
            self.write_capture(root, self.chat_arguments("{}"))
            driver = smoke.Smoke(live.ROUTES["codex"], Mock(capture_segments=[earlier, root]))
            driver.repo = Mock(return_value=root)
            agent = Mock(capture_dir=root)
            agent.prompt.return_value = live.Turn(phase="failed", errors=[message])
            driver.agent = Mock(return_value=agent)
            with patch.object(live, "stream_validation_outcome",
                              wraps=live.stream_validation_outcome) as classify:
                with self.assertRaises(live.ScenarioFailure) as raised:
                    driver.subagents()
            self.assertEqual(raised.exception.outcome, live.HARNESS)
            self.assertEqual(classify.call_args.args[1], [root])

    def test_finished_malformed_chat_arguments_are_model_on_every_surface(self):
        message = "Chat stream validation failed: tool arguments are malformed or ambiguous"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for arguments, outcome in (("{", live.MODEL), ('{"x":1,"x":2}', live.MODEL),
                                       ('{"x":1}', live.HARNESS)):
                events = [
                    {"choices": [{"index": 0, "delta": {"tool_calls": [
                        {"index": 0, "function": {"arguments": arguments}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
                self.write_capture(segment, events)
                self.assertEqual(live.stream_validation_outcome(message, [segment]), outcome)
            self.write_capture(segment, [
                {"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "{"}}]}}]},
                {"choices": [{"index": 0, "finish_reason": "tool_calls"}]},
            ])
            for classify in (
                    lambda: live.classify_incomplete(live.Turn(phase="failed", errors=[message]),
                                                     "turn", segments=[segment]),
                    lambda: live.classify_command_error(message, "compact", segments=[segment]),
                    lambda: live.classify_cli_failure(5, message, "CLI", segments=[segment])):
                with self.assertRaises(live.ScenarioFailure) as raised:
                    classify()
                self.assertEqual(raised.exception.outcome, live.MODEL)
            (segment / "exchange-0001.info").unlink()
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.HARNESS)
            self.assertEqual(live.stream_validation_outcome(message, None), live.HARNESS)

    def test_messages_arguments_need_a_finished_capture(self):
        message = "Messages stream validation failed: terminal Messages tool input was not complete valid JSON"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            events = [
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "tool_use", "input": {}}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "input_json_delta", "partial_json": '{"x":'}},
                {"type": "content_block_stop", "index": 0},
                {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
                {"type": "message_stop"},
            ]
            self.write_capture(segment, events)
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.MODEL)
            info = segment / "exchange-0001.info"
            info.write_text(info.read_text().replace("terminal=Complete", "terminal=ClosedEarly"))
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.MODEL)
            info.write_text(info.read_text().replace("terminal=ClosedEarly", "terminal=Abandoned"))
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.HARNESS)
            self.write_capture(segment, events[:-2])
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.HARNESS)

    def test_subagents_root_failure_uses_its_capture_segments(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, [
                {"choices": [{"delta": {"content": "x"}}], "usage": {"prompt_tokens": 1}},
            ])
            driver = smoke.Smoke(live.ROUTES["codex"], Mock(capture_segments=[segment]))
            driver.repo = Mock(return_value=segment)
            agent = Mock(capture_dir=segment)
            agent.prompt.return_value = live.Turn(
                phase="failed", errors=["stream validation failed"])
            driver.agent = Mock(return_value=agent)
            with self.assertRaises(live.ScenarioFailure) as raised:
                driver.subagents()
            self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_smoke_tui_classifies_only_exhausted_provider_retries(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            telemetry = home / "telemetry"
            telemetry.mkdir()
            with closing(sqlite3.connect(telemetry / "telemetry-v14.sqlite3")) as connection, connection:
                connection.execute("CREATE TABLE model_retries "
                                   "(turn_id TEXT, cause_kind TEXT, "
                                   "next_attempt INTEGER, max_attempts INTEGER, declined_reason TEXT)")
                connection.executemany("INSERT INTO model_retries VALUES (?, ?, ?, ?, ?)", [
                    ("transient", "http_status", 2, 4, None),
                    ("exhausted", "http_status", 4, 4, None),
                    ("other-turn", "http_status", 4, 4, None),
                    ("declined-budget", "http_status", 4, 4, None),
                    ("declined-budget", "http_status", 5, 4, "budget"),
                    ("declined-deadline", "http_status", 4, 4, "deadline"),
                ])
            for turn_id, outcome in (("transient", live.HARNESS),
                                     ("exhausted", live.PROVIDER),
                                     ("declined-budget", live.PROVIDER),
                                     ("declined-deadline", live.HARNESS),
                                     ("no-retries", live.HARNESS)):
                with self.subTest(turn_id=turn_id):
                    with self.assertRaises(live.ScenarioFailure) as raised:
                        smoke.require_durable_completed(
                            {"phase": "failed", "turn_id": turn_id}, "TUI", home=home)
                    self.assertEqual(raised.exception.outcome, outcome)
            with self.assertRaises(sqlite3.ProgrammingError):
                connection.execute("SELECT 1")

    def test_tui_quit_sees_a_split_dialog_after_screen_rollover(self):
        tui = live.Tui.__new__(live.Tui)
        tui.screen = b"old Quit plak?" + b"x" * (200_000 - 14)
        tui.log = io.BytesIO()
        tui.fd, tui.pid = 42, 123
        tui._quit_output = None
        chunks = iter([b"z" * 65536, b"Quit pla", b"k?", *([b"z" * 65536] * 4)])
        writes = []
        clock = itertools.count()

        def exited(pid, flags):
            return (pid, 0) if b"\r" in writes else (0, 0)

        with patch.object(live.time, "monotonic", side_effect=lambda: next(clock) * 0.01), \
                patch.object(live.time, "sleep"), \
                patch.object(live.select, "select", return_value=([42], [], [])), \
                patch.object(live.os, "read", side_effect=lambda fd, size: next(chunks, b"")), \
                patch.object(live.os, "write", side_effect=lambda fd, data: writes.append(data)), \
                patch.object(live.os, "waitpid", side_effect=exited), \
                patch.object(live.os, "kill") as kill:
            self.assertEqual(tui.quit(timeout=1), 0)
        self.assertIn(b"\r", writes)
        kill.assert_not_called()
        self.assertIsNone(tui._quit_output)

    def test_tui_quit_does_not_confirm_a_stale_dialog_and_releases_its_buffer(self):
        tui = live.Tui.__new__(live.Tui)
        tui.screen = b"Quit plak?"
        tui._quit_output = None
        tui.log = io.BytesIO()
        tui.fd, tui.pid = 42, 123
        writes = []
        clock = itertools.count()
        with patch.object(live.time, "monotonic", side_effect=lambda: next(clock) * 0.01), \
                patch.object(live.select, "select", return_value=([42], [], [])), \
                patch.object(live.os, "read", return_value=b"still running"), \
                patch.object(live.os, "write", side_effect=lambda fd, data: writes.append(data)), \
                patch.object(live.os, "waitpid", return_value=(0, 0)), \
                patch.object(live.os, "kill") as kill:
            self.assertIsNone(tui.quit(timeout=1))
        self.assertEqual(writes, [b"\x11"])
        kill.assert_called_once_with(123, live.signal.SIGKILL)
        self.assertIsNone(tui._quit_output)

    def test_tui_quit_releases_its_buffer_when_ctrl_q_fails(self):
        tui = live.Tui.__new__(live.Tui)
        tui._quit_output = None
        with patch.object(tui, "key", side_effect=OSError("synthetic PTY failure")):
            with self.assertRaises(OSError):
                tui.quit()
        self.assertIsNone(tui._quit_output)

    def test_codex_growth_uses_neutral_digits_and_explicit_child_paging(self):
        with tempfile.TemporaryDirectory() as directory:
            bulk = Path(directory) / "bulk.txt"
            with patch.object(long_session, "digit_line", return_value="12345 67890"), \
                    patch.object(long_session, "base64_line", return_value="secretish"):
                long_session.write_bulk(bulk, 1, "codex")
            self.assertEqual(set(bulk.read_text()), set("12345 67890\n"))
        plant = long_session.plant_prompt("codex")
        for pack in ("codex", "claude", "grok"):
            self.assertNotIn("Do not print", long_session.plant_prompt(pack))
            self.assertNotIn("token", long_session.plant_prompt(pack))
        prompt = long_session.child_prompt("codex", 5)
        child = json.loads(prompt.split("Give the child exactly this prompt: ", 1)[1]
                           .split("\nWait for the child", 1)[0])
        self.assertIn(plant.split(" Reply with exactly:", 1)[0], child)
        self.assertIn("python3 page.py k 1", child)
        self.assertIn("notify(a.output)", child)
        self.assertIn("recalled.txt", child)
        self.assertTrue(child.endswith("Reply with exactly: done"))
        self.assertEqual(child.count("Reply with exactly:"), 1)

    def test_child_reply_directive_follows_all_plant_paging_and_recall_steps(self):
        for pack in ("codex", "claude", "grok", "pi"):
            with self.subTest(pack=pack):
                prompt = long_session.child_prompt(pack, 5)
                child = json.loads(prompt.split("Give the child exactly this prompt: ", 1)[1]
                                   .split("\nWait for the child", 1)[0])
                self.assertNotIn("Reply with exactly: ok", child)
                self.assertTrue(child.endswith("Reply with exactly: done"))
                self.assertLess(child.index("acknowledged.txt"), child.index("bulk.txt"))
                self.assertLess(child.index("bulk.txt"), child.index("recalled.txt"))
                self.assertLess(child.index("recalled.txt"), child.index("Reply with exactly:"))

    def compaction_driver(self, repo):
        driver = smoke.Smoke(live.ROUTES["codex"], Mock(capture_segments=[]))
        driver.repo = Mock(return_value=repo)
        agent = Mock(session_id="session", capture_dir=repo)
        agent.prompt.return_value = live.Turn(phase="completed", text="ZEBRA-42", tools=1)
        agent.command.return_value = {"kind": "ok"}
        driver.agent = Mock(return_value=agent)
        return driver, agent

    def test_compaction_fact_cannot_leak_into_a_repository_file(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            driver, agent = self.compaction_driver(repo)

            def prompt(text, **kwargs):
                if "Remember the code word" in text:
                    self.assertNotIn("Do not write", text)
                    (repo / "nested").mkdir()
                    (repo / "nested" / "fact.bin").write_bytes(b"\x00ZEBRA-42\x00")
                return live.Turn(phase="completed", text="ZEBRA-42", tools=1)

            agent.prompt.side_effect = prompt
            with patch.object(live, "session_records", return_value=[
                    {"kind": "compact.finished", "outcome": "committed"}]):
                with self.assertRaises(live.ScenarioFailure) as raised:
                    driver.compaction()
            self.assertEqual(raised.exception.outcome, live.HARNESS)
            self.assertEqual(agent.prompt.call_count, 2)

    def test_compaction_first_prompt_names_the_code_mode_tool_only_for_codex(self):
        codex = smoke.compaction_first_prompt("codex")
        self.assertIn("one exec_command", codex)
        self.assertIn("without max_output_tokens", codex)
        self.assertTrue(codex.endswith("Remember the code word ZEBRA-42. Reply: ok"))
        for pack in ("grok", "claude", "pi"):
            self.assertEqual(
                smoke.compaction_first_prompt(pack),
                "Read fold.txt in full with one read. Remember the code word ZEBRA-42. Reply: ok")

    def test_compaction_first_turn_without_the_read_is_a_model_miss(self):
        with tempfile.TemporaryDirectory() as directory:
            driver, agent = self.compaction_driver(Path(directory))
            agent.prompt.return_value = live.Turn(phase="completed", text="ok", tools=0)
            with self.assertRaises(live.ScenarioFailure) as raised:
                driver.compaction()
            self.assertEqual(raised.exception.outcome, live.MODEL)
            self.assertIn("without reading fold.txt", str(raised.exception))
            self.assertEqual(agent.prompt.call_count, 1)
            agent.command.assert_not_called()

    def test_compaction_fact_survives_without_a_file_shortcut(self):
        with tempfile.TemporaryDirectory() as directory:
            driver, agent = self.compaction_driver(Path(directory))
            with patch.object(live, "session_records", return_value=[
                    {"kind": "compact.finished", "outcome": "committed"}]):
                self.assertIn("context carried", driver.compaction())
            self.assertEqual(agent.prompt.call_count, 3)
            agent.command.assert_called_once_with("compact")


KEEP_ALIVE = b": OPENROUTER PROCESSING\n\n"
IN_BAND_504 = b'data: {"choices":[],"error":{"code":504,"message":"error code: 504"}}\n\n'
FAILED_504 = dict(phase="failed", model_error_kind="Timeout", http_status=504,
                  errors=["OpenRouter returned an in-band error after accepting the request: "
                          "HTTP 504 (timeout) from Liquid"])


class UnfiredFaultTests(unittest.TestCase):
    """An `after-events` reset the provider's own error beat to the cut."""

    def write_exchange(self, segment, number, body, *, request=b'{"messages":[]}',
                       status="Some(200)", terminal="ClosedEarly"):
        name = f"exchange-{number:04}"
        (segment / f"{name}.request").write_bytes(request)
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={number}\nstatus={status}\nconnection=Some(Fresh)\n"
            f"terminal={terminal}\nrecorded_bytes={len(body)}\nincomplete=false\n")

    def judge(self, target_body, *, turn=None, status="Some(200)", terminal="ClosedEarly",
              threshold=3):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            # Model lookups are bodiless GETs; the targeted exchange is the
            # first inference request, and a later replay does not count.
            self.write_exchange(segment, 1, b'{"data":[]}', request=b"", terminal="Complete")
            self.write_exchange(segment, 2, target_body, status=status, terminal=terminal)
            self.write_exchange(segment, 3, b"data: {}\n\ndata: [DONE]\n\n", terminal="Complete")
            turn = turn or live.Turn(**FAILED_504)
            with self.assertRaises(live.ScenarioFailure) as raised:
                smoke.judge_fault_reset(turn, segment / "hello.txt", [segment], threshold)
            return raised.exception

    def test_after_events_parses_the_fault_plan(self):
        plan = live.fault_plan(live.ROUTES["or-lfm-free"], "reset", after_events=3)
        self.assertEqual(smoke.after_events(plan), 3)
        self.assertIsNone(smoke.after_events(live.fault_plan(live.ROUTES["or-lfm-free"], "429")))

    def test_keep_alives_then_an_in_band_504_before_the_cut_is_provider(self):
        failure = self.judge(KEEP_ALIVE * 33 + IN_BAND_504)
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertEqual(str(failure), "the provider ended the targeted stream (exchange-0002) in "
                                       "HTTP 504 after 1 of 3 events, before the reset fired")

    def test_comment_only_blocks_do_not_count_as_events(self):
        self.assertEqual(live.sse_events(KEEP_ALIVE * 33 + IN_BAND_504 + b"data: x"),
                         [IN_BAND_504.rstrip(b"\n")])
        self.assertEqual(len(live.sse_events(b": a\r\nevent: e\r\n: b\r\ndata: 1\r\n\r\n\n\n")), 1)

    def test_a_targeted_http_error_status_is_provider(self):
        failure = self.judge(b'{"error":{"code":503}}', status="Some(503)", terminal="Complete",
                             turn=live.Turn(phase="failed", model_error_kind="Unavailable",
                                            http_status=503))
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertIn("in HTTP 503 after 0 of 3 events", str(failure))

    def test_other_wires_and_a_provider_transport_failure_before_the_cut(self):
        messages = (b'event: message_start\ndata: {"type":"message_start"}\n\n'
                    b'event: error\ndata: {"type":"error","error":{"type":"overloaded_error"}}\n\n')
        failure = self.judge(messages, turn=live.Turn(phase="failed", model_error_kind="Unavailable"))
        self.assertEqual((failure.outcome, str(failure)), (
            live.PROVIDER, "the provider ended the targeted stream (exchange-0002) in HTTP 529 "
                           "after 2 of 3 events, before the reset fired"))
        reset = ("Failed(Failure { phase: ResponseBody, transmission: MayHaveTransmitted, "
                 "kind: DeadlineExceeded, cause: None, http2_reason: None, os_error: None, tls: None, "
                 "tls_alert: None, alpn: None, http2_io: false, connection: None, idle_age: None })")
        failure = self.judge(KEEP_ALIVE, terminal=reset,
                             turn=live.Turn(phase="failed", model_error_kind="Timeout"))
        self.assertEqual(str(failure), "the provider ended the targeted stream (exchange-0002) in "
                                       "a transport failure (DeadlineExceeded) in ResponseBody "
                                       "after 0 of 3 events, "
                                       "before the reset fired")
        # The product typed it a transport failure, not a provider error.
        failure = self.judge(KEEP_ALIVE, terminal=reset,
                             turn=live.Turn(phase="failed", model_error_kind="Transport"))
        self.assertEqual(failure.outcome, live.HARNESS)

    def test_a_request_without_a_conversation_is_not_the_target(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_exchange(segment, 1, b'{"ok":true}', request=b'{"grant_type":"refresh"}',
                                terminal="Complete")
            self.write_exchange(segment, 2, KEEP_ALIVE * 3 + IN_BAND_504)
            failure = smoke.provider_ended_before_cut(live.Turn(**FAILED_504), [segment], 3)
            self.assertIn("(exchange-0002) in HTTP 504", failure)

    def test_other_unobserved_resets_stay_harness(self):
        for name, kwargs in (
                ("no error event", {"target_body": KEEP_ALIVE + b"data: {}\n\n"}),
                ("the cut was reached", {"target_body": b"data: {}\n\n" * 2 + IN_BAND_504}),
                ("an abandoned capture", {"target_body": IN_BAND_504, "terminal": "Abandoned"}),
                ("a 400 error event", {"target_body": IN_BAND_504.replace(b"504", b"400")}),
                ("a completed turn", {"target_body": IN_BAND_504,
                                      "turn": live.Turn(phase="completed")}),
                ("a turn that failed as the product's", {
                    "target_body": IN_BAND_504,
                    "turn": live.Turn(phase="failed", model_error_kind="Protocol")}),
                ("no after-events threshold", {"target_body": IN_BAND_504, "threshold": None})):
            with self.subTest(name):
                failure = self.judge(**kwargs)
                self.assertEqual((failure.outcome, str(failure)),
                                 (live.HARNESS, "injected reset was not observed"))


class CancelWithoutInterruptTests(unittest.TestCase):
    def cancel(self, turn):
        driver = smoke.Smoke(live.ROUTES["or-lfm-free"], Mock(capture_segments=[]))
        driver.repo = Mock(return_value=Path("repo"))
        agent = Mock(capture_dir=Path("segment"), lookup_faults=None)
        agent.prompt.return_value = turn
        driver.agent = Mock(return_value=agent)
        with self.assertRaises(live.ScenarioFailure) as raised:
            driver.cancel()
        agent.quit.assert_called_once()
        return raised.exception

    def test_a_turn_that_failed_before_the_command_is_classified_like_any_turn(self):
        failure = self.cancel(live.Turn(**FAILED_504))
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertEqual(str(failure), "cancel turn: provider in-band error (Timeout)")

    def test_only_a_completed_turn_without_the_command_is_model(self):
        failure = self.cancel(live.Turn(phase="completed", text="done"))
        self.assertEqual((failure.outcome, str(failure)),
                         (live.MODEL, "the model never started the command"))


class NestedSteerAwaitTests(unittest.TestCase):
    """The Code Mode await steer is judged at the moment it is applied."""

    def replay(self, events, *, finished_after: int | None = None):
        """Feed timed `plak agent` events through the scenario; the job ends after event N.

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
            return name == "job-finished" and finished_after is not None and seen["n"] > finished_after

        def run_turn(_agent, text, _timeout, tick, _accept_eof):
            self.prompt = text
            for index, (at, event) in enumerate(events):
                seen["n"], seen["t"] = index, at
                agent.observed.append((at, event))
                tick(None, event)
            return live.Turn(phase="completed")

        with patch.object(jobs, "run_turn", run_turn), \
                patch.object(jobs, "marker", side_effect=finished), \
                patch.object(jobs, "matching", side_effect=lambda lines, token: (
                    [] if finished(repo, "job-finished") else ["hold"])), \
                patch.object(jobs, "command_lines", return_value=[]), \
                patch.object(jobs, "require_script"), patch.object(jobs, "close_agent"), \
                patch.object(jobs, "stop_token"), \
                patch.object(jobs, "job_id_for", return_value="p1"), \
                patch.object(jobs.time, "monotonic", side_effect=lambda: seen["t"]):
            try:
                return driver.steer_await(), sent
            except live.ScenarioFailure as failure:
                return failure, sent

    CELL = {"kind": "tool", "name": "exec", "call_id": "cell", "state": "running"}
    START = {"kind": "tool", "name": "process_session_start", "call_id": "start",
             "parent_call_id": "cell", "state": "running"}
    STARTED = {**START, "state": "done"}
    PROMOTED = {"kind": "job_promoted", "job_id": "p1"}
    WRITE = {"kind": "tool", "name": "process_session_write", "call_id": "write",
             "parent_call_id": "cell", "state": "running"}
    YIELDED = {**WRITE, "state": "done"}
    APPLIED = {"kind": "steer_ack", "state": "applied"}
    QUEUED = {"kind": "steer_ack", "state": "queued", "position": 1}
    CELL_WAIT = {"kind": "tool", "name": "wait", "call_id": "wait", "parent_call_id": "cell",
                 "state": "running"}
    CELL_WAITED = {**CELL_WAIT, "state": "done"}
    CELL_DONE = {**CELL, "state": "done"}

    def timed(self, *events, step=0.1):
        return [(index * step, event) for index, event in enumerate(events)]

    def test_the_prompt_asks_for_one_nested_wait_and_to_follow_a_new_instruction(self):
        self.replay([])
        self.assertIn("one write_stdin call", self.prompt)
        self.assertIn("If a new instruction arrives, follow it immediately without waiting on "
                      "the cell.", self.prompt)
        self.assertNotIn("must not yield", self.prompt)

    def test_a_steer_applied_while_the_job_runs_passes_even_if_the_job_ends_later(self):
        result, sent = self.replay(self.timed(
            self.CELL, self.START, self.STARTED, self.PROMOTED, self.WRITE, self.QUEUED,
            self.YIELDED, self.APPLIED, self.CELL_DONE), finished_after=7)
        self.assertEqual(result, "nested wait yielded on the steer; applied at the outer-cell "
                                 "boundary in 0.3 s; job still running")
        self.assertEqual(sent, [{"text": jobs.STEER_TEXT, "steer": True}])

    def test_the_recorded_shape_waits_on_the_cell_after_the_steer(self):
        result, _sent = self.replay(self.timed(
            self.CELL, self.START, self.STARTED, self.PROMOTED, self.WRITE, self.YIELDED,
            self.APPLIED, self.CELL_WAIT, self.CELL_DONE), finished_after=7)
        self.assertEqual((result.outcome, str(result)), (
            live.MODEL, "model waited on the cell after the steer was applied instead of "
                        "following the steer"))

    def test_only_a_cell_wait_first_seen_after_the_applied_steer_counts(self):
        # A wait started before the steer and finished after it is not the miss.
        result, _sent = self.replay(self.timed(
            self.CELL, self.START, self.STARTED, self.PROMOTED, self.CELL_WAIT, self.WRITE,
            self.YIELDED, self.APPLIED, self.CELL_WAITED, self.CELL_DONE), finished_after=8)
        self.assertIsInstance(result, str)

    def test_a_start_alone_is_not_a_nested_wait(self):
        result, sent = self.replay(self.timed(
            self.CELL, self.START, self.PROMOTED, self.STARTED, self.CELL_DONE))
        self.assertEqual(sent, [])
        self.assertEqual((result.outcome, str(result)), (
            live.MODEL, "no nested wait was in flight inside the cell while the job ran"))

    def test_a_cell_that_already_completed_is_not_an_outer_cell_steer(self):
        result, sent = self.replay(self.timed(
            self.CELL, self.START, self.STARTED, self.PROMOTED, self.CELL_DONE))
        self.assertEqual(sent, [])
        self.assertEqual(result.outcome, live.MODEL)

    def test_latency_and_the_job_state_at_the_applied_steer_decide(self):
        events = [(0.0, self.CELL), (0.1, self.START), (0.2, self.STARTED), (0.3, self.PROMOTED),
                  (0.4, self.WRITE), (0.5, self.YIELDED)]
        result, _ = self.replay([*events, (9.5, self.APPLIED)])
        self.assertEqual((result.outcome, str(result)), (
            live.HARNESS, "steer was applied 9.1 s after it was sent, past 8 s"))
        # The nested wait yielded while the job ran; the job then ended on its
        # own before the outer cell applied the steer: a late nested wait.
        result, _ = self.replay([*events, (2.0, self.APPLIED)], finished_after=5)
        self.assertEqual((result.outcome, str(result)), (
            live.MODEL, "job finished within 1.6 s of the steer, after the nested wait yielded: "
                        "the nested wait began too late"))
        # The nested wait returned only once the job had finished: never yielded.
        result, _ = self.replay([*events, (2.0, self.APPLIED)], finished_after=4)
        self.assertEqual((result.outcome, str(result)), (
            live.HARNESS, "the nested wait returned only when the job finished: it was not "
                          "yielded on the steer"))
        result, _ = self.replay([*events, (0.6, self.QUEUED)])
        self.assertEqual((result.outcome, str(result)), (
            live.HARNESS, "steer was not applied at the outer-cell boundary"))
        # A steer sent with under the 8 s bar of the 25 s hold left is late,
        # whatever the product did with the nested wait.
        with patch.object(jobs, "started_wall", side_effect=lambda _repo: jobs.time.time() - 20.0):
            result, _ = self.replay([*events, (2.0, self.APPLIED)], finished_after=4)
        self.assertEqual(result.outcome, live.MODEL)
        self.assertRegex(str(result), r"^the steer went out with [45]\.\d s of the hold left, "
                                      r"under the 8 s bar")

    def test_the_promotion_prompt_names_the_only_code_mode_argument(self):
        codex = jobs.promotion_prompt(live.ROUTES["codex"], "python3 hold.py 8 t")
        self.assertIn("Call exec_command with only the cmd argument: do not pass "
                      "yield_time_ms or any other argument.", codex)
        grok = jobs.promotion_prompt(live.ROUTES["grok"], "python3 hold.py 8 t")
        self.assertIn("Do not pass a timeout, yield, block_until, or background flag.", grok)
        self.assertNotIn("exec_command", grok)


class InTurnEarlyStopTests(unittest.TestCase):
    def judge(self, repo, words):
        session = long_session.LongSession.__new__(long_session.LongSession)
        with patch.object(long_session, "bulk_windows", return_value=[(0, 1)]), \
                patch.object(long_session, "scheduled_in_window", return_value=[{}]), \
                patch.object(long_session, "assert_shrunk"), \
                patch.object(long_session, "reread_after_commit", return_value=[]):
            with self.assertRaises(live.ScenarioFailure) as raised:
                session._require_in_turn([], "rel-token", repo, words)
        return raised.exception

    def test_an_early_stop_names_the_pages_before_the_recall(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            (repo / long_session.PAGE_DIR).mkdir()
            words = [f"check-{index}" for index in range(1, 53)]
            for index, word in enumerate(words[:37], 1):
                (repo / long_session.PAGE_DIR / f"{index}.txt").write_text(word + "\n")
            failure = self.judge(repo, words)
            self.assertEqual((failure.outcome, str(failure)), (
                live.MODEL, "growth turn ended after 37/52 pages without writing recalled.txt"))
            (repo / "recalled.txt").write_text("rel-token\n")
            failure = self.judge(repo, words)
            self.assertEqual(str(failure), "growth turn wrote check words for 37/52 pages")
            for index, word in enumerate(words, 1):
                (repo / long_session.PAGE_DIR / f"{index}.txt").write_text(word + "\n")
            (repo / "recalled.txt").write_text("other\n")
            failure = self.judge(repo, words)
            self.assertEqual(str(failure), "recalled.txt does not match the pre-compaction code word")
            (repo / "recalled.txt").unlink()
            self.assertEqual(str(self.judge(repo, [])),
                             "growth turn ended without writing recalled.txt")


class IncompleteCaptureNamingTests(unittest.TestCase):
    def test_score_report_names_the_incomplete_scenario_segment_and_exchange(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory) / "segment-001"
            segment.mkdir()
            (segment / "exchange-0001.request").write_bytes(b"{}")
            (segment / "exchange-0001.body").write_bytes(b"")
            (segment / "exchange-0001.info").write_text(
                "process_id=1\nexchange=1\nstatus=None\nterminal=pending\n"
                "recorded_bytes=0\nincomplete=true\n")
            empty = Path(directory) / "segment-002"
            empty.mkdir()
            report = {"capture_complete": False, "binary_sha256_start": "a",
                      "binary_sha256_end": "a", "results": [],
                      "evidence": {"cli_task": {"segments": [str(segment)],
                                                "capture_complete": False, "provider_pending": []},
                                   "tui": {"segments": [str(empty)], "capture_complete": False,
                                           "provider_pending": []},
                                   "cancel": {"segments": [], "capture_complete": False},
                                   "jsonl_task": {"segments": [str(segment)],
                                                  "capture_complete": True}}}
            self.assertEqual(qualification.score_report(1, report), (
                live.HARNESS, "raw exchange evidence missing or incomplete: cli_task: segment-001 "
                              "exchange-0001 pending; tui: segment-002 has no exchange; "
                              "cancel: no capture segment"))
            report["evidence"]["cli_task"]["provider_pending"] = [str(segment)]
            report["evidence"]["cli_task"]["capture_complete"] = True
            report["capture_complete"] = True
            self.assertEqual(qualification.score_report(1, report)[1],
                             "scenario capture missing or incomplete: tui: segment-002 has no "
                             "exchange; cancel: no capture segment")


PLANNED_LONG_SESSION_ROUTES = (
    "codex", "codex-luna-medium", "codex-sol", "codex-astra", "grok", "grok-46",
    "claude-sonnet55-low", "claude-sonnet55-medium", "claude-haiku55-low",
    "claude-haiku55-medium", "claude-opus-low", "claude-opus-medium", "claude-fable",
    "deepseek", "deepseek-messages")


class LongSessionRoundCapTests(unittest.TestCase):
    def test_every_planned_route_plans_under_its_pinned_cap(self):
        with patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": "967232",
                                     "PLAK_LIVE_GROWTH_PAGES": ""}):
            for name in PLANNED_LONG_SESSION_ROUTES:
                route = live.ROUTES[name]
                for scenario, _surface in long_session.CATALOG:
                    with self.subTest(route=name, scenario=scenario):
                        planned = long_session.planned_rounds(route, scenario)
                        cap = long_session.round_cap(route, scenario)
                        self.assertLessEqual(planned + long_session.ROUND_HEADROOM, cap)
                        self.assertGreaterEqual(cap, long_session.ROUND_CAP)
                        self.assertLessEqual(cap, long_session.ROUND_CAP_CEILING)
            sol = live.ROUTES["codex-sol"]
            # 69 check-word pages: 63 of them used the old fixed 128 rounds.
            self.assertEqual(long_session.planned_rounds(sol, "in_turn_compaction"), 138)
            self.assertEqual(long_session.round_cap(sol, "in_turn_compaction"), 170)
            self.assertEqual(long_session.round_cap(sol, "kill_reopen"), long_session.ROUND_CAP)

    def test_the_largest_growth_dial_still_fits_the_spec_07_ceiling(self):
        with patch.dict(os.environ, {"PLAK_LIVE_GROWTH_PAGES": str(long_session.MAX_GROWTH_PAGES)}):
            for name in ("codex", "grok", "claude-sonnet55-low"):
                route = live.ROUTES[name]
                self.assertLessEqual(long_session.planned_rounds(route, "in_turn_compaction")
                                     + long_session.ROUND_HEADROOM, long_session.ROUND_CAP_CEILING)

    def test_the_wrap_up_is_seen_in_the_session_log_or_a_childs_requests(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            (segment / "exchange-0001.request").write_bytes(b'{"input":[]}')
            self.assertFalse(long_session.round_cap_reached([], [segment]))
            self.assertTrue(long_session.round_cap_reached(
                [{"kind": "turn.operation_budget_exhausted"}], []))
            (segment / "exchange-0002.request").write_bytes(
                b'{"input":[{"text":"[harness] This turn has used its entire model-operation '
                b'budget, so no further tool calls are possible."}]}')
            self.assertTrue(long_session.round_cap_reached([], [segment]))
            failure = long_session.round_cap_failure("growth turn", 170, "63/69 pages")
            self.assertEqual((failure.outcome, str(failure)), (
                live.HARNESS, "growth turn ended on the operation-budget wrap-up at the pinned "
                              "round cap 170 after 63/69 pages (driver sizing)"))

    def test_the_cap_explains_a_miss_only_when_the_plan_exceeds_it(self):
        session = long_session.LongSession.__new__(long_session.LongSession)
        session.window, session.route, session.caps = 272_000, live.ROUTES["codex"], {"overflow": 128}
        turn = live.Turn(phase="completed")
        wrapped = [{"kind": "turn.operation_budget_exhausted"}]
        with patch.object(long_session, "overflow_remedy", return_value=None), \
                patch.object(long_session, "reached_trigger", return_value=False):
            # The plan fits: the model spent the cap, so the miss stays model.
            with self.assertRaises(live.ScenarioFailure) as raised:
                session._require_overflow(turn, wrapped, "check words for 40/36 pages")
            self.assertEqual((raised.exception.outcome, str(raised.exception)), (
                live.MODEL, "overflow turn did not reach the compaction trigger (check words for "
                            "40/36 pages); the turn spent the pinned round cap 128, and its plan "
                            "needs 72 rounds"))
            # A plan that does not fit under the pinned cap is driver sizing.
            with patch.object(long_session, "planned_rounds", return_value=138):
                with self.assertRaises(live.ScenarioFailure) as raised:
                    session._require_overflow(turn, wrapped, "check words for 63/69 pages")
            self.assertEqual((raised.exception.outcome, str(raised.exception)), (
                live.HARNESS, "overflow turn ended on the operation-budget wrap-up at the pinned "
                              "round cap 128 after 63/69 pages (driver sizing)"))
            # No wrap-up: the oracle's own miss.
            with patch.object(long_session, "planned_rounds", return_value=138):
                with self.assertRaises(live.ScenarioFailure) as raised:
                    session._require_overflow(turn, [], "check words for 63/69 pages")
            self.assertEqual(raised.exception.outcome, live.MODEL)


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

        with patch.object(jobs, "run_turn", run_turn), \
                patch.object(jobs, "marker", side_effect=lambda _repo, name: name == "job-finished"
                             and finished()), \
                patch.object(jobs, "matching", side_effect=lambda *_: [] if finished() else ["x"]), \
                patch.object(jobs, "command_lines", return_value=[]), \
                patch.object(jobs, "require_script"), patch.object(jobs, "close_agent"), \
                patch.object(jobs, "stop_token"), patch.object(jobs, "job_id_for", return_value="p1"):
            try:
                return driver.steer_command()
            except live.ScenarioFailure as failure:
                return failure

    def test_a_job_that_ends_after_the_applied_steer_still_passes(self):
        self.assertEqual(self.replay([self.PROMOTED, self.APPLIED, self.DONE], finished_after=1),
                         "hint promoted the nested command; steer applied with the job still running")

    def test_the_state_at_the_applied_steer_decides(self):
        failure = self.replay([self.PROMOTED, {"kind": "status"}, self.APPLIED, self.DONE],
                              finished_after=1)
        self.assertEqual((failure.outcome, str(failure)),
                         (live.MODEL, "command finished before the steer was applied"))
        failure = self.replay([self.PROMOTED, self.DONE])
        self.assertEqual((failure.outcome, str(failure)),
                         (live.HARNESS, "steer was not applied at the outer-cell boundary"))
        # A steer only queued, never applied, is the product's: steer_command
        # now requires `applied`, not any `steer_ack`.
        failure = self.replay([self.PROMOTED, {"kind": "steer_ack", "state": "queued"}, self.DONE])
        self.assertEqual((failure.outcome, str(failure)),
                         (live.HARNESS, "steer was not applied at the outer-cell boundary"))


class JobsTurnTimeoutTests(unittest.TestCase):
    def test_a_jobs_turn_without_a_terminal_uses_the_budget_accounting(self):
        agent = live.Agent.__new__(live.Agent)
        agent.events = __import__("queue").Queue()
        agent.env, agent.capture_dir, agent.lookup_faults = None, None, None
        with patch.object(agent, "send", return_value="1"), \
                patch.object(live, "timeout_failure",
                             wraps=live.timeout_failure) as accounted:
            with self.assertRaises(live.ScenarioFailure) as raised:
                jobs.run_turn(agent, "hi", 0.05, None, lambda: False)
        self.assertEqual((raised.exception.outcome, str(raised.exception)),
                         (live.HARNESS, "no turn terminal within 0s"))
        self.assertEqual(accounted.call_args.kwargs["timeout"], 0.05)


class RunawayOutputTests(unittest.TestCase):
    """A model output the 8 MiB capture cap stopped is the model's, not an evidence gap."""

    CAP = live.CAPTURE_RESPONSE_CAP_BYTES
    TERMINAL = ("Failed(Failure { phase: ResponseBody, transmission: MayHaveTransmitted, "
                "kind: Internal, cause: None, http2_reason: None, os_error: None, tls: None, "
                "tls_alert: None, alpn: Some(\"http/1.1\"), http2_io: false, "
                "connection: Some(Fresh), idle_age: None })")

    def write(self, segment, events, *, recorded=None, terminal=None, incomplete="true"):
        body = b"".join(b"event: x\ndata: " + json.dumps(event).encode() + b"\n\n" for event in events)
        filler = {"type": "response.custom_tool_call_input.delta", "item_id": "ctc_1",
                  "delta": "*** End Patch\n"}
        line = b"event: x\ndata: " + json.dumps(filler).encode() + b"\n\n"
        body += line * ((self.CAP - len(body)) // len(line))
        body = (body + b"event: x\ndata: {\"type\":\"respo")[:self.CAP]
        body += b"x" * (self.CAP - len(body))
        (segment / "exchange-0001.request").write_bytes(b'{"input":[]}')
        (segment / "exchange-0001.body").write_bytes(body)
        (segment / "exchange-0001.info").write_text(
            "process_id=1\nexchange=1\nstatus=Some(200)\nconnection=Some(Fresh)\n"
            f"terminal={terminal or self.TERMINAL}\nrecorded_bytes={recorded or self.CAP}\n"
            f"incomplete={incomplete}\n")

    PREAMBLE = [{"type": "response.created", "response": {}},
                {"type": "response.in_progress", "response": {}},
                {"type": "response.output_item.added", "output_index": 0,
                 "item": {"id": "ctc_1", "type": "custom_tool_call", "name": "exec"}}]

    def test_a_runaway_tool_call_at_the_cap_is_model_and_the_capture_is_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write(segment, self.PREAMBLE)
            reasons = live.runaway_outputs([segment])
            self.assertEqual(reasons, [f"model output ran past the 8 MiB capture bound (runaway "
                                       f"output in custom_tool_call exec) at {segment.name} "
                                       "exchange-0001"])
            self.assertTrue(live.capture_integrity([segment]))
            aborted = ("overflow turn: failed Transport agent turn failed: Responses transport "
                       "failed: response stream interrupted [1 requests; last: unclassified]")
            self.assertEqual(live.score_runaway(live.HARNESS, aborted, [segment]),
                             (live.MODEL, reasons[0]))
            self.assertEqual(live.score_runaway(live.PROVIDER, "429", [segment]),
                             (live.PROVIDER, "429"))
            # A cancelled bodiless request after it (the continuation stopped
            # at quit) leaves the runaway the last answered exchange.
            self.add_exchange(segment, 2, b"")
            self.assertEqual(live.score_runaway(live.HARNESS, aborted, [segment])[0], live.MODEL)

    def add_exchange(self, segment, number, body):
        name = f"exchange-{number:04}"
        (segment / f"{name}.request").write_bytes(b'{"input":[]}')
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=1\nexchange={number}\nstatus=Some(200)\nconnection=Some(Fresh)\n"
            f"terminal=Complete\nrecorded_bytes={len(body)}\nincomplete=false\n")

    def test_a_runaway_does_not_relabel_an_unrelated_harness_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write(segment, self.PREAMBLE)
            for detail in ("TUI exited 1 after Ctrl+Q",
                           "prepared input reached the compaction trigger without a committed remedy",
                           "plak agent exited during a turn"):
                with self.subTest(detail):
                    self.assertEqual(live.score_runaway(live.HARNESS, detail, [segment]),
                                     (live.HARNESS, detail))
            # The failing turn ended later than the runaway: a later exchange answered.
            self.add_exchange(segment, 2, b"data: {}\n\ndata: [DONE]\n\n")
            aborted = "follow-up: failed Transport response stream interrupted"
            self.assertEqual(live.score_runaway(live.HARNESS, aborted, [segment]),
                             (live.HARNESS, aborted))
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "segment-001", Path(directory) / "segment-002"
            first.mkdir()
            second.mkdir()
            self.write(first, self.PREAMBLE)
            # The turn that failed ran in a later process, not the runaway's.
            self.add_exchange(second, 1, b"")
            aborted = "follow-up: failed Transport response stream interrupted"
            self.assertEqual(live.score_runaway(live.HARNESS, aborted, [first, second]),
                             (live.HARNESS, aborted))
            self.assertEqual(live.score_runaway(live.HARNESS, aborted, [second, first])[0],
                             live.MODEL)

    def test_every_other_incomplete_capture_stays_an_evidence_gap(self):
        second_item = {"type": "response.output_text.delta", "item_id": "msg_2", "delta": "x"}
        transport = self.TERMINAL.replace("kind: Internal", "kind: Protocol")
        for name, kwargs in (
                ("below the cap", {"recorded": self.CAP - 1}),
                ("another failure", {"terminal": transport}),
                ("two output items", {"events": [*self.PREAMBLE, second_item]}),
                ("a non-output event", {"events": [*self.PREAMBLE, {"type": "response.completed"}]}),
                ("a complete record", {"incomplete": "false"})):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                segment = Path(directory)
                events = kwargs.pop("events", self.PREAMBLE)
                self.write(segment, events, **kwargs)
                self.assertEqual(live.runaway_outputs([segment]), [])
                # A complete record is a terminal Failed exchange, not a gap.
                self.assertEqual(live.capture_integrity([segment]), name == "a complete record")
                aborted = "turn: failed Transport response stream interrupted"
                self.assertEqual(live.score_runaway(live.HARNESS, aborted, [segment]),
                                 (live.HARNESS, aborted))


class FailureLinesTests(unittest.TestCase):
    def test_the_final_line_keeps_the_error_before_it(self):
        stderr = ("plak: composition Direct\nplak: agent turn failed: provider HTTP 500\n"
                  "plak: retry summary\nplak: turn_failed:provider\n")
        self.assertEqual(live.failure_lines(stderr), "plak: agent turn failed: provider HTTP 500 | "
                                                     "plak: turn_failed:provider")
        self.assertEqual(live.failure_lines("plak: done\n"), "plak: done")
        self.assertEqual(live.failure_lines(""), "")


class NestedCtrlBPromotionTests(unittest.TestCase):
    """Code Mode Ctrl+B is judged only on a start the hint actually promoted."""

    class FakeTui:
        lookup_faults = None
        screen = b""

        def type(self, _text):
            pass

        def key(self, _sequence):
            pass

        def pump(self, _seconds):
            pass

        def wait_until(self, predicate, _timeout):
            return predicate()

        def quit(self):
            return 0

    def play(self, *, records_promoted, telemetry):
        driver = jobs.Jobs(live.ROUTES["codex"], Mock(fixture_seed=1))
        driver.prepare = Mock(return_value=(Path("repo"), Path("home")))
        driver.open_tui = Mock(return_value=self.FakeTui())
        with patch.object(jobs, "_discover", return_value="session"), \
                patch.object(jobs, "marker", side_effect=lambda _repo, name: name == "job-started"), \
                patch.object(jobs, "matching", return_value=["hold"]), \
                patch.object(jobs, "command_lines", return_value=[]), \
                patch.object(jobs, "tool_completed_count", return_value=1), \
                patch.object(jobs, "all_session_records", return_value=[]), \
                patch.object(jobs, "command_promoted", return_value=records_promoted), \
                patch.object(jobs, "read_telemetry", return_value=(telemetry, [])), \
                patch.object(jobs, "require_script"), patch.object(jobs, "stop_token"):
            try:
                return driver.ctrl_b()
            except live.ScenarioFailure as failure:
                return failure

    def test_a_promoted_start_passes_from_the_session_log_or_telemetry(self):
        passed = "hint promoted the nested command; Ctrl+B left it running"
        self.assertEqual(self.play(records_promoted=True, telemetry=[]), passed)
        self.assertEqual(self.play(records_promoted=None, telemetry=[
            {"kind": "promoted", "tool": "start", "hint_source": "model_hint", "hint_ms": 250}]),
            passed)

    def test_a_start_the_hint_never_promoted_is_model(self):
        for records_promoted in (False, None):
            with self.subTest(records_promoted=records_promoted):
                failure = self.play(records_promoted=records_promoted, telemetry=[])
                self.assertEqual((failure.outcome, str(failure)),
                                 (live.MODEL, "nested command was not promoted by its hint"))

