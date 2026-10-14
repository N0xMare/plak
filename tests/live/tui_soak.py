#!/usr/bin/env python3
"""Long live coding soak through the real TUI.

Usage: tui_soak.py [route] [--hours N] [--binary PATH]

This is paid, opt-in evidence. It keeps one logical session on a PTY, waits for
durable turn lifecycle terminals, checks every coding task independently, and
quits/reopens the TUI once. Artifacts land in target/live/tui-soak-<route>-<stamp>/.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import signal
import stat
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402

SEED = {
    "arithmetic.py": "def add(a, b):\n    return a + b\n",
    "test_arithmetic.py": (
        "import unittest\n\nfrom arithmetic import add\n\n\n"
        "class NumberTests(unittest.TestCase):\n"
        "    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n"
    ),
}


@dataclasses.dataclass(frozen=True)
class Task:
    name: str
    prompt: str
    check: str


def task(cycle: int, slot: int) -> Task:
    """Return a bounded two-file task with a check independent of TUI output."""
    factor = cycle * 3 + slot + 2
    function = f"scale_{factor}"
    return Task(
        function,
        f"In arithmetic.py add `{function}(value)` returning value * {factor}. In "
        f"test_arithmetic.py import it and add tests for positive and negative values. "
        "Do not delegate. Run `python3 -m unittest -q` and finish the turn.",
        f"from arithmetic import {function}; assert {function}(4) == {factor * 4}; "
        f"assert {function}(-2) == {-factor * 2}",
    )


def check_task(repo: Path, item: Task) -> bool:
    suite = subprocess.run(["python3", "-m", "unittest", "-q"], cwd=repo,
                           capture_output=True, text=True, timeout=120)
    oracle = subprocess.run(["python3", "-c", item.check], cwd=repo,
                            capture_output=True, text=True, timeout=30)
    tests = (repo / "test_arithmetic.py").read_text()
    return suite.returncode == 0 and oracle.returncode == 0 and tests.count(item.name) >= 2


def terminal_records(home: Path, session: str) -> list[dict]:
    return [record for record in live.session_records(home, session)
            if record.get("kind") == "turn.lifecycle"
            and record.get("phase") not in (None, "admitted")]


def terminals(home: Path, session: str) -> list[str]:
    return [record.get("phase", "") for record in terminal_records(home, session)]


def raise_if_provider_terminal(home: Path, session: str, index: int, what: str) -> None:
    """A TUI terminal that is a typed provider refusal is provider, not harness."""
    records = terminal_records(home, session)
    if index >= len(records):
        return
    record = records[index]
    status = record.get("http_status")
    turn = live.Turn(
        phase=record.get("phase"),
        model_error_kind=record.get("model_error_kind"),
        http_status=status if type(status) is int else None,
        errors=[str(record.get("message") or "")],
        refusals=live.durable_refusals(record),
    )
    if turn.completed:
        return
    reason = live.provider_reason_from(turn.model_error_kind, turn.http_status, " ".join(turn.errors))
    if reason:
        raise live.ScenarioFailure(live.PROVIDER, f"{what}: {reason}")
    if turn.refusals:
        raise live.ScenarioFailure(live.PROVIDER, f"{what}: provider refused the request")


def admissions(home: Path, session: str) -> int:
    return sum(record.get("kind") == "turn.lifecycle" and record.get("phase") == "admitted"
               for record in live.session_records(home, session))


def discover_session(home: Path, timeout: float = 30) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        logs = list(home.glob("state/*/sessions/*.events.jsonl"))
        if len(logs) == 1:
            return logs[0].name.removesuffix(".events.jsonl")
        if len(logs) > 1:
            raise live.ScenarioFailure(live.HARNESS, "TUI created more than one root session")
        time.sleep(0.2)
    raise live.ScenarioFailure(live.HARNESS, "TUI session log did not appear")


def usage_since(home: Path, before: live.TokenUsage) -> dict[str, int]:
    """Tokens recorded in the session log since `before`."""
    return live.usage_delta(live.session_usage(home), before).as_dict()


def send_and_wait(tui: live.Tui, home: Path, session: str, prompt: str,
                  timeout: float = 900, sleep_once: bool = False) -> str:
    before = len(terminals(home, session))
    admitted_before = admissions(home, session)
    tui.type(prompt)
    tui.key(b"\r")
    timer = None
    if sleep_once:
        if not tui.wait_until(lambda: admissions(home, session) > admitted_before, 30):
            raise live.ScenarioFailure(live.HARNESS, "TUI did not admit the turn before sleep")
        if len(terminals(home, session)) > before:
            raise live.ScenarioFailure(live.HARNESS, "turn finished before sleep injection")
        # Simulate a sleeping laptop after the turn was durably admitted.
        os.killpg(tui.pid, signal.SIGSTOP)
        timer = threading.Timer(10, os.killpg, (tui.pid, signal.SIGCONT))
        timer.start()
    try:
        found = tui.wait_until(lambda: len(terminals(home, session)) > before, timeout)
    finally:
        if timer:
            timer.join()
    if not found:
        raise live.ScenarioFailure(live.HARNESS, f"no durable terminal within {timeout:.0f}s")
    return terminals(home, session)[before]


def task_evidence(segment: Path) -> dict:
    exchanges = len(list(segment.glob("*.info")))
    return {"segment": str(segment), "captured_exchanges": exchanges,
            "evidence_status": "response" if exchanges else "unexplained_no_response"}


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("route", nargs="?", default="grok", choices=sorted(live.ROUTES))
    parser.add_argument("--hours", type=float, default=2.0)
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    parser.add_argument("--artifacts", type=Path, help="fresh attempt artifact directory")
    return parser


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.hours <= 0:
        parser.error("--hours must be positive")

    route = live.ROUTES[args.route]
    stamp = time.strftime("%Y%m%d-%H%M%S")
    artifacts = args.artifacts or live.ROOT / "target/live" / f"tui-soak-{route.name}-{stamp}"
    artifacts.mkdir(parents=True)
    env = live.Env(args.binary.resolve(), artifacts / "home", artifacts)
    binary_start = live.binary_sha256(env.binary)
    live.write_home(env.home, route)
    repo = live.git_repo(artifacts / "repo", SEED)
    capture_root = env.capture_root
    first_capture = capture_root / "segment-001"
    first_capture.mkdir(mode=0o700)
    tui = live.Tui(env, repo, artifacts / "segment-001.pty", capture_dir=first_capture)
    process_ids = [tui.pid]
    tui.pump(3)
    session = discover_session(env.home)
    started = time.monotonic()
    deadline = started + args.hours * 3600
    records = []
    reopened = False
    slept = False
    cycle = slot = 0
    failure = None
    try:
        while time.monotonic() < deadline or not reopened:
            item = task(cycle, slot)
            began = time.monotonic()
            usage_before = live.session_usage(env.home)
            phase = send_and_wait(tui, env.home, session, item.prompt, sleep_once=not slept)
            slept = True
            passed = phase == "completed" and check_task(repo, item)
            records.append({"task": item.name, "phase": phase, "oracle": passed,
                            "seconds": round(time.monotonic() - began, 1),
                            **task_evidence(tui.capture_dir),
                            **usage_since(env.home, usage_before)})
            print(f"{'pass' if passed else 'FAIL':5} {item.name} terminal={phase}", flush=True)
            if phase != "completed":
                raise_if_provider_terminal(
                    env.home, session, len(terminal_records(env.home, session)) - 1, item.name)
                if phase != "failed":
                    raise live.ScenarioFailure(live.HARNESS, f"{item.name}: terminal={phase}")
                # A provider request can fail after transmission without being
                # safe to replay. The next prompt must still be able to finish
                # the work on this same attached TUI session.
                recovery_before = live.session_usage(env.home)
                followup = send_and_wait(
                    tui, env.home, session,
                    f"The previous request failed. Finish the task for {item.name}, "
                    "run python3 -m unittest -q, then finish the turn.")
                recovered = followup == "completed" and check_task(repo, item)
                records[-1]["recovered"] = followup == "completed"
                records.append({"task": f"{item.name}_recovery", "phase": followup,
                                "oracle": recovered,
                                **task_evidence(tui.capture_dir),
                                **usage_since(env.home, recovery_before)})
                if followup != "completed":
                    raise_if_provider_terminal(
                        env.home, session, len(terminal_records(env.home, session)) - 1,
                        f"{item.name} follow-up")
                    raise live.ScenarioFailure(
                        live.HARNESS, f"{item.name}: follow-up terminal={followup}")
                print(f"{'pass' if recovered else 'MISS':5} {item.name} recovery={followup}",
                      flush=True)
            if not reopened:
                if tui.quit() != 0:
                    raise live.ScenarioFailure(live.HARNESS, "first TUI did not quit cleanly")
                second_capture = capture_root / "segment-002"
                second_capture.mkdir(mode=0o700)
                tui = live.Tui(env, repo, artifacts / "segment-002.pty",
                               args=["--resume", session], capture_dir=second_capture)
                process_ids.append(tui.pid)
                tui.pump(3)
                if discover_session(env.home) != session:
                    raise live.ScenarioFailure(live.HARNESS, "reopen changed the session")
                reopen_before = live.session_usage(env.home)
                phase = send_and_wait(
                    tui, env.home, session,
                    "After the simulated sleep and reopen, create followup.txt containing "
                    "exactly `sleep and reopen verified` (plus a final newline), then finish.")
                followup = repo / "followup.txt"
                if phase != "completed":
                    raise_if_provider_terminal(
                        env.home, session, len(terminal_records(env.home, session)) - 1,
                        "post-sleep/reopen")
                    raise live.ScenarioFailure(live.HARNESS, "post-sleep/reopen turn failed")
                records.append({"task": "reopen_followup", "phase": phase,
                                "oracle": followup.is_file() and
                                followup.read_text() == "sleep and reopen verified\n",
                                **task_evidence(tui.capture_dir),
                                **usage_since(env.home, reopen_before)})
                reopened = True
            slot += 1
            if slot == 3:
                cycle, slot = cycle + 1, 0
    except live.ScenarioFailure as error:
        failure = {"outcome": error.outcome, "detail": str(error)}
    finally:
        try:
            quit_code = tui.quit()
        except OSError:
            quit_code = None

    summary = {**live.route_report(route), "session": session,
               "binary_sha256_start": binary_start,
               "binary_sha256_end": live.binary_sha256(env.binary),
               "hours": round((time.monotonic() - started) / 3600, 2),
               "tasks": len(records), "reopened": reopened, "slept": slept,
               "model_misses": sum(record["phase"] == "completed" and not record["oracle"]
                                   for record in records),
               "turn_failures": sum(record["phase"] == "failed" and not record.get("recovered")
                                    for record in records),
               "clean_quit": quit_code == 0, "failure": failure,
               "token_telemetry": live.session_usage(env.home).as_dict()}
    bodies = list(capture_root.glob("segment-*/*.body"))
    info = list(capture_root.glob("segment-*/*.info"))
    diagnostics = env.home / "logs/plak.log"
    diagnostic_stat = diagnostics.lstat() if diagnostics.exists() else None
    summary["diagnostics_available"] = (diagnostic_stat is not None
                                        and stat.S_ISREG(diagnostic_stat.st_mode)
                                        and stat.S_IMODE(diagnostic_stat.st_mode) == 0o600)
    summary["process_ids"] = process_ids
    segments = env.capture_segments
    summary["capture_segments"] = [str(segment) for segment in segments]
    summary["captured_exchanges"] = len(info)
    summary["capture_complete"] = (bool(segments)
                                   and all(any(segment.glob("*.info")) for segment in segments)
                                   and len(bodies) == len(info)
                                   and live.capture_integrity(segments))
    (artifacts / "report.json").write_text(json.dumps(
        {"summary": summary, "records": records}, indent=2) + "\n")
    print(json.dumps(summary, indent=2) + f"\nartifacts: {artifacts}")
    return 0 if (failure is None and quit_code == 0
                 and binary_start == summary["binary_sha256_end"]
                 and summary["capture_complete"]
                 and summary["diagnostics_available"]) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
