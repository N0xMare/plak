"""Read-only refused-reopen diagnostics. Projection gaps stay unknown."""

from __future__ import annotations

import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class RecoveryShape:
    status: str = "unavailable"
    phase: str = "unavailable"
    obligations: tuple[tuple[int, str], ...] = ()


PHASES = frozenset({"DispatchEvidence", "EffectReconciliation", "CommitRecovery",
                    "Finalization", "EffectDecision", "HostedContinuation"})
ACTIONS = frozenset({"CommitRecovery", "ProviderEvidenceRequired", "EffectEvidenceRequired",
                     "RestoreEffectDecision", "RestoreToolBatch", "RestoreServerPause",
                     "EvaluationAbandonmentRequired", "ProvenNotDispatched", "FinalizationRequired"})


def decode_show(text: str) -> RecoveryShape:
    root = text.partition("Canonical recovery (root session)")[2]
    root = root.partition("Topology children")[0]
    status, phase, obligations = "unavailable", "unavailable", []
    for line in root.splitlines():
        if re.fullmatch(r"  Status\s+recovery required", line):
            status = "recovery required"
        elif re.match(r"  Status\s+quiescent at ", line):
            status = "quiescent"
        elif match := re.fullmatch(r"  Phase\s+(\w+)", line):
            phase = match[1] if match[1] in PHASES else "unknown"
        elif match := re.match(r"  Obligation\s+operation=(\d+) action=(\w+)\b", line):
            action = match[2] if match[2] in ACTIONS else "unknown"
            obligations.append((int(match[1]), action))
    return RecoveryShape(status, phase, tuple(obligations))


def bucket(shape: RecoveryShape) -> str:
    if shape.status == "quiescent" and not shape.obligations:
        return "covered"
    if shape.status != "recovery required":
        return "unknown"
    actions = {action for _, action in shape.obligations}
    if shape.phase not in PHASES or "unknown" in actions:
        return "unknown"
    if actions & {"RestoreEffectDecision", "RestoreToolBatch", "RestoreServerPause",
                  "EvaluationAbandonmentRequired"}:
        return "known-uncovered"
    # Finalization/dispatch alone cannot identify finish reasons, hosted calls,
    # finalized cells, retries, steers, compaction or live process sessions.
    return "unknown"


def detail(shape: RecoveryShape) -> str:
    ordered = ", ".join(f"({op}, {action})" for op, action in shape.obligations)
    return (f"refusal triage: bucket={bucket(shape)} status={shape.status} "
            f"phase={shape.phase} obligations=[{ordered}]")


def rehearsal_verdict(text: str) -> str | None:
    for line in text.splitlines():
        if match := re.fullmatch(r"root: offline=(\w+) recovery_phase=(None|Some\(\w+\)) "
                                 r"evaluator=(verified|refused: recorded_artifact) "
                                 r"process_budgets=(verified|refused)", line):
            return (f"offline={match[1]} recovery_phase={match[2]} "
                    f"evaluator={match[3]} process_budgets={match[4]}")
        if match := re.fullmatch(r"root: refuses: (\w+)", line):
            return "refuses:" + match[1]
    return None


def inspect(binary: Path, repo: Path, home: Path, session: str) -> str:
    env = dict(os.environ)
    env["PLAK_HOME"] = str(home)
    for key in ("PLAK_TEST_FAULTS", "PLAK_TEST_EXCHANGE_CAPTURE_DIR",
                "PLAK_TEST_COMMIT_TRACE", "PLAK_TEST_COMMIT_KILL"):
        env.pop(key, None)
    shape, verdict = RecoveryShape(), None
    for command in ("show", "rehearse"):
        try:
            result = subprocess.run([str(binary), "session", command, session], cwd=repo,
                                    env=env, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            continue
        if command == "show" and result.returncode == 0:
            shape = decode_show(result.stdout)
        elif command == "rehearse":
            verdict = rehearsal_verdict(result.stdout)
    return detail(shape) + (f" rehearsal={verdict}" if verdict else "")


def reopen(factory, binary: Path, repo: Path, home: Path, session: str):
    """Keep the original outcome; only startup deaths/refusals gain diagnostics."""
    import live

    try:
        return factory()
    except live.ScenarioFailure as failure:
        message = str(failure)
        if failure.outcome == live.HARNESS and (
                "did not become ready" in message or "exit code 5" in message
                or "stale" in message.lower() or "unfinished" in message.lower()):
            raise live.ScenarioFailure(failure.outcome,
                                      message + "; " + inspect(binary, repo, home, session)) from failure
        raise
