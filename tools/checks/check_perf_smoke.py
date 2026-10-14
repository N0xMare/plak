#!/usr/bin/env python3
"""Compare one offline performance smoke run against this machine's baseline.

A number measured on one machine says nothing about another, so every seed row
is keyed by a machine id and a run whose machine has no seed row reports that
and exits zero. Within a machine, a metric that moved further than its recorded
tolerance is a regression and exits non-zero.

Usage: tools/checks/check_perf_smoke.py <measurements.jsonl> [--baseline <file>]
       tools/checks/check_perf_smoke.py <measurements.jsonl> --seed
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BASELINE = ROOT / "tests" / "performance" / "baselines" / "offline-perf-smoke.json"

# Metrics where a larger number is better; everything else is a duration or a
# footprint, where smaller is better.
HIGHER_IS_BETTER = {"jsonl_bulk_throughput_mb_per_s"}

# The metrics compared against the tolerance.
#
# Chosen from measured idle-host spread rather than from instinct. Five runs
# on an idle build host (load average 0.42) recorded
# the run-to-run spread of each metric; spec 05 §2.7 carries the table. Four
# metrics sit at or under 6%, which leaves a 50% tolerance plenty of room to
# mean something.
#
# The two that are excluded are excluded for different reasons.
# `jsonl_tool_round_trip_ms` swings 38.9% with no load at all, and bimodally,
# so no tolerance both catches a real regression and stays quiet on an idle
# box. `cold_start_to_ready_ms` is 12.6%, which would survive the tolerance but
# leaves no headroom for a loaded host, and these hosts are shared.
GATED_METRICS = {
    "cli_two_tool_turn_ms",
    "growth_turn_peak_rss_mb",
    "jsonl_bulk_throughput_mb_per_s",
    "tui_transcript_20k_layout_ms",
}


def read_measurements(path: Path) -> list[dict[str, object]]:
    rows = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


def regression(metric: str, measured: float, seed: float, tolerance: float) -> str | None:
    """Return a message when `measured` is worse than `seed` by more than `tolerance`."""
    if seed <= 0:
        return None
    if metric in HIGHER_IS_BETTER:
        worst = seed * (1.0 - tolerance)
        if measured < worst:
            return f"{metric}: {measured:g} is below {worst:g} (seed {seed:g})"
        return None
    worst = seed * (1.0 + tolerance)
    if measured > worst:
        return f"{metric}: {measured:g} is above {worst:g} (seed {seed:g})"
    return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("measurements", type=Path)
    parser.add_argument("--baseline", type=Path, default=BASELINE)
    parser.add_argument(
        "--seed",
        action="store_true",
        help="write these measurements into the baseline for this machine",
    )
    arguments = parser.parse_args()

    rows = read_measurements(arguments.measurements)
    if not rows:
        print("perf smoke: no measurements")
        return 1
    machines = {str(row["machine"]) for row in rows}
    if len(machines) != 1:
        print(f"perf smoke: measurements span {len(machines)} machines")
        return 1
    machine = machines.pop()

    document = (
        json.loads(arguments.baseline.read_text())
        if arguments.baseline.exists()
        else {"version": 1, "tolerance": 0.5, "machines": {}}
    )

    if arguments.seed:
        document.setdefault("machines", {})[machine] = {
            str(row["metric"]): {"value": row["value"], "unit": row["unit"]} for row in rows
        }
        arguments.baseline.parent.mkdir(parents=True, exist_ok=True)
        arguments.baseline.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
        print(f"perf smoke: seeded {len(rows)} metrics for machine {machine}")
        return 0

    seeds = document.get("machines", {}).get(machine)
    if not seeds:
        print(f"perf smoke: no baseline for machine {machine}; reporting only")
        for row in rows:
            print(f"  {row['metric']}: {row['value']} {row['unit']}")
        return 0

    tolerance = float(document.get("tolerance", 0.5))
    failures = []
    measured = set()
    for row in rows:
        metric = str(row["metric"])
        measured.add(metric)
        seed = seeds.get(metric)
        if seed is None:
            print(f"  {metric}: {row['value']} {row['unit']} (no seed)")
            continue
        if metric not in GATED_METRICS:
            # Printed, never judged. The word "regression" is deliberately
            # absent: nothing here can tell a slowdown from a busy neighbour.
            print(
                f"  {metric}: {row['value']} {row['unit']} "
                f"(informational, seed {seed['value']})"
            )
            continue
        message = regression(metric, float(row["value"]), float(seed["value"]), tolerance)
        if message:
            failures.append(message)
        else:
            print(f"  {metric}: {row['value']} {row['unit']} (seed {seed['value']})")
    # A metric this machine seeds but did not measure is *not* a regression. It
    # is a missing measurement, and saying otherwise sends the reader looking
    # for a slowdown that never happened.
    for metric in sorted(set(seeds) - measured):
        print(f"  {metric}: no measurement on this run")
    for message in failures:
        print(f"perf smoke regression: {message}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
