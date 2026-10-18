"""Harness termination classification, recorded separately from reward.

A trial whose agent turn died on a harness condition — context admission
denied, a rejected tool call, an unavailable subscription credential — is not
a model miss. Harbor records reward 0 for both, so the two are
indistinguishable in `result.json` alone. This module reads the evidence the
adapters already harvest under `/logs/agent` and names the termination.

Everything here is pure over harvested bytes: the installed agent calls it
in-cell to write `plak-termination.json`, and `eval/scripts/summarize.py`
calls the same functions offline over an archived job tree, so a run recorded
before the adapter wrote any record still classifies the same way.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Sequence

from harbor_adapter.artifact_identity import record_source_commit
from harbor_adapter.timing import trial_phases


SCHEMA_VERSION = 1

# Trial-level classes. `turn_failed` is always suffixed with a failure kind.
CLASS_COMPLETED = "completed"
CLASS_TURN_FAILED = "turn_failed"
CLASS_AGENT_EXIT_NONZERO = "agent_exit_nonzero"
CLASS_AGENT_INIT_FAILED = "agent_init_failed"
CLASS_HARNESS_EXCEPTION = "harness_exception"
CLASS_RESULT_UNPARSEABLE = "result_unparseable"
CLASS_UNKNOWN = "unknown"

# Failure kinds for `turn_failed:<kind>`, in the order they are matched.
KIND_CREDENTIAL = "credential"
KIND_CONTEXT_CAPACITY = "context-capacity"
KIND_ROUND_LIMIT = "round-limit"
KIND_PROVIDER_RATE_LIMIT = "provider-rate-limit"
KIND_PROVIDER_REJECTION = "provider-rejection"
KIND_TOOL_FAILURE = "tool-failure"
KIND_OTHER = "other"

FAILURE_KINDS = (
    KIND_CREDENTIAL,
    KIND_CONTEXT_CAPACITY,
    KIND_ROUND_LIMIT,
    KIND_PROVIDER_RATE_LIMIT,
    KIND_PROVIDER_REJECTION,
    KIND_TOOL_FAILURE,
    KIND_OTHER,
)

TERMINATION_CLASSES = (
    CLASS_COMPLETED,
    *(f"{CLASS_TURN_FAILED}:{kind}" for kind in FAILURE_KINDS),
    CLASS_AGENT_EXIT_NONZERO,
    CLASS_AGENT_INIT_FAILED,
    CLASS_HARNESS_EXCEPTION,
    CLASS_RESULT_UNPARSEABLE,
    CLASS_UNKNOWN,
)

# `plak run` refusing before it opens a session. This is not a turn that
# failed: no session exists, the session JSONL is zero bytes, and no model
# operation was dispatched. It is not an unexplained nonzero exit either --
# plak names a typed session-creation kind in parentheses, which the class
# carries the way a harness exception carries its type.
#
#   plak: agent could not be initialized (Revalidation): <detail>
INIT_FAILURE_MARKER = "agent could not be initialized"
_INIT_KIND = re.compile(
    r"agent could not be initialized\s*\(([A-Za-z0-9_]+)\)"
)

# How much of `result.json` we could actually read. Anything but `ok` is
# printed, because a trial summary is worthless if the reader cannot say
# whether it read the file.
RESULT_OK = "ok"
RESULT_REPAIRED = "repaired"
RESULT_UNPARSEABLE = "unparseable"
RESULT_MISSING = "missing"

# Harbor raises this when the agent command exits nonzero. It is the ordinary
# shape of a plak turn failure, not a harness exception in its own right.
AGENT_EXIT_EXCEPTION = "NonZeroAgentExitCodeError"

# Ordered: the first marker table that matches wins, so a 429 that also says
# "rejected-before-acceptance" is a rate limit and not a bare rejection.
_KIND_MARKERS: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        KIND_CREDENTIAL,
        (
            "credential is unavailable",
            "subscription credential",
            "needslogin",
            "not authenticated",
            "unauthorized",
            "http 401",
            "invalid api key",
            "no credentials",
        ),
    ),
    (
        KIND_CONTEXT_CAPACITY,
        (
            "context admission denied",
            "context capacity",
            "context overflow",
            "cannot be compacted",
            "admitted ceiling",
            "context length exceeded",
            "maximum context length",
            "context_length_exceeded",
        ),
    ),
    (
        KIND_ROUND_LIMIT,
        (
            "exceeded its model-operation limit",
            "round limit",
            "round budget",
            "max_turns",
            "maximum number of turns",
        ),
    ),
    (
        KIND_PROVIDER_RATE_LIMIT,
        (
            "rate-limit response",
            "rate limit exceeded",
            "rate_limit_exceeded",
            "http 429",
            "429 too many requests",
            "too many requests",
            "ratelimit",
        ),
    ),
    (
        KIND_PROVIDER_REJECTION,
        (
            "temporarily overloaded",
            "provider/model is temporarily unavailable",
            "server_is_overloaded",
            "typed terminal error",
            "provider code:",
            "provider returned",
            "http 5",
            "internal server error",
            "service unavailable",
            "waiting for network",
            "connection failed",
        ),
    ),
    (
        KIND_TOOL_FAILURE,
        (
            "mutating tool outcome",
            "journal reconciliation",
            "local composition host",
            "composition result receiver",
            "tool failed",
            "tool rejected",
            "invalidarguments",
            "js host",
            "quickjs",
        ),
    ),
)

# Lines that mean "a turn ended badly", used to decide turn_failed vs a bare
# nonzero exit. A plak turn failure always prints one of these.
_TURN_FAILURE_MARKERS = (
    "agent turn failed",
    "agent turn requires recovery",
    "turn failed definitively",
    "continuation is blocked",
    "native subscription credential is unavailable",
    "tool-grown context overflow",
)


def classify_failure_kind(text: str) -> str:
    """Names the failure kind a diagnostic describes, `other` when unknown."""
    lowered = (text or "").lower()
    for kind, markers in _KIND_MARKERS:
        if any(marker in lowered for marker in markers):
            return kind
    return KIND_OTHER


def initialization_kind(text: str) -> str | None:
    """The typed session-creation kind plak refused with, if it refused."""
    if INIT_FAILURE_MARKER not in (text or ""):
        return None
    found = _INIT_KIND.search(text)
    return found.group(1) if found else ""


def agent_init_class(kind: str | None) -> str:
    return f"{CLASS_AGENT_INIT_FAILED}:{kind}" if kind else CLASS_AGENT_INIT_FAILED


def has_turn_failure(text: str) -> bool:
    lowered = (text or "").lower()
    return any(marker in lowered for marker in _TURN_FAILURE_MARKERS)


def turn_failed_class(text: str) -> str:
    return f"{CLASS_TURN_FAILED}:{classify_failure_kind(text)}"


# A Harbor exception whose name or message is itself typed provider evidence
# is a turn failure, not an unexplained harness death. `AgentTimeoutError` and
# a compose failure are not: nothing says the provider refused the turn.
_EXCEPTION_FAILURE_KINDS = (
    KIND_CREDENTIAL,
    KIND_CONTEXT_CAPACITY,
    KIND_PROVIDER_RATE_LIMIT,
    KIND_PROVIDER_REJECTION,
)


def harness_exception_class(exception_type: str) -> str:
    """Names the exception in the class, the way pmux names its own deaths."""
    return (
        f"{CLASS_HARNESS_EXCEPTION}:{exception_type}"
        if exception_type
        else CLASS_HARNESS_EXCEPTION
    )


def classify_harness_exception(exception_type: str, exception_message: str) -> str:
    kind = classify_failure_kind(f"{exception_type} {exception_message}")
    if kind in _EXCEPTION_FAILURE_KINDS:
        return f"{CLASS_TURN_FAILED}:{kind}"
    return harness_exception_class(exception_type)


def parse_exit_code(text: str | None) -> int | None:
    for line in (text or "").splitlines():
        token = line.strip()
        if token.isdigit() or (token.startswith("-") and token[1:].isdigit()):
            return int(token)
    return None


@dataclass(frozen=True)
class ToolRejection:
    tool: str
    reason: str
    count: int

    def as_dict(self) -> dict[str, Any]:
        return {"tool": self.tool, "reason": self.reason, "count": self.count}


@dataclass
class TerminationRecord:
    """What ended a trial, independent of what the verifier scored."""

    harness: str
    termination_class: str
    exit_code: int | None = None
    detail: str = ""
    disposition: str = ""
    exception_type: str = ""
    session_ids: tuple[str, ...] = ()
    result_status: str = RESULT_OK
    tool_rejections: tuple[ToolRejection, ...] = ()
    tool_calls_started: int = 0
    tool_calls_completed: int = 0
    tool_calls_failed: int = 0

    @property
    def is_harness_termination(self) -> bool:
        """True when reward 0 would misread this trial as a model miss."""
        return self.termination_class != CLASS_COMPLETED

    @property
    def label(self) -> str:
        """The class as printed, annotated when the result file was not clean."""
        if self.result_status == RESULT_OK:
            return self.termination_class
        return f"{self.termination_class} [{self.result_status}]"

    @property
    def rejection_count(self) -> int:
        return sum(item.count for item in self.tool_rejections)

    def as_dict(self) -> dict[str, Any]:
        return {
            "schemaVersion": SCHEMA_VERSION,
            "harness": self.harness,
            "class": self.termination_class,
            "exit_code": self.exit_code,
            "detail": self.detail,
            "disposition": self.disposition,
            "exception_type": self.exception_type,
            "session_ids": list(self.session_ids),
            "result_status": self.result_status,
            "tool_rejections": [item.as_dict() for item in self.tool_rejections],
            "tool_calls": {
                "started": self.tool_calls_started,
                "completed": self.tool_calls_completed,
                "failed": self.tool_calls_failed,
            },
        }


def parse_session_events(text: str) -> list[dict[str, Any]]:
    """Reads a plak session JSONL, skipping any line that is not an object."""
    events: list[dict[str, Any]] = []
    for line in (text or "").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except (ValueError, TypeError):
            continue
        if isinstance(record, dict):
            events.append(record)
    return events


def tool_rejections(events: Sequence[dict[str, Any]]) -> tuple[ToolRejection, ...]:
    counts: dict[tuple[str, str], int] = {}
    for event in events:
        if event.get("kind") != "tool.rejected":
            continue
        key = (
            str(event.get("name") or "unknown"),
            str(event.get("reason") or "unknown"),
        )
        counts[key] = counts.get(key, 0) + 1
    return tuple(
        ToolRejection(tool=tool, reason=reason, count=count)
        for (tool, reason), count in sorted(counts.items())
    )


def _tool_call_counts(events: Sequence[dict[str, Any]]) -> tuple[int, int, int]:
    started = completed = failed = 0
    for event in events:
        kind = event.get("kind")
        if kind == "tool.started":
            started += 1
        elif kind == "tool.completed":
            completed += 1
            if event.get("state") not in (None, "succeeded"):
                failed += 1
    return started, completed, failed


def _session_failure(events: Sequence[dict[str, Any]]) -> dict[str, Any] | None:
    for event in reversed(list(events)):
        if event.get("kind") in ("turn.failed", "turn.recovery_required"):
            return event
    return None


def classify_plak(
    *,
    exit_code: int | None,
    stderr: str,
    events: Sequence[dict[str, Any]] = (),
    exception_type: str = "",
    exception_message: str = "",
    session_ids: Iterable[str] = (),
) -> TerminationRecord:
    """Classifies one plak trial from its exit code, stderr, and session JSONL.

    The session JSONL carries a typed `turn.failed` for most deaths, but not
    all: a context admission refusal is printed to stderr with no terminal
    session record, so stderr is consulted whenever the JSONL is silent.
    """
    rejections = tool_rejections(events)
    started, completed, failed = _tool_call_counts(events)
    failure = _session_failure(events)
    record = TerminationRecord(
        harness="plak",
        termination_class=CLASS_UNKNOWN,
        exit_code=exit_code,
        exception_type=exception_type,
        session_ids=tuple(session_ids),
        tool_rejections=rejections,
        tool_calls_started=started,
        tool_calls_completed=completed,
        tool_calls_failed=failed,
    )
    if exception_type and exception_type != AGENT_EXIT_EXCEPTION:
        record.termination_class = classify_harness_exception(
            exception_type, exception_message
        )
        record.detail = _first_line(exception_message) or exception_type
        return record
    if exit_code == 0:
        record.termination_class = CLASS_COMPLETED
        return record
    # Pre-dispatch refusal comes first: it precedes any session, so there can
    # be no turn record to prefer, and it must not land in the residual.
    init_kind = initialization_kind(stderr)
    if init_kind is not None:
        record.termination_class = agent_init_class(init_kind)
        record.detail = _first_line(_plak_stderr_diagnostic(stderr))
        return record
    if failure is not None:
        record.disposition = str(failure.get("disposition") or "")
        record.detail = _first_line(str(failure.get("msg") or ""))
        record.termination_class = turn_failed_class(str(failure.get("msg") or ""))
        return record
    diagnostic = _plak_stderr_diagnostic(stderr)
    if diagnostic and has_turn_failure(stderr):
        record.detail = diagnostic
        record.termination_class = turn_failed_class(stderr)
        return record
    if exit_code is None:
        record.termination_class = (
            harness_exception_class(exception_type)
            if exception_type
            else CLASS_UNKNOWN
        )
        record.detail = _first_line(exception_message) or exception_type
        return record
    record.detail = diagnostic
    record.termination_class = CLASS_AGENT_EXIT_NONZERO
    return record


def _first_line(text: str) -> str:
    for line in (text or "").splitlines():
        if line.strip():
            return line.strip()
    return ""


def _plak_stderr_diagnostic(stderr: str) -> str:
    """The last stderr line that is a diagnostic rather than tool chatter."""
    for line in reversed((stderr or "").splitlines()):
        token = line.strip()
        if not token or token.startswith("plak: tool "):
            continue
        return token
    return ""


# Vendor CLIs keep one transcript file per harness under /logs/agent.
VENDOR_LOGS: dict[str, tuple[str, ...]] = {
    "codex": ("codex.txt",),
    "grok-build": ("grok-build.txt", "grok-build-cli.log"),
    "pi": ("pi.txt",),
    "opencode": ("opencode.txt", "opencode-attempts.jsonl"),
    "pmux": ("pmux-stderr.log", "pmux-stdout.txt"),
}


def _vendor_jsonl_tail(text: str) -> tuple[bool, str]:
    """Returns (ended in a typed error, that error's message).

    Codex and Grok Build stream JSONL items; a clean run ends with a
    `turn.completed`. A run whose last typed record is an `error` never
    finished its turn however the process exited.
    """
    last_error = ""
    ended_in_error = False
    for line in (text or "").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except (ValueError, TypeError):
            continue
        if not isinstance(record, dict):
            continue
        kind = record.get("type") or record.get("kind") or ""
        if kind == "error":
            ended_in_error = True
            last_error = str(record.get("message") or record.get("error") or "")
        elif kind:
            ended_in_error = False
    return ended_in_error, last_error


def classify_vendor(
    *,
    harness: str,
    log_text: str,
    exception_type: str = "",
    exception_message: str = "",
    exit_code: int | None = None,
) -> TerminationRecord:
    """Classifies a vendor arm from what its logs allow.

    Vendor CLIs do not record a typed termination, so only two things can be
    read back: the process exit (through Harbor's exception) and an error the
    CLI printed. Anything else is reported as `completed`, which means "the
    CLI exited and said nothing about failing", not "the model tried".
    """
    record = TerminationRecord(
        harness=harness,
        termination_class=CLASS_COMPLETED,
        exit_code=exit_code,
        exception_type=exception_type,
    )
    ended_in_error, last_error = _vendor_jsonl_tail(log_text)
    if exception_type == AGENT_EXIT_EXCEPTION or (
        exit_code is not None and exit_code != 0
    ):
        evidence = " ".join(filter(None, (last_error, exception_message)))
        record.detail = _first_line(last_error or exception_message)
        record.termination_class = (
            turn_failed_class(evidence)
            if evidence and classify_failure_kind(evidence) != KIND_OTHER
            else CLASS_AGENT_EXIT_NONZERO
        )
        return record
    if exception_type:
        record.termination_class = classify_harness_exception(
            exception_type, exception_message
        )
        record.detail = _first_line(exception_message) or exception_type
        return record
    if ended_in_error:
        record.termination_class = turn_failed_class(last_error)
        record.detail = _first_line(last_error)
        return record
    return record


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def read_session_events(agent_dir: Path) -> tuple[list[dict[str, Any]], tuple[str, ...]]:
    """Reads every plak session JSONL the adapter harvested for one trial."""
    # Plak keeps each build's state in its own `state/<format key>/` directory.
    state = agent_dir / "plak-home" / "state"
    events: list[dict[str, Any]] = []
    ids: list[str] = []
    for path in sorted(state.glob("*/sessions/*.events.jsonl")):
        ids.append(path.name.removesuffix(".events.jsonl"))
        events.extend(parse_session_events(read_text(path)))
    return events, tuple(ids)


# Harbor records the adapter class on every trial, including one that died
# in preflight and harvested no agent log at all.
_AGENT_CLASS_HARNESS = (
    ("PlakAgent", "plak"),
    ("FrozenPmux", "pmux"),
    ("FrozenCodex", "codex"),
    ("FrozenGrokBuild", "grok-build"),
    ("FrozenOpenCode", "opencode"),
    ("FrozenPi", "pi"),
)


def configured_harness(result: dict[str, Any]) -> str:
    name = str(((result.get("config") or {}).get("agent") or {}).get("name") or "")
    for marker, harness in _AGENT_CLASS_HARNESS:
        if marker in name:
            return harness
    return "unknown"


def trial_harness(agent_dir: Path, result: dict[str, Any] | None = None) -> str:
    if (agent_dir / "plak-exit.txt").exists() or (agent_dir / "plak-stderr.log").exists():
        return "plak"
    for harness, names in VENDOR_LOGS.items():
        if any((agent_dir / name).exists() for name in names):
            return harness
    return configured_harness(result or {})


def classify_trial_dir(trial_dir: Path) -> TerminationRecord:
    """Classifies one archived Harbor trial directory, plak or vendor."""
    agent_dir = trial_dir / "agent"
    loaded = load_json(trial_dir / "result.json")
    result = loaded.data
    if not loaded.ok:
        # The trial record is unreadable, so nothing may be concluded from it.
        # Reporting `completed` here is how a dead vendor trial read as a pass.
        return TerminationRecord(
            harness=trial_harness(agent_dir, result),
            termination_class=CLASS_RESULT_UNPARSEABLE,
            result_status=loaded.status,
            detail=f"{(trial_dir / 'result.json').name} did not parse",
        )
    info = (result.get("exception_info") or {}) if isinstance(result, dict) else {}
    exception_type = str(info.get("exception_type") or "")
    exception_message = str(info.get("exception_message") or "")
    harness = trial_harness(agent_dir, result)
    if harness == "plak":
        events, ids = read_session_events(agent_dir)
        record = classify_plak(
            exit_code=parse_exit_code(read_text(agent_dir / "plak-exit.txt")),
            stderr=read_text(agent_dir / "plak-stderr.log"),
            events=events,
            exception_type=exception_type,
            exception_message=exception_message,
            session_ids=ids,
        )
        record.result_status = loaded.status
        return record
    log_text = ""
    for name in VENDOR_LOGS.get(harness, ()):
        log_text += read_text(agent_dir / name)
    record = classify_vendor(
        harness=harness,
        log_text=log_text,
        exception_type=exception_type,
        exception_message=exception_message,
    )
    record.result_status = loaded.status
    if harness == "pmux":
        # FrozenPmux already names its own death in pmux-usage.json; keep it
        # rather than re-deriving a weaker answer from the exception alone.
        declared = _load_json(agent_dir / "pmux-usage.json").get(
            "harness_termination"
        )
        if isinstance(declared, str) and declared:
            record.disposition = declared
    return record


# Harbor scrubs the literal value of every `--ae NAME=value` pair out of the
# serialized trial directory with a plain string replace. An arm that passed
# `--ae CODEX_FORCE_AUTH_JSON=true` therefore has every JSON boolean `true` in
# its own result.json rewritten to the bare token `[REDACTED]`, which is not a
# JSON value, so the file stops parsing. This repairs exactly that token, in
# value position only, and the repair is always reported rather than silent.
_SCRUBBED_TRUE = re.compile(r"(?<=[:\[,])(\s*)\[REDACTED\](?=\s*[,\}\]\r\n])")


@dataclass(frozen=True)
class LoadedJson:
    """A parsed document plus how much of it could be read."""

    data: dict[str, Any]
    status: str

    @property
    def ok(self) -> bool:
        return self.status in (RESULT_OK, RESULT_REPAIRED)


def repair_scrubbed_booleans(text: str) -> str:
    """Puts `true` back where Harbor's `--ae` scrub replaced the literal."""
    return _SCRUBBED_TRUE.sub(r"\1true", text)


def load_json(path: Path) -> LoadedJson:
    """Reads a Harbor JSON document, never failing open on a parse error.

    An unreadable `result.json` used to come back as an empty dict, which
    took the task name, the reward, the tokens and the duration with it and
    still left the trial looking like a clean pass. A parse failure is now a
    fact the caller has to carry.
    """
    text = read_text(path)
    if not text.strip():
        return LoadedJson({}, RESULT_MISSING if not path.is_file() else RESULT_UNPARSEABLE)
    loaded = _parse_object(text)
    if loaded is not None:
        return LoadedJson(loaded, RESULT_OK)
    if "[REDACTED]" in text:
        loaded = _parse_object(repair_scrubbed_booleans(text))
        if loaded is not None:
            return LoadedJson(loaded, RESULT_REPAIRED)
    return LoadedJson({}, RESULT_UNPARSEABLE)


def _parse_object(text: str) -> dict[str, Any] | None:
    try:
        loaded = json.loads(text)
    except (ValueError, TypeError):
        return None
    return loaded if isinstance(loaded, dict) else None


def _load_json(path: Path) -> dict[str, Any]:
    """Back-compatible reader for callers that only need the fields."""
    return load_json(path).data


@dataclass
class TrialSummary:
    task: str
    trial: str
    reward: float | None
    record: TerminationRecord
    input_tokens: int | None = None
    output_tokens: int | None = None
    duration_s: float | None = None
    # Identity of the bytes the trial uploaded, from its usage record.
    artifact: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return {
            "task": self.task,
            "trial": self.trial,
            "reward": self.reward,
            "termination": self.record.as_dict(),
            "tokens": {
                "input": self.input_tokens,
                "output": self.output_tokens,
            },
            "duration_s": self.duration_s,
            "artifact": self.artifact,
        }


@dataclass
class JobSummary:
    job_dir: str
    trials: list[TrialSummary] = field(default_factory=list)

    @property
    def class_counts(self) -> dict[str, int]:
        counts: dict[str, int] = {}
        for trial in self.trials:
            key = trial.record.termination_class
            counts[key] = counts.get(key, 0) + 1
        return dict(sorted(counts.items()))

    @property
    def rejection_counts(self) -> dict[str, int]:
        counts: dict[str, int] = {}
        for trial in self.trials:
            for item in trial.record.tool_rejections:
                key = f"{item.tool}:{item.reason}"
                counts[key] = counts.get(key, 0) + item.count
        return dict(sorted(counts.items()))

    @property
    def harness_terminated(self) -> int:
        return sum(1 for trial in self.trials if trial.record.is_harness_termination)

    def artifact_digests(self, name: str) -> dict[str, int]:
        """Distinct sha256s seen for one uploaded binary, with trial counts.

        More than one entry means the job did not run a single artifact, which
        is the thing a pass-then-fail investigation needs to know first.
        """
        counts: dict[str, int] = {}
        for trial in self.trials:
            entry = ((trial.artifact or {}).get("binaries") or {}).get(name) or {}
            sha = str(entry.get("sha256") or "")
            if sha:
                counts[sha] = counts.get(sha, 0) + 1
        return dict(sorted(counts.items()))

    @property
    def source_commits(self) -> dict[str, int]:
        counts: dict[str, int] = {}
        for trial in self.trials:
            commit = record_source_commit(trial.artifact)
            if commit:
                counts[commit] = counts.get(commit, 0) + 1
        return dict(sorted(counts.items()))

    @property
    def result_status_counts(self) -> dict[str, int]:
        counts: dict[str, int] = {}
        for trial in self.trials:
            key = trial.record.result_status
            counts[key] = counts.get(key, 0) + 1
        return dict(sorted(counts.items()))

    @property
    def unreadable_results(self) -> int:
        return sum(
            1 for trial in self.trials if trial.record.result_status != RESULT_OK
        )

    @property
    def rewarded(self) -> int:
        return sum(1 for trial in self.trials if (trial.reward or 0) > 0)

    def as_dict(self) -> dict[str, Any]:
        return {
            "schemaVersion": SCHEMA_VERSION,
            "job": self.job_dir,
            "trials": len(self.trials),
            "rewarded": self.rewarded,
            "harness_terminated": self.harness_terminated,
            "classes": self.class_counts,
            "result_status": self.result_status_counts,
            "artifacts": {
                name: self.artifact_digests(name)
                for name in ("pmuxd", "pmux", "plak")
                if self.artifact_digests(name)
            },
            "source_commits": self.source_commits,
            "tool_rejections": self.rejection_counts,
            "rows": [trial.as_dict() for trial in self.trials],
        }


def _reward(result: dict[str, Any]) -> float | None:
    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    value = rewards.get("reward")
    return float(value) if isinstance(value, (int, float)) else None


def _duration_seconds(result: dict[str, Any]) -> float | None:
    from datetime import datetime

    started = result.get("started_at")
    finished = result.get("finished_at")
    if not isinstance(started, str) or not isinstance(finished, str):
        return None
    try:
        begin = datetime.fromisoformat(started.replace("Z", "+00:00"))
        end = datetime.fromisoformat(finished.replace("Z", "+00:00"))
    except ValueError:
        return None
    return (end - begin).total_seconds()


USAGE_FILES = ("pmux-usage.json", "plak-usage.json")


def trial_artifact(trial_dir: Path) -> dict[str, Any]:
    """The uploaded-artifact identity a trial recorded, if it recorded one.

    Preferred from the harness's own usage file, which is written even when
    Harbor never got a clean result, and falls back to the metadata Harbor
    copied into result.json.
    """
    agent_dir = trial_dir / "agent"
    for name in USAGE_FILES:
        found = load_json(agent_dir / name).data.get("artifact")
        if isinstance(found, dict) and found:
            return found
    vendor = load_json(agent_dir / "vendor-artifact.json").data
    binaries = vendor.get("binaries")
    if (vendor.get("schemaVersion") == 1 and isinstance(vendor.get("harness"), str)
            and isinstance(binaries, dict) and binaries
            and all(isinstance(entry, dict)
                    and isinstance(entry.get("sha256"), str)
                    and re.fullmatch(r"[0-9a-f]{64}", entry["sha256"])
                    and isinstance(entry.get("bytes"), int) and entry["bytes"] > 0
                    for entry in binaries.values())):
        return vendor
    metadata = (
        (load_json(trial_dir / "result.json").data.get("agent_result") or {})
        .get("metadata")
        or {}
    )
    found = metadata.get("artifact")
    return found if isinstance(found, dict) else {}


def summarize_trial(trial_dir: Path) -> TrialSummary:
    result = load_json(trial_dir / "result.json").data
    usage = result.get("agent_result") or {}
    task = str(result.get("task_name") or trial_dir.name.split("__", 1)[0])
    return TrialSummary(
        task=task,
        trial=trial_dir.name,
        reward=_reward(result),
        record=classify_trial_dir(trial_dir),
        input_tokens=_optional_int(usage.get("n_input_tokens")),
        output_tokens=_optional_int(usage.get("n_output_tokens")),
        duration_s=trial_phases(result)["agent_execution_sec"],
        artifact=trial_artifact(trial_dir),
    )


def _optional_int(value: object) -> int | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def is_trial_dir(path: Path) -> bool:
    """A trial directory carries a result and the agent's harvested logs.

    A job directory also holds a `result.json` (the roll-up), so the `agent/`
    sibling is what separates the two. Without it, every job tree would be
    read as a single one-trial job named after its timestamp.
    """
    if not path.is_dir():
        return False
    if not (path / "result.json").is_file():
        return "__" in path.name and (
            (path / "agent").is_dir() or (path / "config.json").is_file()
        )
    if (path / "agent").is_dir():
        return True
    result = load_json(path / "result.json").data
    return bool(result.get("trial_name") and result.get("task_name"))


def is_job_dir(path: Path) -> bool:
    return path.is_dir() and any(
        is_trial_dir(child) for child in path.iterdir() if child.is_dir()
    )


def find_job_dirs(root: Path) -> list[Path]:
    """Finds every job directory at or beneath `root`."""
    if not root.is_dir():
        return []
    if is_job_dir(root):
        return [root]
    found = {
        candidate.parent.parent
        for candidate in root.rglob("result.json")
        if is_trial_dir(candidate.parent) and is_job_dir(candidate.parent.parent)
    }
    found |= {
        candidate.parent.parent
        for candidate in root.rglob("agent")
        if candidate.is_dir() and is_trial_dir(candidate.parent)
        and is_job_dir(candidate.parent.parent)
    }
    return sorted(found)


def summarize_job(job_dir: Path) -> JobSummary:
    summary = JobSummary(job_dir=str(job_dir))
    for trial in sorted(job_dir.iterdir()):
        if is_trial_dir(trial):
            summary.trials.append(summarize_trial(trial))
    summary.trials.sort(key=lambda trial: (trial.task, trial.trial))
    return summary


_HEADERS = ("task", "reward", "termination", "rejections", "in/out tokens", "dur(s)")


def render_table(summary: JobSummary) -> str:
    rows: list[tuple[str, ...]] = [_HEADERS]
    for trial in summary.trials:
        rejections = (
            ", ".join(
                f"{item.tool}:{item.reason}x{item.count}"
                for item in trial.record.tool_rejections
            )
            or "-"
        )
        rows.append(
            (
                trial.task,
                "-" if trial.reward is None else f"{trial.reward:.2f}",
                trial.record.label,
                rejections,
                f"{'-' if trial.input_tokens is None else trial.input_tokens}/"
                f"{'-' if trial.output_tokens is None else trial.output_tokens}",
                "-" if trial.duration_s is None else f"{trial.duration_s:.0f}",
            )
        )
    widths = [max(len(row[i]) for row in rows) for i in range(len(_HEADERS))]
    lines = [summary.job_dir]
    for index, row in enumerate(rows):
        lines.append("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)).rstrip())
        if index == 0:
            lines.append("  ".join("-" * width for width in widths))
    classes = ", ".join(f"{name}={count}" for name, count in summary.class_counts.items())
    lines.append("")
    lines.append(
        f"trials={len(summary.trials)} rewarded={summary.rewarded} "
        f"harness-terminated={summary.harness_terminated}"
    )
    lines.append(f"classes: {classes or '-'}")
    rejections = ", ".join(
        f"{key}={count}" for key, count in summary.rejection_counts.items()
    )
    lines.append(f"tool rejections: {rejections or '-'}")
    for name in ("pmuxd", "plak"):
        digests = summary.artifact_digests(name)
        if not digests:
            continue
        shown = ", ".join(
            f"{sha[:12]}x{count}" for sha, count in digests.items()
        )
        warning = "  <-- MORE THAN ONE" if len(digests) > 1 else ""
        lines.append(f"{name}: {shown}{warning}")
    commits = summary.source_commits
    if commits:
        lines.append(
            "pseudomux: "
            + ", ".join(f"{c[:7]}x{n}" for c, n in commits.items())
        )
    if summary.unreadable_results:
        statuses = ", ".join(
            f"{name}={count}"
            for name, count in summary.result_status_counts.items()
            if name != RESULT_OK
        )
        lines.append(f"result.json not clean: {statuses}")
    return "\n".join(lines)
