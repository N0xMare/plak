"""Content-free invocation bindings for reproducible offline comparisons."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

from harbor_adapter.tuples import (
    RUNG_A_AB_TASKS,
    RUNG_A_EXPANDED_TASKS,
    RUNG_A_PLUMBING_TASKS,
)


def invocation_context(
    *, arm: str, slice_name: str, account: str, model: str, effort: str,
    arch: str, attempts: int, concurrency: int, selected_task: str | None,
    source_commit: str, source_dirty: bool, cohort: str | None = None,
    repeat_block: str | None = None, prompt_variant: str | None = None,
) -> dict:
    if slice_name not in {"plumbing", "ab", "expanded"}:
        raise ValueError("unknown Rung A slice")
    if attempts < 1 or concurrency < 1:
        raise ValueError("attempts and concurrency must be positive")
    tasks = {
        "plumbing": RUNG_A_PLUMBING_TASKS,
        "ab": RUNG_A_AB_TASKS,
        "expanded": RUNG_A_EXPANDED_TASKS,
    }[slice_name]
    if selected_task:
        if selected_task not in tasks:
            raise ValueError("selected task is outside the frozen slice")
        tasks = (selected_task,)
    return {
        "schema_version": 1, "kind": "rung-a-comparison-context",
        "arm": arm, "slice": slice_name, "account": account,
        "model": model, "effort": effort, "arch": arch,
        "prompt_variant": prompt_variant or "default",
        "eval_source_commit": source_commit, "eval_source_dirty": source_dirty,
        "cohort": cohort, "repeat_block": repeat_block,
        "tasks": list(tasks), "attempts_per_task": attempts,
        "planned_trials": len(tasks) * attempts, "concurrency": concurrency,
        "dataset": "plak/rung-a-plumbing" if slice_name == "plumbing"
        else "terminal-bench/terminal-bench-2-1@6",
        "timeout_policy": "task-default; native internal deadlines recorded separately",
        "adapter_task_retries": 0,
        "native_internal_retries": "stock behavior; not assumed identical",
        "effort_semantics": "omit effort override" if effort == "none" else "explicit named effort",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    for name in ("arm", "slice", "account", "model", "effort", "arch"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--attempts", type=int, required=True)
    parser.add_argument("--concurrency", type=int, required=True)
    parser.add_argument("--prompt-variant")
    args = parser.parse_args()
    source = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=args.root, text=True,
    ).strip()
    dirty = bool(subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=normal"], cwd=args.root,
    ))
    record = invocation_context(
        arm=args.arm, slice_name=args.slice, account=args.account,
        model=args.model, effort=args.effort, arch=args.arch,
        attempts=args.attempts, concurrency=args.concurrency,
        selected_task=os.environ.get("HARBOR_INCLUDE_TASK") or None,
        source_commit=source, source_dirty=dirty,
        cohort=os.environ.get("HARBOR_COMPARISON_ID") or None,
        repeat_block=os.environ.get("HARBOR_REPEAT_BLOCK") or None,
        prompt_variant=args.prompt_variant,
    )
    record["input_sha256"] = {
        relative: hashlib.sha256((args.root / relative).read_bytes()).hexdigest()
        for relative in (
            "eval/scripts/rung-a.sh", "eval/harbor_adapter/rate_card.json",
            "eval/harbor_adapter/vendor_pins.py", f"eval/rung-a-{args.slice}.yaml",
        )
    }
    # One sidecar per invocation: a second launch cannot rewrite its bindings.
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(record, stream, indent=2)
        stream.write("\n")


if __name__ == "__main__":
    main()
