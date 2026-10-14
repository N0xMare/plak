#!/usr/bin/env python3
"""Append-only gate timing ledger and informational median warnings.

One JSON object per line, beside the Cargo target directory and outside the
artifact sweep. A warning never fails the gate.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path

FIELDS = ("commit", "tier", "group", "seconds", "exit_status", "platform", "cache")
MIN_SAMPLES = 3
HISTORY = 10
FACTOR = 2


def append_sample(ledger: Path, sample: dict) -> None:
    missing = [key for key in FIELDS if key not in sample]
    if missing:
        raise ValueError(f"gate timing sample missing {missing}")
    if sample["cache"] not in {"warm", "cold"}:
        raise ValueError("cache must be warm or cold")
    row = {key: sample[key] for key in FIELDS}
    row["seconds"] = int(row["seconds"])
    row["exit_status"] = int(row["exit_status"])
    ledger.parent.mkdir(parents=True, exist_ok=True)
    with ledger.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(row, sort_keys=True) + "\n")


def load_samples(ledger: Path) -> list[dict]:
    if not ledger.is_file():
        return []
    rows = []
    for line in ledger.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict) and set(FIELDS) <= row.keys():
            rows.append(row)
    return rows


def _format_seconds(value: float) -> str:
    if float(value) == int(value):
        return str(int(value))
    return f"{value:.1f}"


def warning_lines(samples: list[dict], *, platform: str, groups: list[str]) -> list[str]:
    """Compare the latest successful sample of each requested group.

    The median is taken over the last 10 successful samples for that group and
    platform triple, including the sample just recorded. Fewer than 3 successful
    samples produces no warning. Samples for other triples are ignored.
    """
    lines = []
    for group in groups:
        matched = [
            row for row in samples
            if row.get("group") == group and row.get("platform") == platform
            and int(row.get("exit_status", 1)) == 0
        ]
        if len(matched) < MIN_SAMPLES:
            continue
        window = matched[-HISTORY:]
        current = int(window[-1]["seconds"])
        median = statistics.median(int(row["seconds"]) for row in window)
        # A zero median is the one-second clock, not a baseline.
        if median <= 0:
            continue
        if current > FACTOR * median:
            lines.append(
                f"WARNING: {group} {_format_seconds(current)} s vs median "
                f"{_format_seconds(median)} s: cold cache or regression"
            )
    return lines


def _sample_from_args(args: argparse.Namespace) -> dict:
    return {
        "commit": args.commit,
        "tier": args.tier,
        "group": args.group,
        "seconds": args.seconds,
        "exit_status": args.exit_status,
        "platform": args.platform,
        "cache": args.cache,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    append = sub.add_parser("append", help="append one group sample")
    append.add_argument("--ledger", type=Path, required=True)
    append.add_argument("--commit", required=True)
    append.add_argument("--tier", required=True)
    append.add_argument("--group", required=True)
    append.add_argument("--seconds", required=True)
    append.add_argument("--exit-status", required=True)
    append.add_argument("--platform", required=True)
    append.add_argument("--cache", required=True, choices=("warm", "cold"))

    warn = sub.add_parser("warn", help="print informational median warnings")
    warn.add_argument("--ledger", type=Path, required=True)
    warn.add_argument("--platform", required=True)
    warn.add_argument("--groups", required=True, help="comma-separated groups recorded by this run")

    args = parser.parse_args(argv)
    if args.command == "append":
        append_sample(args.ledger, _sample_from_args(args))
        return 0
    groups = [group for group in args.groups.split(",") if group]
    for line in warning_lines(load_samples(args.ledger), platform=args.platform, groups=groups):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
