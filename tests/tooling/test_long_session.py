"""Offline checks for the opt-in long-session driver. No provider and no network."""

import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import live  # noqa: E402
import long_session  # noqa: E402
import smoke  # noqa: E402
import soak  # noqa: E402

FAKE_PLAK = r'''#!/usr/bin/env python3
import json
import sys
import time
from pathlib import Path

argv = sys.argv[1:]
if argv[:1] == ["session"] and len(argv) >= 3 and argv[1] in ("show", "log"):
    address = argv[2]
    root, dot, child = address.partition(".")
    if argv[1] == "show" and not child:
        sys.stdout.write(
            f"Session {root}\n"
            "\nTopology children (durable snapshot)\n"
            "  Child          c1 node=Completed attempt=Completed\n"
        )
        raise SystemExit(0)
    if argv[1] == "log" and child == "c1" and "--json" in argv:
        # The real projection: a compaction segment is kind `compaction`
        # with the short phrase, not an empty assistant item. A later
        # assistant item is ordinary text plus a tool call.
        rows = [
            {"entry_id": "11111111-1111-4111-8111-111111111111",
             "kind": "compaction",
             "text": "compacted 4 earlier segments (semantic summary)",
             "tool_calls": 0, "tool_results": 0},
            {"entry_id": "22222222-2222-4222-8222-222222222222",
             "kind": "user", "text": "page bulk.txt", "tool_calls": 0, "tool_results": 0},
            {"entry_id": "33333333-3333-4333-8333-333333333333",
             "kind": "assistant", "text": "done", "tool_calls": 1, "tool_results": 0},
        ]
        for row in rows:
            sys.stdout.write(json.dumps(row) + "\n")
        raise SystemExit(0)
    sys.stderr.write("unknown child in session tree\n")
    raise SystemExit(1)

home = Path(__import__("os").environ["PLAK_HOME"])
session = "long-root"
if "--session" in argv:
    session = argv[argv.index("--session") + 1]
log_dir = home / "state" / "r-test" / "sessions"
log_dir.mkdir(parents=True, exist_ok=True)
log_path = log_dir / f"{session}.events.jsonl"
repo = Path.cwd()


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def log(record):
    with log_path.open("a") as handle:
        handle.write(json.dumps(record) + "\n")


def reply(command_id, events):
    emit({"v": 1, "kind": "busy", "id": command_id, "busy": True})
    for event in events:
        event = dict(event)
        event["id"] = command_id
        emit(event)
    emit({"v": 1, "kind": "busy", "id": command_id, "busy": False})


def token():
    path = repo / "codeword.txt"
    return path.read_text().strip() if path.is_file() else ""


def lifecycle(turn_id, phase, **extra):
    return {"v": 1, "kind": "turn_lifecycle", "phase": phase, "turn_id": turn_id, **extra}


emit({"v": 1, "kind": "hello", "session_id": session})
if "--session" in argv and (home / "killed").is_file():
    emit({"v": 1, "kind": "recovered", "claim": "running_tool",
          "cause": "process_lost", "operations": [1]})
    log({"kind": "turn.abandoned", "claim": "running_tool", "cause": "process_lost"})
emit({"v": 1, "kind": "ready"})

for line in sys.stdin:
    message = json.loads(line)
    kind = message.get("kind")
    if kind == "quit":
        emit({"v": 1, "kind": "bye", "id": message.get("id")})
        break
    if kind != "prompt":
        continue
    text = message.get("text") or ""
    command_id = message["id"]
    if "plak-long-kill-" in text:
        import re as _re
        import subprocess as _subprocess
        matched = _re.search(r"plak-long-kill-[0-9a-f]+", text)
        sleeper = matched.group(0) if matched else "plak-long-kill-offline"
        # The driver kills only after this argv is visible, so spawn first.
        _subprocess.Popen(["python3", "-c", "import time; time.sleep(45)", sleeper])
        (home / "killed").write_text("1\n")
        emit({"v": 1, "kind": "busy", "id": command_id, "busy": True})
        emit({"v": 1, "kind": "tool", "id": command_id, "name": "exec_command",
              "state": "running", "activity_kind": "process", "subject": text})
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "kill"})
        log({"kind": "tool.started", "name": "exec_command", "subject": text})
        time.sleep(300)
        break
    if "Spawn one worker child" in text:
        (repo / "acknowledged.txt").write_text(token() + "\n")
        (repo / "recalled.txt").write_text(token() + "\n")
        (repo / "pages").mkdir(exist_ok=True)
        for line in (repo / "bulk.txt").read_text().splitlines():
            if line.startswith("Page "):
                index = line.split()[1]
                word = line.split("Check word: ", 1)[1].split(".", 1)[0]
                (repo / "pages" / f"{index.rstrip('.')}.txt").write_text(word + "\n")
        reply(command_id, [
            {"v": 1, "kind": "tree", "nodes": [
                {"depth": 0, "agent_id": "root", "status": "running"},
                {"depth": 1, "agent_id": "c1", "parent_agent_id": "root",
                 "status": "completed"},
            ]},
            {"v": 1, "kind": "tool", "name": "wait_agent", "state": "succeeded",
             "activity_kind": "agent", "subject": "c1"},
            lifecycle("root", "admitted"),
            lifecycle("root", "completed"),
        ])
        continue
    if "parallel batch" in text:
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "over"})
        log({"kind": "turn.lifecycle", "phase": "failed", "turn_id": "over"})
        log({"kind": "compact.started", "reason": "capacity"})
        log({"kind": "compact.finished", "outcome": "committed",
             "input_tokens_before": 400000, "input_tokens_after": 9000})
        reply(command_id, [
            lifecycle("over", "admitted"),
            lifecycle("over", "failed", model_error_kind="ContextLimit"),
        ])
        continue
    if "after_reopen.txt" in text:
        (repo / "after_reopen.txt").write_text("REOPENED\n")
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "follow"})
        log({"kind": "turn.lifecycle", "phase": "completed", "turn_id": "follow"})
        reply(command_id, [lifecycle("follow", "admitted"), lifecycle("follow", "completed")])
        continue
    if "continuation.txt" in text:
        (repo / "continuation.txt").write_text("CONTINUED\n")
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "next"})
        log({"kind": "turn.lifecycle", "phase": "completed", "turn_id": "next"})
        reply(command_id, [lifecycle("next", "admitted"), lifecycle("next", "completed")])
        continue
    if "followup.txt" in text:
        (repo / "followup.txt").write_text(token() + "\n")
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "follow"})
        log({"kind": "turn.lifecycle", "phase": "completed", "turn_id": "follow"})
        reply(command_id, [lifecycle("follow", "admitted"), lifecycle("follow", "completed")])
        continue
    if "recalled.txt" in text and "bulk.txt" in text:
        (repo / "recalled.txt").write_text(token() + "\n")
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "grow"})
        for _ in range(4):
            log({"kind": "tool.started", "name": "read_file", "subject": "bulk.txt"})
        log({"kind": "compact.started", "reason": "scheduled"})
        log({"kind": "compact.finished", "outcome": "committed",
             "input_tokens_before": 200000, "input_tokens_after": 8000})
        log({"kind": "turn.lifecycle", "phase": "completed", "turn_id": "grow"})
        reply(command_id, [lifecycle("grow", "admitted"), lifecycle("grow", "completed")])
        continue
    if "acknowledged.txt" in text and "codeword.txt" in text:
        (repo / "acknowledged.txt").write_text(token() + "\n")
        log({"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "plant"})
        log({"kind": "tool.started", "name": "read_file", "subject": "codeword.txt"})
        log({"kind": "turn.lifecycle", "phase": "completed", "turn_id": "plant"})
        reply(command_id, [lifecycle("plant", "admitted"), lifecycle("plant", "completed")])
        continue
    reply(command_id, [lifecycle("unknown", "failed", model_error_kind="Protocol")])
'''


class LongSessionOracleTests(unittest.TestCase):
    def test_harness_requires_prepared_input_at_the_window_minus_one_page(self):
        window = long_session.catalog_window("grok-4.7")
        trigger = window - long_session.PAGE_TOKENS
        under = [
            {"kind": "model.operation_usage", "prepared_input_tokens": trigger - 1,
             "peak_prepared_input_tokens": trigger - 1},
            {"kind": "tool.started", "name": "read_file", "subject": "bulk.txt (1-2000)"},
        ]
        self.assertFalse(long_session.reached_trigger(under, window))
        at = [{"kind": "model.operation_usage", "prepared_input_tokens": trigger,
                "peak_prepared_input_tokens": trigger}]
        self.assertTrue(long_session.reached_trigger(at, window))
        self.assertFalse(long_session.reached_trigger(at, None))
        self.assertGreater(long_session.compaction_trigger(window), trigger)
        self.assertEqual(long_session.harness_bar(window), trigger)
        self.assertIsNone(long_session.prepared_peak([
            {"kind": "turn.usage", "peak_prepared_input_tokens": trigger}]))

    def test_peak_between_the_trigger_and_one_page_under_the_window_is_harness(self):
        window = long_session.catalog_window("claude-sonnet-5")
        self.assertEqual(window, 1_000_000)
        trigger = long_session.compaction_trigger(window)
        page_bar = window - long_session.PAGE_TOKENS
        self.assertLess(trigger, page_bar)
        between = (trigger + page_bar) // 2
        rows = [{"kind": "model.operation_usage", "prepared_input_tokens": between,
                 "peak_prepared_input_tokens": between}]
        self.assertTrue(long_session.reached_trigger(rows, window))
        self.assertEqual(long_session.harness_bar(window), trigger)
        under = [{"kind": "model.operation_usage", "prepared_input_tokens": trigger - 1,
                  "peak_prepared_input_tokens": trigger - 1}]
        self.assertFalse(long_session.reached_trigger(under, window))
        grok46 = long_session.catalog_window("grok-4.6")
        self.assertEqual(long_session.compaction_trigger(grok46, "grok-4.6"), (grok46 * 80) // 100)
        self.assertLess(
            long_session.harness_bar(grok46, "grok-4.6"), grok46 - long_session.PAGE_TOKENS)

    def test_default_pages_follow_the_route_window_and_the_capture_cap(self):
        pages = {name: long_session.default_pages(route) for name, route in live.ROUTES.items()}
        self.assertEqual(pages["grok"], 6)
        self.assertEqual(pages["codex"], 6)
        self.assertEqual(pages["api-openai"], 6)
        self.assertEqual(pages["api-openai-medium"], 6)
        self.assertEqual(pages["api-xai"], 6)
        self.assertEqual(pages["api-anthropic"], 16)
        self.assertEqual(pages["claude-sonnet55-low"], 16)
        self.assertEqual(pages["deepseek"], 6)
        self.assertEqual(pages["deepseek-messages"], 6)
        self.assertEqual(pages["grok-46"], 8)
        self.assertTrue(all(1 <= value <= long_session.MAX_GROWTH_PAGES for value in pages.values()))
        self.assertEqual(long_session.growth_timeout(6), 900)
        self.assertEqual(long_session.child_timeout(6), 1500)
        self.assertGreater(long_session.growth_timeout(16), 900)
        self.assertGreater(long_session.child_timeout(16), 1500)

    def test_openrouter_window_comes_from_explicit_admitted_ceiling(self):
        with patch.dict(os.environ, {}, clear=True):
            self.assertIsNone(long_session.catalog_window("deepseek/model"))
        for ceiling in (32_768, 131_072, 967_232):
            with patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": str(ceiling)}):
                self.assertEqual(long_session.catalog_window("deepseek/model"), ceiling)
        for value in ("0", "-1", "invalid"):
            with patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": value}):
                with self.assertRaises(live.ScenarioFailure):
                    long_session.catalog_window("deepseek/model")

    def test_a_small_window_takes_small_calls_and_a_window_step_bar(self):
        with patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": "65536"}):
            window = long_session.catalog_window("deepseek/deepseek-v4.1-flash")
        self.assertEqual(window, 65_536)
        self.assertTrue(long_session.small_window(window))
        self.assertFalse(long_session.small_window(long_session.catalog_window("grok-4.7")))
        lines = long_session.growth_call_lines("pi", window)
        self.assertEqual(lines, 5)
        self.assertLessEqual(lines * long_session.LINE_BYTES, window // 8)
        self.assertEqual(long_session.growth_call_lines("pi", 256_000),
                         long_session.GROWTH_CALL_LINES)
        self.assertEqual(long_session.growth_call_lines("claude", 1_000_000),
                         long_session.CLAUDE_GROWTH_CALL_LINES)
        calls = long_session.pages_for_window(window) * long_session.lines_per_page() / lines
        self.assertLess(calls, long_session.ROUND_CAP)
        self.assertEqual(long_session.harness_bar(window),
                         long_session.compaction_trigger(window))
        self.assertIn("limit 5", long_session.growth_prompt(348, "pi", lines))
        self.assertIn("limit 6", long_session.child_prompt("pi", lines))

    def test_claude_growth_reads_digit_groups_and_other_packs_keep_base64(self):
        line = long_session.digit_line()
        self.assertEqual(len(line), len(long_session.base64_line()))
        self.assertLess(len(line), long_session.LINE_BYTES)
        self.assertTrue(set(line) <= set("0123456789 "))
        with tempfile.TemporaryDirectory() as directory:
            claude = Path(directory) / "claude.txt"
            grok = Path(directory) / "grok.txt"
            count = long_session.write_bulk(claude, 1, "claude")
            self.assertEqual(count, long_session.lines_per_page())
            self.assertEqual(long_session.write_bulk(grok, 1, "grok"), count)
            self.assertTrue(set(claude.read_text()) <= set("0123456789 \n"))
            self.assertTrue(any(char.isalpha() for char in grok.read_text()))
        self.assertEqual(long_session.growth_call_lines("claude"),
                         long_session.CLAUDE_GROWTH_CALL_LINES)
        for pack in ("grok", "codex", "pi"):
            self.assertEqual(long_session.growth_call_lines(pack), long_session.GROWTH_CALL_LINES)
        claude_prompt = long_session.growth_prompt(4, "claude")
        self.assertIn(f"limit {long_session.CLAUDE_GROWTH_CALL_LINES}", claude_prompt)
        self.assertIn(f"limit {long_session.GROWTH_CALL_LINES}", long_session.growth_prompt(4))
        self.assertIn(f"limit {long_session.CLAUDE_GROWTH_CALL_LINES + 1}",
                      long_session.child_prompt("claude"))

    def test_any_tool_that_names_a_fact_file_is_a_reread(self):
        records = [
            {"kind": "turn.lifecycle", "phase": "admitted"},
            {"kind": "tool.started", "name": "grep", "subject": "codeword.txt"},
            {"kind": "compact.finished", "outcome": "committed"},
            {"kind": "tool.started", "name": "run_terminal_command",
             "subject": "cat acknowledged.txt"},
            {"kind": "tool.started", "name": "glob", "subject": "**/acknowledged.txt"},
            {"kind": "turn.lifecycle", "phase": "completed"},
        ]
        reread = long_session.reread_after_commit(records, 0, len(records))
        self.assertEqual([record["name"] for record in reread],
                         ["run_terminal_command", "glob"])

    def test_overflow_remedy_accepts_each_product_path(self):
        scheduled = [
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "over"},
            {"kind": "tool.started", "name": "read_file", "subject": "bulk.txt (1-2000)"},
            {"kind": "compact.started", "reason": "scheduled"},
            {"kind": "compact.finished", "outcome": "committed",
             "input_tokens_before": 10, "input_tokens_after": 3},
            {"kind": "turn.lifecycle", "phase": "completed", "turn_id": "over"},
        ]
        self.assertEqual(long_session.overflow_remedy(scheduled), "in-turn overflow compact")
        placeholders = [
            {"kind": "turn.lifecycle", "phase": "admitted"},
            {"kind": "tool.started", "name": "read_file", "subject": "bulk.txt"},
            {"kind": "context.placeholders", "results": 6},
            {"kind": "turn.lifecycle", "phase": "completed"},
        ]
        self.assertEqual(long_session.overflow_remedy(placeholders),
                         "tool-result placeholder shrink")
        # The host logs capacity, then the kernel logs the same compact as manual.
        capacity = [
            {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "over"},
            {"kind": "compact.started", "reason": "capacity"},
            {"kind": "compact.started", "reason": "manual"},
            {"kind": "compact.finished", "outcome": "committed",
             "input_tokens_before": 10, "input_tokens_after": 3},
        ]
        self.assertEqual(long_session.overflow_remedy(capacity), "capacity-recovery compact")
        self.assertIsNone(long_session.overflow_remedy([
            {"kind": "compact.started", "reason": "capacity"},
            {"kind": "compact.finished", "outcome": "failed"},
        ]))

    def test_scheduled_compaction_inside_a_turn_counts_and_manual_does_not(self):
        records = [
            {"kind": "compact.started", "reason": "manual"},
            {"kind": "compact.finished", "outcome": "committed"},
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "grow"},
            {"kind": "tool.started", "name": "read_file", "subject": "bulk.txt"},
            {"kind": "compact.started", "reason": "scheduled"},
            {"kind": "compact.finished", "outcome": "committed",
             "input_tokens_before": 10, "input_tokens_after": 3},
            {"kind": "turn.lifecycle", "phase": "completed", "turn_id": "grow"},
            {"kind": "compact.started", "reason": "capacity"},
        ]
        self.assertEqual(len(long_session.in_turn_compactions(records)), 1)
        self.assertEqual(long_session.capacity_compactions(records)[0]["reason"], "capacity")
        self.assertEqual(len(long_session.bulk_windows(records)), 1)
        long_session.assert_shrunk(records[5])

    def test_a_larger_successor_and_a_post_cut_reread_fail(self):
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.assert_shrunk(
                {"input_tokens_before": 3, "input_tokens_after": 3})
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        records = [
            {"kind": "turn.lifecycle", "phase": "admitted"},
            {"kind": "tool.started", "name": "read_file", "subject": "codeword.txt"},
            {"kind": "compact.finished", "outcome": "committed"},
            {"kind": "tool.started", "name": "read_file", "subject": "codeword.txt"},
            {"kind": "turn.lifecycle", "phase": "completed"},
        ]
        self.assertEqual(long_session.reread_after_commit(records, 0, 5)[0]["subject"], "codeword.txt")
        self.assertEqual(long_session.reread_after_commit(records, 0, 2), [])

    def test_prompts_do_not_carry_the_fact_token(self):
        token = "ZEBRA-123456"
        for prompt in (long_session.growth_prompt(10), long_session.follow_prompt(),
                       long_session.overflow_prompt(20, [[1, 21], [41, 61]]), long_session.child_prompt(),
                       long_session.plant_prompt()):
            self.assertNotIn(token, prompt)
        long_session.hide_token(long_session.growth_prompt(4), token)
        with self.assertRaises(live.ScenarioFailure):
            long_session.hide_token(f"remember {token}", token)

    def test_growth_pages_rejects_a_bad_driver_dial(self):
        with patch.dict(os.environ, {"PLAK_LIVE_GROWTH_PAGES": "nope"}):
            with self.assertRaises(live.ScenarioFailure):
                long_session.growth_pages()
        with patch.dict(os.environ, {"PLAK_LIVE_GROWTH_PAGES": "0"}):
            with self.assertRaises(live.ScenarioFailure):
                long_session.growth_pages()
        with patch.dict(os.environ, {"PLAK_LIVE_GROWTH_PAGES": "29"}):
            with self.assertRaises(live.ScenarioFailure):
                long_session.growth_pages(live.ROUTES["claude-sonnet55-low"])

    def test_default_smoke_does_not_include_long_sessions(self):
        names = [name for name, _surface in smoke.Smoke.SCENARIOS]
        for name, _surface in long_session.CATALOG:
            self.assertNotIn(name, names)
        selected = [name for name, _surface in smoke.selected_scenarios({"in_turn_compaction"})]
        self.assertEqual(selected, ["in_turn_compaction"])
        self.assertEqual(smoke.selected_scenarios(set()), smoke.Smoke.SCENARIOS)

    def test_driver_refuses_to_launch_without_a_named_scenario(self):
        self.assertEqual(long_session.main(["grok"]), 2)
        self.assertEqual(long_session.main(["grok", "--only", "missing"]), 2)
        with self.assertRaises(SystemExit) as caught:
            long_session.main(["grok", "--all", "--only", "overflow"])
        self.assertEqual(caught.exception.code, 2)
        names = [name for name, _surface in long_session.selected_catalog("", True)]
        self.assertEqual(names, [
            "in_turn_compaction", "overflow", "kill_reopen", "child_compaction",
        ])
        only = [name for name, _surface in long_session.selected_catalog("overflow,kill_reopen", False)]
        self.assertEqual(only, ["overflow", "kill_reopen"])
        with patch.object(long_session, "main", return_value=7) as delegated:
            self.assertEqual(soak.main(["grok", "--long-session", "--only", "overflow"]), 7)
            self.assertEqual(soak.main(["grok", "--long-session", "--all"]), 7)
        self.assertEqual(delegated.call_args_list[0].args[0], ["grok", "--only", "overflow"])
        self.assertEqual(delegated.call_args_list[1].args[0], ["grok", "--all"])

    def test_smoke_counts_tokens_from_a_long_session_private_home(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            private = home / "private"
            log = private / "state" / "r" / "sessions"
            log.mkdir(parents=True)
            (log / "s.events.jsonl").write_text(
                json.dumps({"kind": "turn.usage", "input_tokens": 11, "output_tokens": 4,
                            "cached_input_tokens": 2, "reasoning_tokens": 1}) + "\n")
            usage = smoke.scenario_usage(home / "missing", private, live.TokenUsage())
        self.assertEqual((usage.input_tokens, usage.output_tokens,
                          usage.cached_input_tokens, usage.reasoning_tokens), (11, 4, 2, 1))

    def test_report_names_the_route_and_counts_private_tokens(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "plak"
            binary.write_bytes(b"synthetic executable; never launched")

            def fake_play(_route, env, name):
                private = env.artifacts / name / "home" / "state" / "r" / "sessions"
                private.mkdir(parents=True)
                (private / "s.events.jsonl").write_text(
                    json.dumps({"kind": "turn.usage", "input_tokens": 9, "output_tokens": 3,
                                "cached_input_tokens": 1, "reasoning_tokens": 2}) + "\n")
                # What a Codex `plak agent` hello reports for plakous-default.
                env.bound_compositions.add("native-ptc")
                return "synthetic pass"

            with patch.object(live, "ROOT", root), \
                    patch.object(live, "write_home"), \
                    patch.object(smoke, "capture_complete", return_value=True), \
                    patch.object(long_session, "play", fake_play), \
                    patch.dict(os.environ, {"PLAK_LIVE_GROWTH_PAGES": ""}):
                code = long_session.main(["grok", "--only", "overflow", "--binary", str(binary)])
            report = json.loads(next((root / "target/live").glob("*/report.json")).read_text())
        route = live.ROUTES["grok"]
        self.assertEqual(code, 0)
        self.assertEqual(report["route_class"], route.route_class)
        self.assertEqual(report["account"], route.account)
        self.assertEqual(report["effort"], route.effort)
        self.assertIsNone(report["protocol"])
        self.assertEqual(report["composition_requested"], "plakous-default")
        self.assertEqual(report["composition_bound"], ["native-ptc"])
        self.assertEqual(report["growth_pages"], 6)
        self.assertEqual(report["input_ceiling"], {"tokens": 256_000, "source": "driver catalog"})
        self.assertEqual(report["token_telemetry"]["input_tokens"], 9)
        self.assertEqual(report["token_telemetry"]["reasoning_tokens"], 2)
        self.assertEqual(report["results"][0]["outcome"], "pass")

    def test_openrouter_driver_refuses_before_launch_without_an_input_ceiling(self):
        for ceiling in (None, "0"):
            with self.subTest(ceiling=ceiling), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary = root / "plak"
                binary.write_bytes(b"synthetic executable; never launched")
                environ = {} if ceiling is None else {"PLAK_LIVE_INPUT_CEILING": ceiling}
                with patch.object(live, "ROOT", root), \
                        patch.object(live, "write_home") as launch, \
                        patch.object(long_session, "play") as play, \
                        patch.dict(os.environ, environ, clear=True), \
                        patch("builtins.print"):
                    code = long_session.main(["deepseek-messages", "--all", "--binary", str(binary)])
                report = json.loads(next((root / "target/live").glob("*/report.json")).read_text())
            self.assertEqual(code, 2)
            launch.assert_not_called()
            play.assert_not_called()
            self.assertIn("plak usage --session", report["pre_attempt_error"])
            self.assertIn("admitted_input_ceiling=N", report["pre_attempt_error"])
            self.assertEqual(report["input_ceiling"], {"tokens": None, "source": None})
            self.assertEqual(report["protocol"], "messages")
            self.assertEqual(report["results"], [])

    def test_report_records_the_input_ceiling_and_its_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "plak"
            binary.write_bytes(b"synthetic executable; never launched")
            with patch.object(live, "ROOT", root), \
                    patch.object(live, "write_home"), \
                    patch.object(smoke, "capture_complete", return_value=True), \
                    patch.object(long_session, "play", return_value="synthetic pass"), \
                    patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": "967232",
                                            "PLAK_LIVE_GROWTH_PAGES": ""}), \
                    patch("builtins.print"):
                code = long_session.main(["deepseek", "--only", "overflow", "--binary", str(binary)])
            report = json.loads(next((root / "target/live").glob("*/report.json")).read_text())
        self.assertEqual(code, 0)
        self.assertEqual(report["input_ceiling"],
                         {"tokens": 967_232, "source": "PLAK_LIVE_INPUT_CEILING"})

    def test_bad_dial_is_a_scenario_result(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "plak"
            binary.write_bytes(b"synthetic executable; never launched")
            with patch.object(live, "ROOT", root), \
                    patch.object(live, "write_home"), \
                    patch.dict(os.environ, {"PLAK_LIVE_GROWTH_PAGES": "29"}):
                code = long_session.main(["grok", "--only", "overflow", "--binary", str(binary)])
            report = json.loads(next((root / "target/live").glob("*/report.json")).read_text())
        self.assertEqual(code, 1)
        self.assertEqual(report["results"][0]["outcome"], "harness")
        self.assertIn("1..=28", report["results"][0]["detail"])
        self.assertIsNone(report["growth_pages"])

    def test_child_tool_failure_is_surfaced(self):
        self.assertIsNone(long_session.child_tool_failure([
            {"kind": "tree", "nodes": [{"depth": 1, "agent_id": "c1", "status": "running"}]},
        ]))
        retried = [
            {"kind": "tool", "name": "adopt_candidate", "state": "failed",
             "subject": "cand-c1-a1", "detail": '{"code": "target_changed"}'},
            {"kind": "tool", "name": "adopt_candidate", "state": "done",
             "subject": "cand-c1-a1"},
        ]
        self.assertIsNone(long_session.child_tool_failure(retried))
        self.assertIn("cand-c1-a1", long_session.child_tool_failure(retried[:1]))
        spawn_retried = [
            {"kind": "tool", "name": "spawn_agent", "state": "failed", "subject": "spawn"},
            {"kind": "tool", "name": "spawn_agent", "state": "done", "subject": "spawn"},
        ]
        self.assertEqual(long_session.child_tool_failure(spawn_retried), "spawn")
        text = long_session.child_tool_failure([
            {"kind": "tool", "state": "failed", "subject": "text projection result violates its raised V2 contract"},
        ])
        self.assertIn("V2 contract", text)
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.raise_child_tool_failure(text)
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        self.assertIn("child tool failure", str(raised.exception))
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.raise_child_tool_failure(
                "agent turn failed: response reached its output bound before completion")
        self.assertEqual(raised.exception.outcome, live.MODEL)

    def test_blocked_child_quota_is_provider(self):
        blocked = {
            "account": "openrouter",
            "quota": "rate_limited",
            "resets_at": 1700000000,
        }
        tree = {
            "kind": "tree",
            "nodes": [{
                "depth": 1,
                "agent_id": "c1",
                "status": "failed",
                "attempt": "blocked",
                "blocked": blocked,
            }],
        }
        receipt = {
            "agents": [{
                "agent_id": "c1",
                "status": "failed",
                "attempt": "blocked",
                "blocked": blocked,
            }],
            "mail": [],
            "timed_out": False,
            "woke_by": "settled",
        }
        wrapped = {
            "kind": "tool",
            "name": "wait_agent",
            "state": "done",
            "detail": json.dumps(receipt, indent=2),
        }
        for events in ([tree], [receipt], [wrapped]):
            text = long_session.child_tool_failure(events)
            self.assertIn("rate_limited", text)
            self.assertIn("openrouter", text)
            with self.assertRaises(live.ScenarioFailure) as raised:
                long_session.raise_child_tool_failure(text)
            self.assertEqual(raised.exception.outcome, live.PROVIDER)

        quota = {
            "kind": "tree",
            "nodes": [{
                "depth": 1, "agent_id": "c1", "status": "failed", "attempt": "blocked",
                "blocked": {"account": "openrouter", "quota": "quota_exceeded", "resets_at": 1700000000},
            }],
        }
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.raise_child_tool_failure(long_session.child_tool_failure([quota]))
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

        unavailable = {
            "kind": "tree",
            "nodes": [{
                "depth": 1, "agent_id": "c1", "status": "failed",
                "fault": "provider unavailable",
            }],
        }
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.raise_child_tool_failure(long_session.child_tool_failure([unavailable]))
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

        other = {
            "kind": "tree",
            "nodes": [{
                "depth": 1, "agent_id": "c1", "status": "failed", "attempt": "failed",
                "fault": "bind failed",
                "blocked": {"account": "codex", "quota": "weekly", "resets_at": 1700000000},
            }],
        }
        text = long_session.child_tool_failure([other])
        self.assertIn("weekly", text)
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.raise_child_tool_failure(text)
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        self.assertIsNone(long_session.child_tool_failure([{
            "agents": [{
                "agent_id": "c1", "status": "completed", "attempt": "completed",
            }],
            "woke_by": "settled",
        }]))
        unavailable_quota = {
            "agents": [{
                "agent_id": "c1",
                "status": "failed",
                "attempt": "blocked",
                "blocked": {"account": "openrouter", "quota": "unavailable", "resets_at": 1700000000},
            }],
        }
        raw = json.dumps(unavailable_quota)
        self.assertFalse(live.text_is_provider_refusal(raw))
        self.assertTrue(live.provider_child_text(raw))
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.raise_child_tool_failure(long_session.child_tool_failure([unavailable_quota]))
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        self.assertFalse(long_session.is_committed_compaction_projection(
            {"kind": "history_snapshot", "head": "h", "status": "Working",
             "text": "", "tool_calls": 0, "tool_results": 0}))
        codex = long_session.growth_prompt(40, "codex")
        self.assertIn("exec_command", codex)
        self.assertNotIn("read_file", codex)
        self.assertIn(str(long_session.GROWTH_CALL_LINES), codex)
        self.assertNotIn("2000", long_session.growth_prompt(40))
        self.assertNotIn("2000", long_session.overflow_prompt(20, [[1, 21]]))

    @patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": "131072"})
    def test_overflow_rounds_stay_under_the_round_cap_and_reach_the_trigger(self):
        """No batch is clipped by the observation cap, and together they pass the trigger."""
        for name in ("codex", "grok", "claude-sonnet55-low", "deepseek", "grok-46"):
            route = live.ROUTES[name]
            window = long_session.catalog_window(route.model)
            pack = long_session.pack_of(route)
            line_count = long_session.default_pages(route) * long_session.lines_per_page()
            lines, batches = long_session.overflow_plan(window, pack, line_count, route.model)
            calls = len(batches[0])
            batch_bytes = calls * lines * long_session.LINE_BYTES
            self.assertLessEqual(batch_bytes, long_session.round_cap_bytes(window), name)
            self.assertGreaterEqual(calls, long_session.OVERFLOW_BATCH_CALLS, name)
            self.assertTrue(all(len(batch) == calls for batch in batches), name)
            offsets = [offset for batch in batches for offset in batch]
            self.assertEqual(offsets, sorted(set(offsets)), name)
            self.assertLessEqual(offsets[-1] + lines - 1, line_count, name)
            per_token = long_session.bulk_bytes_per_token(pack)
            self.assertGreater(len(batches) * batch_bytes / per_token,
                               long_session.compaction_trigger(window, route.model), name)
            call_lines = long_session.growth_call_lines(pack, window)
            reads = long_session.growth_reads(window, pack, call_lines, line_count, route.model)
            self.assertLessEqual(reads * call_lines, line_count, name)
            self.assertGreater(reads * call_lines * long_session.LINE_BYTES / per_token,
                               long_session.compaction_trigger(window, route.model), name)
        self.assertEqual(long_session.round_cap_bytes(1_000_000), 262_144)
        self.assertEqual(long_session.round_cap_bytes(65_536), 32_768)
        self.assertEqual(long_session.round_cap_bytes(272_000), 87_040)
        prompt = long_session.overflow_prompt(21, [[1, 22], [43, 64]], "codex")
        self.assertIn("in exactly 4 sequential calls of 21 lines each", prompt)
        self.assertIn("exec_command", prompt)
        self.assertIn("Round 2: offsets 41, 61.",
                      long_session.overflow_prompt(20, [[1, 21], [41, 61]], "grok"))
        self.assertNotIn("read_file", prompt)

    def test_every_planned_read_fits_its_tool_call_cap_and_the_plan_reaches_the_trigger(self):
        """pi `read` stops at 50 KiB, so a pi batch takes more, shorter reads.

        Measured with the bytes a bulk line really has, every read of every
        plan returns whole, every batch stays within the cap share, the file
        holds the plan, and the batches before the last pass the trigger.
        """
        real_line = {pack: len(long_session.bulk_line(pack)) + 1
                     for pack in ("pi", "grok", "claude", "codex")}
        self.assertEqual(long_session.read_call_bytes("pi"), 50 * 1024)
        self.assertEqual(long_session.read_call_bytes("codex"), 40_000)
        self.assertEqual(long_session.read_call_bytes("grok"), long_session.READ_PAGE_BYTES)
        cases = [("pi", window, "deepseek/deepseek-v4.1-flash-1017")
                 for window in (*range(65_536, 967_232, 32_768), 967_232)]
        cases += [("claude", 1_000_000, "claude-sonnet-5-5"), ("grok", 256_000, "grok-4.7"),
                  ("grok", 500_000, "grok-4.6"), ("codex", 272_000, "gpt-6-luna"),
                  ("codex", 500_000, "gpt-6.1-sol")]
        for pack, window, model in cases:
            label = (pack, window)
            with patch.dict(os.environ, {"PLAK_LIVE_INPUT_CEILING": str(window)}):
                route_pages = long_session.pages_for_window(window)
            pages = max(route_pages, long_session.overflow_bulk_pages(window, pack, model))
            line_count = pages * long_session.lines_per_page()
            lines, batches = long_session.overflow_plan(window, pack, line_count, model)
            self.assertEqual((lines, batches),
                             long_session.overflow_plan(window, pack, None, model), label)
            self.assertLessEqual(lines, long_session.read_call_lines(pack), label)
            self.assertLessEqual(lines * real_line[pack], long_session.read_call_bytes(pack), label)
            calls = len(batches[0])
            self.assertLessEqual(calls * lines * long_session.LINE_BYTES,
                                 long_session.round_cap_bytes(window)
                                 * long_session.OVERFLOW_CAP_SHARE, label)
            offsets = [offset for batch in batches for offset in batch]
            self.assertEqual(offsets, list(range(1, 1 + len(offsets) * lines, lines)), label)
            self.assertLessEqual(offsets[-1] + lines - 1, line_count, label)
            round_tokens = calls * lines * real_line[pack] / long_session.bulk_bytes_per_token(pack)
            self.assertGreater((len(batches) - 1) * round_tokens,
                               long_session.compaction_trigger(window, model), label)
            self.assertGreaterEqual(len(batches) * round_tokens,
                                    long_session.harness_bar(window, model), label)
            # Growth and child reads stay under the same per-call cap.
            call_lines = long_session.growth_call_lines(pack, window)
            self.assertLessEqual(call_lines, long_session.read_call_lines(pack), label)
            child_read = (long_session.CODEX_CHILD_HALF_LINES if pack == "codex"
                          else long_session.child_page_lines(pack, call_lines) + 1)
            self.assertLessEqual(child_read, long_session.read_call_lines(pack), label)
        # The measured DeepSeek ceiling: four 32-line reads per round, not two
        # 65-line reads the 50 KiB cap would cut at 34 lines.
        lines, batches = long_session.overflow_plan(
            967_232, "pi", None, "deepseek/deepseek-v4.1-flash-1017")
        self.assertEqual((lines, len(batches[0]), len(batches)), (32, 4, 7))
        prompt = long_session.overflow_prompt(lines, batches, "pi")
        self.assertIn("in 7 rounds using only the read tool", prompt)
        self.assertIn("Round 1: offsets 1, 33, 65, 97.", prompt)
        self.assertIn("each with limit 32", prompt)
        self.assertIn("do not skip or merge rounds", prompt)

    def test_child_pages_carry_the_check_words_the_oracle_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            words = long_session.write_child_pages(repo / "bulk.txt", 3, 4, "claude")
            lines = (repo / "bulk.txt").read_text().splitlines()
            self.assertEqual(len(lines), 3 * 5)
            self.assertEqual(len(set(words)), 3)
            for index, word in enumerate(words):
                after = ("This is the last page." if index == 2
                         else f"The next page starts at line {1 + (index + 1) * 5}.")
                self.assertEqual(lines[index * 5], f"Page {index + 1}. Check word: {word}. {after}")
                self.assertTrue(set("".join(lines[index * 5 + 1:index * 5 + 5]))
                                <= set("0123456789 "))
            self.assertFalse((repo / long_session.CHILD_PAGER).exists())
            self.assertEqual(long_session.pages_written(repo, words), 0)
            (repo / "pages").mkdir()
            (repo / "pages" / "1.txt").write_text(words[0] + "\n")
            (repo / "pages" / "2.txt").write_text(words[0] + "\n")
            self.assertEqual(long_session.pages_written(repo, words), 1)
        child = long_session.child_prompt("claude", 40)
        self.assertIn("pages of 41 lines each; page 1 starts at line 1", child)
        self.assertIn("limit 41", child)

    def test_codex_child_page_is_two_halves_under_the_exec_and_round_caps(self):
        half = long_session.CODEX_CHILD_HALF_LINES
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            words = long_session.write_child_pages(repo / "bulk.txt", 2, 20, "codex")
            halves = {}
            for page in (1, 2):
                for part in (1, 2):
                    halves[page, part] = subprocess.run(
                        [sys.executable, long_session.CHILD_PAGER, str(page), str(part)],
                        cwd=repo, check=True, capture_output=True, text=True).stdout
        self.assertEqual(halves[1, 1].splitlines()[0],
                         f"Page 1. Check word: {words[0]}. The next page is page 2.")
        self.assertEqual(halves[2, 1].splitlines()[0],
                         f"Page 2. Check word: {words[1]}. This is the last page.")
        for (page, part), text in halves.items():
            body = text.splitlines()[1:] if part == 1 else text.splitlines()
            self.assertEqual(len(body), half, (page, part))
            self.assertFalse(any(line.startswith("Page ") for line in body))
            # One exec_command result is at most 40,000 bytes.
            self.assertLess(len(text.encode()), 40_000)
        self.assertLess(len(halves[1, 1].encode()) + len(halves[1, 2].encode()),
                        long_session.round_cap_bytes(272_000))
        self.assertEqual(long_session.catalog_window("gpt-6-luna"), 272_000)
        self.assertEqual(long_session.catalog_window("gpt-6.1-sol"), 500_000)
        self.assertEqual(long_session.child_pages(500_000, "codex", 20, "gpt-6.1-sol"), 10)
        pages = long_session.child_pages(272_000, "codex", 20, "gpt-6-luna")
        self.assertEqual(pages, 6)
        per_page = 2 * half * long_session.LINE_BYTES / long_session.CODEX_BASE64_BYTES_PER_TOKEN
        self.assertGreater((pages - 1) * per_page,
                           long_session.compaction_trigger(272_000, "gpt-6-luna"))
        prompt = long_session.child_prompt("codex", 20)
        child = json.loads(prompt.split("Give the child exactly this prompt: ", 1)[1]
                           .split("\nWait for the child", 1)[0])
        self.assertIn('{cmd: "python3 page.py k 1"}); notify(a.output);\n', child)
        self.assertIn('{cmd: "python3 page.py k 2"}); text(b.output);', child)
        self.assertIn("which page is next", child)
        self.assertNotIn(str(pages), child.split("Then write")[0].split("acknowledged.txt.", 1)[1])
        self.assertNotIn("earlier turn", child)

    def test_child_compaction_projection_and_ordinals(self):
        segment = {
            "entry_id": "11111111-1111-4111-8111-111111111111",
            "kind": "compaction",
            "text": "compacted 4 earlier segments (semantic summary)",
            "tool_calls": 0, "tool_results": 0,
        }
        self.assertTrue(long_session.is_committed_compaction_projection(segment))
        self.assertTrue(long_session.is_committed_compaction_projection(
            {**segment, "text": "compacted earlier segments"}))
        self.assertTrue(long_session.is_committed_compaction_projection(
            {**segment, "text": "compacted 2 earlier segments (placeholder pass)"}))
        self.assertTrue(long_session.is_child_ordinal("c0"))
        self.assertTrue(long_session.is_child_ordinal("c1"))
        self.assertFalse(long_session.is_child_ordinal("c01"))
        self.assertFalse(long_session.is_child_ordinal("child-1"))
        self.assertFalse(long_session.is_committed_compaction_projection(
            {**segment, "text": "Summary of earlier work:"}))
        self.assertFalse(long_session.is_committed_compaction_projection(
            {**segment, "tool_calls": 1}))
        self.assertFalse(long_session.is_committed_compaction_projection(
            {"entry_id": segment["entry_id"], "kind": "assistant", "text": "",
             "tool_calls": 0, "tool_results": 0}))
        self.assertFalse(long_session.is_committed_compaction_projection(
            {**segment, "kind": "user",
             "text": "[plak: oldest tool results replaced this round]"}))
        self.assertFalse(long_session.is_committed_compaction_projection(
            {"kind": "compaction", "text": "compacted earlier segments",
             "tool_calls": 0, "tool_results": 0}))
        events = [
            {"kind": "tree", "nodes": [
                {"depth": 0, "agent_id": "root"},
                {"depth": 1, "agent_id": "c1", "parent_agent_id": "root"},
                {"depth": 1, "agent_id": "c01"},
            ]},
            {"kind": "tool", "name": "wait_agent", "state": "succeeded",
             "subject": "c1,c2 · up to 30s"},
            {"kind": "tool", "name": "read_file", "subject": "c3"},
        ]
        self.assertEqual(long_session.child_ordinals(events), ["c1", "c2"])
        show = (
            "Session x\n\nTopology children (durable snapshot)\n"
            "  Child          c1 node=Completed attempt=Completed\n"
            "  Child          c2 node=Running attempt=Running\n"
        )
        self.assertEqual(long_session.child_ordinals_from_show(show), ["c1", "c2"])

    def test_kill_reopen_accepts_nothing_started_or_running_tool(self):
        self.assertTrue(long_session.accepted_kill_abandonment(
            {"claim": "running_tool", "cause": "process_lost"}))
        self.assertTrue(long_session.accepted_kill_abandonment(
            {"claim": "nothing_started", "cause": "process_lost"}))
        self.assertFalse(long_session.accepted_kill_abandonment(
            {"claim": "model_request", "cause": "process_lost"}))
        self.assertFalse(long_session.accepted_kill_abandonment(
            {"claim": "running_tool", "cause": "interrupted"}))
        self.assertFalse(long_session.accepted_kill_abandonment(
            {"claim": "nothing_started", "cause": "interrupted"}))

    def test_marker_process_is_visible_only_after_exec(self):
        marker = "plak-long-kill-" + os.urandom(4).hex()
        self.assertFalse(long_session.marker_process_running(marker))
        proc = subprocess.Popen(
            ["python3", "-c", "import time; time.sleep(30)", marker],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            seen = False
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                if long_session.marker_process_running(marker):
                    seen = True
                    break
                time.sleep(0.05)
            self.assertTrue(seen, "sleeper argv was not visible to pgrep")
        finally:
            proc.kill()
            proc.wait(timeout=5)

    def test_partial_agent_records_recovered_and_home_override(self):
        agent = live.Agent.__new__(live.Agent)
        agent._observe({"kind": "recovered", "claim": "running_tool", "cause": "process_lost"})
        self.assertEqual(agent.recovered[0]["cause"], "process_lost")


class LongSessionFakeBinaryTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.capture_workspace = Path(directory.name).resolve()
        (self.capture_workspace / "tests").mkdir()
        root = patch.object(live, "ROOT", self.capture_workspace)
        root.start()
        self.addCleanup(root.stop)
        binary = self.capture_workspace / "plak"
        binary.write_text(FAKE_PLAK)
        binary.chmod(binary.stat().st_mode | stat.S_IEXEC)
        self.binary = binary
        # This checkout's occupancy git shim refuses `git init` even outside
        # the copy. A real live run is not under that shim.
        saved = os.environ.get("PATH", "")
        os.environ["PATH"] = os.pathsep.join(
            part for part in saved.split(os.pathsep) if not part.endswith(f"{os.sep}.plak{os.sep}bin"))
        self.addCleanup(os.environ.__setitem__, "PATH", saved)

    def play(self, name: str) -> str:
        artifacts = self.capture_workspace / "artifacts" / name
        artifacts.mkdir(parents=True)
        env = live.Env(self.binary, artifacts / "shared-home", artifacts)
        return long_session.play(live.ROUTES["grok"], env, name)

    def test_fake_session_log_emits_the_compaction_projection_and_a_bad_child_is_pending(self):
        items = long_session.read_child_history(
            self.binary, self.capture_workspace, "long-root", "c1")
        self.assertTrue(any(long_session.is_committed_compaction_projection(item) for item in items))
        self.assertGreater(len(items), 1)
        with self.assertRaises(live.ScenarioFailure) as raised:
            long_session.read_child_history(
                self.binary, self.capture_workspace, "long-root", "c9")
        self.assertEqual(raised.exception.outcome, live.PENDING)
        self.assertIn("session log", str(raised.exception))
        self.assertNotIn("open product gap", str(raised.exception))
        show = long_session.child_ordinals_from_session_show(
            self.binary, self.capture_workspace, "long-root")
        self.assertEqual(show, ["c1"])

    def test_fake_binary_satisfies_each_scenario(self):
        for name, _surface in long_session.CATALOG:
            with self.subTest(name=name):
                detail = self.play(name)
                self.assertIn("wrote", detail) if name != "child_compaction" else self.assertIn(
                    "recalled.txt", detail)

    def test_agent_home_override_is_the_child_plak_home(self):
        env = live.Env(self.binary, self.capture_workspace / "shared", self.capture_workspace)
        private = self.capture_workspace / "private"

        def ready(agent, *_args):
            agent.session_id = "synthetic"
            return {"kind": "ready"}

        with patch.object(live.subprocess, "Popen") as popen, \
                patch.object(live.threading, "Thread"), \
                patch.object(live.Agent, "wait_for", ready):
            agent = live.Agent(env, self.capture_workspace, "agent", args=[], home=private)
            self.assertEqual(popen.call_args.kwargs["env"]["PLAK_HOME"], str(private))
            agent.transcript.close()
            agent.stderr.close()


if __name__ == "__main__":
    unittest.main()
