#!/usr/bin/env python3
"""Run one qualification cell, retaining a first MODEL or PROVIDER and one rerun.

Raw lane drivers remain single-attempt diagnostics. This driver reruns the
whole selected lane in a new home, never inference inside an existing session.
"""

from __future__ import annotations

import argparse
import importlib
import json
import os
import secrets
import shlex
import shutil
import sys
import tempfile
from pathlib import Path

import live

LANES = {"smoke": "smoke", "jobs": "jobs", "long-session": "long_session",
         "soak": "soak", "tui-soak": "tui_soak"}


def score_report(code: int, report: dict) -> tuple[str, str]:
    """Evidence/build failures outrank scored model and provider outcomes."""
    if report.get("pre_attempt_error"):
        return live.HARNESS, report["pre_attempt_error"]
    if report.get("driver_error"):
        return live.HARNESS, "driver error: " + report["driver_error"]
    summary = report.get("summary", report)
    if not summary.get("capture_complete"):
        return live.HARNESS, "raw exchange evidence missing or incomplete"
    if not summary.get("binary_sha256_start") or not summary.get("binary_sha256_end"):
        return live.HARNESS, "binary identity missing"
    if summary.get("rebuild") == "changed":
        if (not summary.get("binary_sha256_rebuilt")
                or summary["binary_sha256_end"] != summary["binary_sha256_rebuilt"]):
            return live.HARNESS, "binary differs from intentional rebuild"
    elif summary["binary_sha256_start"] != summary["binary_sha256_end"]:
        return live.HARNESS, "binary changed during attempt"
    records = report.get("results", report.get("records", []))
    failure = summary.get("failure")
    if failure:
        records = [*records, failure]
    evidence = report.get("evidence", {})
    if any(not item.get("capture_complete") for item in evidence.values()):
        return live.HARNESS, "scenario capture missing or incomplete"
    if summary.get("evidence_gaps") or summary.get("diagnostics_available") is False:
        return live.HARNESS, "diagnostic evidence missing"
    if (summary.get("clean_quit") is False
            or (summary.get("rebuild") == "failed" and summary.get("rebuild_attempted"))):
        return live.HARNESS, "quit or rebuild failed"
    if summary.get("turn_failures"):
        return live.HARNESS, "unclassified failed turn"
    if summary.get("dead_ends") or any(
            record.get("dead_end") or record.get("outcome", "").split()[:1] == ["dead-end"]
            for record in records):
        return live.HARNESS, "soak dead end"
    model_records = [record for record in records
                     if record.get("outcome", "").split()[:1] == [live.MODEL]
                     or (record.get("phase") == "completed" and record.get("oracle") is False)]
    model_reasons = []
    for record in model_records:
        task = record.get("scenario") or record.get("task")
        capture = evidence.get(task, {}) if record.get("scenario") else record
        segments = capture.get("segments") or ([capture["segment"]] if capture.get("segment") else [])
        exchanges = capture.get("captured_exchanges", 0)
        status = capture.get("evidence_status", "response" if capture.get("capture_complete") else "")
        if not task or not segments or not exchanges or status != "response":
            return live.HARNESS, "MODEL record lacks a capture reference"
        detail = record.get("detail") or str(record.get("errors", []))
        model_reasons.append(f"{task}: {detail}; evidence_status={status}, "
                             f"captured_exchanges={exchanges}, segments={segments}")
    if summary.get("model_misses") and not model_records:
        return live.HARNESS, "MODEL summary lacks a capture-referenced task"
    for outcome in (live.HARNESS, live.PENDING, live.PROVIDER, live.MODEL):
        if outcome == live.MODEL and model_reasons:
            return live.MODEL, "; ".join(model_reasons)
        failed = [record for record in records if record.get("outcome", "").split()[:1] == [outcome]]
        if failed:
            return outcome, "; ".join(record.get("detail") or str(record.get("errors", []))
                                      for record in failed)
    if code != 0 or not records:
        return live.HARNESS, "unscored driver failure or empty attempt"
    return live.PASS, "all selected oracles passed"


def run_cell(artifacts: Path, attempt, *, binding: dict | None = None) -> dict:
    artifacts.mkdir(parents=True, exist_ok=True)
    records = []
    for number in (1, 2):
        path = artifacts / f"attempt-{number}"
        try:
            code, report = attempt(path)
        except (Exception, SystemExit) as error:
            code, report = 1, {"capture_complete": False, "driver_error": repr(error)}
        outcome, reason = score_report(code, report)
        records.append({"attempt": number, "artifacts": str(path), "outcome": outcome,
                        "reason": reason, "exit_code": code, "report": report})
        print(f"attempt {number}: {outcome.upper()}: {reason}", flush=True)
        if number == 1 and outcome in (live.MODEL, live.PROVIDER):
            continue
        break
    final = records[-1]["outcome"]
    status = final.upper()
    if len(records) == 2:
        status += f" after {records[0]['outcome'].upper()} rerun"
    result = {"outcome": final, "status": status, "attempts": records,
              "binding": binding or {}}
    (artifacts / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    print(status + f"\nartifacts: {artifacts}", flush=True)
    return result


def qualifies(route: live.Route, result: dict) -> bool:
    outcome = result["outcome"]
    if outcome == live.PASS:
        return True
    if outcome in (live.HARNESS, live.PENDING):
        return False
    if route.route_class == live.INFORMATIONAL:
        return True
    attempts = result["attempts"]
    return (route.name in ("or-mimo", "or-glm", "or-qwen37")
            and len(attempts) == 2
            and all(attempt["outcome"] == live.PROVIDER for attempt in attempts))


def execute(lane: str, route: str, binary: Path, argv: list[str], seed: int, path: Path):
    driver = importlib.import_module(LANES[lane])
    args = [route, "--binary", str(binary), "--artifacts", str(path), *argv]
    if lane in ("smoke", "long-session", "jobs"):
        args += ["--fixture-seed", str(seed)]
    code = driver.main(args)
    return code, json.loads((path / "report.json").read_text())


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("lane", choices=LANES)
    parser.add_argument("route", choices=sorted(live.ROUTES))
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    parser.add_argument("--dry-run", action="store_true")
    args, forwarded = parser.parse_known_args(argv)
    option_error = ("attempt artifacts/seed are owned by this driver; use lane long-session"
                    if any(arg.split("=", 1)[0] in ("--artifacts", "--fixture-seed", "--long-session")
                           for arg in forwarded) else None)
    command = ["python3", f"tests/live/{LANES[args.lane]}.py", args.route,
               "--binary", str(args.binary), *forwarded]
    if args.dry_run:
        if option_error:
            print(option_error, file=sys.stderr)
            return 2
        driver = importlib.import_module(LANES[args.lane])
        try:
            driver.build_parser().parse_args([args.route, *forwarded])
        except SystemExit:
            return 2
        print(shlex.join(command))
        print("up to 2 attempts: first MODEL or PROVIDER may rerun once; "
              "same route/build/prompt, fresh session; HARNESS never reruns")
        return 0
    binary = args.binary.resolve()
    digest = live.binary_sha256(binary)
    root = live.ROOT / "target/live"
    root.mkdir(parents=True, exist_ok=True)
    artifacts = Path(tempfile.mkdtemp(prefix=f"qualification-{args.lane}-{args.route}-", dir=root))
    retained = artifacts / "starting-plak"
    retained_error = None
    if args.lane == "soak":
        shutil.copy2(binary, retained)
        if live.binary_sha256(retained) != digest:
            retained_error = "retained binary digest differs after copying"
    seed = secrets.randbits(64)
    intentional_rebuild = None

    def attempt(path):
        nonlocal intentional_rebuild
        current = live.binary_sha256(binary)
        metadata = {"starting_build_restored": False,
                    "binary_sha256_current": current,
                    "binary_sha256_restored": None,
                    "binary_sha256_intentional": intentional_rebuild}
        if option_error or retained_error:
            return 1, {**metadata, "pre_attempt_error": option_error or retained_error}
        if current != digest and current == intentional_rebuild:
            # The completed soak owns this one known byte-distinct rebuild.
            # Restore its retained starting build, never an unexpected replacement.
            if live.binary_sha256(retained) != digest:
                return 1, {**metadata, "pre_attempt_error": "retained binary digest differs before restoring"}
            replacement = binary.with_name(binary.name + "." + artifacts.name + ".restore")
            try:
                shutil.copy2(retained, replacement)
                if live.binary_sha256(replacement) != digest:
                    return 1, {**metadata, "pre_attempt_error": "retained replacement digest differs"}
                if live.binary_sha256(binary) != intentional_rebuild:
                    return 1, {**metadata, "pre_attempt_error": "binary changed before attempt; no paid attempt"}
                os.replace(replacement, binary)
            finally:
                replacement.unlink(missing_ok=True)
            current = live.binary_sha256(binary)
            metadata.update(starting_build_restored=True, binary_sha256_restored=current)
        if current != digest:
            return 1, {**metadata, "pre_attempt_error": "binary changed before attempt; no paid attempt",
                       "capture_complete": True, "binary_sha256_start": digest,
                       "binary_sha256_end": current}
        code, report = execute(args.lane, args.route, binary, forwarded, seed, path)
        report.update(metadata)
        summary = report.get("summary", report)
        if args.lane == "soak" and summary.get("rebuild") == "changed":
            intentional_rebuild = summary.get("binary_sha256_rebuilt")
        return code, report

    result = run_cell(artifacts, attempt, binding={
        **live.route_report(live.ROUTES[args.route]), "lane": args.lane,
        "binary": str(binary), "binary_sha256_start": digest, "fixture_seed": seed,
        "lane_options": forwarded,
    })
    return 0 if qualifies(live.ROUTES[args.route], result) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
