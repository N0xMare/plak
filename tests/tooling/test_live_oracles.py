"""Offline checks of the live drivers' pure pieces: no binary, no provider."""

import json
import os
import shutil
import queue
import sqlite3
import sys
import tempfile
import time
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
import soak  # noqa: E402


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
        # These tests exercise capture and scoring over fake or mocked callers.
        self.enterContext(patch.object(live.Env, "gateway_env", lambda _env, child: child))
        self.homes = set()
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.capture_workspace = Path(directory.name).resolve()
        (self.capture_workspace / "tests").mkdir()
        root = patch.object(live, "ROOT", self.capture_workspace)
        root.start()
        self.addCleanup(root.stop)

    def home(self, artifacts, name="main"):
        home = live.scratch_home(artifacts, name)
        if home not in self.homes:
            self.homes.add(home)
            self.addCleanup(shutil.rmtree, home)
        return home

    def test_pre_admission_refusal_consumes_unidentified_idle_edge(self):
        agent = live.Agent.__new__(live.Agent)
        agent.events = queue.Queue()
        agent.events.put(
            {"kind": "error", "id": "1", "message": "prompt exceeds the maximum supported size"}
        )
        agent.events.put({"kind": "busy", "busy": False})
        with patch.object(agent, "send", return_value="1"):
            result = agent.prompt("oversized", timeout=0.1)
        self.assertEqual(result.phase, "refused")
        self.assertEqual(result.errors, ["prompt exceeds the maximum supported size"])

    def test_a_rejected_tool_call_counts_as_an_attempt(self):
        """A host rejection must not read as the model skipping the tool."""
        agent = live.Agent.__new__(live.Agent)
        agent.events = queue.Queue()
        for event in (
            {"kind": "tool", "id": "1", "state": "rejected:OccupancyFence"},
            {"kind": "tool", "id": "1", "state": "running"},
            {"kind": "tool", "id": "1", "state": "done"},
            {"kind": "tool", "id": "1", "state": "failed"},
            {"kind": "turn_lifecycle", "id": "1", "phase": "completed"},
            {"kind": "busy", "busy": False},
        ):
            agent.events.put(event)
        with patch.object(agent, "send", return_value="1"):
            turn = agent.prompt("read fold.txt", timeout=1)
        self.assertEqual(turn.tools, 2)

    def test_explore_oracle_checks_answer_not_only_green_repository(self):
        explore = soak.tasks(1)[0]
        with patch.object(live, "unittest_passes", return_value=True):
            self.assertFalse(soak.check(Path("."), explore, "chunked is in recipes.py"))
            self.assertFalse(soak.check(Path("."), explore, "more_itertools.more only"))
            self.assertTrue(
                soak.check(Path("."), explore, "more_itertools.more; tests/test_more.py")
            )
            self.assertTrue(
                soak.check(Path("."), explore, "more_itertools/more.py; tests/test_more.py")
            )

    def test_fault_plan_targets_the_routes_first_inference_request(self):
        plan = live.fault_plan(live.ROUTES["grok"], "reset", after_events=3)
        self.assertEqual(
            plan, "host=cli-chat-proxy.grok.com,path=/v1/responses,nth=1,fault=reset,after-events=3"
        )

    def test_claude_fault_targets_exact_oauth_inference_endpoint(self):
        plan = live.fault_plan(live.ROUTES["claude-sonnet55-low"], "429", retry_after=2)
        self.assertEqual(
            plan, "host=api.anthropic.com,path=/v1/messages?beta=true,nth=1,fault=429,retry-after=2"
        )

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
            log.write_text(
                "\n".join(
                    json.dumps(record)
                    for record in (
                        {"kind": "compact.started"},
                        {"kind": "compact.finished", "outcome": "committed"},
                        {"kind": "compact.finished", "outcome": "failed"},
                    )
                )
                + "\nnot json\n"
            )
            records = live.session_records(home, "abc")
            self.assertEqual(live.committed_compactions(records), 1)
            self.assertEqual(live.session_records(home, "other"), [])

    def test_manual_compact_reports_failed_activity_before_uncorrelated_error(self):
        agent = live.Agent.__new__(live.Agent)
        agent.send = lambda kind: "2"
        events = (
            {"kind": "busy", "busy": True},
            {
                "kind": "tool",
                "activity_kind": "compact",
                "state": "failed",
                "subject": "compaction could not prepare",
            },
            {"kind": "busy", "busy": False},
            {"kind": "error", "id": "2", "message": "late correlated error"},
        )

        def wait_for(predicate, timeout):
            for event in events:
                if predicate(event):
                    return event
            self.fail("no command terminal")

        agent.wait_for = wait_for
        self.assertEqual(
            agent.command("compact"), {"kind": "error", "message": "compaction could not prepare"}
        )

    def test_manual_compact_failure_carries_its_noted_cause_to_classification(self):
        agent = live.Agent.__new__(live.Agent)
        agent.send = lambda kind: "3"
        events = (
            {"kind": "busy", "busy": True},
            {
                "kind": "tool",
                "activity_kind": "compact",
                "state": "failed",
                "subject": "compact failed",
            },
            {
                "kind": "status",
                "phase": "note",
                "id": "3",
                "detail": "compact workflow finished without a compacted checkpoint: RateLimited "
                "\"OpenRouter Messages HTTP 429 (post-acceptance)\"",
            },
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

    def test_isolated_driver_keeps_gate_env_off_the_plak_child(self):
        saved_environ = os.environ.copy()
        saved_source = live._plak_child_source
        try:
            os.environ["TMPDIR"] = "/operator/tmp"
            os.environ["GIT_CONFIG_GLOBAL"] = "/operator/gitconfig"
            os.environ.pop("GIT_CONFIG_NOSYSTEM", None)
            live._plak_child_source = None
            gate = {
                "TMPDIR": "/gate/tmp",
                "PLAK_HOME": "/gate/tmp/plak-isolated-home",
                "GIT_CONFIG_GLOBAL": "/gate/tmp/gitconfig",
                "GIT_CONFIG_NOSYSTEM": "1",
            }
            with patch.object(live.subprocess, "check_output", return_value=json.dumps(gate)):
                live.isolate_test_environment()
                live.isolate_test_environment()
            self.assertEqual(os.environ["TMPDIR"], "/gate/tmp")
            self.assertEqual(os.environ["GIT_CONFIG_NOSYSTEM"], "1")
            base = self.capture_workspace
            child = live.Env(base / "plakd", self.home(base), base).environ()
            self.assertEqual(child["TMPDIR"], "/operator/tmp")
            self.assertEqual(child["GIT_CONFIG_GLOBAL"], "/operator/gitconfig")
            self.assertNotIn("GIT_CONFIG_NOSYSTEM", child)
            self.assertEqual(child["PLAK_HOME"], str(self.home(base)))
            self.assertNotEqual(child["PLAK_HOME"], gate["PLAK_HOME"])
        finally:
            os.environ.clear()
            os.environ.update(saved_environ)
            live._plak_child_source = saved_source

    def test_environment_removes_override(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            env = live.Env(base / "plakd", self.home(base), base)
            source = {
                "PATH": "/bin",
                "OPENROUTER_API_KEY": "fixture-override",
                "PLAK_HOME": "stale-home",
                "PLAK_TEST_FAULTS": "stale-fault",
                "PLAK_TEST_EXCHANGE_CAPTURE_DIR": "stale-capture",
                "PLAK_LOG": "off",
            }
            child = env.environ(source=source)
            self.assertEqual(
                child,
                {
                    "PATH": "/bin",
                    "PLAK_HOME": str(self.home(base)),
                    "PLAK_TEST_EXCHANGE_CAPTURE_DIR": str(env.capture_segments[0]),
                    "PLAK_LOG": live.AUTHORITY_DIAGNOSTICS_FILTER,
                },
            )
            next_child = env.environ("explicit-fault", source=source)
            self.assertEqual(next_child["PLAK_TEST_FAULTS"], "explicit-fault")
            self.assertNotEqual(
                child["PLAK_TEST_EXCHANGE_CAPTURE_DIR"],
                next_child["PLAK_TEST_EXCHANGE_CAPTURE_DIR"],
            )
            for path in [env.capture_root.parent, env.capture_root, *env.capture_segments]:
                self.assertEqual(path.stat().st_mode & 0o777, 0o700)
                self.assertEqual(path.stat().st_uid, os.getuid())
                self.assertEqual(path, path.resolve())
            other = live.Env(env.binary, env.home, env.artifacts)
            self.assertNotEqual(env.capture_root, other.capture_root)

    def test_explicit_capture_segment_must_be_fresh_owned_and_private(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        base = Path(directory.name).resolve()
        env = live.Env(Path("plakd"), self.home(base), base)
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
        env = live.Env(Path("plakd"), self.home(self.capture_workspace), self.capture_workspace)
        with self.assertRaises(live.ScenarioFailure):
            env.environ(source={})
        root.rmdir()
        root.symlink_to(self.capture_workspace / "tests", target_is_directory=True)
        with self.assertRaises(live.ScenarioFailure):
            env.environ(source={})

    def test_smoke_cannot_qualify_missing_or_incomplete_raw_evidence(self):
        binary = self.capture_workspace / "plakd"
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

                with (
                    patch.object(live, "write_home"),
                    patch.object(live, "capture_root_mismatch", return_value=None),
                    patch.object(smoke.Smoke, "SCENARIOS", ((condition, "cli"),)),
                    patch.object(smoke.Smoke, condition, scenario, create=True),
                    patch.object(
                        smoke, "EXPECTED_PENDING", {"pending": "known", "not_started": "known"}
                    ),
                    patch("builtins.print"),
                ):
                    code = smoke.main(["--binary", str(binary)])
                self.assertEqual(code, 0 if condition == "complete" else 1)
                report = max(
                    (self.capture_workspace / "target/live").glob("*/report.json"),
                    key=lambda path: path.stat().st_mtime,
                )
                evidence = json.loads(report.read_text())
                self.assertEqual(evidence["capture_complete"], condition == "complete")
                self.assertEqual(
                    len(evidence["evidence"][condition]["segments"]),
                    0 if condition == "not_started" else 1,
                )

    def write_pending(self, segment, number=2, *, body=b"", dispatched_ago=300.0, body_ago=0.0):
        """A pending exchange; by default dispatched 300 s ago with no first byte."""
        name = f"exchange-{number:04}"
        now = time.time()
        request, body_path = segment / f"{name}.request", segment / f"{name}.body"
        request.write_bytes(b"synthetic request")
        body_path.write_bytes(body)
        os.utime(request, (now - dispatched_ago, now - dispatched_ago))
        os.utime(body_path, (now - body_ago, now - body_ago))
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={number}\nstatus=None\nterminal=pending\n"
            "recorded_bytes=0\nincomplete=true\n"
        )

    # The observed OpenRouter free-route timeline, from the window start: two
    # model lookups, inference exchanges, one in-band 504 and the transient
    # continuation's 30 s wait, and a fresh request pending at the kill.
    OBSERVED = (
        ("gap", 0.2),
        ("lookup", 0.43),
        ("gap", 0.02),
        ("lookup", 0.18),
        ("gap", 0.11),
        ("ok", 41.67, 178),
        ("gap", 0.04),
        ("ok", 40.01, 259),
        ("gap", 0.04),
        ("ok", 70.81, 633),
        ("gap", 0.05),
        ("ok", 42.32, 262),
        ("gap", 0.05),
        ("504", 24.45),
        ("gap", 30.06),
        ("ok", 40.62, 224),
        ("gap", 0.12),
        ("pending",),
    )
    IN_BAND_504 = (
        b": OPENROUTER PROCESSING\n\n"
        * 33
        + b'data: {"choices":[],"error":{"code":504,"message":"error code: 504",'
        b'"metadata":{"error_type":"timeout"}}}\n\n'
    )
    KEEP_ALIVE = b": OPENROUTER PROCESSING\n\n"
    # Output tokens a successful fixture exchange reports: 3 s at 15 tokens/s.
    TOKENS = 45

    def write_timeline(self, segment, steps, kill, timeout=300.0):
        """Exchanges laid out from the window start; returns the number of the last.

        Each inference request carries one conversation that grows by an item
        per exchange; `resend` repeats the previous conversation, `rollback`
        drops its last item and `compact` replaces it with a new, shorter one.
        A pending step takes an optional body and how long before the kill it
        was last written.
        """
        clock, number, items = kill - timeout, 0, [{"role": "user", "content": "task"}]
        for step in steps:
            kind = step[0]
            if kind == "gap":
                clock += step[1]
                continue
            number += 1
            name = f"exchange-{number:04}"
            request, body, info = (
                segment / f"{name}.{suffix}" for suffix in ("request", "body", "info")
            )
            if kind == "lookup":
                request.write_bytes(b"")
            else:
                if kind == "rollback":
                    items = items[:-1]
                elif kind == "compact":
                    items = [{"role": "user", "content": "summary"}]
                elif not kind.startswith("resend"):
                    items = [*items, {"role": "assistant", "content": f"round {number}"}]
                request.write_text(json.dumps({"messages": items}))
            if kind.endswith("pending"):
                payload = step[1] if len(step) > 1 else b""
                body.write_bytes(payload)
                info.write_text(
                    f"process_id=123\nexchange={number}\nstatus=None\n"
                    "terminal=pending\nrecorded_bytes=0\nincomplete=true\n"
                )
                for path in (request, info):
                    os.utime(path, (clock, clock))
                written = kill - step[2] if len(step) > 2 else clock
                os.utime(body, (written, written))
                return number
            tokens = step[2] if len(step) > 2 else self.TOKENS
            usage = (
                b""
                if tokens is None
                else (b'data: {"choices":[],"usage":{"completion_tokens":%d}}\n\n' % tokens)
            )
            answered = (
                b'data: {"choices":[{"index":0,"delta":{"content":"x"}}]}\n\n'
                + usage
                + b"data: [DONE]\n\n"
            )
            payload, status, terminal = {
                "lookup": (b'{"data":[]}', "Some(200)", "Complete"),
                "504": (self.IN_BAND_504, "Some(200)", "ClosedEarly"),
                "429": (b'{"error":{"code":429}}', "Some(429)", "Complete"),
                "abandoned": (b"data: {}\n\n", "Some(200)", "Abandoned"),
            }.get(kind.removeprefix("resend-"), (answered, "Some(200)", "Complete"))
            body.write_bytes(payload)
            info.write_text(
                f"process_id=123\nexchange={number}\nstatus={status}\n"
                f"connection=Some(Reused)\nterminal={terminal}\n"
                f"recorded_bytes={len(payload)}\nincomplete=false\n"
            )
            os.utime(request, (clock, clock))
            clock += step[1]
            for path in (body, info):
                os.utime(path, (clock, clock))
        return number

    def judge_timeline(self, steps, faults=None, route="or-flint-free"):
        base = self.capture_workspace
        env = live.Env(base / "plakd", self.home(base), base)
        env.route = live.ROUTES[route] if route else None
        env.environ(source={})
        segment = env.capture_segments[-1]
        kill = time.time()
        self.write_timeline(segment, steps, kill)
        failure = live.timeout_failure(
            env, segment, "plakd run did not finish in 300s", faults, timeout=300, killed_at=kill
        )
        self.assertEqual(
            env.provider_pending, {segment} if failure.outcome == live.PROVIDER else set()
        )
        return failure

    def test_observed_timeout_is_provider_time_plus_a_credited_wait(self):
        failure = self.judge_timeline(self.OBSERVED)
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertEqual(
            str(failure),
            (
                "plakd run did not finish in 300s: provider did not respond within the scenario "
                "budget (provider 140.0s, token-explained 129.3s, credited wait 30.1s, local 0.6s; "
                "exchange 9 pending)"
            ),
        )

    def test_local_time_over_the_allowance_is_harness(self):
        steps = list(self.OBSERVED)
        steps[5], steps[6] = ("ok", 11.67, 178), ("gap", 30.04)
        failure = self.judge_timeline(steps)
        self.assertEqual(failure.outcome, live.HARNESS)
        self.assertIn("local time exceeds 15s; exchange 9 pending", str(failure))

    def test_a_pending_exchange_the_product_left_stale_is_harness(self):
        for name, pending, route, expected, needle in (
            (
                "keep-alives 10 s before the kill",
                ("pending", self.KEEP_ALIVE, 10.0),
                "deepseek",
                live.PROVIDER,
                "exchange 2 pending",
            ),
            (
                "streaming stopped 70 s before the kill",
                ("pending", b"data: {}\n\n", 70.0),
                "deepseek",
                live.HARNESS,
                "received no byte in the 70s before the kill, more than 60s",
            ),
            (
                "a keep-alive 50 s before the kill",
                ("pending", self.KEEP_ALIVE, 50.0),
                "deepseek",
                live.PROVIDER,
                "exchange 2 pending",
            ),
            (
                "no first byte for 200 s on a 300 s route",
                ("pending",),
                "deepseek",
                live.PROVIDER,
                "exchange 2 pending",
            ),
            (
                "no first byte for 200 s on a 120 s route",
                ("pending",),
                "api-openai",
                live.HARNESS,
                "had no first byte for 200s, past the 120s dispatch deadline",
            ),
            (
                "no first byte for 200 s on an unknown route",
                ("pending",),
                None,
                live.HARNESS,
                "past the 120s dispatch deadline",
            ),
        ):
            with self.subTest(name):
                failure = self.judge_timeline((("ok", 100.0), pending), route=route)
                self.assertEqual(failure.outcome, expected)
                self.assertIn(needle, str(failure))

    def test_a_lookup_pending_past_its_cap_is_harness(self):
        base = self.capture_workspace
        segment = base / "segment"
        segment.mkdir()
        kill = time.time()
        self.write_timeline(segment, (("ok", 230.0), ("pending",)), kill)
        (segment / "exchange-0002.request").write_bytes(b"")
        os.utime(segment / "exchange-0002.request", (kill - 70.0, kill - 70.0))
        provider, account = live.budget_attribution(segment, kill, 300, 300.0)
        self.assertFalse(provider)
        self.assertIn("lookup exchange 2 pending 70s, past its 60s cap", account)

    def test_a_kill_inside_a_capped_provider_wait_is_provider(self):
        for error, into, expected in (
            ("504", 20.0, live.PROVIDER),
            ("429", 20.0, live.PROVIDER),
            ("504", 34.0, live.PROVIDER),
            ("504", 50.0, live.HARNESS),
            ("504", 90.0, live.HARNESS),
            ("ok", 20.0, live.HARNESS),
        ):
            with self.subTest(error=error, into=into):
                steps = (
                    ("gap", 0.5),
                    ("lookup", 0.4),
                    ("ok", 300 - 0.9 - 30 - into),
                    (error, 30.0),
                    ("gap", into),
                )
                failure = self.judge_timeline(steps)
                self.assertEqual(failure.outcome, expected)
                if expected == live.PROVIDER:
                    self.assertIn(
                        f"killed {into:.1f}s into the wait after HTTP {error} on exchange 3",
                        str(failure),
                    )

    def test_rounds_their_own_output_explains_are_not_a_provider_timeout(self):
        # A healthy reasoning model: 25 rounds of 11 s, each reporting 150
        # output tokens (reasoning included), then a fresh request.
        steps = [step for _ in range(25) for step in (("ok", 11.0, 150), ("gap", 0.1))]
        failure = self.judge_timeline((*steps[:-1], ("pending",)))
        self.assertEqual(failure.outcome, live.HARNESS)
        self.assertIn("the exchanges' own output, not the provider, used the budget", str(failure))
        # Short answers do not make the rounds the provider's: the first-token
        # allowance explains each round's wait before its output.
        steps = [step for _ in range(25) for step in (("ok", 11.0, 50), ("gap", 0.1))]
        self.assertEqual(self.judge_timeline((*steps[:-1], ("pending",))).outcome, live.HARNESS)
        # Without usage nothing of a successful exchange is the provider's.
        steps = [step for _ in range(25) for step in (("ok", 11.0, None), ("gap", 0.1))]
        self.assertEqual(self.judge_timeline((*steps[:-1], ("pending",))).outcome, live.HARNESS)

    def test_output_tokens_by_wire(self):
        for body, tokens in (
            (b'data: {"choices":[],"usage":{"completion_tokens":224}}\n\ndata: [DONE]\n\n', 224),
            (
                b'event: message_start\ndata: {"type":"message_start","message":{"usage":'
                b'{"output_tokens":1}}}\n\nevent: message_delta\ndata: {"type":"message_delta",'
                b'"usage":{"output_tokens":812}}\n\n',
                812,
            ),
            (
                b'event: response.completed\ndata: {"type":"response.completed","response":'
                b'{"usage":{"output_tokens":95,"output_tokens_details":{"reasoning_tokens":60}}}}\n\n',
                95,
            ),
            (b'{"choices":[],"usage":{"completion_tokens":7}}', 7),
            (b"data: {}\n\ndata: [DONE]\n\n", None),
        ):
            with self.subTest(body=body[:40]):
                self.assertEqual(live.output_tokens(body), tokens)

    def test_a_request_loop_or_an_unfinished_exchange_is_harness(self):
        for name, steps, expected, needle in (
            (
                "an identical resend after a completed exchange",
                (("ok", 100.0), ("resend", 100.0), ("pending",)),
                live.HARNESS,
                "exchange 2 resent exchange 1's conversation without a provider failure",
            ),
            (
                "a rollback to an earlier conversation",
                (("ok", 100.0), ("ok", 100.0), ("rollback", 50.0), ("pending",)),
                live.HARNESS,
                "exchange 3 resent exchange 1's conversation",
            ),
            (
                "an identical resend right after a provider error",
                (("ok", 100.0), ("504", 100.0), ("gap", 30.0), ("resend", 30.0), ("pending",)),
                live.PROVIDER,
                "exchange 4 pending",
            ),
            (
                "a retry without backoff",
                (("ok", 100.0), ("504", 100.0), ("gap", 0.1), ("resend", 60.0), ("pending",)),
                live.HARNESS,
                "exchange 3 retried exchange 2 without backoff",
            ),
            (
                "a compaction that shrinks the conversation",
                (("ok", 100.0), ("ok", 100.0), ("compact", 50.0), ("pending",)),
                live.PROVIDER,
                "exchange 4 pending",
            ),
            (
                "an abandoned exchange in the window",
                (("ok", 100.0), ("abandoned", 100.0), ("pending",)),
                live.HARNESS,
                "exchange 2 ended without a protocol terminal or provider failure",
            ),
            (
                "pending after the provider finished",
                (("ok", 290.0), ("pending", b"data: {}\n\ndata: [DONE]\n\n", 1.0)),
                live.HARNESS,
                "pending after the provider finished",
            ),
            (
                "a hang after a completed exchange",
                (("ok", 200.0),),
                live.HARNESS,
                "killed 100.0s after exchange 1 ended",
            ),
        ):
            with self.subTest(name):
                failure = self.judge_timeline(steps)
                self.assertEqual(failure.outcome, expected)
                self.assertIn(needle, str(failure))

    def test_identical_resends_are_bounded_by_the_retry_budget(self):
        for resends, expected in ((3, live.PROVIDER), (4, live.HARNESS)):
            with self.subTest(resends=resends):
                steps = [("ok", 200.0), ("504", 30.0)]
                steps += [
                    step for _ in range(resends - 1) for step in (("gap", 1.0), ("resend-504", 2.0))
                ]
                steps += [("gap", 1.0), ("resend-pending",)]
                failure = self.judge_timeline(steps)
                self.assertEqual(failure.outcome, expected, str(failure))
                if expected == live.HARNESS:
                    self.assertIn("resent one request 4 times", str(failure))

    def test_too_many_lookups_in_the_window_are_harness(self):
        steps = [step for _ in range(7) for step in (("lookup", 0.2), ("gap", 0.1))]
        failure = self.judge_timeline((*steps, ("ok", 150.0), ("pending",)), route="deepseek")
        self.assertEqual(failure.outcome, live.HARNESS)
        self.assertIn("7 model lookups in the window", str(failure))

    def test_injected_faults_and_missing_captures_keep_a_timeout_harness(self):
        failure = self.judge_timeline(self.OBSERVED, faults="fault=stall,seconds=40")
        self.assertEqual(
            (failure.outcome, str(failure)), (live.HARNESS, "plakd run did not finish in 300s")
        )
        env = live.Env(
            self.capture_workspace / "plakd",
            self.capture_workspace / "home",
            self.capture_workspace,
        )
        missing = live.timeout_failure(env, self.capture_workspace / "missing", "x", timeout=300)
        self.assertEqual((missing.outcome, str(missing)), (live.HARNESS, "x (capture incomplete)"))
        self.assertEqual(env.provider_pending, set())

    def test_children_sharing_a_segment_unite_their_provider_spans(self):
        base = self.capture_workspace
        segment = base / "segment"
        segment.mkdir()
        kill = time.time()
        self.write_timeline(segment, (("ok", 150.0), ("ok", 1.0), ("pending",)), kill)
        # Exchange 2 becomes a child's own conversation, overlapping the parent's.
        child = segment / "exchange-0002.request"
        child.write_text(json.dumps({"messages": [{"role": "user", "content": "child task"}]}))
        os.utime(child, (kill - 120.0, kill - 120.0))
        for suffix in (".body", ".info"):
            os.utime(segment / f"exchange-0002{suffix}", (kill - 60.0, kill - 60.0))
        provider, account = live.budget_attribution(segment, kill, 300, 300.0)
        self.assertTrue(provider, account)
        self.assertIn(
            "token-explained 8.0s, credited wait 0.0s, local 1.0s; exchange 3 pending", account
        )

    def test_in_band_provider_errors_by_wire(self):
        for event, status in (
            (b'data: {"error":{"code":503}}', 503),
            (b'data: {"error":{"code":400}}', None),
            (b'event: error\ndata: {"type":"error","error":{"type":"overloaded_error"}}', 529),
            (b'event: error\ndata: {"type":"error","error":{"type":"rate_limit_error"}}', 429),
            (
                b'event: error\ndata: {"type":"error","error":{"type":"invalid_request_error"}}',
                None,
            ),
            (
                b'event: response.failed\ndata: {"type":"response.failed","response":'
                b'{"error":{"code":"server_error"}}}',
                500,
            ),
            (b'event: error\ndata: {"type":"error","code":"rate_limit_exceeded"}', 429),
            (
                b'data: {"type":"response.failed","response":{"error":{"code":"invalid_prompt"}}}',
                None,
            ),
        ):
            with self.subTest(event=event):
                self.assertEqual(live.event_error_status(event), status)
        fields = {"status": "Some(200)", "terminal": "Complete"}
        self.assertEqual(
            live.exchange_failure(fields, b'data: {"error":{"code":502}}\n\ndata: [DONE]\n\n'),
            "HTTP 502",
        )

        def terminal(kind, reset="None", phase="ResponseBody"):
            return (
                f"Failed(Failure {{ phase: {phase}, transmission: MayHaveTransmitted, "
                f"kind: {kind}, cause: None, http2_reason: {reset}, os_error: None, tls: None, "
                "tls_alert: None, alpn: None, http2_io: false, connection: None, idle_age: None, connect_attempts: None })"
            )

        for kind, reset, phase, label in (
            ("Io", "None", "ResponseBody", "a transport failure (Io) in ResponseBody"),
            ("Tls", "None", "ResponseHead", "a transport failure (Tls) in ResponseHead"),
            (
                "DeadlineExceeded",
                "None",
                "ResponseBody",
                "a transport failure (DeadlineExceeded) in ResponseBody",
            ),
            (
                "Protocol",
                "Some(2)",
                "ResponseBody",
                "a transport failure (Protocol) in ResponseBody",
            ),
            ("Protocol", "None", "ResponseBody", None),
            ("Internal", "None", "ResponseBody", None),
            ("LimitExceeded(ResponseBodyBytes)", "None", "ResponseBody", None),
            ("InvalidResponse", "None", "ResponseHead", None),
            ("Cancelled", "None", "ResponseBody", None),
            ("Io", "None", "Connect", None),
        ):
            with self.subTest(kind=kind, reset=reset, phase=phase):
                self.assertEqual(
                    live.exchange_failure(
                        {"status": "Some(200)", "terminal": terminal(kind, reset, phase)}, b""
                    ),
                    label,
                )
        self.assertEqual(
            live.exchange_failure({"status": "Some(408)", "terminal": "Complete"}, b""), "HTTP 408"
        )

    def timed_out_run(self, env, *, pending: bool, faults: str | None = None):
        """`plakd run` killed at its timeout, its capture as the process left it."""

        def run(*args, **kwargs):
            segment = Path(kwargs["env"]["PLAK_TEST_EXCHANGE_CAPTURE_DIR"])
            steps = (("ok", 100.0), ("pending",)) if pending else (("ok", 100.0),)
            self.write_timeline(segment, steps, time.time())
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
            (segment / "exchange-0002.info").read_text().replace("exchange=2", "exchange=10")
        )
        self.assertEqual(live.pending_exchange(segment), 10)
        self.assertFalse(live.capture_integrity([segment], [segment]))
        for name in ("exchange-0002", "exchange-0010"):
            for kind in ("request", "body", "info"):
                (segment / f"{name}.{kind}").unlink()
        info = segment / "exchange-0001.info"
        self.write_pending(segment, 1)
        valid = info.read_text()
        for metadata in (
            valid.replace("terminal=pending", "terminal=Complete"),
            valid.replace("incomplete=true", "incomplete=false"),
            valid.replace("status=None", "status=Some(200)"),
            valid.replace("exchange=1", "exchange=2"),
            valid + "terminal=pending\n",
        ):
            with self.subTest(metadata=metadata):
                info.write_text(metadata)
                self.assertIsNone(live.pending_exchange(segment))
        info.write_text(valid)
        (segment / "exchange-0001.request").unlink()
        self.assertIsNone(live.pending_exchange(segment))

    def test_run_cli_timeout_on_a_pending_exchange_is_provider(self):
        base = self.capture_workspace
        env = live.Env(base / "plakd", self.home(base), base)
        env.route = live.ROUTES["grok"]
        failure = self.timed_out_run(env, pending=True)
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertRegex(
            str(failure),
            r"^plakd run did not finish in 300s: provider did not "
            r"respond within the scenario budget \(provider 292\.0s, "
            r"token-explained 8\.0s, credited wait 0\.0s, local 0\.0s; "
            r"exchange 2 pending\)$",
        )
        self.assertEqual(env.provider_pending, {env.capture_segments[-1]})
        self.assertEqual(
            live.provider_pending(env, env.capture_segments, live.PROVIDER), env.capture_segments
        )
        self.assertEqual(live.provider_pending(env, env.capture_segments, live.HARNESS), [])

    def test_run_cli_timeout_without_a_pending_exchange_is_harness(self):
        base = self.capture_workspace
        env = live.Env(base / "plakd", self.home(base), base)
        failure = self.timed_out_run(env, pending=False)
        self.assertEqual(failure.outcome, live.HARNESS)
        self.assertRegex(
            str(failure),
            r"^plakd run did not finish in 300s \(provider 92\.0s, .*"
            r"killed 200\.0s after exchange 1 ended\)$",
        )
        # A fault the scenario injected must be absorbed, so it never relabels a hang.
        failure = self.timed_out_run(env, pending=True, faults="fault=stall,seconds=40")
        self.assertEqual(failure.outcome, live.HARNESS)
        self.assertEqual(env.provider_pending, set())

    def test_turn_timeout_on_a_pending_exchange_is_provider(self):
        base = self.capture_workspace
        for pending, faults, expected in (
            (True, None, live.PROVIDER),
            (False, None, live.HARNESS),
            (True, "fault=stall", live.HARNESS),
        ):
            with self.subTest(pending=pending, faults=faults):
                env = live.Env(base / "plakd", self.home(base), base)
                env.route = live.ROUTES["grok"]
                env.environ(source={})
                segment = env.capture_segments[-1]
                self.write_capture(segment)
                if pending:
                    self.write_pending(segment, dispatched_ago=100.0)
                agent = live.Agent.__new__(live.Agent)
                agent.env, agent.capture_dir, agent.lookup_faults = env, segment, faults
                agent.events = queue.Queue()
                with (
                    patch.object(agent, "send", return_value="1"),
                    self.assertRaises(live.ScenarioFailure) as failure,
                ):
                    agent.prompt("hi", timeout=0.05)
                self.assertEqual(failure.exception.outcome, expected)
                self.assertTrue(str(failure.exception).startswith("turn: no expected event within"))
                self.assertEqual(
                    env.provider_pending, {segment} if expected == live.PROVIDER else set()
                )

    def test_provider_timeout_keeps_the_cell_provider_and_a_hang_stays_harness(self):
        binary = self.capture_workspace / "plakd"
        binary.write_bytes(b"synthetic executable; never launched")
        for pending in (True, False):
            with self.subTest(pending=pending):

                def scenario(runner):
                    self.timed_out_run(runner.env, pending=pending)
                    raise self.timed_out_run(runner.env, pending=pending)

                with (
                    patch.object(live, "write_home"),
                    patch.object(live, "capture_root_mismatch", return_value=None),
                    patch.object(smoke.Smoke, "SCENARIOS", (("timeout", "cli"),)),
                    patch.object(smoke.Smoke, "timeout", scenario, create=True),
                    patch("builtins.print"),
                ):
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

    def test_drivers_default_to_maintainer_and_remove_capture_opt_in(self):
        def inspect(parser, argv):
            self.assertEqual(parser.get_default("binary"), live.ROOT / "target/maintainer/plakd")
            self.assertNotIn("--capture-exchanges", parser._option_string_actions)
            raise RuntimeError("stop before any launch")

        for driver in (smoke, soak):
            with (
                self.subTest(driver=driver.__name__),
                patch.object(driver.argparse.ArgumentParser, "parse_args", inspect),
            ):
                with self.assertRaisesRegex(RuntimeError, "stop before any launch"):
                    driver.main([])
        self.assertIn("--maintainer", soak.REBUILD)

    def test_wrap_up_stays_out_of_the_default_smoke(self):
        default = [name for name, _ in smoke.Smoke.SCENARIOS]
        self.assertNotIn("wrap_up", default)
        self.assertNotIn("py_ptc", default)
        self.assertEqual([name for name, _ in smoke.Smoke.OPT_IN], ["wrap_up", "py_ptc"])
        self.assertEqual([name for name, _ in smoke.selected_scenarios({"py_ptc"})], ["py_ptc"])
        self.assertEqual([name for name, _ in smoke.selected_scenarios(set())], default)
        self.assertEqual([name for name, _ in smoke.selected_scenarios({"wrap_up"})], ["wrap_up"])
        self.assertEqual(smoke.selected_scenarios({"missing"}), ())
        self.assertEqual([name for name, _ in smoke.selected_scenarios({"wrap_up"})], ["wrap_up"])

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
            ([{**answer[0], "text": smoke.PY_PTC_TOKEN}], exchanges, "reversed token"),
        ):
            with self.subTest(reason=reason), self.assertRaisesRegex(live.ScenarioFailure, reason):
                smoke._require_py_ptc_oracle("", records, captured)

    def test_streamed_text_reassembles_messages_tool_input(self):
        fragments = ['{"code": "print(TOK', 'EN[:', ':-1])"}']
        events = [
            {
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "name": "exec_py", "input": {}},
            }
        ]
        events += [
            {
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": part},
            }
            for part in fragments
        ]
        events.append(
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "handled "},
            }
        )
        events.append(
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "ValueError"},
            }
        )
        body = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events)
        self.assertNotIn("TOKEN[::-1]", body)
        text = smoke.streamed_text(body)
        self.assertIn("TOKEN[::-1]", text)
        self.assertIn("handled ValueError", text)
        self.assertEqual(smoke.streamed_text("not a stream"), "not a stream")

    def test_run_cli_can_use_a_private_home(self):
        base = self.capture_workspace
        env = live.Env(base / "plakd", self.home(base), base)
        private = self.home(base, "private")
        with patch.object(
            live.subprocess, "run", return_value=Mock(returncode=0, stdout="ok", stderr="")
        ) as run:
            live.run_cli(env, base, ["--", "hi"], base / "run.log", home=private)
        self.assertEqual(run.call_args.kwargs["env"]["PLAK_HOME"], str(private))
        self.assertIn("PLAK_TEST_EXCHANGE_CAPTURE_DIR", run.call_args.kwargs["env"])

    def test_run_cli_closes_stdin_and_scores_a_truncated_answer_as_completed(self):
        base = self.capture_workspace
        env = live.Env(base / "plakd", self.home(base), base)
        truncated = "plakd: answer truncated at its 64-token output limit\nplakd: turn_failed:output-limit\n"
        with patch.object(
            live.subprocess,
            "run",
            return_value=Mock(returncode=5, stdout="MAGENTA\n", stderr=truncated),
        ) as run:
            code, out = live.run_cli(env, base, ["--", "hi"], base / "run.log")
        self.assertIs(run.call_args.kwargs["stdin"], live.subprocess.DEVNULL)
        self.assertEqual((code, out), (0, "MAGENTA\n"))
        log = (base / "run.log").read_text()
        self.assertTrue(log.startswith("exit 5 (answer committed at its output limit"))
        self.assertIn("turn_failed:output-limit", log)
        # The line must be the final status; any other exit 5 stays a failure.
        for code, stderr in (
            (5, "plakd: turn_failed:output-limit\nplakd: agent turn failed: x\n"),
            (1, truncated),
            (5, "plakd: agent turn failed: x\n"),
        ):
            with patch.object(
                live.subprocess, "run", return_value=Mock(returncode=code, stdout="", stderr=stderr)
            ):
                self.assertEqual(live.run_cli(env, base, [], base / "run.log")[0], code)

    def test_a_providers_in_band_5xx_or_429_is_provider_on_every_surface(self):
        text = (
            "agent turn failed: OpenRouter Chat operation failed: OpenRouter returned an "
            "in-band error after accepting the request: HTTP 504 (timeout) from Upstream "
            "[1 requests; last: timeout] · HTTP 504"
        )
        self.assertEqual(live.in_band_provider_status(text), 504)
        self.assertIsNone(live.stream_validation_outcome(text, None))
        for kind, status in (("Timeout", 504), (None, None)):
            turn = live.Turn(
                phase="failed", model_error_kind=kind, http_status=status, errors=[text]
            )
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_incomplete(turn, "coding turn", segments=[])
            self.assertEqual(raised.exception.outcome, live.PROVIDER, kind)
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, f"plakd: {text}\n", "cli_task", segments=[])
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        self.assertTrue(live.provider_child_text(text))
        prefix = "OpenRouter returned an in-band error after accepting the request: "
        for status in (429, 502, 520, 521, 522, 523, 524):
            self.assertEqual(
                live.in_band_provider_status(
                    f"{prefix}HTTP {status} (unavailable): Provider returned error"
                ),
                status,
            )
        for other in (
            prefix + "HTTP 400: bad request",
            prefix + "HTTP 401",
            prefix + "Provider returned error",
            prefix + "Provider returned error (from Upstream)",
            prefix + "unclassified",
            "OpenRouter Chat stream validation failed: Chat chunk omitted model",
        ):
            self.assertIsNone(live.in_band_provider_status(other), other)
        other = live.Turn(phase="failed", model_error_kind="Protocol", errors=[prefix + "HTTP 400"])
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(other, "coding turn", segments=[])
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        self.assertEqual(live.provider_reason_from(None, 504, ""), "504")

    def test_an_attempt_summary_label_never_relabels_a_product_failure(self):
        # A replay that met a rate limit and then ended on a 400: whatever the
        # summary says, the terminal is the 400.
        text = (
            "agent turn failed: OpenRouter Chat operation failed: OpenRouter Chat HTTP 400 "
            "(acceptance-unknown): Invalid request [2 requests; last: rate limited]"
        )
        self.assertFalse(live.text_is_provider_refusal(text))
        self.assertFalse(live.provider_child_text(text))
        self.assertEqual(live.without_attempt_summary("a [2 requests; last: timeout] b"), "a  b")
        for kind, status in (("Protocol", 400), (None, 400), (None, None)):
            turn = live.Turn(
                phase="failed", model_error_kind=kind, http_status=status, errors=[text]
            )
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_incomplete(turn, "coding turn", segments=[])
            self.assertEqual(raised.exception.outcome, live.HARNESS, (kind, status))
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_cli_failure(5, f"plakd: {text}\n", "cli_task", segments=[])
        self.assertEqual(raised.exception.outcome, live.HARNESS)
        # A typed non-provider status outranks provider prose.
        self.assertIsNone(live.provider_reason_from(None, 400, "too many requests"))

    def test_typed_terminals_do_not_classify_quoted_product_prose(self):
        misleading = (
            "model lookup failed after 3 attempts: HTTP 503 "
            "returned an in-band error after accepting the request: HTTP 504 (timeout) "
            "response reached its output bound before completion "
            "[2 requests; last: rate limited]"
        )
        self.assertIsNone(live.provider_reason_from("Protocol", None, misleading))
        self.assertFalse(live.output_limit_failure("Protocol", misleading))
        self.assertEqual(
            live.provider_reason_from("Unavailable", 503, "renamed diagnostic"), "Unavailable 503"
        )
        turn = live.Turn(
            phase="failed", model_error_kind="Transport", errors=["renamed diagnostic"]
        )
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(turn, "turn")
        self.assertEqual(raised.exception.model_error_kind, "Transport")

    def test_a_typed_in_band_provider_error_without_a_rendered_status_is_provider(self):
        # Text cut before its status still carries the typed kind.
        text = (
            "agent turn failed: OpenRouter Chat operation failed: OpenRouter returned an "
            "in-band error after accepting the request: Provider returned error"
        )
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
        self.assertIsNone(
            live.in_band_provider_status(
                prefix + "Provider returned error [1 requests; last: x] compact failed: HTTP 502"
            )
        )
        self.assertIsNone(live.in_band_provider_status(prefix + "HTTP 5025"))
        self.assertIsNone(
            live.in_band_provider_status("Chat provider returned an in-band error: error code: 504")
        )
        self.assertEqual(
            live.in_band_provider_status(prefix + "HTTP 503 (unavailable): type=server_error"), 503
        )
        # The status counts only with the class the product typed it as; a
        # quota 429 carries none and is provider through its typed kind.
        for unclassed in (
            prefix + "HTTP 503",
            prefix + "HTTP 429: type=insufficient_quota",
            prefix + "HTTP 502 (overloaded)",
        ):
            self.assertIsNone(live.in_band_provider_status(unclassed), unclassed)
        quota = live.Turn(
            phase="failed",
            model_error_kind="QuotaExceeded",
            http_status=429,
            errors=[prefix + "HTTP 429: type=insufficient_quota"],
        )
        with self.assertRaises(live.ScenarioFailure) as raised:
            live.classify_incomplete(quota, "coding turn", segments=[])
        self.assertEqual(raised.exception.outcome, live.PROVIDER)
        text = prefix + "HTTP 429 (rate limited)"
        self.assertFalse(live._absorbed_injection("RateLimited", None, "429", text))
        self.assertTrue(
            live._absorbed_injection("RateLimited", 429, "429", "provider rate limited")
        )

    def test_unaccepted_compatibility_notice_names_its_remedy(self):
        for stderr in (
            "plakd: the codex compatibility notice needs an answer on stdin and none came; "
            "accept it once with `plakd models codex` from a terminal, or pipe `y` to this command\n",
            "plakd: the codex compatibility notice is not accepted; run `plakd models codex` to "
            "review and accept it (the login is kept)\n",
        ):
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.classify_cli_failure(3, stderr, "cli_task")
            self.assertEqual(raised.exception.outcome, live.HARNESS)
            self.assertIn("compatibility notice is not accepted", str(raised.exception))
            self.assertIn("`plakd models codex`", str(raised.exception))

    def test_wrap_up_oracle_rejects_binding_loss_and_an_empty_answer(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "run.log"
            log.write_text("exit 1\n--- stdout\n\n--- stderr\nstable-prefix binding differs\n")
            with self.assertRaisesRegex(live.ScenarioFailure, "stable-prefix binding differs"):
                smoke._reject_binding_loss("wrap-up turn", log, "", [])
            log.write_text("exit 0\n--- stdout\nDONE\n--- stderr\n")
            smoke._reject_binding_loss(
                "wrap-up turn", log, "DONE", [{"kind": "turn.assistant", "text": "DONE"}]
            )
            with self.assertRaisesRegex(live.ScenarioFailure, "ProtectedHistory"):
                smoke._reject_binding_loss(
                    "wrap-up turn", log, "DONE", [{"kind": "error", "msg": "ProtectedHistory"}]
                )
            with self.assertRaisesRegex(live.ScenarioFailure, "without assistant text"):
                smoke._require_cli_answer("session_id=abc\n", [], "wrap-up turn")
            smoke._require_cli_answer("session_id=abc\nDONE\n", [], "wrap-up turn")
            smoke._require_cli_answer(
                "session_id=abc\n", [{"kind": "turn.assistant", "text": "DONE"}], "wrap-up turn"
            )

    def test_headless_soak_records_reopen_failure_instead_of_losing_report(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            env = live.Env(base / "plakd", self.home(base), base)
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
            env = live.Env(base / "plakd", self.home(base), base)
            runner = soak.Soak(live.ROUTES["grok"], env, base)
            agent = Mock(proc=Mock(pid=1, poll=Mock(return_value=0)), capture_dir=base)
            agent.prompt.return_value = live.Turn(phase="failed", model_error_kind="OutputLimit")
            runner.open = lambda _faults: agent
            record = runner.run_task(soak.Task("explore", "inspect"), 1, None, False)
        self.assertEqual(record.outcome, "model")
        self.assertFalse(record.dead_end)
        self.assertEqual(agent.prompt.call_count, 1)

    def test_fixture_git_ignores_the_operators_global_signing_config(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            operator = base / "operator.gitconfig"
            operator.write_text("[commit]\n\tgpgsign = true\n[gpg]\n\tprogram = false\n")
            with patch.dict(os.environ, {"GIT_CONFIG_GLOBAL": str(operator)}):
                live.git_repo(base / "repo", {"a.txt": "a\n"})
                (base / "repo" / "b.txt").write_text("b\n")
                long_session.commit_extra(base / "repo", "more")
            log = soak.subprocess.run(
                ["git", "log", "--format=%G?", "-n", "2"],
                cwd=base / "repo",
                check=True,
                capture_output=True,
                text=True,
            ).stdout.split()
            self.assertEqual(log, ["N", "N"])

    def test_plant_resets_a_differently_fixed_function_and_keeps_later_code(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "repo"
            _path, name, good, bad = soak.BUGS["ilen"]
            live.git_repo(
                repo,
                {"more.py": f"def first():\n    return 1\n\n\ndef ilen(iterable):\n    {good}\n"},
            )
            source = repo / "more.py"
            pinned = soak.subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True, text=True
            ).stdout.strip()
            # An earlier cycle fixed ilen in other words and added a function.
            source.write_text(
                "def first():\n    return 1\n\n\n@staticmethod\ndef ilen(iterable):\n"
                "    return sum(1 for _ in iterable)\n\n\n"
                "def every_nth_1(iterable, n):\n    return iterable[::n]\n"
            )
            with (
                patch.object(soak, "REPO_COMMIT", pinned),
                patch.dict(soak.BUGS, {"ilen": ("more.py", name, good, bad)}),
            ):
                soak.plant(repo, "ilen")
                text = source.read_text()
                self.assertIn(bad, text)
                self.assertNotIn("sum(1 for _", text)
                self.assertNotIn("@staticmethod", text)
                self.assertIn("def every_nth_1(iterable, n):", text)
                self.assertIn("def first():", text)

                # A plant that cannot run is that task's harness result, not a crash.
                source.write_text("def ilen(:\n")
                env = live.Env(repo / "plakd", repo / "home", Path(directory))
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
            self.assertEqual(
                soak.segment_evidence(capture, diagnostics, 0), (0, [], "diagnostics_missing")
            )
            diagnostics.write_text("old predicate=route.catalog_time\n")
            diagnostics.chmod(0o600)
            offset = diagnostics.stat().st_size
            self.assertEqual(
                soak.segment_evidence(capture, diagnostics, offset),
                (0, [], "unexplained_no_response"),
            )
            diagnostics.write_text(
                diagnostics.read_text() + 'predicate="native.route_current.generation" '
                'private=do-not-report\n'
            )
            self.assertEqual(
                soak.segment_evidence(capture, diagnostics, offset),
                (0, ["native.route_current.generation"], "authority_witness_no_response"),
            )
            (capture / "exchange-0001.info").write_text("incomplete=false\n")
            self.assertEqual(
                soak.segment_evidence(capture, diagnostics, offset),
                (1, ["native.route_current.generation"], "response"),
            )
            diagnostics.chmod(0o644)
            self.assertEqual(
                soak.segment_evidence(capture, diagnostics, offset), (1, [], "diagnostics_missing")
            )

    def write_capture(
        self,
        segment,
        name="exchange-0001",
        body=b"response",
        terminal="Complete",
        status="Some(200)",
    ):
        (segment / f"{name}.request").write_bytes(b"synthetic request")
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={int(name.split('-')[1])}\n"
            f"status={status}\nconnection=Some(Reused)\nterminal={terminal}\n"
            f"recorded_bytes={len(body)}\n"
            "incomplete=false\n"
        )

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
                "",
                "incomplete=false\n",
                "not metadata\n",
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
            attempts = ("None", "Some(ConnectAttempts { ipv4: 1, ipv6: 1 })")
            for kind, connect in [
                (kind, connect)
                for kind in ("Io", "LimitExceeded(ResponseBodyBytes)")
                for connect in attempts
            ]:
                terminal = (
                    "Failed(Failure { phase: ResponseBody, "
                    f"transmission: MayHaveTransmitted, kind: {kind}, "
                    "cause: Some(\"unexpected_eof\"), http2_reason: None, "
                    "os_error: None, tls: None, tls_alert: None, alpn: Some(\"h2\"), "
                    "http2_io: true, connection: Some(Fresh), "
                    f"idle_age: Some(1.5s), connect_attempts: {connect} }})"
                )
                for body in (b"partial response", b""):
                    with self.subTest(kind=kind, connect=connect, body=body):
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
            binary = Path(directory) / "plakd"
            binary.write_bytes(b"first binary")
            first = live.binary_sha256(binary)
            binary.write_bytes(b"second binary")
            self.assertNotEqual(first, live.binary_sha256(binary))


if __name__ == "__main__":
    unittest.main()


class AgentStartupFailureTests(unittest.TestCase):
    """A startup failure is read from stderr only after the process exits."""

    def test_the_final_stderr_line_after_bye_decides_the_class(self):
        # `bye` reaches stdout before the exiting process writes its final
        # stderr line, here an exhausted lookup on a provider timeout.
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            binary = base / "plakd"
            binary.write_text(
                "#!/bin/sh\n"
                "echo 'retrying model lookup (2/3): HTTP transport failed in ResponseHead "
                "(MayHaveTransmitted, DeadlineExceeded)' >&2\n"
                "echo '{\"kind\": \"bye\", \"v\": 1}'\n"
                "sleep 1\n"
                "echo 'plakd: resolve codex/gpt-6.1-sol: model lookup failed after 2 attempts: "
                "transport timeout' >&2\n"
                "exit 5\n"
            )
            binary.chmod(0o755)
            home = live.scratch_home(base)
            self.addCleanup(shutil.rmtree, home)
            env = live.Env(binary, home, base, owns_gateway=False)
            with self.assertRaises(live.ScenarioFailure) as raised:
                live.Agent(env, base, "startup", args=[])
            self.assertEqual(raised.exception.outcome, live.PROVIDER)
            self.assertIn(
                "model lookup failed after 2 attempts: transport timeout", str(raised.exception)
            )


class SmokeEvidenceTests(unittest.TestCase):
    def test_smoke_compaction_accepts_context_limit_then_capacity_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            driver, agent = self.compaction_driver(Path(directory))
            driver.route = live.ROUTES["or-glm"]
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
                self.assertEqual(
                    driver.compaction(), "capacity recovery compaction, context carried"
                )
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
                ("or-glm", [started, failed, committed]),
                ("or-glm", [started, committed]),
                ("deepseek", [dict(failed, phase="completed"), started, committed]),
            ):
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
                self.assertEqual(
                    driver.compaction(), "capacity recovery compaction, context carried"
                )

    def test_smoke_context_limit_without_capacity_commit_stays_harness(self):
        with tempfile.TemporaryDirectory() as directory:
            for rows in (
                [],
                [{"kind": "compact.finished", "outcome": "committed"}],
                [
                    {"kind": "compact.started", "reason": "capacity"},
                    {"kind": "compact.finished", "outcome": "failed"},
                ],
            ):
                driver, agent = self.compaction_driver(Path(directory))
                agent.prompt.return_value = live.Turn(
                    phase="failed", model_error_kind="ContextLimit"
                )
                with patch.object(long_session, "poll_records", return_value=rows):
                    with self.assertRaises(live.ScenarioFailure) as raised:
                        driver.compaction()
                self.assertEqual(raised.exception.outcome, live.HARNESS)
                self.assertEqual(agent.prompt.call_count, 1)

    def test_durable_terminal_classifies_only_exhausted_provider_retries(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            telemetry = home / "telemetry"
            telemetry.mkdir()
            with (
                closing(sqlite3.connect(telemetry / "telemetry-v14.sqlite3")) as connection,
                connection,
            ):
                connection.execute(
                    "CREATE TABLE model_retries "
                    "(turn_id TEXT, cause_kind TEXT, "
                    "next_attempt INTEGER, max_attempts INTEGER, declined_reason TEXT)"
                )
                connection.executemany(
                    "INSERT INTO model_retries VALUES (?, ?, ?, ?, ?)",
                    [
                        ("transient", "http_status", 2, 4, None),
                        ("exhausted", "http_status", 4, 4, None),
                        ("other-turn", "http_status", 4, 4, None),
                        ("declined-budget", "http_status", 4, 4, None),
                        ("declined-budget", "http_status", 5, 4, "budget"),
                        ("declined-deadline", "http_status", 4, 4, "deadline"),
                    ],
                )
            for turn_id, outcome in (
                ("transient", live.HARNESS),
                ("exhausted", live.PROVIDER),
                ("declined-budget", live.PROVIDER),
                ("declined-deadline", live.HARNESS),
                ("no-retries", live.HARNESS),
            ):
                with self.subTest(turn_id=turn_id):
                    with self.assertRaises(live.ScenarioFailure) as raised:
                        smoke.require_durable_completed(
                            {"phase": "failed", "turn_id": turn_id}, "durable turn", home=home
                        )
                    self.assertEqual(raised.exception.outcome, outcome)
            with self.assertRaises(sqlite3.ProgrammingError):
                connection.execute("SELECT 1")

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
            with patch.object(
                live,
                "session_records",
                return_value=[{"kind": "compact.finished", "outcome": "committed"}],
            ):
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
                "Read fold.txt in full with one read. Remember the code word ZEBRA-42. Reply: ok",
            )

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
            with patch.object(
                live,
                "session_records",
                return_value=[{"kind": "compact.finished", "outcome": "committed"}],
            ):
                self.assertIn("context carried", driver.compaction())
            self.assertEqual(agent.prompt.call_count, 3)
            agent.command.assert_called_once_with("compact")


class ArgumentCaptureTests(unittest.TestCase):
    def test_fault_reset_does_not_pass_a_capture_proven_model_argument_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, self.chat_arguments("{"))
            turn = live.Turn(
                phase="failed",
                model_error_kind="Protocol",
                errors=[
                    "OpenRouter Chat stream validation failed: "
                    "tool arguments are malformed or ambiguous",
                    "stream reset observed",
                ],
            )
            with self.assertRaises(live.ScenarioFailure) as raised:
                smoke.judge_fault_reset(turn, segment / "hello.txt", [segment])
            self.assertEqual(raised.exception.outcome, live.MODEL)

    def write_capture(self, segment, events, exchange=1):
        body = "".join(f"data: {json.dumps(event)}\n\n" for event in events).encode()
        name = f"exchange-{exchange:04}"
        (segment / f"{name}.request").write_bytes(b"synthetic request")
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={exchange}\nstatus=Some(200)\nconnection=Some(Fresh)\n"
            f"terminal=Complete\nrecorded_bytes={len(body)}\nincomplete=false\n"
        )

    def chat_arguments(self, arguments):
        return [
            {
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "tool_calls": [{"index": 0, "function": {"arguments": arguments}}]
                        },
                    }
                ]
            },
            {"choices": [{"index": 0, "finish_reason": "tool_calls"}]},
        ]

    def messages_arguments(self, arguments):
        return [
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "input": {}},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": arguments},
            },
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            {"type": "message_stop"},
        ]

    def test_messages_duplicate_keys_are_valid_but_non_json_constants_are_not(self):
        message = "Messages stream validation failed: terminal Messages tool input was not complete valid JSON"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for arguments, outcome in (
                ('{"x":1,"x":2}', live.HARNESS),
                ('{"x":NaN}', live.MODEL),
                ('{"x":Infinity}', live.MODEL),
                ("[]", live.MODEL),
            ):
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
                (messages, self.messages_arguments("{"), self.chat_arguments("{")),
            ):
                with self.subTest(marker=marker):
                    self.write_capture(segment, mismatched)
                    self.assertEqual(
                        live.stream_validation_outcome(marker, [segment]), live.HARNESS
                    )
                    self.write_capture(segment, matching)
                    self.assertEqual(live.stream_validation_outcome(marker, [segment]), live.MODEL)
                    bare = marker.split("stream validation failed: ", 1)[1]
                    self.assertIsNone(live.stream_validation_outcome(bare, [segment]))

    def test_other_provider_argument_failures_without_stream_prefix_stay_harness(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for diagnostic, events in (
                ("OpenAI tool arguments are malformed or ambiguous", self.chat_arguments("{")),
                (
                    "Anthropic terminal Messages tool input was not complete valid JSON",
                    self.messages_arguments("{"),
                ),
            ):
                with self.subTest(diagnostic=diagnostic):
                    self.write_capture(segment, events)
                    turn = live.Turn(
                        phase="failed", model_error_kind="Protocol", errors=[diagnostic]
                    )
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
            self.assertEqual(
                live.stream_validation_outcome(message, [earlier, failing]), live.HARNESS
            )
            self.write_capture(failing, self.chat_arguments("{"))
            self.write_capture(failing, self.chat_arguments("{}"), exchange=2)
            self.assertEqual(live.stream_validation_outcome(message, [failing]), live.HARNESS)
            self.write_capture(failing, self.chat_arguments("{"), exchange=3)
            self.assertEqual(live.stream_validation_outcome(message, [failing]), live.MODEL)

    def test_invalid_argument_evidence_orders_exchanges_by_number(self):
        message = (
            "OpenRouter Chat stream validation failed: tool arguments are malformed or ambiguous"
        )
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, self.chat_arguments("{"), exchange=9999)
            self.write_capture(segment, self.chat_arguments("{}"), exchange=10000)
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.HARNESS)
            self.write_capture(segment, self.chat_arguments("{"), exchange=10000)
            info = segment / "exchange-10000.info"
            info.write_text(
                info.read_text().replace("terminal=Complete", "terminal=ProtocolComplete")
            )
            self.assertEqual(live.stream_validation_outcome(message, [segment]), live.MODEL)

    def test_invalid_argument_evidence_requires_the_highest_exchange_to_be_finished(self):
        message = (
            "OpenRouter Chat stream validation failed: tool arguments are malformed or ambiguous"
        )
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_capture(segment, self.chat_arguments("{"))
            for terminal, outcome in (("ClosedEarly", live.MODEL), ("Abandoned", live.HARNESS)):
                with self.subTest(terminal=terminal):
                    self.write_capture(segment, self.chat_arguments("{"), exchange=2)
                    info = segment / "exchange-0002.info"
                    info.write_text(
                        info.read_text().replace("terminal=Complete", f"terminal={terminal}")
                    )
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
                (
                    messages,
                    self.messages_arguments(unterminated),
                    self.messages_arguments(unterminated)[:2],
                ),
            ):
                for events, outcome in ((finished, live.MODEL), (unfinished, live.HARNESS)):
                    with self.subTest(marker=marker, events=len(events)):
                        self.write_capture(segment, events)
                        info = segment / "exchange-0001.info"
                        info.write_text(
                            info.read_text().replace("terminal=Complete", "terminal=ClosedEarly")
                        )
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
            with patch.object(
                live, "stream_validation_outcome", wraps=live.stream_validation_outcome
            ) as classify:
                with self.assertRaises(live.ScenarioFailure) as raised:
                    driver.subagents()
            self.assertEqual(raised.exception.outcome, live.HARNESS)
            self.assertEqual(classify.call_args.args[1], [root])

    def test_finished_malformed_chat_arguments_are_model_on_every_surface(self):
        message = "Chat stream validation failed: tool arguments are malformed or ambiguous"
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            for arguments, outcome in (
                ("{", live.MODEL),
                ('{"x":1,"x":2}', live.MODEL),
                ('{"x":1}', live.HARNESS),
            ):
                events = [
                    {
                        "choices": [
                            {
                                "index": 0,
                                "delta": {
                                    "tool_calls": [
                                        {"index": 0, "function": {"arguments": arguments}}
                                    ]
                                },
                            }
                        ]
                    },
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
                self.write_capture(segment, events)
                self.assertEqual(live.stream_validation_outcome(message, [segment]), outcome)
            self.write_capture(
                segment,
                [
                    {
                        "choices": [
                            {
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{"index": 0, "function": {"arguments": "{"}}]
                                },
                            }
                        ]
                    },
                    {"choices": [{"index": 0, "finish_reason": "tool_calls"}]},
                ],
            )
            for classify in (
                lambda: live.classify_incomplete(
                    live.Turn(phase="failed", errors=[message]), "turn", segments=[segment]
                ),
                lambda: live.classify_command_error(message, "compact", segments=[segment]),
                lambda: live.classify_cli_failure(5, message, "CLI", segments=[segment]),
            ):
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
                {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "input": {}},
                },
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": '{"x":'},
                },
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
            self.write_capture(
                segment,
                [
                    {"choices": [{"delta": {"content": "x"}}], "usage": {"prompt_tokens": 1}},
                ],
            )
            driver = smoke.Smoke(live.ROUTES["codex"], Mock(capture_segments=[segment]))
            driver.repo = Mock(return_value=segment)
            agent = Mock(capture_dir=segment)
            agent.prompt.return_value = live.Turn(
                phase="failed", errors=["stream validation failed"]
            )
            driver.agent = Mock(return_value=agent)
            with self.assertRaises(live.ScenarioFailure) as raised:
                driver.subagents()
            self.assertEqual(raised.exception.outcome, live.PROVIDER)


KEEP_ALIVE = b": OPENROUTER PROCESSING\n\n"
IN_BAND_504 = b'data: {"choices":[],"error":{"code":504,"message":"error code: 504"}}\n\n'
FAILED_504 = dict(
    phase="failed",
    model_error_kind="Timeout",
    http_status=504,
    errors=[
        "OpenRouter returned an in-band error after accepting the request: "
        "HTTP 504 (timeout) from Liquid"
    ],
)


class UnfiredFaultTests(unittest.TestCase):
    """An `after-events` reset the provider's own error beat to the cut."""

    def write_exchange(
        self,
        segment,
        number,
        body,
        *,
        request=b'{"messages":[]}',
        status="Some(200)",
        terminal="ClosedEarly",
    ):
        name = f"exchange-{number:04}"
        (segment / f"{name}.request").write_bytes(request)
        (segment / f"{name}.body").write_bytes(body)
        (segment / f"{name}.info").write_text(
            f"process_id=123\nexchange={number}\nstatus={status}\nconnection=Some(Fresh)\n"
            f"terminal={terminal}\nrecorded_bytes={len(body)}\nincomplete=false\n"
        )

    def judge(
        self, target_body, *, turn=None, status="Some(200)", terminal="ClosedEarly", threshold=3
    ):
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
        plan = live.fault_plan(live.ROUTES["or-flint-free"], "reset", after_events=3)
        self.assertEqual(smoke.after_events(plan), 3)
        self.assertIsNone(smoke.after_events(live.fault_plan(live.ROUTES["or-flint-free"], "429")))

    def test_keep_alives_then_an_in_band_504_before_the_cut_is_provider(self):
        failure = self.judge(KEEP_ALIVE * 33 + IN_BAND_504)
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertEqual(
            str(failure),
            "the provider ended the targeted stream (exchange-0002) in "
            "HTTP 504 after 1 of 3 events, before the reset fired",
        )

    def test_comment_only_blocks_do_not_count_as_events(self):
        self.assertEqual(
            live.sse_events(KEEP_ALIVE * 33 + IN_BAND_504 + b"data: x"), [IN_BAND_504.rstrip(b"\n")]
        )
        self.assertEqual(len(live.sse_events(b": a\r\nevent: e\r\n: b\r\ndata: 1\r\n\r\n\n\n")), 1)

    def test_a_targeted_http_error_status_is_provider(self):
        failure = self.judge(
            b'{"error":{"code":503}}',
            status="Some(503)",
            terminal="Complete",
            turn=live.Turn(phase="failed", model_error_kind="Unavailable", http_status=503),
        )
        self.assertEqual(failure.outcome, live.PROVIDER)
        self.assertIn("in HTTP 503 after 0 of 3 events", str(failure))

    def test_other_wires_and_a_provider_transport_failure_before_the_cut(self):
        messages = (
            b'event: message_start\ndata: {"type":"message_start"}\n\n'
            b'event: error\ndata: {"type":"error","error":{"type":"overloaded_error"}}\n\n'
        )
        failure = self.judge(
            messages, turn=live.Turn(phase="failed", model_error_kind="Unavailable")
        )
        self.assertEqual(
            (failure.outcome, str(failure)),
            (
                live.PROVIDER,
                "the provider ended the targeted stream (exchange-0002) in HTTP 529 "
                "after 2 of 3 events, before the reset fired",
            ),
        )
        reset = (
            "Failed(Failure { phase: ResponseBody, transmission: MayHaveTransmitted, "
            "kind: DeadlineExceeded, cause: None, http2_reason: None, os_error: None, tls: None, "
            "tls_alert: None, alpn: None, http2_io: false, connection: None, idle_age: None, connect_attempts: None })"
        )
        failure = self.judge(
            KEEP_ALIVE, terminal=reset, turn=live.Turn(phase="failed", model_error_kind="Timeout")
        )
        self.assertEqual(
            str(failure),
            "the provider ended the targeted stream (exchange-0002) in "
            "a transport failure (DeadlineExceeded) in ResponseBody "
            "after 0 of 3 events, "
            "before the reset fired",
        )
        # The product typed it a transport failure, not a provider error.
        failure = self.judge(
            KEEP_ALIVE, terminal=reset, turn=live.Turn(phase="failed", model_error_kind="Transport")
        )
        self.assertEqual(failure.outcome, live.HARNESS)

    def test_a_retried_head_timeout_before_the_cut_is_provider(self):
        # The recorded shape: the targeted Messages request got no response
        # head within the dispatch deadline, the product retried it and the
        # turn completed, so the reset armed after three events never fired.
        head = (
            "Failed(Failure { phase: ResponseHead, transmission: MayHaveTransmitted, "
            "kind: DeadlineExceeded, cause: None, http2_reason: None, os_error: None, tls: None, "
            "tls_alert: None, alpn: Some(\"http/1.1\"), http2_io: false, "
            "connection: Some(Fresh), idle_age: None, connect_attempts: None })"
        )
        retried = live.Turn(
            phase="completed",
            retries=[{"attempt": 2, "cause_kind": "transport_after_send", "max_attempts": 4}],
        )
        failure = self.judge(b"", status="None", terminal=head, turn=retried)
        self.assertEqual(
            (failure.outcome, str(failure)),
            (
                live.PROVIDER,
                "the provider ended the targeted stream (exchange-0002) in a transport "
                "failure (DeadlineExceeded) in ResponseHead after 0 of 3 events, before "
                "the reset fired; the product retried it and completed the turn",
            ),
        )
        # Without a transport or provider retry, or without a replay of the
        # targeted request, it stays harness.
        other_retry = live.Turn(phase="completed", retries=[{"cause_kind": "model_output"}])
        for name, turn, later in (
            ("no retry", live.Turn(phase="completed"), b'{"messages":[]}'),
            ("not a replay cause", other_retry, b'{"messages":[]}'),
            ("no later request", retried, None),
            ("a different request", retried, b'{"messages":[{"role":"user"}]}'),
        ):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                segment = Path(directory)
                self.write_exchange(segment, 1, b"", status="None", terminal=head)
                if later is not None:
                    self.write_exchange(
                        segment, 2, b"data: {}\n\n", request=later, terminal="Complete"
                    )
                with self.assertRaises(live.ScenarioFailure) as raised:
                    smoke.judge_fault_reset(turn, segment / "hello.txt", [segment], 3)
                self.assertEqual(raised.exception.outcome, live.HARNESS)

    def test_a_request_without_a_conversation_is_not_the_target(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write_exchange(
                segment, 1, b'{"ok":true}', request=b'{"grant_type":"refresh"}', terminal="Complete"
            )
            self.write_exchange(segment, 2, KEEP_ALIVE * 3 + IN_BAND_504)
            failure = smoke.provider_ended_before_cut(live.Turn(**FAILED_504), [segment], 3)
            self.assertIn("(exchange-0002) in HTTP 504", failure)

    def test_other_unobserved_resets_stay_harness(self):
        for name, kwargs in (
            ("no error event", {"target_body": KEEP_ALIVE + b"data: {}\n\n"}),
            ("the cut was reached", {"target_body": b"data: {}\n\n" * 2 + IN_BAND_504}),
            ("an abandoned capture", {"target_body": IN_BAND_504, "terminal": "Abandoned"}),
            ("a 400 error event", {"target_body": IN_BAND_504.replace(b"504", b"400")}),
            (
                "a completed turn",
                {"target_body": IN_BAND_504, "turn": live.Turn(phase="completed")},
            ),
            (
                "a turn that failed as the product's",
                {
                    "target_body": IN_BAND_504,
                    "turn": live.Turn(phase="failed", model_error_kind="Protocol"),
                },
            ),
            ("no after-events threshold", {"target_body": IN_BAND_504, "threshold": None}),
        ):
            with self.subTest(name):
                failure = self.judge(**kwargs)
                self.assertEqual(
                    (failure.outcome, str(failure)),
                    (live.HARNESS, "injected reset was not observed"),
                )


class CancelWithoutInterruptTests(unittest.TestCase):
    def cancel(self, turn):
        driver = smoke.Smoke(live.ROUTES["or-flint-free"], Mock(capture_segments=[]))
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
        self.assertEqual(
            (failure.outcome, str(failure)), (live.MODEL, "the model never started the command")
        )


class RunawayOutputTests(unittest.TestCase):
    """A model output the 8 MiB capture cap stopped is the model's, not an evidence gap."""

    CAP = live.CAPTURE_RESPONSE_CAP_BYTES
    TERMINAL = (
        "Failed(Failure { phase: ResponseBody, transmission: MayHaveTransmitted, "
        "kind: Internal, cause: None, http2_reason: None, os_error: None, tls: None, "
        "tls_alert: None, alpn: Some(\"http/1.1\"), http2_io: false, "
        "connection: Some(Fresh), idle_age: None, connect_attempts: None })"
    )

    def write(self, segment, events, *, recorded=None, terminal=None, incomplete="true"):
        body = b"".join(
            b"event: x\ndata: " + json.dumps(event).encode() + b"\n\n" for event in events
        )
        filler = {
            "type": "response.custom_tool_call_input.delta",
            "item_id": "ctc_1",
            "delta": "*** End Patch\n",
        }
        line = b"event: x\ndata: " + json.dumps(filler).encode() + b"\n\n"
        body += line * ((self.CAP - len(body)) // len(line))
        body = (body + b"event: x\ndata: {\"type\":\"respo")[: self.CAP]
        body += b"x" * (self.CAP - len(body))
        (segment / "exchange-0001.request").write_bytes(b'{"input":[]}')
        (segment / "exchange-0001.body").write_bytes(body)
        (segment / "exchange-0001.info").write_text(
            "process_id=1\nexchange=1\nstatus=Some(200)\nconnection=Some(Fresh)\n"
            f"terminal={terminal or self.TERMINAL}\nrecorded_bytes={recorded or self.CAP}\n"
            f"incomplete={incomplete}\n"
        )

    PREAMBLE = [
        {"type": "response.created", "response": {}},
        {"type": "response.in_progress", "response": {}},
        {
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"id": "ctc_1", "type": "custom_tool_call", "name": "exec"},
        },
    ]

    def test_a_runaway_tool_call_at_the_cap_is_model_and_the_capture_is_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write(segment, self.PREAMBLE)
            reasons = live.runaway_outputs([segment])
            self.assertEqual(
                reasons,
                [
                    f"model output ran past the 8 MiB capture bound (runaway "
                    f"output in custom_tool_call exec) at {segment.name} "
                    "exchange-0001"
                ],
            )
            self.assertTrue(live.capture_integrity([segment]))
            aborted = (
                "overflow turn: failed Transport agent turn failed: Responses transport "
                "failed: response stream interrupted [1 requests; last: unclassified]"
            )
            self.assertEqual(
                live.score_runaway(live.HARNESS, aborted, [segment]), (live.MODEL, reasons[0])
            )
            self.assertEqual(
                live.score_runaway(
                    live.HARNESS, "renamed diagnostic", [segment], model_error_kind="Transport"
                ),
                (live.MODEL, reasons[0]),
            )
            self.assertEqual(
                live.score_runaway(live.HARNESS, aborted, [segment], model_error_kind="Protocol"),
                (live.HARNESS, aborted),
            )
            self.assertEqual(
                live.score_runaway(live.PROVIDER, "429", [segment]), (live.PROVIDER, "429")
            )
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
            f"terminal=Complete\nrecorded_bytes={len(body)}\nincomplete=false\n"
        )

    def test_a_runaway_does_not_relabel_an_unrelated_harness_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            segment = Path(directory)
            self.write(segment, self.PREAMBLE)
            for detail in (
                "TUI exited 1 after Ctrl+Q",
                "prepared input reached the compaction trigger without a committed remedy",
                "plakd agent exited during a turn",
            ):
                with self.subTest(detail):
                    self.assertEqual(
                        live.score_runaway(live.HARNESS, detail, [segment]), (live.HARNESS, detail)
                    )
            # The failing turn ended later than the runaway: a later exchange answered.
            self.add_exchange(segment, 2, b"data: {}\n\ndata: [DONE]\n\n")
            aborted = "follow-up: failed Transport response stream interrupted"
            self.assertEqual(
                live.score_runaway(live.HARNESS, aborted, [segment]), (live.HARNESS, aborted)
            )
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "segment-001", Path(directory) / "segment-002"
            first.mkdir()
            second.mkdir()
            self.write(first, self.PREAMBLE)
            # The turn that failed ran in a later process, not the runaway's.
            self.add_exchange(second, 1, b"")
            aborted = "follow-up: failed Transport response stream interrupted"
            self.assertEqual(
                live.score_runaway(live.HARNESS, aborted, [first, second]), (live.HARNESS, aborted)
            )
            self.assertEqual(
                live.score_runaway(live.HARNESS, aborted, [second, first])[0], live.MODEL
            )

    def test_every_other_incomplete_capture_stays_an_evidence_gap(self):
        second_item = {"type": "response.output_text.delta", "item_id": "msg_2", "delta": "x"}
        transport = self.TERMINAL.replace("kind: Internal", "kind: Protocol")
        for name, kwargs in (
            ("below the cap", {"recorded": self.CAP - 1}),
            ("another failure", {"terminal": transport}),
            ("two output items", {"events": [*self.PREAMBLE, second_item]}),
            ("a non-output event", {"events": [*self.PREAMBLE, {"type": "response.completed"}]}),
            ("a complete record", {"incomplete": "false"}),
        ):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                segment = Path(directory)
                events = kwargs.pop("events", self.PREAMBLE)
                self.write(segment, events, **kwargs)
                self.assertEqual(live.runaway_outputs([segment]), [])
                # A complete record is a terminal Failed exchange, not a gap.
                self.assertEqual(live.capture_integrity([segment]), name == "a complete record")
                aborted = "turn: failed Transport response stream interrupted"
                self.assertEqual(
                    live.score_runaway(live.HARNESS, aborted, [segment]), (live.HARNESS, aborted)
                )


class FailureLinesTests(unittest.TestCase):
    def test_the_final_line_keeps_the_error_before_it(self):
        stderr = (
            "plakd: composition Direct\nplakd: agent turn failed: provider HTTP 500\n"
            "plakd: retry summary\nplakd: turn_failed:provider\n"
        )
        self.assertEqual(
            live.failure_lines(stderr),
            "plakd: agent turn failed: provider HTTP 500 | plakd: turn_failed:provider",
        )
        self.assertEqual(live.failure_lines("plakd: done\n"), "plakd: done")
        self.assertEqual(live.failure_lines(""), "")


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
