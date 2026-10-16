#!/usr/bin/env python3
"""A few minutes of short live scenarios against one route.

Usage: smoke.py [route] [--binary PATH] [--only NAME,...]

Each scenario builds its own small git repo, drives the real `plak` (CLI
one-shot, `plak agent` JSONL, or the TUI on a PTY) and checks product-level
outcomes: files changed, tests pass, the session continues. A failure is
classed as harness (Plak's), model (the turn completed but the work is
wrong), provider (the real provider refused) or pending (a known gap that an
in-flight slice closes). Artifacts land in target/live/smoke-<route>-<stamp>/.

`wrap_up` and `py_ptc` are opt-in and stay out of the default run. Select
them with `--only wrap_up` or `--only py_ptc`. The feature scenarios in
`features.py` (image input, fetch_url, MCP, hosted retrieval, image
generation) are opt-in the same way; a route row Plak refuses is `refused`. Long-session scenarios
(`in_turn_compaction`, `overflow`, `kill_reopen`, `child_compaction`) are
also opt-in; they live in `long_session.py` and are selected the same way.
"""

from __future__ import annotations

import argparse
import json
import signal
import sys
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402
import jobs  # noqa: E402
import features  # noqa: E402
from live import HARNESS, MODEL, PASS, PENDING, PROVIDER, ScenarioFailure  # noqa: E402

CALC = "def add(a, b):\n    return a - b\n\n\ndef subtract(a, b):\n    return a - b\n"
TEST_CALC = (
    "import unittest\n\nfrom calc import add, subtract\n\n\n"
    "class CalcTest(unittest.TestCase):\n"
    "    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\n"
    "    def test_subtract(self):\n        self.assertEqual(subtract(5, 3), 2)\n"
)
TASK = ("calc.py has a bug that makes `python3 -m unittest` fail. Fix it, add a test "
        "to test_calc.py for subtracting a negative number, and run the tests.")
CONTINUE = "Reply with exactly the word: continued"
WRAP_UP_PROMPT = (
    "Run the shell command `ls`, then `cat` each file one at a time, waiting "
    "for each result before the next command. Do not combine commands. After "
    "those commands, answer DONE."
)
# The turn after the wrap-up. Wording is the prompt, not an answer oracle.
WRAP_UP_FOLLOW = "say continued"
PY_PTC_TOKEN = "kestrel-4721-ochre"
PY_PTC_PROMPT = (
    "Use exec_py for all of this. In a first cell: read the file token.txt with "
    "one of the nested tools, print its contents, store the stripped contents in "
    "a global named TOKEN, then run int('not a number') inside try/except "
    "ValueError and print 'handled ValueError'. In a second, separate exec_py "
    "cell, evaluate TOKEN[::-1] as its last expression. Reply with exactly the "
    "value the second cell returned."
)
BINDING_LOSS = ("ProtectedHistory", "stable-prefix binding differs")

# Scenarios whose failure a slice still in flight is expected to fix. A failure
# here is reported as pending, with the reason, instead of as a tool bug.
EXPECTED_PENDING: dict[str, str] = {}


def calc_repo(path: Path) -> Path:
    return live.git_repo(path, {"calc.py": CALC, "test_calc.py": TEST_CALC})


def check_calc(repo: Path) -> None:
    if not live.unittest_passes(repo):
        raise ScenarioFailure(MODEL, "tests still fail after the turn")
    if (repo / "test_calc.py").read_text().count("def test_") < 3:
        raise ScenarioFailure(MODEL, "no test was added")


def require_completed(turn: live.Turn, what: str, injected: bool = False,
                      injected_fault: str | None = None,
                      segments: list[Path] | None = None) -> None:
    """Classify an incomplete turn. `injected` names a fault the product must absorb."""
    fault = injected_fault if injected else None
    live.classify_incomplete(turn, what, injected_fault=fault, segments=segments)


def continues(agent: live.Agent) -> None:
    """The session answers a fresh prompt: no dead end that needs /clear."""
    require_completed(agent.prompt(CONTINUE, timeout=180), "follow-up after the fault")


def durable_terminals(records: list[dict]) -> list[dict]:
    """Return durable turn terminals, excluding their admission records."""
    return [record for record in records
            if record.get("kind") == "turn.lifecycle"
            and record.get("phase") not in (None, "admitted")]


def require_durable_completed(record: dict, what: str,
                              segments: list[Path] | None = None,
                              home: Path | None = None) -> None:
    """Classify a durable TUI terminal with the same policy as JSONL turns."""
    if home is not None and record.get("phase") != "completed":
        retry = jobs.last_retry(home, str(record.get("turn_id") or ""))
        if retry is not None:
            cause, attempt, limit = retry
            if cause in jobs.PROVIDER_RETRY_CAUSES and attempt == limit:
                raise ScenarioFailure(PROVIDER, f"{what}: model_retries {cause} exhausted")
    status = record.get("http_status")
    require_completed(live.Turn(phase=record.get("phase"),
                                model_error_kind=record.get("model_error_kind"),
                                http_status=status if type(status) is int else None,
                                errors=[str(record.get("message") or "")],
                                refusals=live.durable_refusals(record)),
                      what, segments=segments)


def fault_was_observed(turn: live.Turn, segments: list[Path] | None) -> bool:
    """The injected reset showed up as a retry or a captured interrupted exchange."""
    if any(retry.get("cause_kind") == "stream_interrupted" for retry in turn.retries):
        return True
    blob = " ".join(turn.errors).lower()
    if "stream" in blob and ("interrupt" in blob or "reset" in blob):
        return True
    if not segments:
        return False
    for segment in segments:
        if not segment.is_dir():
            continue
        for info in segment.glob("exchange-*.info"):
            try:
                text = info.read_text(errors="replace").lower()
            except OSError:
                continue
            if "reset" in text or "stream" in text:
                return True
    return False


def discarded_replay(turn: live.Turn) -> bool:
    """A retry that dropped the failed request's partial output."""
    return any(retry.get("cause_kind") == "stream_interrupted"
               and retry.get("output_discarded") is True for retry in turn.retries)


def judge_fault_reset(turn: live.Turn, hello: Path, segments: list[Path] | None) -> str:
    """A reset is a safe replay, a declined unsafe replay, or a provider refusal.

    The injected fault has to have been observed. A replay that discarded its
    partial output and completed with one effect is the safe path. A turn that
    fails closed without replaying, and does not repeat the write, is the
    correctly declined unsafe path. A typed provider refusal, including HTTP
    429, is provider even though a reset was also injected.
    """
    if not fault_was_observed(turn, segments):
        # A real provider HTTP error on the faulted request ends it before any
        # stream event, so the reset (armed after N events) never fires.
        if any(retry.get("cause_kind") == "http_status" for retry in turn.retries):
            raise ScenarioFailure(PROVIDER, "a provider HTTP error consumed the faulted request "
                                            "before the reset fired")
        raise ScenarioFailure(HARNESS, "injected reset was not observed")
    try:
        require_completed(turn, "reset turn", segments=segments)
    except ScenarioFailure as failure:
        if failure.outcome in (live.PROVIDER, live.MODEL) or "stream validation" in str(failure):
            raise
        if discarded_replay(turn):
            raise ScenarioFailure(HARNESS, "discarded replay did not complete the turn") from failure
        if hello.is_file() and hello.read_text().count("hello") > 1:
            raise ScenarioFailure(HARNESS, "declined replay still duplicated its effect") from failure
        return "unsafe replay declined; fault observed; effect not repeated"
    if not discarded_replay(turn):
        raise ScenarioFailure(HARNESS, "completed reset was not a discarded-output replay")
    if not hello.is_file():
        raise ScenarioFailure(MODEL, "turn completed without writing the file")
    if hello.read_text().count("hello") != 1:
        raise ScenarioFailure(HARNESS, "the replay duplicated its effect")
    return "cut stream replayed with output discarded; turn completed; session continued"


class Smoke:
    def __init__(self, route: live.Route, env: live.Env):
        self.route, self.env = route, env

    def repo(self, name: str, factory=calc_repo) -> Path:
        return factory(self.env.artifacts / name / "repo")

    def agent(self, name: str, repo: Path, faults: str | None = None,
              args: list[str] | None = None) -> live.Agent:
        return live.Agent(self.env, repo, f"{name}/agent", args=args or [], faults=faults)

    # --- CLI one-shot -------------------------------------------------------

    def cli_task(self) -> str:
        repo = self.repo("cli_task")
        code, _ = live.run_cli(self.env, repo, [*self.route.flags(), "--", TASK],
                               self.env.artifacts / "cli_task" / "run.log")
        if code != 0:
            _raise_cli_failure(code, self.env.artifacts / "cli_task" / "run.log", "cli_task")
        check_calc(repo)
        return "bug fixed, test added, suite passes"

    def cli_resume(self) -> str:
        repo = self.repo("cli_resume")
        log = self.env.artifacts / "cli_resume"
        code, out = live.run_cli(self.env, repo, [*self.route.flags(), "--print-session-id",
                                                  "--", "Remember the word MAGENTA. Reply: ok"],
                                 log / "first.log")
        session = next((line.split("=", 1)[1] for line in out.splitlines()
                        if line.startswith("session_id=")), None)
        if code != 0:
            _raise_cli_failure(code, log / "first.log", "first run")
        if not session:
            raise ScenarioFailure(HARNESS, f"first run exited {code} without a session id")
        code, out = live.run_cli(self.env, repo, ["--session", session, "--",
                                                  "What word did I ask you to remember? "
                                                  "Reply with the word only."],
                                 log / "second.log")
        if code != 0:
            _raise_cli_failure(code, log / "second.log", "plak run --session")
        if "MAGENTA" not in out.upper():
            raise ScenarioFailure(MODEL, "resumed turn lost the remembered word")
        return "second run continued the session"

    def wrap_up(self) -> str:
        """One CLI turn that spends `[turn] max_model_rounds`, then the next turn.

        The private home leaves the shared smoke home at the product default.
        Prefix-drift telemetry is not persisted, so a clean follow-up and the
        absence of a binding error are what a Responses replay failure would
        show up as.
        """
        repo = self.repo("wrap_up")
        home = self.env.artifacts / "wrap_up" / "home"
        live.write_home(home, self.route, self.env.home)
        live.pin_max_model_rounds(home, 2)
        log_dir = self.env.artifacts / "wrap_up"
        code, out = live.run_cli(
            self.env, repo,
            [*self.route.flags(), "--print-session-id", "--", WRAP_UP_PROMPT],
            log_dir / "first.log", timeout=600, home=home)
        session = _session_id(out)
        records = live.session_records(home, session) if session else []
        _reject_binding_loss("wrap-up turn", log_dir / "first.log", out, records)
        if code != 0 or not session:
            _raise_cli_failure(code, log_dir / "first.log", "wrap-up turn",
                               missing_session=not session)
        terminals = durable_terminals(records)
        if not terminals:
            raise ScenarioFailure(HARNESS, "wrap-up turn produced no durable terminal")
        require_durable_completed(terminals[-1], "wrap-up turn")
        _require_cli_answer(out, records, "wrap-up turn")
        if not any(record.get("kind") == "turn.operation_budget_exhausted"
                   for record in records):
            raise ScenarioFailure(MODEL, "turn finished without the operation-budget wrap-up")
        code, out = live.run_cli(
            self.env, repo, ["--session", session, "--", WRAP_UP_FOLLOW],
            log_dir / "second.log", timeout=240, home=home)
        records = live.session_records(home, session)
        _reject_binding_loss("turn after wrap-up", log_dir / "second.log", out, records)
        if code != 0:
            _raise_cli_failure(code, log_dir / "second.log", "turn after wrap-up")
        terminals = durable_terminals(records)
        if len(terminals) < 2:
            raise ScenarioFailure(HARNESS, "turn after wrap-up produced no durable terminal")
        require_durable_completed(terminals[-1], "turn after wrap-up")
        return ("wrap-up answered; turn.operation_budget_exhausted recorded; "
                "next turn completed; prefix-drift telemetry is not persisted")

    def py_ptc(self) -> str:
        """One CLI turn on the Python harness: a nested tool call, printed output,
        a handled exception, and a second cell that reads the first cell's global.

        The token exists only in the repository, so a reversed token in the answer
        means a nested tool read it. The raw captured exchanges must show
        `exec_py`, the second cell's `TOKEN[::-1]` and the handled-exception output
        returned to the model.
        """
        segment_start = len(self.env.capture_segments)
        repo = live.git_repo(self.env.artifacts / "py_ptc" / "repo",
                             {"token.txt": PY_PTC_TOKEN + "\n"})
        home = self.env.artifacts / "py_ptc" / "home"
        live.write_home(home, self.route, self.env.home)
        live.pin_composition(home, "py-ptc")
        log = self.env.artifacts / "py_ptc" / "turn.log"
        code, out = live.run_cli(
            self.env, repo,
            [*self.route.flags(), "--composition", "py-ptc", "--print-session-id", "--",
             PY_PTC_PROMPT],
            log, timeout=600, home=home)
        session = _session_id(out)
        records = live.session_records(home, session) if session else []
        if code != 0 or not session:
            _raise_cli_failure(code, log, "py-ptc turn", missing_session=not session)
        terminals = durable_terminals(records)
        if not terminals:
            raise ScenarioFailure(HARNESS, "py-ptc turn produced no durable terminal")
        require_durable_completed(terminals[-1], "py-ptc turn")
        exchanges = "\n".join(
            streamed_text(body.read_text(errors="replace"))
            for segment in self.env.capture_segments[segment_start:]
            for body in sorted(segment.glob("*.body")))
        _require_py_ptc_oracle(out, records, exchanges)
        return "two exec_py cells; nested read, handled exception, global reused"

    # --- JSONL ---------------------------------------------------------------

    def jsonl_task(self) -> str:
        repo = self.repo("jsonl_task")
        agent = self.agent("jsonl_task", repo)
        turn = agent.prompt(TASK)
        agent.quit()
        require_completed(turn, "coding turn")
        check_calc(repo)
        return f"{turn.tools} tool calls, {turn.seconds:.0f}s"

    def jsonl_resume(self) -> str:
        repo = self.repo("jsonl_resume")
        agent = self.agent("jsonl_resume", repo)
        require_completed(agent.prompt("Write the word OCTOPUS into notes.txt."), "first turn")
        session = agent.session_id
        if agent.quit() != 0:
            raise ScenarioFailure(HARNESS, "plak agent exited non-zero after quit")
        agent = self.agent("jsonl_resume", repo, args=["--session", session])
        if agent.session_id != session:
            raise ScenarioFailure(HARNESS, "reopen attached a different session")
        turn = agent.prompt("What word did you write into notes.txt? Reply with the word only.")
        agent.quit()
        require_completed(turn, "turn after reopen")
        if "OCTOPUS" not in turn.text.upper():
            raise ScenarioFailure(MODEL, "reopened session lost its history")
        return "reopened session remembered its work"

    def compaction(self) -> str:
        import long_session

        repo = self.repo("compaction")
        # The newest intent and its tool batch are protected (spec 01 §10.1),
        # so the first turn is the foldable group. Its read is larger than a
        # semantic summary. The code word is only in that prompt: fold.txt
        # cannot answer the follow-up.
        (repo / "fold.txt").write_text(live.compaction_padding())
        agent = self.agent("compaction", repo)
        first = agent.prompt(
            "Read fold.txt in full with one read. Remember the code word ZEBRA-42. "
            "Reply: ok",
            timeout=300)
        capacity_recovered = first.phase == "failed" and first.model_error_kind == "ContextLimit"
        committed = 0
        if capacity_recovered:
            window = long_session.catalog_window(self.route.model)
            if window is not None and window > long_session.OPENROUTER_CUSTOM_INPUT:
                agent.quit()
                raise ScenarioFailure(HARNESS, "context limit on a large catalog window")

            def capacity_after_failed_turn(records: list[dict]) -> bool:
                for index, record in enumerate(records):
                    if record.get("kind") == "turn.lifecycle" \
                            and record.get("phase") not in (None, "admitted"):
                        return (record.get("phase") == "failed"
                                and long_session.capacity_recovery_committed(records[index + 1:]))
                return False

            records = long_session.poll_records(
                self.env.home, agent.session_id, capacity_after_failed_turn)
            if not capacity_after_failed_turn(records):
                agent.quit()
                raise ScenarioFailure(HARNESS, "context limit produced no committed capacity compact")
            committed = live.committed_compactions(records)
        else:
            require_completed(first, "first turn", segments=[agent.capture_dir])
        require_completed(agent.prompt("Now read test_calc.py. Reply: ok"), "second turn",
                          segments=[agent.capture_dir])
        if not capacity_recovered:
            answer = agent.command("compact")
            if answer.get("kind") == "error":
                live.classify_command_error(str(answer.get("message") or ""), "manual compact",
                                            segments=[agent.capture_dir])
            for _ in range(30):  # the log line can trail the command's answer
                committed = live.committed_compactions(live.session_records(self.env.home,
                                                                            agent.session_id))
                if committed:
                    break
                time.sleep(1)
        # The fact must come from folded history, not a repository shortcut.
        for path in repo.rglob("*"):
            if ".git" in path.relative_to(repo).parts or not path.is_file():
                continue
            if b"ZEBRA-42" in path.read_bytes():
                agent.quit()
                raise ScenarioFailure(HARNESS, "code word leaked into a repository file")
        turn = agent.prompt("What was the code word? Reply with it only.")
        agent.quit()
        if not committed:
            raise ScenarioFailure(HARNESS, "no committed compaction in the session log")
        require_completed(turn, "turn after compaction", segments=[agent.capture_dir])
        if "ZEBRA-42" not in turn.text.upper():
            raise ScenarioFailure(MODEL, "the code word did not survive compaction")
        if capacity_recovered:
            return "capacity recovery compaction, context carried"
        return "committed compaction, context carried"

    def cancel(self) -> str:
        repo = self.repo("cancel")
        agent = self.agent("cancel", repo)
        sent = {"interrupt": False}

        def interrupt_on_tool(event: dict) -> None:
            if event.get("kind") == "tool" and event.get("state") == "running" and not sent["interrupt"]:
                sent["interrupt"] = True
                agent.send("interrupt")

        turn = agent.prompt("Run the shell command `sleep 60` and then tell me it finished.",
                            timeout=120, on_event=interrupt_on_tool)
        if not sent["interrupt"]:
            raise ScenarioFailure(MODEL, "the model never started the command")
        if turn.phase != "cancelled":
            raise ScenarioFailure(HARNESS, f"interrupted turn ended {turn.phase}")
        continues(agent)
        agent.quit()
        return "cancelled mid-tool, next prompt completed"

    def fault(self, name: str, plan: str, retried: bool, fault_name: str) -> str:
        """One injected fault on the first inference request, then a clean follow-up.

        A fault before acceptance (`retried`) should be absorbed: the turn still
        completes. A stream cut after acceptance but before any tool call on a
        turn request can only cost a duplicate inference (spec 01, replay of
        cost-only requests): it replays once with its partial output discarded,
        and the turn completes without a duplicated effect.
        """
        repo = self.repo(name)
        agent = self.agent(name, repo, faults=plan)
        turn = agent.prompt("Create hello.txt containing the word hello.", timeout=240)
        continues(agent)
        agent.quit()
        if retried:
            require_completed(turn, "faulted turn", injected=True, injected_fault=fault_name,
                              segments=[agent.capture_dir])
            if not (repo / "hello.txt").exists():
                raise ScenarioFailure(MODEL, "turn completed without writing the file")
            return "fault absorbed, turn completed; session continued"
        return judge_fault_reset(turn, repo / "hello.txt", [agent.capture_dir])

    def fault_429(self) -> str:
        return self.fault("fault_429", live.fault_plan(self.route, "429", retry_after=2),
                          True, "429")

    def fault_529(self) -> str:
        return self.fault("fault_529", live.fault_plan(self.route, "529"), True, "529")

    def fault_reset(self) -> str:
        return self.fault("fault_reset",
                          live.fault_plan(self.route, "reset", after_events=3), False, "reset")

    def fault_stall(self) -> str:
        return self.fault("fault_stall", live.fault_plan(self.route, "stall", seconds=20,
                                                         after_events=2), True, "stall")

    def subagents(self) -> str:
        repo = self.repo("subagents")
        agent = self.agent("subagents", repo)
        turn = agent.prompt(
            "Spawn two worker children at once from the worker template, tier mutate, each "
            "with workspace=fork: one writes alpha.txt containing alpha, the other writes "
            "beta.txt containing beta. Wait for both, adopt both candidates into this "
            "checkout, then reply done.", timeout=420)
        agent.quit()
        require_completed(turn, "root turn", segments=[agent.capture_dir])
        if len(agent.children) < 2:
            raise ScenarioFailure(MODEL, f"root spawned {len(agent.children)} children, not 2")
        missing = [f for f in ("alpha.txt", "beta.txt") if not (repo / f).exists()]
        if missing:
            raise ScenarioFailure(MODEL, f"children did not write {missing}")
        return "two forked children, both candidates adopted"

    # --- TUI -----------------------------------------------------------------

    def tui(self) -> str:
        repo = self.repo("tui")
        existing = set(self.env.home.glob("state/*/sessions/*.events.jsonl"))
        tui = live.Tui(self.env, repo, self.env.artifacts / "tui" / "pty.log")
        tui.pump(8)
        tui.type("Create tui.txt containing the word ready.")
        tui.key(b"\r")

        session: str | None = None

        def first_turn_terminal() -> bool:
            nonlocal session
            sessions = set(self.env.home.glob("state/*/sessions/*.events.jsonl")) - existing
            if len(sessions) > 1:
                raise ScenarioFailure(HARNESS, "TUI created more than one root session")
            if len(sessions) == 1:
                session = next(iter(sessions)).name.removesuffix(".events.jsonl")
            return session is not None and bool(durable_terminals(
                live.session_records(self.env.home, session)))

        finished = tui.wait_until(first_turn_terminal, 300)
        if not finished or session is None:
            tui.quit()
            raise ScenarioFailure(HARNESS, "first TUI turn produced no durable terminal")
        first = durable_terminals(live.session_records(self.env.home, session))[0]
        try:
            require_durable_completed(first, "first TUI turn",
                                      segments=[tui.capture_dir], home=self.env.home)
        except ScenarioFailure:
            tui.quit()
            raise
        if not (repo / "tui.txt").exists():
            tui.quit()
            raise ScenarioFailure(MODEL, "first TUI turn completed without writing tui.txt")

        tui.type(CONTINUE)
        tui.key(b"\r")

        def answered_on_durable_session() -> bool:
            records = live.session_records(self.env.home, session)
            return len(durable_terminals(records)) >= 2

        answered = tui.wait_until(answered_on_durable_session, 180)
        code = tui.quit()
        if code != 0:
            raise ScenarioFailure(HARNESS, f"TUI exited {code} after Ctrl+Q")
        if not answered:
            raise ScenarioFailure(HARNESS, "second TUI turn produced no durable terminal")
        records = live.session_records(self.env.home, session)
        require_durable_completed(durable_terminals(records)[1], "second TUI turn",
                                  segments=[tui.capture_dir], home=self.env.home)
        if not any(record.get("kind") == "turn.assistant" and
                   record.get("text", "").strip() == "continued" for record in records):
            raise ScenarioFailure(MODEL, "second TUI turn completed without the requested answer")
        return "two TUI turns, clean quit"

    SCENARIOS = (
        ("cli_task", "cli"), ("cli_resume", "cli"), ("jsonl_task", "jsonl"),
        ("jsonl_resume", "jsonl"), ("compaction", "jsonl"), ("cancel", "jsonl"),
        ("fault_429", "jsonl"), ("fault_529", "jsonl"), ("fault_reset", "jsonl"),
        ("fault_stall", "jsonl"), ("subagents", "jsonl"), ("tui", "tui"),
    )
    # Named by `--only`, absent from the default run. Listing them on
    # SCENARIOS would make every paid smoke hit the round cap.
    OPT_IN = (("wrap_up", "cli"), ("py_ptc", "cli"))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("route", nargs="?", default="grok", choices=sorted(live.ROUTES))
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    parser.add_argument("--fixture-seed", type=int, help="replay randomized scenario inputs")
    parser.add_argument("--artifacts", type=Path, help="fresh attempt artifact directory")
    parser.add_argument("--only", default="",
                        help="comma-separated scenario names, including long-session names")
    return parser


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    route = live.ROUTES[args.route]
    stamp = time.strftime("%Y%m%d-%H%M%S")
    artifacts = args.artifacts or live.ROOT / "target" / "live" / f"smoke-{route.name}-{stamp}"
    env = live.Env(args.binary.resolve(), artifacts / "home", artifacts)
    env.fixture_seed = args.fixture_seed
    binary_start = live.binary_sha256(env.binary)
    live.write_home(env.home, route)
    smoke = Smoke(route, env)
    only = {name for name in args.only.split(",") if name}
    results = []
    evidence = {}
    for name, surface in selected_scenarios(only):
        (artifacts / name).mkdir(parents=True, exist_ok=True)
        started = time.monotonic()
        usage_before = live.session_usage(env.home)
        segment_start = len(env.capture_segments)
        try:
            detail, outcome = run_named(smoke, name), PASS
        except ScenarioFailure as failure:
            outcome, detail = failure.outcome, str(failure)
        except Exception as error:  # a bug in this driver is a harness failure too
            outcome, detail = HARNESS, f"driver error: {error!r}"
            (artifacts / name / "traceback.txt").write_text(traceback.format_exc())
        if outcome not in (PASS, features.REFUSED) and name in EXPECTED_PENDING:
            outcome, detail = PENDING, f"{detail} [{EXPECTED_PENDING[name]}]"
        segments = env.capture_segments[segment_start:]
        complete = capture_complete(segments)
        evidence[name] = {"segments": [str(segment) for segment in segments],
                          "capture_complete": complete,
                          "captured_exchanges": sum(len(list(segment.glob("*.info")))
                                                    for segment in segments)}
        if outcome == PASS and not complete:
            outcome, detail = HARNESS, "raw exchange evidence missing or incomplete"
        usage = scenario_usage(env.home, artifacts / name / "home", usage_before)
        result = live.Result(name, surface, outcome, detail, time.monotonic() - started,
                             usage.input_tokens, usage.output_tokens,
                             usage.cached_input_tokens, usage.reasoning_tokens)
        results.append(result)
        print(f"{result.outcome:8} {name}: {detail}", flush=True)
    binary_end = live.binary_sha256(env.binary)
    # A refused route row stops before inference, so it has no exchange to capture.
    scored = [name for name, item in evidence.items()
              if next(r.outcome for r in results if r.scenario == name) != features.REFUSED]
    scored_segments = [Path(segment) for name in scored
                       for segment in evidence[name]["segments"]]
    evidence_complete = (not scored or capture_complete(scored_segments)) and all(
        evidence[name]["capture_complete"] for name in scored)
    (artifacts / "report.json").write_text(json.dumps(
        {**live.route_report(route), "binary": str(env.binary),
         "binary_sha256_start": binary_start,
         "binary_sha256_end": binary_end,
         "token_telemetry": live.sum_usage(
             live.TokenUsage(r.input_tokens, r.output_tokens,
                             r.cached_input_tokens, r.reasoning_tokens)
             for r in results),
         "evidence": evidence,
         "capture_complete": evidence_complete,
         "results": [r.__dict__ for r in results]}, indent=2) + "\n")
    print("\n" + live.table(results) + f"\n\nartifacts: {artifacts}")
    return 0 if (all(r.outcome in (PASS, PENDING, features.REFUSED) for r in results)
                 and evidence_complete
                 and binary_start == binary_end) else 1


def scenario_usage(env_home: Path, private: Path, before: live.TokenUsage) -> live.TokenUsage:
    """Token delta for one scenario, including a private home's session logs.

    Long-session, wrap_up and py_ptc keep their logs off the shared smoke home.
    """
    usage = live.usage_delta(live.session_usage(env_home), before)
    for home in sorted(private.parent.glob(private.name + "*")):
        if home.is_dir():
            usage = usage.add(live.session_usage(home))
    return usage


def run_named(runner: Smoke, name: str) -> str:
    """A default or opt-in smoke scenario, or a long-session scenario."""
    if name in dict((*Smoke.SCENARIOS, *Smoke.OPT_IN)):
        return getattr(runner, name)()
    if name in dict(features.CATALOG):
        return features.play(runner.route, runner.env, name)
    import long_session
    return long_session.play(runner.route, runner.env, name)


def selected_scenarios(only: set[str]) -> tuple[tuple[str, str], ...]:
    """The default run, or the named subset of default and opt-in scenarios."""
    if not only:
        return Smoke.SCENARIOS
    import long_session
    catalog = (*Smoke.SCENARIOS, *Smoke.OPT_IN, *long_session.CATALOG, *features.CATALOG)
    return tuple(item for item in catalog if item[0] in only)


def _session_id(out: str) -> str | None:
    return next((line.split("=", 1)[1] for line in out.splitlines()
                 if line.startswith("session_id=")), None)


def _reject_binding_loss(what: str, log: Path, out: str, records: list[dict]) -> None:
    blob = "\n".join((log.read_text(errors="replace"), out, json.dumps(records)))
    found = next((needle for needle in BINDING_LOSS if needle in blob), None)
    if found:
        raise ScenarioFailure(HARNESS, f"{what}: {found}")


def _stderr_excerpt(log: Path) -> str:
    text = log.read_text(errors="replace") if log.is_file() else ""
    stderr = text.split("--- stderr\n", 1)[-1]
    return " ".join(stderr.split())[:200]


def _raise_cli_failure(code: int, log: Path, what: str, missing_session: bool = False,
                       segments: list[Path] | None = None) -> None:
    detail = _stderr_excerpt(log)
    if missing_session and code == 0:
        raise ScenarioFailure(HARNESS, f"{what} exited 0 without a session id")
    live.classify_cli_failure(code, detail, what, segments=segments)


def _require_cli_answer(out: str, records: list[dict], what: str) -> None:
    text = "\n".join(line for line in out.splitlines()
                     if not line.startswith("session_id=")).strip()
    logged = any(record.get("kind") == "turn.assistant" and str(record.get("text", "")).strip()
                 for record in records)
    if not text and not logged:
        raise ScenarioFailure(MODEL, f"{what} completed without assistant text")


def streamed_text(body: str) -> str:
    """A captured response body plus its Messages-stream tool input and text.

    Messages SSE splits a tool call's input across `input_json_delta`
    fragments, so a string the model wrote (`TOKEN[::-1]`) is rarely
    contiguous in the raw body. The request bodies cannot stand in: they
    repeat the prompt, which names the same strings.
    """
    blocks: dict[int, list[str]] = {}
    for line in body.splitlines():
        if not line.startswith("data: "):
            continue
        try:
            event = json.loads(line[len("data: "):])
        except ValueError:
            continue
        delta = event.get("delta") if isinstance(event, dict) else None
        if not isinstance(delta, dict):
            continue
        piece = delta.get("partial_json") or delta.get("text")
        if isinstance(piece, str):
            blocks.setdefault(int(event.get("index", 0)), []).append(piece)
    return "\n".join([body, *("".join(parts) for _, parts in sorted(blocks.items()))])


def _require_py_ptc_oracle(out: str, records: list[dict], exchanges: str) -> None:
    if "exec_py" not in exchanges or "TOKEN[::-1]" not in exchanges:
        raise ScenarioFailure(MODEL, "py-ptc turn did not run the second exec_py cell")
    if "handled ValueError" not in exchanges:
        raise ScenarioFailure(MODEL, "py-ptc turn has no handled-exception output")
    answer = "\n".join([out, *(str(record.get("text", "")) for record in records
                               if record.get("kind") == "turn.assistant")])
    if PY_PTC_TOKEN[::-1] not in answer:
        raise ScenarioFailure(MODEL, "py-ptc answer lacks the reversed token")


def capture_complete(segments: list[Path]) -> bool:
    """A successful smoke needs terminal raw evidence from every launched process."""
    return (bool(segments) and all(any(segment.glob("*.info")) for segment in segments)
            and live.capture_integrity(segments))


if __name__ == "__main__":
    signal.signal(signal.SIGINT, signal.default_int_handler)
    raise SystemExit(main(sys.argv[1:]))
