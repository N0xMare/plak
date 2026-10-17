#!/usr/bin/env python3
"""One long live session doing real work on a pinned open-source repository.

Usage: soak.py [route] [--hours N] [--binary PATH] [--no-rebuild]

The session works through a cycle of software tasks on more-itertools, pinned
at one commit and cloned under target/live/. Every task reopens the session in
a new `plak agent` process, so reopen is exercised throughout. Along the way
the driver injects transport faults, freezes the process group with SIGSTOP
and SIGCONT to simulate a laptop sleeping, compacts, and once rebuilds `plak`
and reopens the session on the new binary. It records per-task time, tokens,
peak context, compactions, errors and dead ends: a failed turn after which a
plain follow-up also fails. Artifacts land in target/live/soak-<route>-<stamp>/.
"""

from __future__ import annotations

import argparse
import ast
import dataclasses
import hashlib
import json
import re
import signal
import stat
import subprocess
import sys
import threading
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402
from live import ScenarioFailure  # noqa: E402

REPO_URL = "https://github.com/more-itertools/more-itertools.git"
REPO_COMMIT = "4998a257373f36e91edef7324572ed8df10f39d1"  # v10.5.0
REBUILD = ["cargo", "build", "--release", "--locked", "--features", "plak/test-hooks", "-p", "plak"]

# Bugs the driver plants before asking for a fix: (file, function, correct
# text, broken text). The correct text is in the pinned commit's function.
BUGS = {
    "ilen": ("more_itertools/more.py", "ilen", "return sum(compress(repeat(1), zip(iterable)))",
             "return sum(compress(repeat(2), zip(iterable)))"),
    "quantify": ("more_itertools/recipes.py", "quantify", "return sum(map(pred, iterable))",
                 "return sum(map(bool, iterable))"),
}


@dataclasses.dataclass
class Task:
    name: str
    prompt: str
    check: str | None = None  # Python run in the repo that must exit 0
    bug: str | None = None


def tasks(cycle: int) -> list[Task]:
    """One cycle of work. Names carry the cycle so repeated cycles add new code."""
    fn = f"every_nth_{cycle}"
    return [
        Task("explore", "Which module defines `chunked`, and which test file covers it? "
             "Answer in two short lines."),
        Task("fix_ilen", "The test suite (`python3 -m unittest -q`) fails. Find the bug and "
             "fix it, then run the suite.", bug="ilen"),
        Task("feature", f"Add a function `{fn}(iterable, n)` to more_itertools/more.py that "
             f"yields every n-th item starting with the first, export it in `__all__`, add a "
             f"stub to more.pyi and tests to tests/test_more.py, then run the suite.",
             check=f"from more_itertools import {fn}; "
                   f"assert list({fn}(range(10), 3)) == [0, 3, 6, 9]"),
        Task("fix_quantify", "The test suite fails again. Find and fix the bug, then run "
             "the suite.", bug="quantify"),
        Task("docs", f"Add `{fn}` to the API listing in docs/api.rst next to `chunked`.",
             check=f"assert '{fn}' in open('docs/api.rst').read()"),
    ]


# Faults sprinkled over the run, one plan per task process, cycling.
def fault_plans(route: live.Route) -> list[str | None]:
    return [None, live.fault_plan(route, "429", retry_after=1),
            live.fault_plan(route, "reset", after_events=4).replace("nth=1", "nth=2"),
            None, live.fault_plan(route, "stall", seconds=15, after_events=3)]


@dataclasses.dataclass
class Record:
    task: str
    cycle: int
    faults: str | None
    outcome: str = ""
    seconds: float = 0.0
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0
    peak_context: int = 0
    tools: int = 0
    compactions: int = 0
    slept: bool = False
    dead_end: bool = False
    errors: list[str] = dataclasses.field(default_factory=list)
    process_id: int | None = None
    captured_exchanges: int = 0
    authority_predicates: list[str] = dataclasses.field(default_factory=list)
    evidence_status: str = ""
    segment: str | None = None


def segment_evidence(capture_dir: Path, diagnostics: Path,
                     diagnostic_offset: int) -> tuple[int, list[str], str]:
    """Classify a process without treating an absent response as a successful capture."""
    info = list(capture_dir.glob("*.info"))
    if (not diagnostics.is_file() or diagnostics.is_symlink()
            or stat.S_IMODE(diagnostics.stat().st_mode) != 0o600):
        return len(info), [], "diagnostics_missing"
    lines = diagnostics.read_bytes()[diagnostic_offset:].decode(errors="replace")
    predicates = sorted(set(re.findall(
        r'\bpredicate="?((?:route|native)\.[a-z_]+(?:\.[a-z_]+)*)"?', lines)))
    if info:
        return len(info), predicates, "response"
    if predicates:
        return 0, predicates, "authority_witness_no_response"
    return 0, [], "unexplained_no_response"


def prepare_repo(cache: Path, work: Path) -> Path:
    if not (cache / ".git").exists():
        subprocess.run(["git", "clone", "-q", REPO_URL, str(cache)], check=True)
    subprocess.run(["git", "clone", "-q", "--local", str(cache), str(work)], check=True)
    subprocess.run(["git", "-c", "advice.detachedHead=false", "checkout", "-q", REPO_COMMIT],
                   cwd=work, check=True)
    return work


def function_span(text: str, name: str) -> tuple[int, int] | None:
    """0-based [start, end) lines of the module-level function `name`, decorators included."""
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return None
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == name:
            start = min([node.lineno, *(item.lineno for item in node.decorator_list)]) - 1
            return start, node.end_lineno
    return None


def plant(repo: Path, bug: str) -> None:
    """Replace the function with the pinned commit's version, broken.

    An earlier cycle's fix may restore the function correctly but in other
    words, and later tasks add code to the same file, so neither a text match
    nor restoring the whole file works. Only the function is reset.
    """
    path, name, good, bad = BUGS[bug]
    pinned = subprocess.run(["git", "show", f"{REPO_COMMIT}:{path}"], cwd=repo,
                            capture_output=True, text=True, check=True).stdout
    pinned_span = function_span(pinned, name)
    if pinned_span is None:
        raise ScenarioFailure(live.HARNESS, f"cannot plant {bug}: {name} is not in {REPO_COMMIT}")
    original = pinned.splitlines(keepends=True)[pinned_span[0]:pinned_span[1]]
    broken = "".join(original)
    if good not in broken:
        raise ScenarioFailure(live.HARNESS, f"cannot plant {bug}: the pinned {name} moved")
    lines = (repo / path).read_text().splitlines(keepends=True)
    span = function_span("".join(lines), name)
    if span is None:
        raise ScenarioFailure(live.HARNESS, f"cannot plant {bug}: {path} has no parseable {name}")
    lines[span[0]:span[1]] = [broken.replace(good, bad, 1)]
    (repo / path).write_text("".join(lines))


def check(repo: Path, task: Task, answer: str = "") -> bool:
    if not live.unittest_passes(repo):
        return False
    if task.name == "explore":
        return ("more_itertools.more" in answer or "more_itertools/more.py" in answer) \
            and "test_more.py" in answer
    if task.check:
        return subprocess.run(["python3", "-c", task.check], cwd=repo).returncode == 0
    return True


class Soak:
    def __init__(self, route: live.Route, env: live.Env, repo: Path):
        self.route, self.env, self.repo = route, env, repo
        self.capture_root = env.capture_root
        self.session: str | None = None
        self.segment = 0
        self.peak_context = 0

    def open(self, faults: str | None) -> live.Agent:
        self.segment += 1
        args = ["--session", self.session] if self.session else []
        capture_dir = self.capture_root / f"segment-{self.segment:03d}"
        capture_dir.mkdir(mode=0o700)
        agent = live.Agent(self.env, self.repo, f"segment-{self.segment:03d}", args=args,
                           faults=faults, capture_dir=capture_dir)
        if self.session and agent.session_id != self.session:
            raise ScenarioFailure(live.HARNESS, "reopen attached a different session")
        self.session = agent.session_id
        return agent

    def run_task(self, task: Task, cycle: int, faults: str | None, sleep: bool) -> Record:
        record = Record(task.name, cycle, faults)
        segment_before = self.segment
        diagnostics = self.env.home / "logs/plak.log"
        diagnostic_offset = diagnostics.stat().st_size if diagnostics.is_file() else 0
        before = live.committed_compactions(live.session_records(self.env.home, self.session)) \
            if self.session else 0
        agent = None
        timer = None

        def observe(event: dict) -> None:
            nonlocal timer
            record.peak_context = max(record.peak_context,
                                      event.get("peak_prepared_input_tokens", 0))
            self._note_peak(event)
            if sleep and timer is None and event.get("kind") == "tool":
                agent.signal(signal.SIGSTOP)  # the lid closes mid-tool
                record.slept = True
                timer = threading.Timer(20, agent.signal, (signal.SIGCONT,))
                timer.start()

        started = time.monotonic()
        try:
            if task.bug:
                plant(self.repo, task.bug)
            agent = self.open(faults)
            record.process_id = agent.proc.pid
            turn = agent.prompt(task.prompt, timeout=900, on_event=observe)
            answer = turn.text
            record.tools, record.errors = turn.tools, turn.errors
            record.input_tokens, record.output_tokens = turn.input_tokens, turn.output_tokens
            record.cached_input_tokens = turn.cached_input_tokens
            record.reasoning_tokens = turn.reasoning_tokens
            after = ""
            if not turn.completed:
                reason = live.provider_reason_from(
                    turn.model_error_kind, turn.http_status, " ".join(turn.errors))
                validation = live.stream_validation_outcome(
                    " ".join(turn.errors), [agent.capture_dir])
                if reason or validation == live.PROVIDER:
                    record.outcome = "provider"
                    record.errors.append(reason or "provider stream violated the documented protocol")
                    raise live.ScenarioFailure(live.PROVIDER, record.errors[-1])
                after = f" after {turn.phase}:{turn.model_error_kind}"
                follow = agent.prompt("Continue the task.", timeout=900, on_event=observe)
                answer = follow.text
                record.input_tokens += follow.input_tokens
                record.output_tokens += follow.output_tokens
                record.cached_input_tokens += follow.cached_input_tokens
                record.reasoning_tokens += follow.reasoning_tokens
                record.errors += follow.errors
                record.dead_end = not follow.completed
            if record.dead_end:
                record.outcome = "dead-end" + after
            else:
                record.outcome = ("pass" if check(self.repo, task, answer) else "model") + after
        except ScenarioFailure as failure:
            record.outcome = failure.outcome if failure.outcome in (
                live.PROVIDER, live.MODEL, live.HARNESS) else "harness"
            record.errors = [str(failure)]
        finally:
            if timer:
                timer.join()
            if agent is not None and agent.proc.poll() is None:
                agent.quit(timeout=60)
        if self.segment == segment_before:
            record.evidence_status = "not_started"
        else:
            capture_dir = self.capture_root / f"segment-{self.segment:03d}"
            record.segment = str(capture_dir)
            (record.captured_exchanges, record.authority_predicates,
             record.evidence_status) = segment_evidence(
                 capture_dir, diagnostics, diagnostic_offset)
            if (record.outcome.startswith(("pass", "model"))
                    and record.captured_exchanges == 0
                    and record.evidence_status != "diagnostics_missing"):
                record.evidence_status = "unexplained_no_response"
        record.seconds = time.monotonic() - started
        if self.session:
            record.compactions = live.committed_compactions(
                live.session_records(self.env.home, self.session)) - before
        return record

    def _note_peak(self, event: dict) -> None:
        value = event.get("peak_prepared_input_tokens", 0)
        if type(value) is int and value > self.peak_context:
            self.peak_context = value

    def _logged_peak(self) -> int:
        if not self.session:
            return 0
        import long_session
        peak = long_session.prepared_peak(live.session_records(self.env.home, self.session))
        return peak or 0

    def _prompt(self, text: str, what: str) -> None:
        agent = self.open(None)
        try:
            turn = agent.prompt(text, timeout=300, on_event=self._note_peak)
            if not turn.completed:
                live.classify_incomplete(turn, what, segments=[agent.capture_dir])
        finally:
            if agent.proc.poll() is None:
                agent.quit()

    def _ensure_foldable_context(self) -> None:
        """A manual compact of a tiny history cannot prove a shrink.

        The latest intent is protected, so the padding read is followed by a
        short turn. That leaves the large read in the older group. A session
        that has already prepared at least that much is left alone.
        """
        if not live.needs_compaction_fold(max(self.peak_context, self._logged_peak())):
            return
        (self.repo / "fold.txt").write_text(live.compaction_padding())
        self._prompt("Read fold.txt in full with one read. Reply: ok", "fold padding")
        self._prompt("Reply: ok", "fold tail")

    def compact(self) -> None:
        self._ensure_foldable_context()
        agent = self.open(None)
        try:
            answer = agent.command("compact", timeout=600)
            if answer.get("kind") == "error":
                live.classify_command_error(
                    str(answer.get("message") or ""), "manual compact",
                    segments=[agent.capture_dir])
        finally:
            agent.quit()


def rebuild(binary: Path, log: Path) -> bool:
    """Build a demonstrably different executable, then restore the source tree.

    A mere touch can leave an identical artifact and does not show that a
    session crosses a changed build. The used marker changes the executable
    bytes without changing product behavior.
    """
    source = live.ROOT / "bin/plak/src/main.rs"
    original = source.read_bytes()
    before = hashlib.sha256(binary.read_bytes()).digest() if binary.is_file() else None
    marker = f'\n#[used]\nstatic LIVE_SOAK_REBUILD: &[u8] = b"{time.time_ns()}";\n'.encode()
    try:
        source.write_bytes(original + marker)
        result = subprocess.run(REBUILD, cwd=live.ROOT, capture_output=True, text=True)
    finally:
        source.write_bytes(original)
    log.write_text(result.stdout + result.stderr)
    after = hashlib.sha256(binary.read_bytes()).digest() if binary.is_file() else None
    return result.returncode == 0 and before is not None and after is not None and before != after


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("route", nargs="?", default="grok", choices=sorted(live.ROUTES))
    parser.add_argument("--hours", type=float, default=1.0)
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    parser.add_argument("--artifacts", type=Path, help="fresh attempt artifact directory")
    parser.add_argument("--no-rebuild", action="store_true")
    return parser


def main(argv: list[str]) -> int:
    # Long-session scenarios are opt-in and bounded. They are not the hour loop.
    if "--long-session" in argv:
        import long_session
        forwarded = [arg for arg in argv if arg != "--long-session"]
        return long_session.main(forwarded)
    parser = build_parser()
    args = parser.parse_args(argv)
    route = live.ROUTES[args.route]
    artifacts = args.artifacts or live.ROOT / "target" / "live" / f"soak-{route.name}-{time.strftime('%Y%m%d-%H%M%S')}"
    artifacts.mkdir(parents=True)
    env = live.Env(args.binary.resolve(), artifacts / "home", artifacts)
    binary_start = live.binary_sha256(env.binary)
    live.write_home(env.home, route)
    repo = prepare_repo(live.ROOT / "target/live/cache/more-itertools", artifacts / "repo")
    capture_root = env.capture_root
    soak = Soak(route, env, repo)
    plans = fault_plans(route)
    started = time.monotonic()
    deadline = started + args.hours * 3600
    records: list[Record] = []
    rebuilt = args.no_rebuild
    rebuild_ok = args.no_rebuild
    binary_rebuilt = None
    cycle, index = 0, 0
    failed = False
    crash = None
    try:
        while time.monotonic() < deadline:
            cycle += 1
            for task in tasks(cycle):
                if time.monotonic() >= deadline:
                    break
                record = soak.run_task(task, cycle, plans[index % len(plans)], sleep=index % 4 == 2)
                index += 1
                records.append(record)
                print(f"{record.outcome:28} c{cycle} {task.name:13} {record.seconds:5.0f}s "
                      f"ctx={record.peak_context} comp={record.compactions}"
                      f"{' slept' if record.slept else ''}{' faults' if record.faults else ''}"
                      f"{' dead-end' if record.dead_end else ''}", flush=True)
                # A provider refusal is recorded and stops the hour. It is not a
                # harness failure and it is not retried.
                if record.outcome in ("harness", "provider") or record.dead_end:
                    failed = True
                    break
                if index % 3 == 0:
                    try:
                        soak.compact()
                    except ScenarioFailure as error:
                        records.append(Record("compact", cycle, None, outcome=error.outcome,
                                              errors=[str(error)]))
                        failed = True
                        break
                if not rebuilt and time.monotonic() > started + args.hours * 1800:
                    rebuilt = True
                    rebuild_ok = rebuild(env.binary, artifacts / "rebuild.log")
                    binary_rebuilt = live.binary_sha256(env.binary)
                    print(f"rebuild: {'ok' if rebuild_ok else 'FAILED (see rebuild.log)'}", flush=True)
            if failed:
                break
    except Exception:
        # The report below still records every task that ran.
        crash = traceback.format_exc()
        print(crash, file=sys.stderr, flush=True)
    summary = {
        **live.route_report(route), "task_repo_commit": REPO_COMMIT,
        "binary_sha256_start": binary_start,
        "binary_sha256_rebuilt": binary_rebuilt,
        "binary_sha256_end": live.binary_sha256(env.binary),
        "session": soak.session, "tasks": len(records),
        "passed": sum(r.outcome.startswith("pass") for r in records),
        "harness_failures": sum(r.outcome == "harness" for r in records),
        "provider_failures": sum(r.outcome == "provider" for r in records),
        "dead_ends": sum(r.dead_end for r in records),
        "compactions": live.committed_compactions(live.session_records(env.home, soak.session))
        if soak.session else 0,
        "tokens": sum(r.input_tokens + r.output_tokens for r in records),
        "token_telemetry": live.sum_usage(
            live.TokenUsage(r.input_tokens, r.output_tokens,
                            r.cached_input_tokens, r.reasoning_tokens)
            for r in records),
        "hours": round((time.monotonic() - started) / 3600, 2),
        "tasks_per_hour": round(len(records) * 3600 / (time.monotonic() - started), 1),
        "peak_context": max((r.peak_context for r in records), default=0),
        "rebuild": "skipped" if args.no_rebuild else ("changed" if rebuild_ok else "failed"),
        "rebuild_attempted": rebuilt and not args.no_rebuild,
        "crash": crash,
    }
    bodies = list(capture_root.glob("segment-*/*.body"))
    info = list(capture_root.glob("segment-*/*.info"))
    summary["captured_exchanges"] = len(info)
    segments = env.capture_segments
    summary["capture_segments"] = [str(segment) for segment in segments]
    summary["capture_complete"] = (bool(info) and len(bodies) == len(info)
                                   and live.capture_integrity(segments))
    summary["evidence_gaps"] = sum(
        record.evidence_status in ("diagnostics_missing", "unexplained_no_response")
        for record in records)
    (artifacts / "report.json").write_text(json.dumps(
        {"summary": summary, "records": [dataclasses.asdict(r) for r in records]}, indent=2) + "\n")
    print(json.dumps(summary, indent=2) + f"\nartifacts: {artifacts}")
    return 0 if (crash is None and summary["harness_failures"] == 0 and summary["provider_failures"] == 0
                 and summary["dead_ends"] == 0
                 and rebuild_ok and summary["capture_complete"]
                 and summary["binary_sha256_end"] == (binary_start if args.no_rebuild else binary_rebuilt)
                 and summary["evidence_gaps"] == 0) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
