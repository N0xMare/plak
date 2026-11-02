#!/usr/bin/env python3
"""Summarize per-test timing from the nextest lane JUnit reports.

Each lane profile in `.config/nextest.toml` writes
`target/nextest/<profile>/junit.xml`. This prints, per lane, its wall time,
summed test time, effective parallelism, flaky and retried tests and its test
span, then the slowest tests and the per-binary and per-package totals across
the lanes read. Informational only: it gates nothing.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
import sys
import xml.etree.ElementTree as ElementTree

ROOT = Path(__file__).resolve().parents[2]
LANES = ("offline", "serial", "durability")


@dataclass(frozen=True)
class TestTime:
    lane: str
    binary: str
    name: str
    seconds: float
    started: float | None
    failed: bool
    # Failed attempts before the final one (nextest retries).
    retries: int

    @property
    def package(self) -> str:
        return self.binary.split("::", 1)[0]


@dataclass(frozen=True)
class Lane:
    name: str
    wall: float
    tests: tuple[TestTime, ...]


def timestamp(value: str | None) -> float | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def parse(lane: str, path: Path) -> Lane:
    root = ElementTree.parse(path).getroot()
    suites = [root] if root.tag == "testsuite" else root.findall("testsuite")
    tests: list[TestTime] = []
    for suite in suites:
        binary = suite.get("name", "")
        for case in suite.findall("testcase"):
            tests.append(
                TestTime(
                    lane=lane,
                    binary=case.get("classname") or binary,
                    name=case.get("name", ""),
                    seconds=float(case.get("time") or 0),
                    started=timestamp(case.get("timestamp")),
                    failed=case.find("failure") is not None or case.find("error") is not None,
                    retries=len(case.findall("flakyFailure"))
                    + len(case.findall("flakyError"))
                    + len(case.findall("rerunFailure"))
                    + len(case.findall("rerunError")),
                )
            )
    wall = float(root.get("time") or 0) or sum(test.seconds for test in tests)
    return Lane(lane, wall, tuple(tests))


def test_span(lane: Lane) -> tuple[float, TestTime | None]:
    """The span from the first test start to the last finish, and the last finisher.

    Without per-test timestamps the span is unknown; the longest test is the
    lower bound the lane cannot beat.
    """
    timed = [test for test in lane.tests if test.started is not None]
    if not timed:
        longest = max(lane.tests, key=lambda test: test.seconds, default=None)
        return (longest.seconds if longest else 0.0), longest
    first = min(test.started for test in timed if test.started is not None)
    last = max(timed, key=lambda test: (test.started or 0) + test.seconds)
    return (last.started or 0) + last.seconds - first, last


def label(test: TestTime) -> str:
    return f"{test.binary} {test.name}"


def report(lanes: list[Lane], top: int) -> list[str]:
    lines: list[str] = []
    for lane in lanes:
        summed = sum(test.seconds for test in lane.tests)
        span, last = test_span(lane)
        longest = max(lane.tests, key=lambda test: test.seconds, default=None)
        failed = sum(test.failed for test in lane.tests)
        flaky = sum(1 for test in lane.tests if test.retries and not test.failed)
        retries = sum(test.retries for test in lane.tests)
        lines.append(
            f"lane {lane.name}: {len(lane.tests)} tests, wall {lane.wall:.1f} s, summed {summed:.1f} s, "
            f"parallelism {summed / lane.wall if lane.wall else 0:.1f}x"
            + (f", {failed} failed" if failed else "")
            + (f", {flaky} flaky, {retries} retries" if retries else "")
        )
        if longest is not None:
            lines.append(f"  longest test: {longest.seconds:.2f} s {label(longest)}")
        if last is not None and any(test.started is not None for test in lane.tests):
            lines.append(
                f"  test span: {span:.1f} s from the first start, ending with {label(last)}"
            )
        elif last is not None:
            lines.append(
                f"  test span: no per-test timestamps; at least {span:.1f} s ({label(last)})"
            )
    tests = [test for lane in lanes for test in lane.tests]
    lines.append(f"slowest {min(top, len(tests))} tests:")
    for test in sorted(tests, key=lambda test: -test.seconds)[:top]:
        lines.append(f"  {test.seconds:8.2f} s  {test.lane:<10} {label(test)}")
    for title, key in (
        ("binaries", lambda test: test.binary),
        ("packages", lambda test: test.package),
    ):
        totals: dict[str, list[float]] = defaultdict(list)
        for test in tests:
            totals[key(test)].append(test.seconds)
        lines.append(f"top {min(top, len(totals))} {title} by summed test time:")
        for name, times in sorted(totals.items(), key=lambda item: -sum(item[1]))[:top]:
            lines.append(
                f"  {sum(times):8.1f} s  {len(times):5} tests  max {max(times):7.2f} s  {name}"
            )
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lanes", nargs="*", default=list(LANES), help="nextest profiles to read")
    parser.add_argument(
        "--dir",
        type=Path,
        default=ROOT / "target/nextest",
        help="the nextest store directory holding <profile>/junit.xml",
    )
    parser.add_argument("--top", type=int, default=15)
    args = parser.parse_args(argv)
    lanes: list[Lane] = []
    for name in args.lanes:
        path = args.dir / name / "junit.xml"
        if not path.is_file():
            print(f"lane {name}: no JUnit report at {path}; run that lane first")
            continue
        try:
            lanes.append(parse(name, path))
        except (ElementTree.ParseError, ValueError) as error:
            print(f"lane {name}: unreadable JUnit report {path}: {error}", file=sys.stderr)
    if not lanes:
        return 1
    print("\n".join(report(lanes, args.top)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
