"""Offline checks of the live drivers' pure pieces: no binary, no provider."""

import json
import os
import queue
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import live  # noqa: E402
import qualification  # noqa: E402
import smoke  # noqa: E402
import soak  # noqa: E402
import tui_soak  # noqa: E402


class CompactionFoldTests(unittest.TestCase):
    def test_padding_is_larger_than_a_summary_and_hides_the_code_word(self):
        text = live.compaction_padding()
        self.assertGreaterEqual(len(text.encode("ascii")), live.COMPACTION_FOLD_TOKENS * 4)
        self.assertNotIn("ZEBRA-42", text)
        self.assertNotIn("ZEBRA", text)
        self.assertLess(len(text.encode("ascii")), 262_144)
        self.assertTrue(live.needs_compaction_fold(None))
        self.assertTrue(live.needs_compaction_fold(0))
        self.assertTrue(live.needs_compaction_fold(live.COMPACTION_FOLD_TOKENS - 1))
        self.assertFalse(live.needs_compaction_fold(live.COMPACTION_FOLD_TOKENS))
        self.assertTrue(live.needs_compaction_fold(True))

class LiveOracleTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.capture_workspace = Path(directory.name).resolve()
        (self.capture_workspace / "tests").mkdir()
        root = patch.object(live, "ROOT", self.capture_workspace)
        root.start()
        self.addCleanup(root.stop)

    def test_pre_admission_refusal_consumes_unidentified_idle_edge(self):
        agent = live.Agent.__new__(live.Agent)
        agent.events = queue.Queue()
        agent.events.put({"kind": "error", "id": "1",
                          "message": "prompt exceeds the maximum supported size"})
        agent.events.put({"kind": "busy", "busy": False})
        with patch.object(agent, "send", return_value="1"):
            result = agent.prompt("oversized", timeout=0.1)
        self.assertEqual(result.phase, "refused")
        self.assertEqual(result.errors, ["prompt exceeds the maximum supported size"])

    def test_a_rejected_tool_call_counts_as_an_attempt(self):
        """A host rejection must not read as the model skipping the tool."""
        agent = live.Agent.__new__(live.Agent)
        agent.events = queue.Queue()
        for event in ({"kind": "tool", "id": "1", "state": "rejected:OccupancyFence"},
                      {"kind": "tool", "id": "1", "state": "running"},
                      {"kind": "tool", "id": "1", "state": "done"},
                      {"kind": "tool", "id": "1", "state": "failed"},
                      {"kind": "turn_lifecycle", "id": "1", "phase": "completed"},
                      {"kind": "busy", "busy": False}):
            agent.events.put(event)
        with patch.object(agent, "send", return_value="1"):
            turn = agent.prompt("read fold.txt", timeout=1)
        self.assertEqual(turn.tools, 2)

    def test_explore_oracle_checks_answer_not_only_green_repository(self):
        explore = soak.tasks(1)[0]
        with patch.object(live, "unittest_passes", return_value=True):
            self.assertFalse(soak.check(Path("."), explore, "chunked is in recipes.py"))
            self.assertFalse(soak.check(Path("."), explore, "more_itertools.more only"))
            self.assertTrue(soak.check(Path("."), explore,
                                       "more_itertools.more; tests/test_more.py"))
            self.assertTrue(soak.check(Path("."), explore,
                                       "more_itertools/more.py; tests/test_more.py"))

    def test_fault_plan_targets_the_routes_first_inference_request(self):
        plan = live.fault_plan(live.ROUTES["grok"], "reset", after_events=3)
        self.assertEqual(plan, "host=cli-chat-proxy.grok.com,path=/v1/responses,nth=1,"
                               "fault=reset,after-events=3")

    def test_claude_fault_targets_exact_oauth_inference_endpoint(self):
        plan = live.fault_plan(live.ROUTES["claude"], "429", retry_after=2)
        self.assertEqual(plan, "host=api.anthropic.com,path=/v1/messages?beta=true,nth=1,"
                               "fault=429,retry-after=2")

    def test_messages_home_pins_protocol_without_effort(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / "home"
            live.write_home(home, live.ROUTES["deepseek-messages"], Path(directory))
            for agent in ("root-agent", "worker"):
                config = (home / "agents" / f"{agent}.toml").read_text()
                self.assertIn('protocol = "messages"', config)
                self.assertNotIn("effort =", config)

    def test_committed_compactions_come_from_any_format_key(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            log = home / "state" / "r1.e1" / "sessions" / "abc.events.jsonl"
            log.parent.mkdir(parents=True)
            log.write_text("\n".join(json.dumps(record) for record in (
                {"kind": "compact.started"},
                {"kind": "compact.finished", "outcome": "committed"},
                {"kind": "compact.finished", "outcome": "failed"},
            )) + "\nnot json\n")
            records = live.session_records(home, "abc")
            self.assertEqual(live.committed_compactions(records), 1)
            self.assertEqual(live.session_records(home, "other"), [])

    def test_manual_compact_reports_failed_activity_before_uncorrelated_error(self):
        agent = live.Agent.__new__(live.Agent)
        agent.send = lambda kind: "2"
        events = (
            {"kind": "busy", "busy": True},
            {"kind": "tool", "activity_kind": "compact", "state": "failed",
             "subject": "compaction could not prepare"},
            {"kind": "busy", "busy": False},
            {"kind": "error", "id": "2", "message": "late correlated error"},
        )

        def wait_for(predicate, timeout):
            for event in events:
                if predicate(event):
                    return event
            self.fail("no command terminal")

        agent.wait_for = wait_for
        self.assertEqual(agent.command("compact"),
                         {"kind": "error", "message": "compaction could not prepare"})

    def test_manual_compact_failure_carries_its_noted_cause_to_classification(self):
        agent = live.Agent.__new__(live.Agent)
        agent.send = lambda kind: "3"
        events = (
            {"kind": "busy", "busy": True},
            {"kind": "tool", "activity_kind": "compact", "state": "failed",
             "subject": "compact failed"},
            {"kind": "status", "phase": "note", "id": "3",
             "detail": "compact workflow finished without a compacted checkpoint: RateLimited "
                       "\"OpenRouter Messages HTTP 429 (post-acceptance)\""},
            {"kind": "status", "phase": "note", "id": "9", "detail": "unrelated note"},
            {"kind": "busy", "busy": False},
        )

        def wait_for(predicate, timeout):
            for event in events:
                if predicate(event):
                    return event
            self.fail("no command terminal")

        agent.wait_for = wait_for
        answer = agent.command("compact")
        self.assertIn("RateLimited", answer["message"])
        self.assertNotIn("unrelated", answer["message"])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_command_error(answer["message"], "manual compact")
        self.assertEqual(raised.exception.outcome, live.PROVIDER)

    def test_home_binds_root_worker_and_stored_credential(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source = base / "source"
            (source / "secrets").mkdir(parents=True)
            (source / "secrets" / "openrouter.key").write_text("fixture-value\n")
            home = base / "home"
            live.write_home(home, live.ROUTES["deepseek"], source)
            root = (home / "agents" / "root-agent.toml").read_text()
            copied = home / "secrets" / "openrouter.key"
            self.assertNotIn("may_spawn", root)
            self.assertNotIn("effort", root)
            self.assertIn('working_set = ["worker"]', (home / "config.toml").read_text())
            self.assertEqual(home.stat().st_mode & 0o777, 0o700)
            self.assertEqual(copied.read_text(), "fixture-value\n")
            self.assertEqual(copied.stat().st_mode & 0o777, 0o600)

    def test_environment_removes_override(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            env = live.Env(base / "plak", base / "home", base)
            source = {"PATH": "/bin", "OPENROUTER_API_KEY": "fixture-override",
                      "PLAK_HOME": "stale-home", "PLAK_TEST_FAULTS": "stale-fault",
                      "PLAK_TEST_EXCHANGE_CAPTURE_DIR": "stale-capture", "PLAK_LOG": "off"}
            child = env.environ(source=source)
            self.assertEqual(child, {"PATH": "/bin", "PLAK_HOME": str(base / "home"),
                                     "PLAK_TEST_EXCHANGE_CAPTURE_DIR": str(env.capture_segments[0]),
                                     "PLAK_LOG": live.AUTHORITY_DIAGNOSTICS_FILTER})
            next_child = env.environ("explicit-fault", source=source)
            self.assertEqual(next_child["PLAK_TEST_FAULTS"], "explicit-fault")
            self.assertNotEqual(child["PLAK_TEST_EXCHANGE_CAPTURE_DIR"],
                                next_child["PLAK_TEST_EXCHANGE_CAPTURE_DIR"])
            for path in [env.capture_root.parent, env.capture_root, *env.capture_segments]:
                self.assertEqual(path.stat().st_mode & 0o777, 0o700)
                self.assertEqual(path.stat().st_uid, os.getuid())
                self.assertEqual(path, path.resolve())
            other = live.Env(env.binary, env.home, env.artifacts)
            self.assertNotEqual(env.capture_root, other.capture_root)

    def test_explicit_capture_segment_must_be_fresh_owned_and_private(self):
        env = live.Env(Path("plak"), Path("home"), Path("artifacts"))
        segment = env.capture_root / "explicit"
        child = env.environ(capture_dir=segment, source={})
        self.assertEqual(child["PLAK_TEST_EXCHANGE_CAPTURE_DIR"], str(segment))
        with self.assertRaises(live.ScenarioFailure):
            env.environ(capture_dir=segment)
        for kind in ("nonempty", "public", "symlink", "foreign"):
            with self.subTest(kind=kind):
                path = env.capture_root / kind
                if kind == "symlink":
                    path.symlink_to(segment, target_is_directory=True)
                else:
                    path.mkdir(mode=0o700)
                    if kind == "nonempty":
                        (path / "old.info").touch()
                    elif kind == "public":
                        path.chmod(0o755)
                if kind == "foreign":
                    metadata = Mock(st_mode=0o40700, st_uid=os.getuid() + 1)
                    with patch.object(Path, "lstat", return_value=metadata):
                        with self.assertRaises(live.ScenarioFailure):
                            env.environ(capture_dir=path)
                else:
                    with self.assertRaises(live.ScenarioFailure):
                        env.environ(capture_dir=path)

    def test_capture_root_rejects_public_or_symlinked_authority_directory(self):
        root = self.capture_workspace / "tests/provider-authority"
        root.mkdir(mode=0o755)
        env = live.Env(Path("plak"), Path("home"), Path("artifacts"))
        with self.assertRaises(live.ScenarioFailure):
            env.environ(source={})
        root.rmdir()
        root.symlink_to(self.capture_workspace / "tests", target_is_directory=True)
        with self.assertRaises(live.ScenarioFailure):
            env.environ(source={})

    def test_all_launch_surfaces_automatically_install_capture(self):
        base = self.capture_workspace
        env = live.Env(base / "plak", base / "home", base)
        with patch.object(live.subprocess, "run", return_value=Mock(
                returncode=0, stdout="ok", stderr="")) as run:
            live.run_cli(env, base, [], base / "run.log")
            self.assertEqual(run.call_args.kwargs["env"]["PLAK_TEST_EXCHANGE_CAPTURE_DIR"],
                             str(env.capture_segments[-1]))

        def ready(agent, *args):
            agent.session_id = "synthetic"
            return {"kind": "ready"}

        with patch.object(live.subprocess, "Popen") as popen, \
                patch.object(live.threading, "Thread"), \
                patch.object(live.Agent, "wait_for", ready):
            agent = live.Agent(env, base, "agent", args=[], faults="fault")
            self.assertEqual(popen.call_args.kwargs["env"]["PLAK_TEST_EXCHANGE_CAPTURE_DIR"],
                             str(agent.capture_dir))
            self.assertEqual(popen.call_args.kwargs["env"]["PLAK_TEST_FAULTS"], "fault")
            agent.transcript.close()
            agent.stderr.close()
        with patch.object(live.pty, "fork", return_value=(123, 456)), \
                patch.object(live.fcntl, "ioctl"):
            tui = live.Tui(env, base, base / "pty.log")
            self.assertEqual(tui.capture_dir, env.capture_segments[-1])
            tui.log.close()
        self.assertEqual(len(set(env.capture_segments)), 3)

    def test_smoke_cannot_qualify_missing_or_incomplete_raw_evidence(self):
        binary = self.capture_workspace / "plak"
        binary.write_bytes(b"synthetic executable; never launched")
        for condition in ("complete", "empty", "incomplete", "pending", "not_started"):
            with self.subTest(condition=condition):
                def scenario(runner):
                    if condition == "not_started":
                        raise live.ScenarioFailure(live.HARNESS, "launch refused")
                    runner.env.environ(source={})
                    segment = runner.env.capture_segments[-1]
                    if condition != "empty":
                        self.write_capture(segment)
                    if condition in ("incomplete", "pending"):
                        (segment / "exchange-0001.info").write_text("incomplete=true\n")
                    if condition == "pending":
                        raise live.ScenarioFailure(live.HARNESS, "known failure")
                    return "synthetic success"

                with patch.object(live, "write_home"), \
                        patch.object(live, "capture_root_mismatch", return_value=None), \
                        patch.object(smoke.Smoke, "SCENARIOS", ((condition, "cli"),)), \
                        patch.object(smoke.Smoke, condition, scenario, create=True), \
                        patch.object(smoke, "EXPECTED_PENDING",
                                     {"pending": "known", "not_started": "known"}), \
                        patch("builtins.print"):
                    code = smoke.main(["--binary", str(binary)])
                self.assertEqual(code, 0 if condition == "complete" else 1)
                report = next((self.capture_workspace / "target/live").glob("*/report.json"))
                evidence = json.loads(report.read_text())
                self.assertEqual(evidence["capture_complete"], condition == "complete")
                self.assertEqual(len(evidence["evidence"][condition]["segments"]),
                                 0 if condition == "not_started" else 1)

    def write_pending(self, segment, number=2):
        name = f"exchange-{number:04}"
        (segment / f"{name}.request").write_bytes(b"synthetic request")
        (segment / f"{name}.body").write_bytes(b"")
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={number}\nstatus=None\nterminal=pending\n"
            "recorded_bytes=0\nincomplete=true\n")

    def timed_out_run(self, env, *, pending: bool, faults: str | None = None):
        """`plak run` killed at its timeout, its capture as the process left it."""
        def run(*args, **kwargs):
            segment = Path(kwargs["env"]["PLAK_TEST_EXCHANGE_CAPTURE_DIR"])
            self.write_capture(segment)
            if pending:
                self.write_pending(segment)
            raise live.subprocess.TimeoutExpired(args[0], kwargs["timeout"])

        base = self.capture_workspace
        with patch.object(live.subprocess, "run", side_effect=run):
            with self.assertRaises(live.ScenarioFailure) as failure:
                live.run_cli(env, base, ["--", "hi"], base / "run.log", timeout=300, faults=faults)
        return failure.exception

    def test_pending_exchange_is_only_the_last_exchanges_pre_dispatch_marker(self):
        segment = self.capture_workspace / "segment"
        segment.mkdir()
        self.assertIsNone(live.pending_exchange(segment))
        self.write_capture(segment)
        self.assertIsNone(live.pending_exchange(segment))
        self.write_pending(segment, 2)
        self.assertEqual(live.pending_exchange(segment), 2)
        self.assertFalse(live.capture_integrity([segment]))
        self.assertTrue(live.capture_integrity([segment], [segment]))
        # Only the highest-numbered exchange may be pending, ordered by number.
        self.write_capture(segment, "exchange-0010")
        self.assertIsNone(live.pending_exchange(segment))
        self.assertFalse(live.capture_integrity([segment], [segment]))
        (segment / "exchange-0010.info").write_text(
            (segment / "exchange-0002.info").read_text().replace("exchange=2", "exchange=10"))
        self.assertEqual(live.pending_exchange(segment), 10)
        self.assertFalse(live.capture_integrity([segment], [segment]))
        for name in ("exchange-0002", "exchange-0010"):
            for kind in ("request", "body", "info"):
                (segment / f"{name}.{kind}").unlink()
        info = segment / "exchange-0001.info"
        self.write_pending(segment, 1)
        valid = info.read_text()
        for metadata in (valid.replace("terminal=pending", "terminal=Complete"),
                         valid.replace("incomplete=true", "incomplete=false"),
                         valid.replace("status=None", "status=Some(200)"),
                         valid.replace("exchange=1", "exchange=2"),
                         valid + "terminal=pending\n"):
            with self.subTest(metadata=metadata):
                info.write_text(metadata)
                self.assertIsNone(live.pending_exchange(segment))
        info.write_text(valid)
        (segment / "exchange-0001.request").unlink()
        self.assertIsNone(live.pending_exchange(segment))

    def test_run_cli_timeout_on_a_pending_exchange_is_provider(self):
        base = self.capture_workspace
        env = live.Env(base / "plak", base / "home", base)
        failure = self.timed_out_run(env, pending=True)
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertEqual(str(failure), "plak run did not finish in 300s: provider did not "
                                       "respond within the scenario budget (exchange 2 pending)")
        self.assertEqual(env.provider_pending, {env.capture_segments[-1]})
        self.assertEqual(live.provider_pending(env, env.capture_segments, live.PROVIDER),
                         env.capture_segments)
        self.assertEqual(live.provider_pending(env, env.capture_segments, live.HARNESS), [])

    def test_run_cli_timeout_without_a_pending_exchange_is_harness(self):
        base = self.capture_workspace
        env = live.Env(base / "plak", base / "home", base)
        failure = self.timed_out_run(env, pending=False)
        self.assertEqual((failure.outcome, str(failure)),
                         (live.HARNESS, "plak run did not finish in 300s"))
        # A fault the scenario injected must be absorbed, so it never relabels a hang.
        failure = self.timed_out_run(env, pending=True, faults="fault=stall,seconds=40")
        self.assertEqual(failure.outcome, live.HARNESS)
        self.assertEqual(env.provider_pending, set())
        missing = live.timeout_failure(env, base / "missing", "plak run did not finish in 300s")
        self.assertEqual(missing.outcome, live.HARNESS)

    def test_turn_timeout_on_a_pending_exchange_is_provider(self):
        base = self.capture_workspace
        for pending, faults, expected in ((True, None, live.PROVIDER), (False, None, live.HARNESS),
                                          (True, "fault=stall", live.HARNESS)):
            with self.subTest(pending=pending, faults=faults):
                env = live.Env(base / "plak", base / "home", base)
                env.environ(source={})
                segment = env.capture_segments[-1]
                self.write_capture(segment)
                if pending:
                    self.write_pending(segment)
                agent = live.Agent.__new__(live.Agent)
                agent.env, agent.capture_dir, agent.lookup_faults = env, segment, faults
                agent.events = queue.Queue()
                with patch.object(agent, "send", return_value="1"), \
                        self.assertRaises(live.ScenarioFailure) as failure:
                    agent.prompt("hi", timeout=0.05)
                self.assertEqual(failure.exception.outcome, expected)
                self.assertTrue(str(failure.exception).startswith("turn: no expected event within"))
                self.assertEqual(env.provider_pending, {segment} if expected == live.PROVIDER else set())

    def test_provider_timeout_keeps_the_cell_provider_and_a_hang_stays_harness(self):
        binary = self.capture_workspace / "plak"
        binary.write_bytes(b"synthetic executable; never launched")
        for pending in (True, False):
            with self.subTest(pending=pending):
                def scenario(runner):
                    self.timed_out_run(runner.env, pending=pending)
                    raise self.timed_out_run(runner.env, pending=pending)

                with patch.object(live, "write_home"), \
                        patch.object(live, "capture_root_mismatch", return_value=None), \
                        patch.object(smoke.Smoke, "SCENARIOS", (("timeout", "cli"),)), \
                        patch.object(smoke.Smoke, "timeout", scenario, create=True), \
                        patch("builtins.print"):
                    code = smoke.main(["--binary", str(binary)])
                self.assertEqual(code, 1)
                path = next((self.capture_workspace / "target/live").glob("*/report.json"))
                report = json.loads(path.read_text())
                path.unlink()
                item = report["evidence"]["timeout"]
                self.assertEqual(report["capture_complete"], True)
                self.assertEqual(item["capture_complete"], True)
                self.assertEqual(len(item["provider_pending"]), 2 if pending else 0)
                outcome, reason = qualification.score_report(code, report)
                self.assertEqual(outcome, live.PROVIDER if pending else live.HARNESS)
                self.assertIn(live.PROVIDER_PENDING if pending else "did not finish", reason)

    def test_drivers_default_to_release_and_remove_capture_opt_in(self):
        def inspect(parser, argv):
            self.assertEqual(parser.get_default("binary"), live.ROOT / "target/release/plak")
            self.assertNotIn("--capture-exchanges", parser._option_string_actions)
            raise RuntimeError("stop before any launch")

        for driver in (smoke, soak, tui_soak):
            with self.subTest(driver=driver.__name__), \
                    patch.object(driver.argparse.ArgumentParser, "parse_args", inspect):
                with self.assertRaisesRegex(RuntimeError, "stop before any launch"):
                    driver.main([])
        self.assertIn("--release", soak.REBUILD)

    def test_wrap_up_stays_out_of_the_default_smoke(self):
        default = [name for name, _ in smoke.Smoke.SCENARIOS]
        self.assertNotIn("wrap_up", default)
        self.assertNotIn("py_ptc", default)
        self.assertEqual([name for name, _ in smoke.Smoke.OPT_IN], ["wrap_up", "py_ptc"])
        self.assertEqual([name for name, _ in smoke.selected_scenarios({"py_ptc"})],
                         ["py_ptc"])
        self.assertEqual([name for name, _ in smoke.selected_scenarios(set())], default)
        self.assertEqual([name for name, _ in smoke.selected_scenarios({"wrap_up"})],
                         ["wrap_up"])
        self.assertEqual(smoke.selected_scenarios({"missing"}), ())
        self.assertEqual(
            [name for name, _ in smoke.selected_scenarios({"tui", "wrap_up"})],
            ["tui", "wrap_up"])

    def test_pin_max_model_rounds_appends_the_turn_cap(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            home = base / "home"
            live.write_home(home, live.ROUTES["grok"], base)
            live.pin_max_model_rounds(home, 2)
            text = (home / "config.toml").read_text()
            self.assertIn("[turn]\nmax_model_rounds = 2\n", text)
            self.assertIn('agent = "root-agent"', text)
            self.assertIn('account = "grok"', (home / "agents" / "root-agent.toml").read_text())
            with self.assertRaises(live.ScenarioFailure):
                live.pin_max_model_rounds(home, 2)

    def test_pin_composition_switches_every_template(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            home = base / "home"
            live.write_home(home, live.ROUTES["grok"], base)
            live.pin_composition(home, "py-ptc")
            for name in ("root-agent", "worker"):
                text = (home / "agents" / f"{name}.toml").read_text()
                self.assertIn('composition = "py-ptc"\n', text)
                self.assertNotIn("plakous-default", text)
            with self.assertRaises(live.ScenarioFailure):
                live.pin_composition(home, "py-ptc")

    def test_py_ptc_oracle_requires_the_second_cell_the_handled_error_and_the_answer(self):
        exchanges = '{"name": "exec_py", "code": "TOKEN[::-1]"} handled ValueError'
        answer = [{"kind": "turn.assistant", "text": smoke.PY_PTC_TOKEN[::-1]}]
        smoke._require_py_ptc_oracle("", answer, exchanges)
        smoke._require_py_ptc_oracle(smoke.PY_PTC_TOKEN[::-1], [], exchanges)
        for records, captured, reason in (
                (answer, exchanges.replace("TOKEN[::-1]", ""), "second exec_py cell"),
                (answer, exchanges.replace("exec_py", ""), "second exec_py cell"),
                (answer, exchanges.replace("handled ValueError", ""), "handled-exception"),
                ([{**answer[0], "text": smoke.PY_PTC_TOKEN}], exchanges, "reversed token")):
            with self.subTest(reason=reason), \
                    self.assertRaisesRegex(live.ScenarioFailure, reason):
                smoke._require_py_ptc_oracle("", records, captured)

    def test_streamed_text_reassembles_messages_tool_input(self):
        fragments = ['{"code": "print(TOK', 'EN[:', ':-1])"}']
        events = [{"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "name": "exec_py", "input": {}}}]
        events += [{"type": "content_block_delta", "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": part}}
                   for part in fragments]
        events.append({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": "handled "}})
        events.append({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": "ValueError"}})
        body = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events)
        self.assertNotIn("TOKEN[::-1]", body)
        text = smoke.streamed_text(body)
        self.assertIn("TOKEN[::-1]", text)
        self.assertIn("handled ValueError", text)
        self.assertEqual(smoke.streamed_text("not a stream"), "not a stream")

    def test_run_cli_can_use_a_private_home(self):
        base = self.capture_workspace
        env = live.Env(base / "plak", base / "home", base)
        private = base / "private-home"
        with patch.object(live.subprocess, "run", return_value=Mock(
                returncode=0, stdout="ok", stderr="")) as run:
            live.run_cli(env, base, ["--", "hi"], base / "run.log", home=private)
        self.assertEqual(run.call_args.kwargs["env"]["PLAK_HOME"], str(private))
        self.assertIn("PLAK_TEST_EXCHANGE_CAPTURE_DIR", run.call_args.kwargs["env"])

    def test_run_cli_closes_stdin_and_scores_a_truncated_answer_as_completed(self):
        base = self.capture_workspace
        env = live.Env(base / "plak", base / "home", base)
        truncated = "plak: answer truncated at its 64-token output limit\nplak: turn_failed:output-limit\n"
        with patch.object(live.subprocess, "run", return_value=Mock(
                returncode=5, stdout="MAGENTA\n", stderr=truncated)) as run:
            code, out = live.run_cli(env, base, ["--", "hi"], base / "run.log")
        self.assertIs(run.call_args.kwargs["stdin"], live.subprocess.DEVNULL)
        self.assertEqual((code, out), (0, "MAGENTA\n"))
        log = (base / "run.log").read_text()
        self.assertTrue(log.startswith("exit 5 (answer committed at its output limit"))
        self.assertIn("turn_failed:output-limit", log)
        # The line must be the final status; any other exit 5 stays a failure.
        for code, stderr in ((5, "plak: turn_failed:output-limit\nplak: agent turn failed: x\n"),
                             (1, truncated), (5, "plak: agent turn failed: x\n")):
            with patch.object(live.subprocess, "run", return_value=Mock(
                    returncode=code, stdout="", stderr=stderr)):
                self.assertEqual(live.run_cli(env, base, [], base / "run.log")[0], code)

    def test_a_providers_in_band_5xx_or_429_is_provider_on_every_surface(self):
        text = ("agent turn failed: OpenRouter Chat operation failed: OpenRouter returned an "
                "in-band error after accepting the request: HTTP 504 (timeout) from Upstream "
                "[1 requests; last: timeout] · HTTP 504")
        self.assertEqual(live.in_band_provider_status(text), 504)
        self.assertIsNone(live.stream_validation_outcome(text, None))
        for kind, status in (("Timeout", 504), (None, None)):
            turn = live.Turn(phase="failed", model_error_kind=kind, http_status=status,
                             errors=[text])
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_incomplete(turn, "coding turn", segments=[])
            self.assertEqual(raised.exception.outcome, live.PROVIDER, kind)
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, f"plak: {text}\n", "cli_task", segments=[])
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        self.assertTrue(live.provider_child_text(text))
        prefix = "OpenRouter returned an in-band error after accepting the request: "
        for status in (429, 502, 520, 521, 522, 523, 524):
            self.assertEqual(live.in_band_provider_status(
                f"{prefix}HTTP {status} (unavailable): Provider returned error"), status)
        for other in (prefix + "HTTP 400: bad request",
                      prefix + "HTTP 401",
                      prefix + "Provider returned error",
                      prefix + "Provider returned error (from Upstream)",
                      prefix + "unclassified",
                      "OpenRouter Chat stream validation failed: Chat chunk omitted model"):
            self.assertIsNone(live.in_band_provider_status(other), other)
        other = live.Turn(phase="failed", model_error_kind="Protocol", errors=[prefix + "HTTP 400"])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(other, "coding turn", segments=[])
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        self.assertEqual(live.provider_reason_from(None, 504, ""), "504")

    def test_a_typed_in_band_provider_error_without_a_rendered_status_is_provider(self):
        # Text cut before its status still carries the typed kind.
        text = ("agent turn failed: OpenRouter Chat operation failed: OpenRouter returned an "
                "in-band error after accepting the request: Provider returned error")
        for kind in ("Unavailable", "RateLimited", "Timeout"):
            turn = live.Turn(phase="failed", model_error_kind=kind, errors=[text])
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_incomplete(turn, "coding turn", injected_fault="529", segments=[])
            self.assertEqual(raised.exception.outcome, live.PROVIDER, kind)
        untyped = live.Turn(phase="failed", errors=[text])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(untyped, "coding turn", segments=[])
        self.assertEqual(raised.exception.outcome, live.HARNESS)

    def test_the_in_band_status_stays_inside_its_own_diagnostic(self):
        prefix = "OpenRouter returned an in-band error after accepting the request: "
        self.assertIsNone(live.in_band_provider_status(
            prefix + "Provider returned error [1 requests; last: x] compact failed: HTTP 502"))
        self.assertIsNone(live.in_band_provider_status(prefix + "HTTP 5025"))
        self.assertIsNone(live.in_band_provider_status(
            "Chat provider returned an in-band error: error code: 504"))
        self.assertEqual(live.in_band_provider_status(
            prefix + "HTTP 503 (unavailable): type=server_error"), 503)
        text = prefix + "HTTP 429 (rate limited)"
        self.assertFalse(live._absorbed_injection("RateLimited", None, "429", text))
        self.assertTrue(live._absorbed_injection("RateLimited", 429, "429", "provider rate limited"))

    def test_output_limit_failure_is_model_on_every_surface(self):
        typed = live.Turn(phase="failed", model_error_kind="OutputLimit",
                          errors=["agent turn failed: model request failed"])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(typed, "jsonl turn")
        self.assertEqual(raised.exception.outcome, live.MODEL)
        self.assertIn("OutputLimit", str(raised.exception))
        rendered = "plak: agent turn failed: response reached its output bound before completion\n"
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, rendered, "cli_task")
        self.assertEqual(raised.exception.outcome, live.MODEL)
        responses = ("plak: agent turn failed: Responses provider stopped at the output bound "
                     "before the response was complete\n")
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, responses, "cli_task")
        self.assertEqual(raised.exception.outcome, live.MODEL)
        # Only the final stderr line is the turn's failure: an earlier line
        # quoting the diagnostic (a recovered retry, a log) does not decide it.
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(
                5, rendered + "plak: agent turn failed: session store is closed\n\n", "cli_task")
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, "plak: retrying\n" + rendered + "\n", "cli_task")
        self.assertEqual(raised.exception.outcome, live.MODEL)
        # Another failure that merely mentions an output bound stays harness.
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, "plak: requested output bound exceeds the admitted maximum",
                                      "cli_task")
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        # TUI: the lifecycle row has no kind; the turn's `turn.failed` text does.
        records = [
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "t1"},
            {"kind": "turn.output_limit_retry", "detail": "retrying"},
            {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "t1"},
            {"kind": "turn.failed", "disposition": "definite",
             "msg": "agent turn failed: response reached its output bound before completion"},
            {"kind": "turn.lifecycle", "phase": "admitted", "turn_id": "t2"},
            {"kind": "turn.lifecycle", "phase": "failed", "turn_id": "t2"},
            {"kind": "turn.failed", "msg": "agent turn failed: protocol"},
        ]
        first, second = smoke.durable_terminals(records)
        self.assertIn("output bound", first["message"])
        self.assertEqual(second["message"], "agent turn failed: protocol")
        self.assertNotIn("message", records[2])
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_durable_completed(first, "first TUI turn")
        self.assertEqual(raised.exception.outcome, live.MODEL)
        with self.assertRaises(live.ScenarioFailure) as raised:
            smoke.require_durable_completed(second, "second TUI turn")
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        home = self.capture_workspace / "home"
        log = home / "state" / "k" / "sessions" / "s.events.jsonl"
        log.parent.mkdir(parents=True)
        log.write_text("".join(json.dumps(record) + "\n" for record in records))
        self.assertTrue(tui_soak.output_limit_terminal(home, "s", 0))
        self.assertFalse(tui_soak.output_limit_terminal(home, "s", 1))
        self.assertFalse(tui_soak.output_limit_terminal(home, "s", -1))

    def test_unaccepted_compatibility_notice_names_its_remedy(self):
        for stderr in (
            "plak: the codex compatibility notice needs an answer on stdin and none came; "
            "accept it once with `plak models codex` from a terminal, or pipe `y` to this command\n",
            "plak: the codex compatibility notice is not accepted; run `plak models codex` to "
            "review and accept it (the login is kept)\n",
        ):
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_cli_failure(3, stderr, "cli_task")
            self.assertEqual(raised.exception.outcome, live.HARNESS)
            self.assertIn("compatibility notice is not accepted", str(raised.exception))
            self.assertIn("`plak models codex`", str(raised.exception))

    def test_wrap_up_oracle_rejects_binding_loss_and_an_empty_answer(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "run.log"
            log.write_text("exit 1\n--- stdout\n\n--- stderr\nstable-prefix binding differs\n")
            with self.assertRaisesRegex(live.ScenarioFailure, "stable-prefix binding differs"):
                smoke._reject_binding_loss("wrap-up turn", log, "", [])
            log.write_text("exit 0\n--- stdout\nDONE\n--- stderr\n")
            smoke._reject_binding_loss("wrap-up turn", log, "DONE",
                                       [{"kind": "turn.assistant", "text": "DONE"}])
            with self.assertRaisesRegex(live.ScenarioFailure, "ProtectedHistory"):
                smoke._reject_binding_loss(
                    "wrap-up turn", log, "DONE", [{"kind": "error", "msg": "ProtectedHistory"}])
            with self.assertRaisesRegex(live.ScenarioFailure, "without assistant text"):
                smoke._require_cli_answer("session_id=abc\n", [], "wrap-up turn")
            smoke._require_cli_answer("session_id=abc\nDONE\n", [], "wrap-up turn")
            smoke._require_cli_answer(
                "session_id=abc\n", [{"kind": "turn.assistant", "text": "DONE"}], "wrap-up turn")

    def test_tui_soak_tasks_have_independent_oracles(self):
        first = tui_soak.task(0, 0)
        later = tui_soak.task(1, 2)
        self.assertNotEqual(first.name, later.name)
        self.assertIn(first.name, first.prompt)
        self.assertIn(first.name, first.check)

    def test_tui_soak_terminal_filter_ignores_admission(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            log = home / "state" / "r1" / "sessions" / "abc.events.jsonl"
            log.parent.mkdir(parents=True)
            log.write_text("\n".join(json.dumps(record) for record in (
                {"kind": "turn.lifecycle", "phase": "admitted"},
                {"kind": "tool.completed", "state": "succeeded"},
                {"kind": "turn.lifecycle", "phase": "completed"},
            )) + "\n")
            self.assertEqual(tui_soak.terminals(home, "abc"), ["completed"])

    def test_headless_soak_records_reopen_failure_instead_of_losing_report(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            env = live.Env(base / "plak", base / "home", base)
            runner = soak.Soak(live.ROUTES["grok"], env, base)

            def refused(_faults):
                raise live.ScenarioFailure(live.HARNESS, "reopen refused")

            runner.open = refused
            record = runner.run_task(soak.Task("explore", "inspect"), 1, None, False)
            self.assertEqual(record.outcome, "harness")
            self.assertIn("reopen refused", record.errors)

    def test_headless_soak_output_limit_turn_is_a_model_miss_not_a_dead_end(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            env = live.Env(base / "plak", base / "home", base)
            runner = soak.Soak(live.ROUTES["grok"], env, base)
            agent = Mock(proc=Mock(pid=1, poll=Mock(return_value=0)), capture_dir=base)
            agent.prompt.return_value = live.Turn(phase="failed", model_error_kind="OutputLimit")
            runner.open = lambda _faults: agent
            record = runner.run_task(soak.Task("explore", "inspect"), 1, None, False)
        self.assertEqual(record.outcome, "model")
        self.assertFalse(record.dead_end)
        self.assertEqual(agent.prompt.call_count, 1)

    def test_plant_resets_a_differently_fixed_function_and_keeps_later_code(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "repo"
            _path, name, good, bad = soak.BUGS["ilen"]
            live.git_repo(repo, {"more.py":
                                 f"def first():\n    return 1\n\n\ndef ilen(iterable):\n    {good}\n"})
            source = repo / "more.py"
            pinned = soak.subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True,
                                         capture_output=True, text=True).stdout.strip()
            # An earlier cycle fixed ilen in other words and added a function.
            source.write_text("def first():\n    return 1\n\n\n@staticmethod\ndef ilen(iterable):\n"
                              "    return sum(1 for _ in iterable)\n\n\n"
                              "def every_nth_1(iterable, n):\n    return iterable[::n]\n")
            with patch.object(soak, "REPO_COMMIT", pinned), \
                    patch.dict(soak.BUGS, {"ilen": ("more.py", name, good, bad)}):
                soak.plant(repo, "ilen")
                text = source.read_text()
                self.assertIn(bad, text)
                self.assertNotIn("sum(1 for _", text)
                self.assertNotIn("@staticmethod", text)
                self.assertIn("def every_nth_1(iterable, n):", text)
                self.assertIn("def first():", text)

                # A plant that cannot run is that task's harness result, not a crash.
                source.write_text("def ilen(:\n")
                env = live.Env(repo / "plak", repo / "home", Path(directory))
                runner = soak.Soak(live.ROUTES["grok"], env, repo)
                runner.open = Mock(side_effect=AssertionError("must not open"))
                record = runner.run_task(soak.Task("fix_ilen", "fix", bug="ilen"), 2, None, False)
            self.assertEqual(record.outcome, "harness")
            self.assertEqual(record.evidence_status, "not_started")
            self.assertIn("no parseable ilen", record.errors[0])

    def test_capture_segment_distinguishes_authority_refusal_from_missing_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            capture = base / "segment-001"
            capture.mkdir()
            diagnostics = base / "plak.log"
            self.assertEqual(soak.segment_evidence(capture, diagnostics, 0),
                             (0, [], "diagnostics_missing"))
            diagnostics.write_text("old predicate=route.catalog_time\n")
            diagnostics.chmod(0o600)
            offset = diagnostics.stat().st_size
            self.assertEqual(soak.segment_evidence(capture, diagnostics, offset),
                             (0, [], "unexplained_no_response"))
            diagnostics.write_text(diagnostics.read_text() +
                                   'predicate="native.route_current.generation" '
                                   'private=do-not-report\n')
            self.assertEqual(soak.segment_evidence(capture, diagnostics, offset),
                             (0, ["native.route_current.generation"],
                              "authority_witness_no_response"))
            (capture / "exchange-0001.info").write_text("incomplete=false\n")
            self.assertEqual(soak.segment_evidence(capture, diagnostics, offset),
                             (1, ["native.route_current.generation"], "response"))
            diagnostics.chmod(0o644)
            self.assertEqual(soak.segment_evidence(capture, diagnostics, offset),
                             (1, [], "diagnostics_missing"))

    def write_capture(self, segment, name="exchange-0001", body=b"response",
                      terminal="Complete", status="Some(200)"):
        (segment / f"{name}.request").write_bytes(b"synthetic request")
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={int(name.split('-')[1])}\n"
            f"status={status}\nconnection=Some(Reused)\nterminal={terminal}\n"
            f"recorded_bytes={len(body)}\n"
            "incomplete=false\n")

    def test_capture_integrity_rejects_a_missing_pair_but_has_no_lifetime_limit(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            first, second = base / "first", base / "second"
            first.mkdir()
            second.mkdir()
            self.write_capture(first)
            self.assertTrue(live.capture_integrity([first, second]))
            (second / "exchange-0001.body").write_bytes(b"unpaired")
            self.assertFalse(live.capture_integrity([first, second]))
            self.write_capture(second)
            self.assertTrue(live.capture_integrity([first, second]))
            for number in range(2, 514):
                self.write_capture(second, f"exchange-{number:04}", body=b"")
            self.assertTrue(live.capture_integrity([first, second]))

    def test_capture_integrity_rejects_wrong_names_even_with_equal_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for kind in ("request", "body", "info"):
                with self.subTest(kind=kind):
                    self.write_capture(segment)
                    original = segment / f"exchange-0001.{kind}"
                    renamed = segment / f"exchange-0002.{kind}"
                    original.rename(renamed)
                    self.assertFalse(live.capture_integrity([segment]))
                    renamed.rename(original)

    def test_capture_integrity_rejects_wrong_size_and_missing_request(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment)
            (segment / "exchange-0001.body").write_bytes(b"short")
            self.assertFalse(live.capture_integrity([segment]))
            self.write_capture(segment)
            (segment / "exchange-0001.request").unlink()
            self.assertFalse(live.capture_integrity([segment]))

    def test_capture_integrity_rejects_invalid_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment)
            info = segment / "exchange-0001.info"
            valid = info.read_text()
            invalid = [
                "", "incomplete=false\n", "not metadata\n",
                valid.replace("terminal=Complete", "terminal=pending"),
                valid.replace("terminal=Complete", "terminal="),
                valid.replace("terminal=Complete", "terminal=bogus"),
                valid.replace("incomplete=false", "incomplete=true"),
                valid.replace("incomplete=false", "incomplete=no"),
                valid.replace("recorded_bytes=8\n", ""),
                valid.replace("recorded_bytes=8", "recorded_bytes=-1"),
                valid.replace("recorded_bytes=8", "recorded_bytes=8.0"),
                valid.replace("status=Some(200)", "status=bogus"),
                valid.replace("connection=Some(Reused)\n", ""),
                valid.replace("connection=Some(Reused)", "connection=Some(Pooled)"),
                valid + "recorded_bytes=8\n",
            ]
            for metadata in invalid:
                with self.subTest(metadata=metadata):
                    info.write_text(metadata)
                    self.assertFalse(live.capture_integrity([segment]))
            info.write_bytes(b"\xff")
            self.assertFalse(live.capture_integrity([segment]))
            info.unlink()
            self.assertFalse(live.capture_integrity([segment]))

    def test_capture_integrity_accepts_fully_captured_transport_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for kind in ("Io", "LimitExceeded(ResponseBodyBytes)"):
                terminal = ("Failed(Failure { phase: ResponseBody, "
                            f"transmission: MayHaveTransmitted, kind: {kind}, "
                            "cause: Some(\"unexpected_eof\"), http2_reason: None, "
                            "os_error: None, tls: None, tls_alert: None, alpn: Some(\"h2\"), "
                            "http2_io: true, connection: Some(Fresh), "
                            "idle_age: Some(1.5s) })")
                for body in (b"partial response", b""):
                    with self.subTest(kind=kind, body=body):
                        self.write_capture(segment, body=body, terminal=terminal, status="None")
                        self.assertTrue(live.capture_integrity([segment]))

    def test_capture_integrity_preserves_empty_segment_for_authority_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertTrue(live.capture_integrity([Path(directory)]))
            self.assertFalse(live.capture_integrity([Path(directory) / "missing"]))

    def test_capture_integrity_accepts_protocol_terminal_but_rejects_gap_markers(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, terminal="ProtocolComplete")
            self.assertTrue(live.capture_integrity([segment]))
            for marker in ("incomplete.log", "retention.info"):
                (segment / marker).write_text("evidence omitted\n")
                self.assertFalse(live.capture_integrity([segment]))
                (segment / marker).unlink()

    def test_live_binary_identity_is_the_executable_bytes_not_a_task_repo_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "plak"
            binary.write_bytes(b"first binary")
            first = live.binary_sha256(binary)
            binary.write_bytes(b"second binary")
            self.assertNotEqual(first, live.binary_sha256(binary))


if __name__ == "__main__":
    unittest.main()
