"""Digest-locked comparison plans. Job IDs stay empty until a paid Harbor run."""

from __future__ import annotations

import hashlib
import json
import re
from typing import Any

from harbor_adapter.tuples import DATASET, DATASETS, SMOKE_TASKS


SCHEMA_VERSION = 1

REQUIRED_POLICY = (
    "model",
    "effort",
    "account",
    "timeout_policy",
    "attempts",
)


# A digest is only publishable when it is shaped like one: `sha256:` plus
# 64 lowercase hex characters. Presence alone locked nothing.
DIGEST_PATTERN = re.compile(r"^sha256:[0-9a-f]{64}$")


class PlanError(ValueError):
    pass


def _require_digest(value: Any, label: str) -> None:
    if not isinstance(value, str) or not DIGEST_PATTERN.match(value):
        raise PlanError(
            f"{label} must be 'sha256:' followed by 64 lowercase hex characters"
        )


def smoke_tasks() -> tuple[str, ...]:
    return SMOKE_TASKS


def canonical_bytes(plan: dict[str, Any]) -> bytes:
    return (json.dumps(plan, sort_keys=True, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


def plan_digest(plan: dict[str, Any]) -> str:
    return "sha256:" + hashlib.sha256(canonical_bytes(plan)).hexdigest()


def validate_plan(plan: dict[str, Any], *, require_jobs: bool = False) -> None:
    if plan.get("schemaVersion") != SCHEMA_VERSION:
        raise PlanError("schemaVersion must be 1")
    if plan.get("dataset") not in DATASETS:
        raise PlanError(f"dataset must be one of {DATASETS}")
    tasks = plan.get("tasks")
    if not isinstance(tasks, list) or not tasks:
        raise PlanError("tasks must be a non-empty list")
    for task in tasks:
        if not isinstance(task, dict) or "name" not in task:
            raise PlanError("each task needs a name")
        if require_jobs:
            _require_digest(
                task.get("digest"), f"task {task.get('name')!r} digest"
            )
    policy = plan.get("policy")
    if not isinstance(policy, dict):
        raise PlanError("policy is required")
    for key in REQUIRED_POLICY:
        if key not in policy:
            raise PlanError(f"policy.{key} is required")
    arms = plan.get("arms")
    if not isinstance(arms, dict) or len(arms) < 2:
        raise PlanError("arms must name at least two harnesses")
    for name, arm in arms.items():
        if not isinstance(arm, dict):
            raise PlanError(f"arm {name!r} must be an object")
        if require_jobs:
            job = arm.get("job")
            if not isinstance(job, dict) or not job.get("id"):
                raise PlanError(f"arm {name!r} is missing a Harbor job id")
            _require_digest(job.get("lockDigest"), f"arm {name!r} job.lockDigest")
            _require_digest(
                arm.get("candidateProvenanceDigest"),
                f"arm {name!r} candidateProvenanceDigest",
            )


def draft_plan(
    *,
    plan_id: str,
    account: str,
    model: str,
    effort: str,
    arms: tuple[str, ...],
    attempts: int = 1,
    dataset: str | None = None,
    tasks: tuple[str, ...] | None = None,
    timeout_policy: str | None = None,
) -> dict[str, Any]:
    resolved_dataset = dataset or DATASET
    resolved_tasks = tasks or SMOKE_TASKS
    if timeout_policy is None:
        if resolved_dataset == DATASET:
            timeout_policy = "terminal-bench-4.0-task-default"
        elif resolved_dataset.startswith("terminal-bench/terminal-bench-2-1"):
            timeout_policy = "terminal-bench-2-1-task-default"
        else:
            timeout_policy = "rung-a-plumbing-task-default"
    body = {
        "schemaVersion": SCHEMA_VERSION,
        "id": plan_id,
        "dataset": resolved_dataset,
        "tasks": [{"name": name, "digest": None} for name in resolved_tasks],
        "policy": {
            "model": model,
            "effort": effort,
            "account": account,
            "timeout_policy": timeout_policy,
            "attempts": attempts,
        },
        "arms": {
            name: {
                "job": {"id": None, "lockDigest": None},
                "candidateProvenanceDigest": None,
            }
            for name in arms
        },
    }
    validate_plan(body, require_jobs=False)
    return body
