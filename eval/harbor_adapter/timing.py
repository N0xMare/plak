"""Split Harbor trial wall time so installer overhead is not harness score."""

from __future__ import annotations

from datetime import datetime
from typing import Any


def parse_timestamp(value: str | None) -> datetime | None:
    if not value:
        return None
    text = value.replace("Z", "+00:00")
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    return parsed


def span_seconds(block: dict[str, Any] | None) -> float | None:
    if not isinstance(block, dict):
        return None
    start = parse_timestamp(block.get("started_at") if isinstance(block.get("started_at"), str) else None)
    end = parse_timestamp(block.get("finished_at") if isinstance(block.get("finished_at"), str) else None)
    if start is None or end is None:
        return None
    if (start.utcoffset() is None) != (end.utcoffset() is None):
        return None
    seconds = (end - start).total_seconds()
    return seconds if seconds >= 0 else None


def trial_phases(result: dict[str, Any]) -> dict[str, Any]:
    """Score agent_execution. Keep setup/verifier as overhead. Errors are not reward 0."""
    exception = result.get("exception_info")
    exception_type = None
    if isinstance(exception, dict):
        exception_type = exception.get("exception_type")
    verifier_result = result.get("verifier_result")
    rewards = verifier_result.get("rewards") if isinstance(verifier_result, dict) else {}
    reward = rewards.get("reward") if isinstance(rewards, dict) else None
    return {
        "task": result.get("task_name") or result.get("trial_name"),
        "agent_setup_sec": span_seconds(result.get("agent_setup")),
        "agent_execution_sec": span_seconds(result.get("agent_execution")),
        "verifier_sec": span_seconds(result.get("verifier")),
        "reward": None if exception_type else reward,
        "exception_type": exception_type,
    }
