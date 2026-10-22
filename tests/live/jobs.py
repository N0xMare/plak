#!/usr/bin/env python3
"""Opt-in live scenarios for background jobs.

Usage: jobs.py [route] [--binary PATH] [--only NAME,...]

Each scenario is one short turn on a real route. The model is told to run a
script in the scratch repo; the oracle is that script's marker files, the
process list, JSONL job events, the session log, a captured request, or the
TUI screen. The model's own reply is never the oracle. Nothing here is a
gate, and the module does not call a provider by itself.

`/stop` is not a slash command. `stop_jobs` confirms Processes `S`
(`Stop all processes?`), which stops background processes and not children.
A user stop is still a process notice, so the idle root wakes
(`crates/hosting/hosted/src/runtime/host_loop/wake.rs` lines 156-158;
`[turn] auto_continue` defaults true). The idle HUD reads `1 agent` and
`1 process` from the composer row of a PTY frame taken while that job is
live. A cursor move that repaints one counter does not drop the other.
A child command that never left the foreground is model: the HUD counts
promoted jobs only.
`plak run` waits for a promoted job instead of stopping it when the first
turn ends. Each scenario's process token is `prefix` plus eight hex
characters from `os.urandom` (or a cell's replay seed), so concurrent cells
never share a `ps` or `pkill` pattern.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import re
import signal
import sqlite3
import subprocess
import sys
import time
import traceback
from contextlib import contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402
import crash_triage  # noqa: E402
from live import HARNESS, MODEL, PASS, PROVIDER, ScenarioFailure  # noqa: E402

FOREGROUND_BUDGET = 1
FOREGROUND_HOLD = 45
BUDGET_MS = FOREGROUND_BUDGET * 1000
STEER_LIMIT_S = 8.0
# Faster than this is an immediate background hint (250 ms), not the 1 s budget.
PROMOTION_SOON_S = 0.5
# Slower than this, with the process still running, missed the 1 s budget.
PROMOTION_LATE_S = 6.0
# spawn_agent is async. Wait for the child's job, then for its terminal.
CHILD_WAIT_S = 120.0
# Quiet window is 2 s (spec 02). Leave room for the stop and the admission.
WAKE_AFTER_STOP_S = 30.0
# model_retries.cause_kind values that mean the provider, not the harness.
PROVIDER_RETRY_CAUSES = frozenset({
    "http_status", "connect", "record_rejected_in_transit",
    "transport_after_send", "stream_interrupted",
})
AGENT_BADGE = "1 agent"
PROCESS_BADGE = "1 process"
# Session-log names for a shell start. Wait and poll tools are absent on
# purpose: an open poll must not look like a foreground command.
SHELL_STARTS = frozenset({
    "run_terminal_command",
    "Bash",
    "bash",
    "exec_command",
    "shell",
    "Shell",
    "process_session_start",
    "process-session-start",
    "plakous.process-session-start.v2",
    "plakous.process-session-start.v3",
    "run_process",
})
# Composer border segment: middle dot, a glyph, then "1 agent" / "1 process".
# A transcript quote has no glyph between the dot and the count.
_HUD_PAINT = re.compile(
    "\u00b7[ \t]+\\S[ \t]+1 agent(?:[ \t]+\u00b7[ \t]+\\S[ \t]+1 process)?"
    "|\u00b7[ \t]+\\S[ \t]+1 process"
)
STOP_TITLE = "Stop all processes?"
WARNING = "Plak stopped unexpectedly while this command was running."
STEER_TEXT = "Reply with the single word steered."

HOLD_PY = (
    "import pathlib\n"
    "import sys\n"
    "import time\n"
    "pathlib.Path('job-started').write_text(str(time.time()) + '\\n')\n"
    "time.sleep(int(sys.argv[1]))\n"
    "pathlib.Path('job-finished').write_text('1\\n')\n"
)
SHORT_PY = (
    "import pathlib\n"
    "import time\n"
    "pathlib.Path('short-started').write_text('1\\n')\n"
    "time.sleep(2)\n"
    "pathlib.Path('short-finished').write_text('1\\n')\n"
)
_ANSI = re.compile(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")

# Name, surface. All of them are opt-in relative to smoke; this module is the set.
SCENARIOS = (
    ("promotion_budget", "jsonl"),
    ("steer_command", "jsonl"),
    ("steer_await", "jsonl"),
    ("notice_while_busy", "jsonl"),
    ("run_promoted", "cli"),
    ("crash_jobs", "jsonl"),
    ("child_settle", "jsonl"),
    ("ctrl_b", "tui"),
    ("stop_jobs", "tui"),
    ("wake_while_idle", "tui"),
    ("idle_badge", "tui"),
)


def pin_jobs(home: Path, foreground_seconds: int) -> None:
    """Append `[jobs] foreground_seconds` to a home `write_home` just created.

    The bound is read when the session is created, so it has to be in the
    scratch home before `plak` starts. Spec 07: 1..=600, and it is not a kill timer.
    """
    if not 1 <= foreground_seconds <= 600:
        raise ScenarioFailure(HARNESS, "foreground_seconds is outside 1..=600")
    path = home / "config.toml"
    text = path.read_text()
    if "[jobs]" in text or "foreground_seconds" in text:
        raise ScenarioFailure(HARNESS, "PLAK_HOME already sets job bounds")
    if not text.endswith("\n"):
        text += "\n"
    path.write_text(f"{text}\n[jobs]\nforeground_seconds = {foreground_seconds}\n")


def visible(data: bytes) -> str:
    """PTY bytes with ANSI sequences removed, so a HUD count is searchable."""
    return _ANSI.sub("", data.decode("utf-8", errors="replace"))


def selected(only: set[str]) -> tuple[tuple[str, str], ...]:
    """The whole set, or the named subset. An unknown name is a usage error."""
    if not only:
        return SCENARIOS
    known = {name for name, _surface in SCENARIOS}
    unknown = sorted(only - known)
    if unknown:
        raise ValueError(f"unknown scenario: {', '.join(unknown)}")
    return tuple(item for item in SCENARIOS if item[0] in only)


def hold_command(seconds: int, token: str) -> str:
    return f"python3 hold.py {seconds} {token}"


def pack_of(route: live.Route) -> str:
    """Which process pack this route exposes. Codex and OpenAI are Code Mode."""
    if route.account in ("codex", "openai") or route.model.startswith("gpt-"):
        return "codex"
    if route.account in ("claude", "anthropic") or route.model.startswith("claude-"):
        return "claude"
    if route.account == "openrouter":
        return "pi"
    return "grok"


def code_mode(route: live.Route) -> bool:
    """Native Code Mode. Nested starts promote on hint, never on steer.

    Nested dispatches get neither Steer nor Background preemption, but a
    nested wait still yields its partial result on a job-terminal Notice
    (spec 01 §8.5).
    """
    return pack_of(route) == "codex"


def immediate_hint(route: live.Route) -> str:
    """The pack's documented immediate-background hint, as a prompt sentence.

    Codex `yield_time_ms` 0 and Grok `block_until_ms` 0 are a 250 ms first
    observation. Claude `run_in_background` and pi `timeout` 0 select hint
    zero. Spec 01 §8.5.
    """
    pack = pack_of(route)
    if pack == "codex":
        return "Pass yield_time_ms 0 on that command so it is promoted immediately."
    if pack == "claude":
        return "Pass run_in_background true on that command so it is promoted immediately."
    if pack == "pi":
        return "Pass timeout 0 on that command so it is promoted immediately."
    return "Pass block_until_ms 0 on that command so it is promoted immediately."


def wait_hint(route: live.Route) -> str:
    """How this pack waits out a job that `immediate_hint` already promoted.

    The child blocks in this wait until the command exits, then submits.
    A long start hint would keep the command in the foreground, and the HUD
    does not count that (spec 02 §5.7). The hold is 25 s, so 60 s covers it.
    """
    pack = pack_of(route)
    if pack == "codex":
        return (
            "When that call returns a session id while the command is still running, "
            "wait until the command exits by calling write_stdin with empty chars and "
            "yield_time_ms of at least 60000 on that session id, then submit_result. "
            "Do not stop the command and do not submit_result before it exits."
        )
    if pack == "claude":
        return (
            "When that call returns while the command is still running, "
            "wait until the command exits by reading the output file it named with Read, "
            "repeating that Read until the command has exited "
            "(the script writes job-finished when it does), then submit_result. "
            "Do not stop the command and do not submit_result before it exits."
        )
    if pack == "pi":
        return (
            "When that call returns an id while the command is still running, "
            "wait until the command exits by calling wait on that id with timeout_ms "
            "of at least 60000, then submit_result. "
            "Do not stop the command and do not submit_result before it exits."
        )
    return (
        "When that call returns a task id while the command is still running, "
        "wait until the command exits by calling get_command_or_subagent_output "
        "on that task id with timeout_ms of at least 60000, then submit_result. "
        "Do not stop the command and do not submit_result before it exits."
    )


def idle_child_task(route: live.Route, command: str) -> str:
    """Root prompt for `idle_badge`: one promoted child job, root goes idle."""
    return (
        "Spawn exactly one worker. Its entire task is to run:\n"
        f"{command}\n"
        f"{immediate_hint(route)} "
        f"{wait_hint(route)}\n"
        "After you spawn it, end your own turn without waiting for the worker.\n"
        "Reply with the single word done."
    )


def stop_hint(route: live.Route) -> str:
    """How this pack stops a job `immediate_hint` already promoted.

    Codex has no process-stop tool (`exec_command` and `write_stdin` only),
    so the child waits out the command. The others stop it.
    """
    pack = pack_of(route)
    if pack == "codex":
        return (
            "wait until the command exits by calling write_stdin with empty chars and "
            "yield_time_ms of at least 60000 on that session id. Do not leave it running."
        )
    if pack == "claude":
        return "stop that process with TaskStop on its id. Do not wait for the sleep to finish."
    if pack == "pi":
        return "stop that process with stop on its id. Do not wait for the sleep to finish."
    return (
        "stop that process with kill_command_or_subagent on its task id. "
        "Do not wait for the sleep to finish."
    )


def settle_child_task(route: live.Route, command: str) -> str:
    """Root prompt for the live_work half of `child_settle`."""
    return (
        "Spawn exactly one worker. Its entire task is to do these steps in order:\n"
        f"1. Run exactly this shell command and no other command:\n{command}\n"
        f"{immediate_hint(route)} "
        "When that call returns while the command is still running, do not stop it "
        "and do not wait for it.\n"
        "2. Immediately call submit_result once, while that process is still running. "
        "The call is refused while the process is live. Do not stop or wait before "
        "this submit_result.\n"
        f"3. After that refusal, {stop_hint(route)}\n"
        "4. Call submit_result again with a schema-valid result.\n"
        "Do not run the command yourself. After you spawn the worker, end your turn "
        "without waiting for it.\n"
    )


def close_child_task(route: live.Route, command: str) -> str:
    """Root prompt for the close half of `child_settle`. The driver closes."""
    return (
        "Spawn exactly one worker. Its entire task is to run exactly:\n"
        f"{command}\n"
        f"{immediate_hint(route)}\n"
        "When that call returns while the command is still running, stop. "
        "Do not submit_result, do not stop the process, and do not wait for it.\n"
        "Do not run the command yourself. After you spawn the worker, reply with "
        "the single word done.\n"
    )


def exchange_settled(segment: Path) -> bool:
    """The newest exchange in this segment reached a terminal capture.

    `incomplete=false` plus `terminal=Complete` or `terminal=ProtocolComplete`
    means the provider response was finished, so a later SIGKILL cannot
    truncate it. A pending or incomplete info file is not settled.
    """
    infos = sorted(segment.glob("exchange-*.info"))
    if not infos:
        return False
    try:
        lines = set(infos[-1].read_text(errors="replace").splitlines())
    except OSError:
        return False
    terminal = "terminal=Complete" in lines or "terminal=ProtocolComplete" in lines
    return terminal and "incomplete=false" in lines


def judge_nested_steer(*, promoted: bool, steered: bool, acked: bool,
                       finished: bool, still_running: bool) -> str:
    """Hint promotion, then a steer at the outer-cell boundary. Not preemption."""
    if not promoted:
        raise ScenarioFailure(MODEL, "nested command was not promoted by its hint")
    if finished and not still_running:
        raise ScenarioFailure(MODEL, "command finished before the outer-cell steer")
    if not steered or not acked:
        raise ScenarioFailure(HARNESS, "steer was not applied at the outer-cell boundary")
    if not still_running or finished:
        raise ScenarioFailure(HARNESS, "outer-cell steer stopped the nested job")
    return "hint promoted the nested command; steer landed at the outer-cell boundary"


def judge_nested_ctrl_b(*, promoted: bool, sent: bool, finished: bool,
                        still_running: bool) -> str:
    """Ctrl+B at the outer cell does not preempt or kill a hint-promoted job."""
    if not promoted:
        raise ScenarioFailure(MODEL, "nested command was not promoted by its hint")
    if not sent:
        raise ScenarioFailure(HARNESS, "Ctrl+B was not sent at the outer-cell boundary")
    if not still_running or finished:
        raise ScenarioFailure(HARNESS, "Ctrl+B stopped the nested job")
    return "hint promoted the nested command; Ctrl+B left it running"


def fresh_token(prefix: str, random_bytes=os.urandom) -> str:
    """A process token no other concurrent route's `ps` or `pkill -f` can match."""
    return prefix + random_bytes(4).hex()


def command_lines() -> list[str]:
    try:
        result = subprocess.run(
            ["ps", "-ax", "-o", "command="],
            capture_output=True, text=True, timeout=5, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ScenarioFailure(HARNESS, f"ps failed: {error}") from error
    return result.stdout.splitlines()


def matching(lines: list[str], token: str) -> list[str]:
    """Process commands that are this scenario's script, not a prompt that quotes it.

    macOS framework Pythons re-exec as `.../MacOS/Python`, so the interpreter
    name is matched case-insensitively.
    """
    return [
        line for line in lines
        if token in line and "hold.py" in line and "python" in line.lower()
    ]


def stop_token(token: str) -> None:
    """Stop a fixture script that outlived its scenario.

    `token` includes the per-run hex from `fresh_token`, so this cannot match
    another route's fixture.
    """
    subprocess.run(
        ["pkill", "-f", token],
        check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def read_telemetry(home: Path) -> tuple[list[dict], list[dict]]:
    """Job and turn rows already flushed. A missing or locked store is empty.

    Telemetry is dropped on process exit when it is still queued, so a missing
    row is not a product failure. Callers treat it as corroboration.
    """
    paths = sorted(home.glob("telemetry/telemetry-v*.sqlite3"))
    if not paths:
        paths = sorted(home.glob("**/telemetry-v*.sqlite3"))
    if not paths:
        return [], []
    try:
        connection = sqlite3.connect(f"file:{paths[-1]}?mode=ro", uri=True, timeout=1)
    except sqlite3.Error:
        return [], []
    jobs: list[dict] = []
    turns: list[dict] = []
    try:
        connection.row_factory = sqlite3.Row
        for row in connection.execute(
            "SELECT kind, tool, hint_source, hint_ms, delivery_point, turn_id FROM job_events"
        ):
            jobs.append({key: row[key] for key in row.keys()})
        for row in connection.execute("SELECT turn_id, origin FROM turns"):
            turns.append({"turn_id": row["turn_id"], "origin": row["origin"]})
    except sqlite3.Error:
        return [], []
    finally:
        connection.close()
    return jobs, turns


def promotion_of(rows: list[dict]) -> dict | None:
    promoted = [row for row in rows if row.get("kind") == "promoted" and row.get("tool") == "start"]
    return promoted[-1] if promoted else None


def judge_budget(*, delta: float | None, finished: bool, promoted: bool,
                 hint_source: str | None, hint_ms: int | None) -> str:
    """Promotion at the configured default budget, while the script is still running.

    A flushed telemetry row names the hint the command carried. The wall-clock
    delta is still required: `hint_ms` is the configured budget, not the wait.
    A `default` hint whose milliseconds are not the pinned budget is the
    product ignoring `[jobs] foreground_seconds`, which is harness, even when
    those milliseconds would otherwise look like an immediate background hint.
    """
    if hint_source == "default" and hint_ms != BUDGET_MS:
        raise ScenarioFailure(HARNESS, f"default hint was {hint_ms} ms, not {BUDGET_MS}")
    if hint_source == "background" or hint_ms in (0, 250):
        raise ScenarioFailure(MODEL, "command carried an immediate background hint")
    if hint_source == "model_hint" or (hint_source is not None and hint_ms not in (None, BUDGET_MS)):
        raise ScenarioFailure(MODEL, f"command carried hint {hint_source} {hint_ms} ms")
    if not promoted or finished:
        raise ScenarioFailure(HARNESS, "command was not promoted before it finished")
    if delta is None:
        raise ScenarioFailure(HARNESS, "promotion had no timestamp")
    if delta < PROMOTION_SOON_S:
        if hint_source is None:
            raise ScenarioFailure(MODEL, "promoted before the 1 s budget")
        raise ScenarioFailure(HARNESS, "default budget promoted before 1 s")
    if delta > PROMOTION_LATE_S:
        raise ScenarioFailure(HARNESS, "promotion was later than the 1 s budget")
    if hint_source == "default":
        return f"promoted at the {BUDGET_MS} ms default budget"
    return f"promoted {delta:.1f} s after start; telemetry row not flushed"


def judge_steer(*, latency: float | None, steered: bool, finished: bool,
                still_running: bool, acked: bool) -> str:
    """A steer returns the foreground command without waiting for it or killing it."""
    if not steered:
        raise ScenarioFailure(MODEL, "no foreground command was running to steer")
    if finished and not still_running:
        raise ScenarioFailure(MODEL, "command finished before the steer")
    if not acked or latency is None or latency > STEER_LIMIT_S:
        raise ScenarioFailure(HARNESS, f"steer did not preempt within {STEER_LIMIT_S:.0f} s")
    if not still_running or finished:
        raise ScenarioFailure(HARNESS, "steer stopped the command instead of promoting it")
    return f"steer preempted in {latency:.1f} s; job still running"


def judge_await(*, latency: float | None, steered: bool, finished: bool,
                still_running: bool, acked: bool) -> str:
    """A steer returns a wait on an already promoted job and leaves that job running."""
    if not steered:
        raise ScenarioFailure(MODEL, "no await started after the job was promoted")
    if not acked or latency is None or latency > STEER_LIMIT_S:
        raise ScenarioFailure(HARNESS, f"steer did not return the await within {STEER_LIMIT_S:.0f} s")
    if not still_running or finished:
        raise ScenarioFailure(HARNESS, "steer stopped the awaited job")
    return f"await yielded to steer in {latency:.1f} s; job still running"


def judge_ctrl_b(*, latency: float | None, finished: bool, still_running: bool,
                 returned: bool) -> str:
    """Ctrl+B promotes the running foreground command and does not stop it."""
    if finished and not still_running:
        raise ScenarioFailure(MODEL, "command finished instead of staying in the foreground")
    if not returned or latency is None or latency > STEER_LIMIT_S:
        raise ScenarioFailure(HARNESS, "Ctrl+B did not background the foreground command")
    if not still_running or finished:
        raise ScenarioFailure(HARNESS, "Ctrl+B stopped the command")
    return f"Ctrl+B promoted in {latency:.1f} s; job still running"


def judge_stop(*, confirmed: bool, still_running: bool, finished: bool,
               woke: bool, origin: str | None) -> str:
    """Processes `S` stops a live job, and that stop wakes the idle root.

    `try_child_wake_with_jobs` treats every job that is not `Running` as a
    notice (`crates/hosting/hosted/src/runtime/host_loop/wake.rs` lines
    156-158). `[turn] auto_continue` defaults true (spec 02), so a user stop
    admits another turn. A missing telemetry origin is not a failure.
    """
    if finished:
        raise ScenarioFailure(MODEL, "command finished before the stop")
    if not confirmed:
        raise ScenarioFailure(HARNESS, "Stop all processes confirmation did not appear")
    if still_running:
        raise ScenarioFailure(HARNESS, "process still running after the stop")
    if not woke:
        raise ScenarioFailure(HARNESS, "user stop did not wake the idle root")
    if origin == "user_prompt":
        raise ScenarioFailure(HARNESS, "stop wake was recorded as a user prompt")
    if origin not in (None, "host_wake"):
        raise ScenarioFailure(HARNESS, f"stop wake origin {origin}")
    return "Processes S stopped the live job and woke the idle root"


def judge_notice(*, both_started: bool, short_finished: bool, during_turn: bool,
                 notice_seen: bool, delivery: str | None, steered: bool,
                 notice_in_turn: bool = False) -> str:
    """A busy notice is delivered inside the turn that was busy, never by a wake.

    Spec 01 §8.5: when the short job exits, Notice makes the open wait on the
    hold job yield an ordinary partial result, so the notice can commit at
    that tool-round boundary before the driver's steer lands (`notice_in_turn`:
    a request of this turn, sent before its terminal, carries it). When the
    steer lands first, the steer and the notice share one commit
    (`notice_seen`). An idle wake, or a turn none of whose requests carries
    the notice, is harness.
    """
    if not both_started:
        raise ScenarioFailure(MODEL, "the two commands did not both start")
    if not short_finished or not during_turn:
        raise ScenarioFailure(MODEL, "the short command did not finish during the turn")
    if delivery == "idle_wake":
        raise ScenarioFailure(HARNESS, "completion was delivered as an idle wake")
    if steered and notice_seen:
        return "completion notice was delivered at the steering boundary"
    if notice_in_turn:
        return "completion notice was delivered at a Notice yield inside the turn"
    if not steered:
        raise ScenarioFailure(HARNESS, "no steering boundary was created after the short job finished")
    raise ScenarioFailure(HARNESS, "no request of the turn included the process notice")


def judge_run(*, code: int, stderr: str, finished: bool) -> str:
    """`plak run` waits for a promoted job and continues when it exits.

    A failed exit is classified like every `plak run` exit; `run_cli` already
    returned a committed answer cut at its output limit as exit 0.
    """
    if code != 0:
        live.classify_cli_failure(code, stderr, "plak run")
    if "waiting for p" not in stderr:
        if finished:
            raise ScenarioFailure(MODEL, "model waited for the command instead of leaving it promoted")
        raise ScenarioFailure(HARNESS, "run did not wait for the promoted job")
    if "stopped p" in stderr or not finished or "continuing" not in stderr or "exited" not in stderr:
        raise ScenarioFailure(HARNESS, "run did not continue after the promoted job exited")
    return "plak run waited for the promoted job and continued"


# submit_result accepted the output. `submitted` is not settled (spec 03 §5).
SUBMITTED_ATTEMPTS = frozenset({"submitted", "completed", "adopted"})
# Running and submitted are the only non-terminal attempt statuses.
SETTLED_ATTEMPTS = frozenset({
    "completed", "completed_without_submission", "failed", "cancelled",
    "closed", "adopted", "blocked", "recovery_required",
})


def judge_child(*, started: bool, owner: str | None, first_code: str | None,
                submitted: bool, settled: bool, terminal: bool, still_running: bool,
                stop_reason: str | None, detail: str = "") -> str:
    """The first submit while the job is live is `live_work`; then it settles.

    Spec 03 §4.6 refuses `submit_result` while the caller's own job is running.
    The child then stops that job or lets it finish, submits, and the job is
    terminal when the attempt settles. A model stop is what the prompt asked
    for. Score model only when the child did not do that. A rate limit,
    quota exhaustion, or provider-unavailable refusal in the child's fault
    text or in a wait/tree `blocked` object is `provider`.
    """
    if live.provider_reason_from(None, None, detail) or live.provider_child_text(detail):
        raise ScenarioFailure(PROVIDER, f"child failure: {' '.join(detail.split())[:160]}")
    if not started:
        raise ScenarioFailure(MODEL, "the child did not start the command")
    if owner in ("", "root"):
        raise ScenarioFailure(MODEL, "the command was not owned by a child")
    if first_code is None or first_code == "ok":
        raise ScenarioFailure(MODEL, "the child did not submit while its job was live")
    if first_code != "live_work":
        raise ScenarioFailure(HARNESS, f"first submit while the job was live returned {first_code}")
    if not submitted:
        if settled:
            raise ScenarioFailure(MODEL, "the child did not submit after the live_work refusal")
        raise ScenarioFailure(HARNESS, "child did not submit after the refusal")
    if not settled:
        raise ScenarioFailure(HARNESS, "child attempt did not settle after submit")
    if still_running or not terminal:
        raise ScenarioFailure(HARNESS, "child job was not terminal when the attempt settled")
    if stop_reason not in (None, "model", "attempt_ended"):
        raise ScenarioFailure(HARNESS, f"job ended as {stop_reason}")
    return f"{owner} submit was refused while p was live, then the job was terminal"


def judge_close(*, started: bool, finished_early: bool, stop_reason: str | None,
                still_running: bool, detail: str = "") -> str:
    """`close_agent` stops the child's live job with `attempt ended`.

    Interrupt does not. Spec 01 §8.5: retiring an interrupted child whose
    attempt remains live is not a job-ending release. Spec 03 §5.2: close
    stops that attempt's jobs with `attempt ended` before sealing.
    """
    if live.provider_reason_from(None, None, detail):
        raise ScenarioFailure(PROVIDER, f"child failure: {' '.join(detail.split())[:160]}")
    if not started:
        raise ScenarioFailure(MODEL, "the child did not start the command")
    if finished_early:
        raise ScenarioFailure(MODEL, "the command finished before close")
    if stop_reason == "model":
        raise ScenarioFailure(MODEL, "the model stopped the job instead of leaving it for close")
    if stop_reason != "attempt_ended" or still_running:
        raise ScenarioFailure(HARNESS, "close did not stop the child job at attempt end")
    return "close stopped the child job at attempt end"


def judge_crash(*, unknown: bool, warned: bool) -> str:
    """Reopen settles a job that was live at the crash as outcome unknown, once."""
    if not unknown:
        raise ScenarioFailure(HARNESS, "reopen did not report the live job as outcome_unknown")
    if not warned:
        raise ScenarioFailure(HARNESS, "outcome_unknown was not delivered as a notice")
    return "reopen reported outcome unknown for the live job"


def judge_wake(*, idle_with_job: bool, hud: bool, second_admission: bool,
               origin: str | None, finished: bool) -> str:
    """An idle interactive root wakes when a promoted job exits, without a new prompt."""
    if not idle_with_job:
        raise ScenarioFailure(MODEL, "root was not idle with the job still running")
    if not hud:
        raise ScenarioFailure(HARNESS, "idle HUD did not show the live process")
    if not finished or not second_admission:
        raise ScenarioFailure(HARNESS, "idle root did not wake when the job finished")
    if origin == "user_prompt":
        raise ScenarioFailure(HARNESS, "wake turn was recorded as a user prompt")
    if origin not in (None, "host_wake"):
        raise ScenarioFailure(HARNESS, f"wake origin {origin}")
    return "idle root woke from the job completion"


def judge_badge(*, root_idle: bool, child_alive: bool, agent: bool, process: bool,
                promoted: bool | None) -> str:
    """The idle HUD counts a live child and that child's promoted running job.

    A command still in the foreground never promotes, so a missing process
    count is the model. `promoted` is None when no shell start names the token.
    """
    if not root_idle:
        raise ScenarioFailure(MODEL, "root did not go idle while the child was live")
    if not child_alive:
        raise ScenarioFailure(MODEL, "child job was not live while the root was idle")
    if promoted is False:
        raise ScenarioFailure(MODEL, "child command was not promoted")
    if not agent or not process:
        raise ScenarioFailure(HARNESS, "idle HUD did not show the live child and process")
    return "idle HUD showed 1 agent and 1 process"


def exit_before_turn_terminal(events: list[dict], job_id: str) -> bool:
    """True when `job_id` exits before this turn publishes a terminal phase."""
    exited = False
    for event in events:
        if (event.get("kind") == "job_terminal" and event.get("job_id") == job_id
                and event.get("status") == "exited"):
            exited = True
        elif event.get("kind") == "turn_lifecycle" and event.get("phase") not in (None, "admitted"):
            return exited
    return False


def latency_until(observed: list[tuple[float, dict]], start: float | None, predicate) -> float | None:
    if start is None:
        return None
    for at, event in observed:
        if at >= start and predicate(event):
            return at - start
    return None


def job_id_for(events: list[dict], token: str) -> str | None:
    for event in events:
        if event.get("kind") == "job_started" and token in str(event.get("command", "")):
            return event.get("job_id")
    return None


def terminal_for(events: list[dict], job_id: str | None) -> dict | None:
    found = None
    for event in events:
        if job_id and event.get("kind") == "job_terminal" and event.get("job_id") == job_id:
            found = event
    return found


def screen_after(data: bytes, mark: int) -> str:
    """PTY text painted after `mark`. Earlier bytes can quote the HUD."""
    if mark < 0:
        mark = 0
    return visible(data[mark:])


def last_hud_paint(text: str) -> str:
    """The last composer HUD counter segment in `text`, or empty.

    One paint is the middle-dot segment for "1 agent" and, beside it, "1 process".
    Counters on the same row, separated only by padding, are one paint: a
    cursor move rewrites one spinner and leaves the other on that row.
    A transcript quote of those words is not a segment. A later row that
    drops a counter is the row to judge.
    """
    found = list(_HUD_PAINT.finditer(text))
    if not found:
        return ""
    last = found[-1]
    line_start = text.rfind("\n", 0, last.start()) + 1
    on_line = [match for match in found if match.start() >= line_start]
    if len(on_line) > 1 and text[on_line[0].end():on_line[-1].start()].strip() == "":
        return text[on_line[0].start():on_line[-1].end()]
    start = last.start()
    # Agent and process can be two writes a few cells apart. A later row is not.
    if len(found) > 1 and last.start() - found[-2].end() <= 8 and "\n" not in text[found[-2].end():last.start()]:
        start = found[-2].start()
    return text[start:last.end()]


def render_frame(data: bytes) -> str:
    """The visible rows of a PTY byte stream.

    Absolute cursor addresses share one row, so a spinner refresh of one HUD
    counter does not erase the other. SGR and private modes are ignored.
    Enough for the composer border; not a terminal emulator.
    """
    text = data.decode("utf-8", errors="replace")
    rows, cols = 64, 240
    grid = [[" "] * cols for _ in range(rows)]
    row = col = 0

    def clip_row(value: int) -> int:
        return min(max(value, 0), rows - 1)

    def clip_col(value: int) -> int:
        return min(max(value, 0), cols - 1)

    def put(char: str) -> None:
        nonlocal row, col
        if char == "\n":
            row = clip_row(row + 1)
            col = 0
            return
        if char == "\r":
            col = 0
            return
        if char == "\b":
            col = clip_col(col - 1)
            return
        if char < " ":
            return
        grid[row][col] = char
        if col + 1 < cols:
            col += 1

    def number(token: str, default: int) -> int:
        token = token.lstrip("?")
        return int(token) if token.isdigit() else default

    index = 0
    limit = len(text)
    while index < limit:
        char = text[index]
        if char != "\x1b":
            put(char)
            index += 1
            continue
        index += 1
        if index >= limit:
            break
        kind = text[index]
        if kind == "[":
            index += 1
            params = ""
            while index < limit and text[index] in "0123456789;?:":
                params += text[index]
                index += 1
            if index >= limit:
                break
            final = text[index]
            index += 1
            parts = params.split(";") if params else []
            if final in "Hf":
                row = clip_row(number(parts[0] if parts else "", 1) - 1)
                col = clip_col(number(parts[1] if len(parts) > 1 else "", 1) - 1)
            elif final == "A":
                row = clip_row(row - number(parts[0] if parts else "", 1))
            elif final == "B":
                row = clip_row(row + number(parts[0] if parts else "", 1))
            elif final == "C":
                col = clip_col(col + number(parts[0] if parts else "", 1))
            elif final == "D":
                col = clip_col(col - number(parts[0] if parts else "", 1))
            elif final == "G":
                col = clip_col(number(parts[0] if parts else "", 1) - 1)
            elif final == "K":
                mode = number(parts[0] if parts else "", 0)
                if mode == 1:
                    for cell in range(0, col + 1):
                        grid[row][cell] = " "
                elif mode == 2:
                    grid[row] = [" "] * cols
                else:
                    for cell in range(col, cols):
                        grid[row][cell] = " "
            elif final == "J" and number(parts[0] if parts else "", 0) == 2:
                grid = [[" "] * cols for _ in range(rows)]
                row = col = 0
            continue
        if kind == "]":
            index += 1
            while index < limit and text[index] != "\x07":
                if text[index] == "\x1b" and index + 1 < limit and text[index + 1] == "\\":
                    index += 2
                    break
                index += 1
            else:
                index += 1
            continue
        if kind in "()":
            index += 2
            continue
        index += 1
    lines = ["".join(cells).rstrip() for cells in grid]
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def latest_hud(data: bytes, mark: int) -> str:
    """The composer HUD on the screen `data` has painted.

    A cursor move after `mark` repaints the row drawn before it, so the frame
    is the whole buffer: slicing at `mark` would drop the other counter.
    Bytes that are not a HUD paint do not hide an earlier border. A later row
    that drops a counter is that row. Used by `idle_badge` and `wake_while_idle`.
    """
    if mark < 0:
        mark = 0
    elif mark > len(data):
        mark = len(data)
    # `mark` bounds a caller's slice; the paint before it stays until overwritten.
    _ = mark
    return last_hud_paint(render_frame(data))


def _tool_blob(record: dict) -> str:
    return " ".join(
        str(record.get(key) or "")
        for key in ("name", "exposed_name", "subject", "arguments", "detail")
    )


def _is_shell_start(record: dict) -> bool:
    return record.get("name") in SHELL_STARTS or record.get("exposed_name") in SHELL_STARTS


def command_promoted(records: list[dict], token: str) -> bool | None:
    """Whether the shell start that names `token` has returned.

    None: no shell start names the token. A spawn whose task quotes the
    command does not count. False: that start is still open, so the command
    is still foreground and the HUD must not count it. True: the start
    completed, which is promotion while the process is still running.
    """
    starts = [
        record.get("call_id")
        for record in records
        if record.get("kind") == "tool.started"
        and _is_shell_start(record)
        and token in _tool_blob(record)
    ]
    if not starts:
        completed = any(
            record.get("kind") == "tool.completed"
            and _is_shell_start(record)
            and token in _tool_blob(record)
            for record in records
        )
        return True if completed else None
    done = {
        record.get("call_id")
        for record in records
        if record.get("kind") == "tool.completed"
    }
    if any(call_id in done for call_id in starts):
        return True
    return False


def all_session_records(home: Path) -> list[dict]:
    """Every session log under `home`. The child's file is not the root's."""
    records: list[dict] = []
    seen: set[str] = set()
    for path in sorted(home.glob("state/*/sessions/*.events.jsonl")):
        session = path.name.removesuffix(".events.jsonl")
        if session in seen:
            continue
        seen.add(session)
        records.extend(live.session_records(home, session))
    return records


def _json_strings(value: object, out: list[str]) -> None:
    if isinstance(value, str):
        out.append(value)
    elif isinstance(value, list):
        for item in value:
            _json_strings(item, out)
    elif isinstance(value, dict):
        for item in value.values():
            _json_strings(item, out)


def _parse_capture_json(raw: str) -> object | None:
    """One request document, or the JSON objects of an SSE body."""
    stripped = raw.lstrip()
    if stripped.startswith("{") or stripped.startswith("["):
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return None
    values: list[object] = []
    for line in raw.splitlines():
        if not line.startswith("data: "):
            continue
        blob = line[6:].strip()
        if not blob or blob == "[DONE]":
            continue
        try:
            values.append(json.loads(blob))
        except json.JSONDecodeError:
            continue
    return values or None


def capture_texts(path: Path) -> list[str]:
    """Decoded message strings from one capture file.

    A request JSON string stores `<process_notice id=\\"pN\\">` with escaped
    quotes. Parsing yields the notice text. A file that is not JSON is the
    raw text, so an unescaped capture still matches.
    """
    try:
        raw = path.read_text(errors="replace")
    except OSError:
        return []
    parsed = _parse_capture_json(raw)
    if parsed is None:
        return [raw]
    texts: list[str] = []
    _json_strings(parsed, texts)
    return texts


def _exchange_files(segments: list[Path]):
    for segment in segments:
        if not segment.is_dir():
            continue
        for path in segment.iterdir():
            if path.suffix in {".request", ".body"}:
                yield path


def notice_for(segments: list[Path], job_id: str | None) -> bool:
    """The captured request names this job's notice, not the prompt's token."""
    if not job_id:
        return False
    needle = f'<process_notice id="{job_id}"'
    for path in _exchange_files(segments):
        if any(needle in text for text in capture_texts(path)):
            return True
    return False


def notice_on_steer(segments: list[Path], job_id: str | None) -> bool:
    """The notice shares an exchange with the steer, so it was that boundary.

    Both strings are read from the parsed request, not from the serialized
    bytes. Spec 01 §8.5: a pending steer and the notice share one commit.
    """
    if not job_id:
        return False
    needle = f'<process_notice id="{job_id}"'
    for path in _exchange_files(segments):
        texts = capture_texts(path)
        if any(needle in text for text in texts) and any("steered" in text for text in texts):
            return True
    return False


def notice_in_turn(segments: list[Path], job_id: str | None, terminal_wall: float | None) -> bool:
    """Some request persisted before the turn's terminal carries the notice.

    The capture writes each `.request` before dispatch, so its mtime orders it
    against the terminal the driver observed; a later wake turn's requests
    are after it. A notice committed at a Notice yield counts here whether or
    not the later steer request replays it.
    """
    if not job_id or terminal_wall is None:
        return False
    needle = f'<process_notice id="{job_id}"'
    for path in _exchange_files(segments):
        if path.suffix != ".request":
            continue
        try:
            persisted = path.stat().st_mtime
        except OSError:
            continue
        if persisted <= terminal_wall and any(needle in text for text in capture_texts(path)):
            return True
    return False


def _content_text(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(_content_text(item) for item in value)
    if isinstance(value, dict):
        if isinstance(value.get("text"), str):
            return str(value["text"])
        if "content" in value:
            return _content_text(value["content"])
        return json.dumps(value)
    return ""


def _call_name_id(obj: dict) -> tuple[str, str] | None:
    if obj.get("type") == "function_call" and isinstance(obj.get("name"), str):
        return obj["name"], str(obj.get("call_id") or obj.get("id") or "")
    if obj.get("type") == "tool_use" and isinstance(obj.get("name"), str):
        return obj["name"], str(obj.get("id") or "")
    function = obj.get("function")
    if isinstance(function, dict) and isinstance(function.get("name"), str):
        call_id = obj.get("id") or obj.get("call_id")
        if call_id is not None and obj.get("type") in (None, "function"):
            return function["name"], str(call_id)
    return None


def _result_text(obj: dict) -> tuple[str, str] | None:
    """`(call id, outcome text)` for one tool result, or None."""
    if obj.get("type") == "function_call_output" and "output" in obj:
        return str(obj.get("call_id") or ""), _content_text(obj.get("output"))
    if obj.get("type") == "tool_result" or obj.get("role") == "tool":
        call_id = str(obj.get("tool_use_id") or obj.get("tool_call_id") or obj.get("call_id") or "")
        return call_id, _content_text(obj.get("content"))
    return None


def _submit_result_texts(value: object) -> list[str]:
    """Outcome text of each `submit_result` call, in request order.

    Grok and OpenAI Responses use `function_call` / `function_call_output`.
    Claude Messages uses `tool_use` / `tool_result`. Chat uses `tool_calls`
    and a `role: tool` message. The id pairs a call with its result.
    """
    pending: list[str] = []
    texts: list[str] = []

    def walk(node: object) -> None:
        if isinstance(node, list):
            for item in node:
                walk(item)
            return
        if not isinstance(node, dict):
            return
        named = _call_name_id(node)
        if named is not None and named[0] == "submit_result":
            pending.append(named[1])
        result = _result_text(node)
        if result is not None and pending:
            call_id, body = result
            if call_id and call_id in pending:
                texts.append(body)
                pending.remove(call_id)
            elif not call_id:
                texts.append(body)
                pending.pop(0)
        for key, item in node.items():
            if key in ("arguments", "output", "input") and isinstance(item, str):
                continue
            if key == "content" and result is not None:
                continue
            walk(item)

    walk(value)
    return texts


def outcome_code(text: str) -> str:
    """`live_work`, `ok` for a submit receipt, or another code.

    The receipt is `{agent_id, attempt}` (spec 03 §4.6). A refusal is
    `{"code","message"}`. Either may be wrapped in a `result` string.
    """
    blob: object = text
    for _ in range(4):
        if isinstance(blob, str):
            try:
                blob = json.loads(blob)
            except json.JSONDecodeError:
                break
            continue
        if not isinstance(blob, dict):
            break
        code = blob.get("code")
        if isinstance(code, str) and code:
            return code
        if isinstance(blob.get("agent_id"), str) and isinstance(blob.get("attempt"), str):
            return "ok"
        nxt = None
        for key in ("result", "output", "content", "text"):
            if key in blob:
                nxt = blob[key]
                break
        if nxt is None:
            break
        blob = nxt
    if '"code":"live_work"' in text or '"code": "live_work"' in text:
        return "live_work"
    return "other"


def submit_outcomes(segments: list[Path]) -> list[str]:
    """Ordered submit outcomes from the longest captured request history.

    Each later exchange repeats the earlier calls, so the longest list is
    the child's history. An empty list is no submit in the capture.
    """
    best: list[str] = []
    for path in _exchange_files(segments):
        if path.suffix != ".request":
            continue
        try:
            raw = path.read_text(errors="replace")
        except OSError:
            continue
        parsed = _parse_capture_json(raw)
        if parsed is None:
            continue
        codes = [outcome_code(text) for text in _submit_result_texts(parsed)]
        if not codes:
            # Code Mode can put the refusal in a cell result instead of a
            # submit_result tool pair. Ignore strings that are not an outcome.
            texts: list[str] = []
            _json_strings(parsed, texts)
            codes = [code for text in texts if (code := outcome_code(text)) != "other"]
        if len(codes) >= len(best):
            best = codes
    return best


def capture_has(segments: list[Path], needle: str) -> bool:
    for segment in segments:
        for path in segment.iterdir():
            if path.suffix not in {".request", ".body"}:
                continue
            try:
                text = path.read_text(errors="replace")
            except OSError:
                continue
            if needle in text:
                return True
    return False


def child_wait_state(events: list[dict], token: str, *, child_settled: bool) -> str:
    """What `child_settle` is still waiting for. Never a short cutoff.

    `wait_started` until `job_started` names the token. `settled_without_job`
    when the child reached a terminal phase without that start; the start
    window still runs to its deadline, because the event can arrive later.
    `wait_terminal` until that job's `job_terminal`. `ready` once the terminal
    is in the log.
    """
    job_id = job_id_for(events, token)
    if job_id is None:
        return "settled_without_job" if child_settled else "wait_started"
    if terminal_for(events, job_id) is None:
        return "wait_terminal"
    return "ready"


def root_is_idle(records: list[dict]) -> bool:
    """True when the root's latest admitted turn has completed, and no later one is open."""
    admitted: list[object] = []
    terminals: list[tuple[object, object]] = []
    for record in records:
        if record.get("kind") != "turn.lifecycle":
            continue
        phase = record.get("phase")
        turn_id = record.get("turn_id")
        if phase == "admitted":
            admitted.append(turn_id)
        elif phase is not None:
            terminals.append((turn_id, phase))
    if not admitted or not terminals:
        return False
    turn_id, phase = terminals[-1]
    return phase == "completed" and turn_id is not None and turn_id == admitted[-1]


def classify_turn(turn: live.Turn, what: str, segments: list[Path] | None = None, *,
                  lookup_faults: str | None = None) -> None:
    live.classify_incomplete(turn, what, segments=segments, lookup_faults=lookup_faults)


def require_script(repo: Path, name: str, expected: str) -> None:
    if (repo / name).read_text() != expected:
        raise ScenarioFailure(MODEL, f"{name} was modified")


def marker(repo: Path, name: str) -> bool:
    return (repo / name).is_file()


def started_wall(repo: Path) -> float | None:
    """Wall time the fixture script recorded as it began, not a file mtime."""
    path = repo / "job-started"
    if not path.is_file():
        return None
    try:
        return float(path.read_text().strip())
    except ValueError:
        return None


def drain(agent: live.Agent, seconds: float) -> None:
    """Read events still queued after a turn returns, so a late terminal is visible."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            agent.note(agent.events.get(timeout=min(0.2, deadline - time.monotonic())))
        except queue.Empty:
            continue


def terminals(home: Path, session: str) -> list[str]:
    return [record.get("phase", "") for record in live.session_records(home, session)
            if record.get("kind") == "turn.lifecycle"
            and record.get("phase") not in (None, "admitted")]


def last_retry(home: Path, turn_id: str) -> tuple[str, int, int, str | None] | None:
    """The turn's last `model_retries` row: cause, next attempt, max attempts
    and declined reason.

    A missing store, or a row that does not name both attempt counters, is no
    evidence. `rowid` order is insertion order.
    """
    if not turn_id:
        return None
    paths = sorted(home.glob("telemetry/telemetry-v*.sqlite3"))
    if not paths:
        paths = sorted(home.glob("**/telemetry-v*.sqlite3"))
    if not paths:
        return None
    try:
        connection = sqlite3.connect(f"file:{paths[-1]}?mode=ro", uri=True, timeout=1)
    except sqlite3.Error:
        return None
    try:
        rows = connection.execute(
            "SELECT cause_kind, next_attempt, max_attempts, declined_reason FROM model_retries "
            "WHERE turn_id = ? ORDER BY rowid",
            (turn_id,),
        ).fetchall()
    except sqlite3.Error:
        return None
    finally:
        connection.close()
    if not rows:
        return None
    cause, nxt, limit, declined = rows[-1]
    if type(nxt) is not int or type(limit) is not int:
        return None
    return str(cause), nxt, limit, declined if isinstance(declined, str) else None


def retry_budget_exhausted(retry: tuple[str, int, int, str | None] | None) -> bool:
    """The last allowed attempt failed on a provider cause.

    Either the last row dispatched that attempt (`next_attempt ==
    max_attempts`), or it declined the proposed attempt after it for `budget`
    (`next_attempt == max_attempts + 1`). Any other declined row is not
    exhaustion: the last allowed attempt was never sent.
    """
    if retry is None:
        return False
    cause, nxt, limit, declined = retry
    if cause not in PROVIDER_RETRY_CAUSES:
        return False
    if declined is None:
        return nxt == limit
    return declined == "budget" and nxt == limit + 1


def classify_tui_turn(home: Path, session: str, index: int, what: str, *,
                      lookup_faults: str | None = None) -> None:
    """Classify a TUI turn from the session log, then from retry telemetry.

    `emit_turn_lifecycle` (`crates/hosting/hosted/src/runtime/turn.rs` lines
    204-208) writes `turn_id` and `phase` only. `model_error_kind` and
    `http_status` are absent there. A provider failure is still provider when
    the row itself carries them, or when the turn's last `model_retries` row
    shows a provider cause exhausted the budget (`retry_budget_exhausted`).
    A product failure after a transient retry that was not the last allowed
    attempt stays harness. The turn's `turn.failed` text stands in for the
    row's missing message, so a typed OutputLimit diagnostic is model.
    """
    records = [record for record in live.with_failure_messages(live.session_records(home, session))
               if record.get("kind") == "turn.lifecycle"
               and record.get("phase") not in (None, "admitted")]
    if index >= len(records):
        raise ScenarioFailure(HARNESS, f"{what}: no logged turn terminal")
    record = records[index]
    status = record.get("http_status")
    turn = live.Turn(
        lookup_faults=lookup_faults,
        phase=record.get("phase"),
        model_error_kind=record.get("model_error_kind"),
        http_status=status if type(status) is int else None,
        errors=[str(record.get("message") or "")],
        refusals=live.durable_refusals(record),
    )
    if turn.completed:
        return
    lookup = live.lookup_failure(" ".join(turn.errors), lookup_faults)
    if lookup:
        raise ScenarioFailure(lookup[1], f"{what}: {lookup[0]}")
    retry = last_retry(home, str(record.get("turn_id") or ""))
    if retry_budget_exhausted(retry):
        raise ScenarioFailure(PROVIDER, f"{what}: model_retries {retry[0]} exhausted")
    classify_turn(turn, what)


def admitted_ids(home: Path, session: str) -> list[str]:
    return [str(record.get("turn_id", "")) for record in live.session_records(home, session)
            if record.get("kind") == "turn.lifecycle" and record.get("phase") == "admitted"]


def child_has_settled(home: Path, root: str) -> bool:
    """A non-root session has published a terminal turn phase."""
    for path in sorted(home.glob("state/*/sessions/*.events.jsonl")):
        session = path.name.removesuffix(".events.jsonl")
        if session != root and terminals(home, session):
            return True
    return False


def spawned_children(events: list[dict], root: str) -> list[str]:
    """Child agent ids in the order a tree snapshot first named them."""
    found: list[str] = []
    for event in events:
        if event.get("kind") != "tree":
            continue
        for node in event.get("nodes") or []:
            agent_id = node.get("agent_id")
            if isinstance(agent_id, str) and agent_id != root and agent_id not in found:
                found.append(agent_id)
    return found


def child_attempt(events: list[dict], agent_id: str | None) -> str | None:
    """The latest tree `attempt` status for `agent_id`."""
    if not agent_id:
        return None
    found = None
    for event in events:
        if event.get("kind") != "tree":
            continue
        for node in event.get("nodes") or []:
            if node.get("agent_id") == agent_id and isinstance(node.get("attempt"), str):
                found = node["attempt"]
    return found


def tool_completed_count(home: Path, session: str) -> int:
    return sum(record.get("kind") == "tool.completed"
               for record in live.session_records(home, session))


@contextmanager
def launch_home(env: live.Env, home: Path):
    """Point one process launch at a private home, then restore the run home."""
    previous = env.home
    env.home = home
    try:
        yield
    finally:
        env.home = previous


def run_turn(agent: live.Agent, text: str, timeout: float, tick, accept_eof) -> live.Turn:
    """Drive one prompt, noting every stdout event, and poll between events.

    `prompt` waits on a filtered subset, so a steer or a kill would not get a
    chance to look at the script's marker files while the tool is in flight.
    """
    command_id = agent.send("prompt", text=text)
    turn, started, terminal = live.Turn(), time.monotonic(), False
    while True:
        remaining = timeout - (time.monotonic() - started)
        if remaining <= 0:
            raise ScenarioFailure(HARNESS, f"no turn terminal within {timeout:.0f}s")
        try:
            event = agent.events.get(timeout=min(0.25, remaining))
        except queue.Empty:
            if tick:
                tick(turn, None)
            continue
        agent.note(event)
        if tick:
            tick(turn, event)
        kind = event.get("kind")
        if kind == "eof":
            if accept_eof():
                turn.phase = turn.phase or "killed"
                turn.seconds = time.monotonic() - started
                return turn
            raise ScenarioFailure(HARNESS, "plak agent exited during a turn")
        if kind == "assistant":
            turn.text += event.get("text", "")
        elif kind == "tool" and event.get("state") == "running":
            turn.tools += 1
        elif kind == "error" and event.get("id") in (None, command_id):
            turn.errors.append(event.get("message", ""))
        elif kind == "model_refusal" and event.get("id") in (None, command_id):
            turn.refusals.append(live.refusal_summary(event))
        elif kind == "retry":
            turn.retries.append(event)
        elif kind == "turn":
            live.apply_usage_event(turn, event)
        elif kind == "turn_lifecycle" and event.get("phase") != "admitted":
            turn.phase = event.get("phase")
            turn.model_error_kind = event.get("model_error_kind")
            turn.http_status = event.get("http_status")
            terminal = True
        elif kind == "busy" and event.get("busy") is False and terminal:
            turn.seconds = time.monotonic() - started
            return turn
        elif kind == "busy" and event.get("busy") is False and turn.errors and not turn.tools:
            turn.phase, turn.seconds = "refused", time.monotonic() - started
            return turn


def close_agent(agent: live.Agent | None) -> None:
    if agent is None:
        return
    try:
        agent.quit()
    except Exception:
        try:
            agent.signal(signal.SIGKILL)
            agent.proc.wait(timeout=5)
        except Exception:
            pass


class Jobs:
    def __init__(self, route: live.Route, env: live.Env):
        import random

        self.route, self.env = route, env
        self.random_bytes = (os.urandom if env.fixture_seed is None
                             else random.Random(env.fixture_seed).randbytes)

    def fresh_token(self, prefix: str) -> str:
        return fresh_token(prefix, self.random_bytes)

    def prepare(self, name: str, foreground: int, extra: dict[str, str] | None = None) -> tuple[Path, Path]:
        files = {"hold.py": HOLD_PY, "README.md": "job fixture\n"}
        if extra:
            files.update(extra)
        repo = live.git_repo(self.env.artifacts / name / "repo", files)
        home = self.env.artifacts / name / "home"
        live.write_home(home, self.route, self.env.home)
        pin_jobs(home, foreground)
        return repo, home

    def open_agent(self, name: str, repo: Path, home: Path, args: list[str] | None = None) -> live.Agent:
        with launch_home(self.env, home):
            return live.Agent(self.env, repo, f"{name}/agent", args=args or [], observe=True)

    def open_tui(self, name: str, repo: Path, home: Path) -> live.Tui:
        with launch_home(self.env, home):
            return live.Tui(self.env, repo, self.env.artifacts / name / "tui.log")

    def promotion_budget(self) -> str:
        token = self.fresh_token("plakjobbudget")
        repo, home = self.prepare("promotion_budget", FOREGROUND_BUDGET)
        agent = None
        state = {"at": None, "finished": False}
        try:
            agent = self.open_agent("promotion_budget", repo, home)

            def tick(_turn, event) -> None:
                if state["at"] is not None or event is None or event.get("kind") != "job_promoted":
                    return
                events = [item for _, item in agent.observed]
                if job_id_for(events, token) != event.get("job_id"):
                    return
                state["at"] = time.monotonic()
                state["finished"] = marker(repo, "job-finished")

            turn = run_turn(agent, (
                "Run exactly this shell command and no other tool:\n"
                f"{hold_command(8, token)}\n"
                "Do not pass a timeout, yield, block_until, or background flag.\n"
                "When the tool returns, reply with the single word promoted."
            ), 180, tick, lambda: False)
            classify_turn(turn, "promotion")
            require_script(repo, "hold.py", HOLD_PY)
            events = [event for _, event in agent.observed]
            if any(event.get("kind") == "job_promoted" for event in events) \
                    and job_id_for(events, token) is None:
                raise ScenarioFailure(MODEL, "promoted command was not the fixture script")
            began = started_wall(repo)
            delta = None
            if began is not None and state["at"] is not None:
                delta = (time.time() - (time.monotonic() - state["at"])) - began
            row = promotion_of(read_telemetry(home)[0])
            return judge_budget(
                delta=delta, finished=state["finished"], promoted=state["at"] is not None,
                hint_source=None if row is None else row.get("hint_source"),
                hint_ms=None if row is None else row.get("hint_ms"),
            )
        finally:
            close_agent(agent)
            stop_token(token)

    def steer_command(self) -> str:
        token = self.fresh_token("plakjobsteer")
        nested = code_mode(self.route)
        repo, home = self.prepare("steer_command", FOREGROUND_HOLD)
        agent = None
        state = {"at": None, "running": False, "finished": False, "snap": False, "promoted": False}
        try:
            agent = self.open_agent("steer_command", repo, home)

            def tick(_turn, event) -> None:
                # The running line can beat the script's marker. Poll on the
                # empty reads too, or the steer waits until the 45 s promotion.
                if event is not None and event.get("kind") == "tool" and event.get("state") == "running":
                    state["tool"] = True
                if nested:
                    if event is not None and event.get("kind") == "job_promoted":
                        events = [item for _, item in agent.observed]
                        if job_id_for(events, token) == event.get("job_id"):
                            state["promoted"] = True
                    if state["at"] is None and state["promoted"] and matching(command_lines(), token) \
                            and not marker(repo, "job-finished"):
                        state["at"] = time.monotonic()
                        state["running"] = True
                        state["finished"] = False
                        agent.send("prompt", text=STEER_TEXT, steer=True)
                    return
                if state["at"] is None and state.get("tool") and marker(repo, "job-started") \
                        and matching(command_lines(), token):
                    state["at"] = time.monotonic()
                    agent.send("prompt", text=STEER_TEXT, steer=True)
                    return
                if event is None:
                    return
                if state["at"] is not None and not state["snap"] and event.get("kind") == "job_promoted":
                    state["running"] = bool(matching(command_lines(), token))
                    state["finished"] = marker(repo, "job-finished")
                    state["snap"] = True

            if nested:
                prompt = (
                    "Inside one code cell, run exactly this shell command and no other tool:\n"
                    f"{hold_command(20, token)}\n"
                    f"{immediate_hint(self.route)}\n"
                    "Do not wait for the command and do not poll it. End the cell."
                )
            else:
                prompt = (
                    "Run exactly this shell command and no other tool until it returns:\n"
                    f"{hold_command(20, token)}\n"
                    "Do not pass a timeout, yield, block_until, or background flag. Wait for it."
                )
            turn = run_turn(agent, prompt, 180, tick, lambda: False)
            classify_turn(turn, "steer", agent.capture_dir and [agent.capture_dir])
            require_script(repo, "hold.py", HOLD_PY)
            events = [event for _, event in agent.observed]
            acked = any(event.get("kind") == "steer_ack" for event in events)
            if nested:
                return judge_nested_steer(
                    promoted=state["promoted"], steered=state["at"] is not None, acked=acked,
                    finished=marker(repo, "job-finished"),
                    still_running=bool(matching(command_lines(), token)),
                )
            latency = latency_until(
                agent.observed, state["at"],
                lambda event: event.get("kind") == "job_promoted",
            )
            return judge_steer(
                latency=latency, steered=state["at"] is not None,
                finished=state["finished"], still_running=state["running"], acked=acked,
            )
        finally:
            close_agent(agent)
            stop_token(token)

    def steer_await(self) -> str:
        token = self.fresh_token("plakjobawait")
        nested = code_mode(self.route)
        repo, home = self.prepare("steer_await", FOREGROUND_BUDGET)
        agent = None
        state = {"at": None, "promoted": False, "before": [], "await": None,
                 "running": False, "finished": False}
        try:
            agent = self.open_agent("steer_await", repo, home)

            def tick(_turn, event) -> None:
                if nested and state["at"] is None and state["promoted"] \
                        and matching(command_lines(), token) and not marker(repo, "job-finished"):
                    state["at"] = time.monotonic()
                    state["running"] = True
                    agent.send("prompt", text=STEER_TEXT, steer=True)
                    return
                if event is None:
                    return
                if state["at"] is not None:
                    if not state.get("snap") and event.get("kind") == "tool" \
                            and event.get("call_id") == state["await"] and event.get("state") != "running":
                        state["running"] = bool(matching(command_lines(), token))
                        state["finished"] = marker(repo, "job-finished")
                        state["snap"] = True
                    return
                if not state["promoted"]:
                    # The start command's running line is before promotion.
                    # Only a call that begins after that is the await.
                    if event.get("kind") == "tool" and event.get("state") == "running":
                        call = event.get("call_id")
                        if call and call not in state["before"]:
                            state["before"].append(call)
                    if event.get("kind") == "job_promoted":
                        events = [item for _, item in agent.observed]
                        if job_id_for(events, token) == event.get("job_id"):
                            state["promoted"] = True
                    return
                if event.get("kind") != "tool" or event.get("state") != "running":
                    return
                call = event.get("call_id")
                if not call or call in state["before"]:
                    return
                if matching(command_lines(), token) and not marker(repo, "job-finished"):
                    state["await"] = call
                    state["at"] = time.monotonic()
                    agent.send("prompt", text=STEER_TEXT, steer=True)

            if nested:
                prompt = (
                    "Inside one code cell, run exactly this shell command and no other tool:\n"
                    f"{hold_command(25, token)}\n"
                    f"{immediate_hint(self.route)}\n"
                    "When that call returns while the command is still running, stay inside "
                    "the cell. Do not stop the job. The outer cell must not yield on a steer "
                    "until that nested call has settled."
                )
            else:
                prompt = (
                    "Run exactly this shell command first, with no timeout, yield, "
                    "block_until, or background flag:\n"
                    f"{hold_command(25, token)}\n"
                    "When that tool returns while the command is still running, wait on "
                    "that job with your wait or poll tool and a timeout of at least 60 seconds. "
                    "Do not stop the job, do not read its output file, and do not start any other tool."
                )
            turn = run_turn(agent, prompt, 180, tick, lambda: False)
            classify_turn(turn, "await", [agent.capture_dir])
            require_script(repo, "hold.py", HOLD_PY)
            events = [event for _, event in agent.observed]
            acked = any(event.get("kind") == "steer_ack" for event in events)
            if nested:
                return judge_nested_steer(
                    promoted=state["promoted"], steered=state["at"] is not None, acked=acked,
                    finished=marker(repo, "job-finished"),
                    still_running=bool(matching(command_lines(), token)),
                )
            await_id = state["await"]
            latency = latency_until(
                agent.observed, state["at"],
                lambda event: event.get("kind") == "tool" and event.get("call_id") == await_id
                and event.get("state") != "running",
            )
            return judge_await(
                latency=latency, steered=state["at"] is not None,
                finished=state["finished"], still_running=state["running"], acked=acked,
            )
        finally:
            close_agent(agent)
            stop_token(token)

    def notice_while_busy(self) -> str:
        short, hold = self.fresh_token("plakjobshort"), self.fresh_token("plakjobbusy")
        repo, home = self.prepare("notice_while_busy", FOREGROUND_BUDGET, {"short.py": SHORT_PY})
        agent = None
        segment = len(self.env.capture_segments)
        state = {"steered": False, "terminal_wall": None}
        try:
            agent = self.open_agent("notice_while_busy", repo, home)

            def tick(_turn, event) -> None:
                # Spec 01 §8.5: the short job's exit makes the open wait yield
                # at a Notice boundary, and a steer is a second boundary. Steer
                # after the short job has finished and while the later command
                # is still the open turn. Poll on the empty reads too; the
                # marker is a file, not an event.
                if (event and state["terminal_wall"] is None
                        and event.get("kind") == "turn_lifecycle"
                        and event.get("phase") not in (None, "admitted")):
                    state["terminal_wall"] = time.time()
                if state["steered"]:
                    return
                if marker(repo, "short-finished") and matching(command_lines(), hold) \
                        and not marker(repo, "job-finished"):
                    agent.send("prompt", text=STEER_TEXT, steer=True)
                    state["steered"] = True

            turn = run_turn(agent, (
                "Run exactly these two shell commands, in this order, and no others.\n"
                f"First: python3 short.py {short}\n"
                f"{immediate_hint(self.route)} "
                "When it returns while still running, immediately run:\n"
                f"{hold_command(12, hold)}\n"
                "Wait for that second command. Do not wait on the first and do not stop either."
            ), 240, tick, lambda: False)
            classify_turn(turn, "notice", [agent.capture_dir])
            require_script(repo, "hold.py", HOLD_PY)
            require_script(repo, "short.py", SHORT_PY)
            events = [event for _, event in agent.observed]
            short_id = job_id_for(events, short)
            rows, _turns = read_telemetry(home)
            delivery = next((row.get("delivery_point") for row in rows
                             if row.get("kind") == "notices"), None)
            segments = self.env.capture_segments[segment:]
            noticed = notice_for(segments, short_id) and notice_on_steer(segments, short_id)
            mentioned = any(event.get("kind") == "tool" and hold in json.dumps(event) for event in events)
            return judge_notice(
                both_started=short_id is not None and (job_id_for(events, hold) is not None or mentioned),
                short_finished=marker(repo, "short-finished"),
                during_turn=bool(short_id) and exit_before_turn_terminal(events, short_id),
                notice_seen=bool(noticed),
                delivery=delivery,
                steered=state["steered"],
                notice_in_turn=notice_in_turn(segments, short_id, state["terminal_wall"]),
            )
        finally:
            close_agent(agent)
            stop_token(hold)
            stop_token(short)

    def run_promoted(self) -> str:
        token = self.fresh_token("plakjobrun")
        repo, home = self.prepare("run_promoted", FOREGROUND_BUDGET)
        log = self.env.artifacts / "run_promoted" / "run.log"
        try:
            code, _out = live.run_cli(
                self.env, repo,
                [*self.route.flags(), "--print-session-id", "--", (
                    "Run exactly this shell command and no other tool:\n"
                    f"{hold_command(5, token)}\n"
                    f"{immediate_hint(self.route)}\n"
                    "When that call returns while the command is still running, end your turn. "
                    "Do not wait for the command, do not poll it, do not stop it, and do not "
                    "start another tool.\n"
                    "Reply with the single word done."
                )],
                log, timeout=240, home=home,
            )
            require_script(repo, "hold.py", HOLD_PY)
            stderr = ""
            if log.is_file():
                text = log.read_text(errors="replace")
                stderr = text.split("--- stderr\n", 1)[-1]
            return judge_run(code=code, stderr=stderr, finished=marker(repo, "job-finished"))
        finally:
            stop_token(token)

    def crash_jobs(self) -> str:
        token = self.fresh_token("plakjobcrash")
        repo, home = self.prepare("crash_jobs", FOREGROUND_BUDGET)
        agent = None
        state = {"armed": None, "killed": False}
        try:
            agent = self.open_agent("crash_jobs", repo, home)

            def tick(_turn, event) -> None:
                if state["killed"]:
                    return
                if state["armed"] is None and event and event.get("kind") == "job_promoted":
                    events = [item for _, item in agent.observed]
                    if job_id_for(events, token) == event.get("job_id"):
                        state["armed"] = time.monotonic()
                # Kill only after the provider exchange is terminal, while the
                # promoted job is still live, so the capture is not truncated.
                if state["armed"] is None or marker(repo, "job-finished"):
                    return
                if not matching(command_lines(), token):
                    return
                if exchange_settled(agent.capture_dir):
                    agent.signal(signal.SIGKILL)
                    state["killed"] = True

            run_turn(agent, (
                "Run exactly this shell command and no other tool:\n"
                f"{hold_command(40, token)}\n"
                f"{immediate_hint(self.route)}\n"
                "When that call returns while the command is still running, reply done. "
                "Do not wait for the command, do not poll it, and do not stop it."
            ), 180, tick, lambda: state["killed"])
            if not state["killed"]:
                raise ScenarioFailure(MODEL, "no promoted job was live to crash")
            session = agent.session_id
            close_agent(agent)
            agent = None
            if not session:
                raise ScenarioFailure(HARNESS, "crashed session had no id")
            segment = len(self.env.capture_segments)
            agent = crash_triage.reopen(
                lambda: self.open_agent("crash_jobs", repo, home, args=["--session", session]),
                self.env.binary, repo, home, session)
            if agent.session_id != session:
                raise ScenarioFailure(HARNESS, "reopen attached a different session")
            follow = agent.prompt("Reply with the single word continued.", timeout=180)
            classify_turn(follow, "follow-up after the crash")
            events = [event for _, event in agent.observed]
            job = terminal_for(events, job_id_for(events, token))
            warned = capture_has(self.env.capture_segments[segment:], WARNING)
            return judge_crash(
                unknown=bool(job) and job.get("status") == "outcome_unknown",
                warned=warned,
            )
        finally:
            close_agent(agent)
            stop_token(token)

    def child_settle(self) -> str:
        token = self.fresh_token("plakjobchild")
        close_token = self.fresh_token("plakjobclose")
        repo, home = self.prepare("child_settle", FOREGROUND_BUDGET)
        agent = None
        segment = len(self.env.capture_segments)
        try:
            agent = self.open_agent("child_settle", repo, home)
            turn = run_turn(
                agent, settle_child_task(self.route, hold_command(30, token)),
                240, None, lambda: False,
            )
            classify_turn(turn, "child")
            require_script(repo, "hold.py", HOLD_PY)
            root = agent.session_id or ""
            # The refusal is in the child's next provider request. The attempt
            # status is on the root's tree snapshots. JSONL does not project
            # the child transcript.
            job_id = None
            job = None
            attempt = None
            outcomes: list[str] = []
            deadline = time.monotonic() + CHILD_WAIT_S
            while time.monotonic() < deadline and not outcomes:
                drain(agent, 0.5)
                events = [event for _, event in agent.observed]
                job_id = job_id or job_id_for(events, token)
                outcomes = submit_outcomes(self.env.capture_segments[segment:])
            if job_id is not None:
                deadline = time.monotonic() + CHILD_WAIT_S
                while time.monotonic() < deadline:
                    drain(agent, 0.5)
                    events = [event for _, event in agent.observed]
                    job = terminal_for(events, job_id)
                    child = spawned_children(events, root)
                    attempt = child_attempt(events, child[-1] if child else None)
                    outcomes = submit_outcomes(self.env.capture_segments[segment:])
                    submitted = attempt in SUBMITTED_ATTEMPTS or "ok" in outcomes[1:]
                    settled = attempt in SETTLED_ATTEMPTS or child_has_settled(home, root)
                    if outcomes and job is not None and submitted and settled:
                        break
            events = [event for _, event in agent.observed]
            job_id = job_id or job_id_for(events, token)
            job = terminal_for(events, job_id)
            child = spawned_children(events, root)
            attempt = child_attempt(events, child[-1] if child else None)
            outcomes = submit_outcomes(self.env.capture_segments[segment:])
            first = outcomes[0] if outcomes else None
            submitted = attempt in SUBMITTED_ATTEMPTS or "ok" in outcomes[1:]
            settled = attempt in SETTLED_ATTEMPTS or child_has_settled(home, root)
            lived = judge_child(
                started=job_id is not None,
                owner=None if job is None else job.get("owner"),
                first_code=first,
                submitted=submitted,
                settled=settled,
                terminal=job is not None,
                still_running=bool(matching(command_lines(), token)),
                stop_reason=None if job is None else job.get("stop_reason"),
                detail="" if job is None else str(job.get("output") or ""),
            )
            # Parent close, not interrupt: interrupt keeps the job (spec 01).
            marker_path = repo / "job-finished"
            if marker_path.is_file():
                marker_path.unlink()
            before = set(spawned_children(events, root))
            follow = run_turn(
                agent, close_child_task(self.route, hold_command(40, close_token)),
                180, None, lambda: False,
            )
            classify_turn(follow, "close")
            close_id = None
            close_child = None
            deadline = time.monotonic() + CHILD_WAIT_S
            while time.monotonic() < deadline and close_child is None:
                drain(agent, 0.5)
                events = [event for _, event in agent.observed]
                fresh = [item for item in spawned_children(events, root) if item not in before]
                if fresh and job_id_for(events, close_token) and matching(command_lines(), close_token) \
                        and not marker(repo, "job-finished"):
                    close_child = fresh[-1]
                    close_id = job_id_for(events, close_token)
            if close_child is not None:
                agent.send("close_node", agent_id=close_child)
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    drain(agent, 0.5)
                    closed = terminal_for([event for _, event in agent.observed], close_id)
                    if closed is not None and not matching(command_lines(), close_token):
                        break
            events = [event for _, event in agent.observed]
            close_id = close_id or job_id_for(events, close_token)
            closed = terminal_for(events, close_id)
            ended = judge_close(
                started=close_id is not None,
                finished_early=marker(repo, "job-finished") and (
                    closed is None or closed.get("stop_reason") != "attempt_ended"
                ),
                stop_reason=None if closed is None else closed.get("stop_reason"),
                still_running=bool(matching(command_lines(), close_token)),
                detail="" if closed is None else str(closed.get("output") or ""),
            )
            return f"{lived}; {ended}"
        finally:
            close_agent(agent)
            stop_token(token)
            stop_token(close_token)

    def ctrl_b(self) -> str:
        token = self.fresh_token("plakjobctrlb")
        nested = code_mode(self.route)
        repo, home = self.prepare("ctrl_b", FOREGROUND_HOLD)
        tui = None
        try:
            tui = self.open_tui("ctrl_b", repo, home)
            session = _discover(home)
            if nested:
                tui.type(
                    "Inside one code cell, run exactly this shell command and no other tool:\n"
                    f"{hold_command(20, token)}\n"
                    f"{immediate_hint(self.route)}\n"
                    "Do not wait for it and do not stop it."
                )
            else:
                tui.type(
                    "Run exactly this shell command and wait for it. Do not background it "
                    "and do not pass a timeout, yield, or block_until:\n"
                    f"{hold_command(20, token)}"
                )
            tui.key(b"\r")
            sent = {"at": None}

            def running_foreground() -> bool:
                if sent["at"] is not None:
                    return True
                live_job = marker(repo, "job-started") and matching(command_lines(), token) \
                    and not marker(repo, "job-finished")
                # Direct tools are still in the foreground when the tool has not
                # completed. Nested Code Mode may already have returned the
                # hint-promoted call; Ctrl+B is the outer-cell boundary.
                foreground = tool_completed_count(home, session) == 0 or nested
                if live_job and foreground:
                    sent["at"] = time.monotonic()
                    tui.key(b"\x02")
                    return True
                return False

            if not tui.wait_until(running_foreground, 120):
                if terminals(home, session):
                    classify_tui_turn(home, session, len(terminals(home, session)) - 1, "ctrl+b", lookup_faults=tui.lookup_faults)
                raise ScenarioFailure(MODEL, "no foreground command was running for Ctrl+B")
            if nested:
                # The outer cell is not required to preempt the nested call.
                # Give the key a moment, then judge that the job survived it.
                tui.pump(1)
                require_script(repo, "hold.py", HOLD_PY)
                return judge_nested_ctrl_b(
                    promoted=True, sent=sent["at"] is not None,
                    finished=marker(repo, "job-finished"),
                    still_running=bool(matching(command_lines(), token)),
                )
            returned = tui.wait_until(
                lambda: tool_completed_count(home, session) > 0, STEER_LIMIT_S,
            )
            latency = None if sent["at"] is None else time.monotonic() - sent["at"]
            require_script(repo, "hold.py", HOLD_PY)
            return judge_ctrl_b(
                latency=latency, finished=marker(repo, "job-finished"),
                still_running=bool(matching(command_lines(), token)), returned=returned,
            )
        finally:
            if tui is not None:
                tui.quit()
            stop_token(token)

    def stop_jobs(self) -> str:
        token = self.fresh_token("plakjobstop")
        repo, home = self.prepare("stop_jobs", FOREGROUND_BUDGET)
        tui = None
        try:
            tui = self.open_tui("stop_jobs", repo, home)
            session = _discover(home)
            before = len(terminals(home, session))
            tui.type(
                "Run exactly this shell command:\n"
                f"{hold_command(30, token)}\n"
                "Do not pass a timeout, yield, block_until, or background flag.\n"
                "When the tool returns while the command is still running, end your turn. "
                "Do not wait for it and do not stop it.\n"
                "Reply with the single word done."
            )
            tui.key(b"\r")
            if not tui.wait_until(lambda: len(terminals(home, session)) > before, 180):
                if terminals(home, session):
                    classify_tui_turn(home, session, len(terminals(home, session)) - 1, "stop", lookup_faults=tui.lookup_faults)
                raise ScenarioFailure(HARNESS, "root turn did not finish")
            classify_tui_turn(home, session, before, "stop", lookup_faults=tui.lookup_faults)
            require_script(repo, "hold.py", HOLD_PY)
            if marker(repo, "job-finished"):
                raise ScenarioFailure(MODEL, "command finished before the stop")
            if not matching(command_lines(), token):
                raise ScenarioFailure(MODEL, "job was not running when the turn ended")
            time.sleep(1)
            admitted = len(admitted_ids(home, session))
            tui.type("/ps")
            tui.key(b"\r")
            tui.pump(0.5)
            tui.key(b"S")
            confirmed = tui.wait_until(lambda: STOP_TITLE in visible(tui.screen), 5)
            if len(admitted_ids(home, session)) != admitted:
                raise ScenarioFailure(HARNESS, "the stop gesture was sent as a prompt")
            if confirmed:
                time.sleep(0.8)
                tui.key(b"\r")
            gone = False
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                tui.pump(0.2)
                if not matching(command_lines(), token):
                    gone = True
                    break
            woke = tui.wait_until(
                lambda: len(admitted_ids(home, session)) > admitted, WAKE_AFTER_STOP_S,
            )
            origin = None
            admitted_now = admitted_ids(home, session)
            if len(admitted_now) > admitted:
                _rows, turns = read_telemetry(home)
                origin = next((row.get("origin") for row in turns
                               if row.get("turn_id") == admitted_now[admitted]), None)
            return judge_stop(
                confirmed=confirmed, still_running=not gone,
                finished=marker(repo, "job-finished"), woke=woke, origin=origin,
            )
        finally:
            if tui is not None:
                tui.quit()
            stop_token(token)

    def wake_while_idle(self) -> str:
        token = self.fresh_token("plakjobwake")
        repo, home = self.prepare("wake_while_idle", FOREGROUND_BUDGET)
        tui = None
        try:
            tui = self.open_tui("wake_while_idle", repo, home)
            session = _discover(home)
            tui.type(
                "Run exactly this shell command:\n"
                f"{hold_command(20, token)}\n"
                "Do not pass a timeout, yield, block_until, or background flag.\n"
                "When the tool returns while the command is still running, end your turn "
                "without waiting for it and without stopping it.\n"
                "Reply with the single word done."
            )
            tui.key(b"\r")
            mark = {"n": None}

            def root_finished() -> bool:
                if len(terminals(home, session)) < 1:
                    return False
                if mark["n"] is None:
                    mark["n"] = len(tui.screen)
                return True

            if not tui.wait_until(root_finished, 180):
                if terminals(home, session):
                    classify_tui_turn(home, session, len(terminals(home, session)) - 1, "wake", lookup_faults=tui.lookup_faults)
                raise ScenarioFailure(HARNESS, "root turn did not finish")
            classify_tui_turn(home, session, 0, "wake", lookup_faults=tui.lookup_faults)
            require_script(repo, "hold.py", HOLD_PY)
            painted = mark["n"] or 0

            def idle_snapshot() -> bool:
                # HUD and the process list together. Sampling either one on
                # the first paint races the promotion. A border drawn before
                # the terminal still counts when nothing repainted after it.
                return (not marker(repo, "job-finished")
                        and bool(matching(command_lines(), token))
                        and PROCESS_BADGE in latest_hud(tui.screen, painted))

            idle_with_job = tui.wait_until(idle_snapshot, WAKE_AFTER_STOP_S)
            hud = idle_with_job
            woke = tui.wait_until(
                lambda: len(admitted_ids(home, session)) >= 2 and marker(repo, "job-finished"),
                45,
            )
            second = admitted_ids(home, session)
            origin = None
            if len(second) >= 2:
                _jobs, turns = read_telemetry(home)
                origin = next((row.get("origin") for row in turns
                               if row.get("turn_id") == second[1]), None)
            return judge_wake(
                idle_with_job=idle_with_job, hud=hud, second_admission=woke,
                origin=origin, finished=marker(repo, "job-finished"),
            )
        finally:
            if tui is not None:
                tui.quit()
            stop_token(token)

    def idle_badge(self) -> str:
        token = self.fresh_token("plakjobbadge")
        repo, home = self.prepare("idle_badge", FOREGROUND_BUDGET)
        tui = None
        try:
            tui = self.open_tui("idle_badge", repo, home)
            session = _discover(home)
            tui.type(idle_child_task(self.route, hold_command(25, token)))
            tui.key(b"\r")
            mark = {"n": None}

            def root_finished() -> bool:
                if len(terminals(home, session)) < 1:
                    return False
                if mark["n"] is None:
                    mark["n"] = len(tui.screen)
                return True

            if not tui.wait_until(root_finished, 180):
                if terminals(home, session):
                    classify_tui_turn(home, session, len(terminals(home, session)) - 1, "badge", lookup_faults=tui.lookup_faults)
                raise ScenarioFailure(HARNESS, "root turn did not finish")
            classify_tui_turn(home, session, 0, "badge", lookup_faults=tui.lookup_faults)
            require_script(repo, "hold.py", HOLD_PY)
            painted = mark["n"] or 0
            captured = {"hud": "", "live": False}

            def badge_while_live() -> bool:
                # Sample only while ps still shows the fixture. The frame is the
                # composer row at that moment, including a cursor-split update.
                if marker(repo, "job-finished") or not matching(command_lines(), token):
                    return False
                hud = latest_hud(tui.screen, painted)
                captured["hud"] = hud
                captured["live"] = True
                return AGENT_BADGE in hud and PROCESS_BADGE in hud

            alive = tui.wait_until(
                lambda: bool(matching(command_lines(), token)) and not marker(repo, "job-finished"),
                30,
            )
            if alive:
                # The hold is 25 s. Keep the frame from while the job is live;
                # a paint that arrives only as the TUI quits is not this sample.
                tui.wait_until(badge_while_live, 25)
            screen = captured["hud"]
            return judge_badge(
                root_idle=root_is_idle(live.session_records(home, session)),
                child_alive=bool(captured["live"]),
                agent=AGENT_BADGE in screen, process=PROCESS_BADGE in screen,
                promoted=command_promoted(all_session_records(home), token),
            )
        finally:
            if tui is not None:
                tui.quit()
            stop_token(token)


def _discover(home: Path, timeout: float = 30) -> str:
    """The first session log. A child may add another; the root was created first."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        logs = sorted(home.glob("state/*/sessions/*.events.jsonl"),
                      key=lambda path: path.stat().st_mtime)
        if logs:
            return logs[0].name.removesuffix(".events.jsonl")
        time.sleep(0.2)
    raise ScenarioFailure(HARNESS, "session log did not appear")


def _capture_complete(segments: list[Path]) -> bool:
    return bool(segments) and all(any(segment.glob("*.info")) for segment in segments) \
        and live.capture_integrity(segments)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("route", nargs="?", default="grok", choices=sorted(live.ROUTES))
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    parser.add_argument("--fixture-seed", type=int, help="replay randomized scenario inputs")
    parser.add_argument("--artifacts", type=Path, help="fresh attempt artifact directory")
    parser.add_argument("--only", default="", help="comma-separated scenario names")
    return parser


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        chosen = selected({name for name in args.only.split(",") if name})
    except ValueError as error:
        print(f"jobs: {error}", file=sys.stderr)
        return 2
    route = live.ROUTES[args.route]
    stamp = time.strftime("%Y%m%d-%H%M%S")
    artifacts = args.artifacts or live.ROOT / "target" / "live" / f"jobs-{route.name}-{stamp}"
    env = live.Env(args.binary.resolve(), artifacts / "home", artifacts)
    env.fixture_seed = args.fixture_seed
    if not env.binary.is_file():
        print(f"jobs: plak binary not found: {env.binary}", file=sys.stderr)
        return 2
    binary_start = live.binary_sha256(env.binary)
    live.write_home(env.home, route)
    jobs = Jobs(route, env)
    results = []
    evidence = {}
    for name, surface in chosen:
        (artifacts / name).mkdir(parents=True, exist_ok=True)
        started = time.monotonic()
        usage_before = live.session_usage(env.home)
        segment_start = len(env.capture_segments)
        try:
            detail, outcome = getattr(jobs, name)(), PASS
        except ScenarioFailure as failure:
            outcome, detail = failure.outcome, str(failure)
        except Exception as error:
            outcome, detail = HARNESS, f"driver error: {error!r}"
            (artifacts / name / "traceback.txt").write_text(traceback.format_exc())
        segments = env.capture_segments[segment_start:]
        complete = _capture_complete(segments)
        evidence[name] = {
            "segments": [str(segment) for segment in segments],
            "capture_complete": complete,
            "captured_exchanges": sum(len(list(segment.glob("*.info"))) for segment in segments),
        }
        if outcome == PASS and not complete:
            outcome, detail = HARNESS, "raw exchange evidence missing or incomplete"
        usage = live.usage_delta(live.session_usage(env.home), usage_before)
        # Private homes hold the scenario's own logs. Count those too.
        # JSONL `turn` events are the other projection; a jobs run whose
        # session log never received `turn.usage` was reported as all zeros.
        private = artifacts / name / "home"
        if private.is_dir():
            usage = usage.add(live.usage_delta(live.session_usage(private), live.TokenUsage()))
        usage = live.larger_usage(usage, live.transcript_usage(artifacts / name / "agent.jsonl"))
        result = live.Result(
            name, surface, outcome, detail, time.monotonic() - started,
            usage.input_tokens, usage.output_tokens,
            usage.cached_input_tokens, usage.reasoning_tokens,
        )
        results.append(result)
        print(f"{result.outcome:8} {name}: {detail}", flush=True)
    binary_end = live.binary_sha256(env.binary)
    evidence_complete = all(item["capture_complete"] for item in evidence.values())
    (artifacts / "report.json").write_text(json.dumps({
        **live.route_report(route, bound=env.bound_compositions),
        "binary": str(env.binary),
        "binary_sha256_start": binary_start,
        "binary_sha256_end": binary_end,
        "token_telemetry": live.sum_usage(
            live.TokenUsage(result.input_tokens, result.output_tokens,
                            result.cached_input_tokens, result.reasoning_tokens)
            for result in results),
        "evidence": evidence,
        "capture_complete": evidence_complete,
        "results": [result.__dict__ for result in results],
    }, indent=2) + "\n")
    print("\n" + live.table(results) + f"\n\nartifacts: {artifacts}")
    return 0 if (all(result.outcome == PASS for result in results)
                 and evidence_complete and binary_start == binary_end) else 1


if __name__ == "__main__":
    signal.signal(signal.SIGINT, signal.default_int_handler)
    raise SystemExit(main(sys.argv[1:]))
