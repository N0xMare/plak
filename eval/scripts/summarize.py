#!/usr/bin/env python3
"""Print harness termination and reward per trial for archived Harbor jobs.

    eval/scripts/summarize.py .plak/harbor/jobs/ab-plak-openrouter
    eval/scripts/summarize.py --json .plak/harbor/jobs
    eval/scripts/summarize.py --write .plak/harbor/jobs/ab-plak-codex

Reads only what the adapters already harvest under each trial's `agent/`
directory, so it works on job trees recorded before the adapter wrote any
`plak-termination.json`. No Docker, no Harbor, no model calls.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "eval"))

from harbor_adapter.termination import (  # noqa: E402
    RESULT_OK,
    RESULT_REPAIRED,
    find_job_dirs,
    render_table,
    summarize_job,
)
from harbor_adapter.report import build_report, render_markdown  # noqa: E402

SUMMARY_NAME = "termination-summary.json"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        default=None,
        help="job directories, or any parent of them (default .plak/harbor/jobs)",
    )
    parser.add_argument(
        "--json", action="store_true", help="emit one JSON document instead of tables"
    )
    parser.add_argument(
        "--write",
        action="store_true",
        help=f"also write {SUMMARY_NAME} into each job directory",
    )
    parser.add_argument(
        "--report", type=Path, metavar="DIR",
        help="write eval-report.json and eval-report.md to DIR",
    )
    parser.add_argument(
        "--selection", type=Path, metavar="INDEX",
        help="restrict to result hashes pinned in an immutable selection index",
    )
    parser.add_argument(
        "--prompt-ablation", action="store_true",
        help="compare only same-binary Plak legacy/parity prompt arms",
    )
    parser.add_argument(
        "--require-readable", action="store_true",
        help="fail if a trial result is missing or cannot be parsed",
    )
    parser.add_argument(
        "--expect-trials", type=int,
        help="fail unless this many trial results were found",
    )
    args = parser.parse_args(argv)
    if args.selection is not None and args.write:
        print("summarize: --selection cannot be combined with per-job --write", file=sys.stderr)
        return 2

    roots = [Path(path) for path in (args.paths or [ROOT / ".plak" / "harbor" / "jobs"])]
    jobs: list[Path] = []
    for root in roots:
        if not root.exists():
            print(f"summarize: no such path {root}", file=sys.stderr)
            return 2
        jobs.extend(find_job_dirs(root))
    summaries = [summarize_job(job) for job in sorted(set(jobs))]
    try:
        report = build_report(jobs, args.selection, roots,
                              prompt_ablation=args.prompt_ablation)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"summarize: selection/report error: {exc}", file=sys.stderr)
        return 2
    found = len(report["trials"]) if args.selection else sum(len(summary.trials) for summary in summaries)
    unreadable = (
        [trial for trial in report["trials"] if trial["result_status"] not in (RESULT_OK, RESULT_REPAIRED)]
        if args.selection else
        [trial for summary in summaries for trial in summary.trials
         if trial.record.result_status not in (RESULT_OK, RESULT_REPAIRED)]
    )
    count_matches = (args.expect_trials is None or
                     (args.expect_trials > 0 and found == args.expect_trials))
    if args.write:
        for job, summary in zip(sorted(set(jobs)), summaries):
            (job / SUMMARY_NAME).write_text(
                json.dumps(summary.as_dict(), indent=2) + "\n", encoding="utf-8"
            )
        # Preserve an invocation receipt even when Harbor died before creating
        # its first job, or only a subset of the expected trials exists.
        invocation = {
            "schemaVersion": 1,
            "status": "complete" if jobs and count_matches and not unreadable else "incomplete",
            "expected_trials": args.expect_trials,
            "found_trials": found,
            "unreadable_trials": len(unreadable),
            "jobs": [str(job) for job in sorted(set(jobs))],
        }
        for root in roots:
            (root / "termination-invocation.json").write_text(
                json.dumps(invocation, indent=2) + "\n", encoding="utf-8"
            )
    if args.report is not None:
        args.report.mkdir(parents=True, exist_ok=True)
        (args.report / "eval-report.json").write_text(
            json.dumps(report, indent=2) + "\n", encoding="utf-8"
        )
        (args.report / "eval-report.md").write_text(
            render_markdown(report, args.report), encoding="utf-8"
        )
    if not jobs:
        print("summarize: no job directories found", file=sys.stderr)
        return 1
    if not count_matches:
        print(f"summarize: expected {args.expect_trials} trials, found {found}",
              file=sys.stderr)
        return 1
    if args.require_readable and unreadable:
        print(f"summarize: {len(unreadable)} unreadable trial results", file=sys.stderr)
        return 1
    if args.json:
        payload = {**report, "jobs": [summary.as_dict() for summary in summaries]}
        print(json.dumps(payload, indent=2))
        return 0
    for index, summary in enumerate(summaries):
        if index:
            print()
        print(render_table(summary))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
