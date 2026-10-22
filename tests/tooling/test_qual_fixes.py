"""Offline regressions for the qualification drivers; no provider or binary."""

import io
import itertools
import json
import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import live  # noqa: E402
import long_session  # noqa: E402
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
