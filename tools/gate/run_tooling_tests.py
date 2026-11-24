#!/usr/bin/env python3
"""Run the `tests/tooling` unittest modules a few at a time.

Start expensive tree readers and build fixtures early, and split the mock
gate driver by independent test case. Four processes at a time keep the
load light: a few modules assert process-cleanup deadlines against the wall
clock. Any subprocess failure fails the run.

In batch mode, changes outside the tooling inputs only run the tree readers.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
from unittest import defaultTestLoader

from changed_packages import batch_changes

REPO_ROOT = Path(__file__).resolve().parents[2]
SUITE = REPO_ROOT / "tests" / "tooling"
WORKERS = 4
EARLY_MODULES = (
    "test_policy_validators",
    "test_shipping_artifacts",
    "test_check_prod_loc",
    "test_nextest_lanes",
)
TREE_READERS = {
    "test_agent_kind_docs",
    "test_artifact_linkage",
    "test_check_prod_loc",
    "test_configuration_placement",
    "test_host_client",
    "test_live_jobs",
    "test_nextest_lanes",
    "test_pinned_callback_ports",
    "test_policy_validators",
    "test_uuid_features",
}
FULL_TOOLING_PREFIXES = (
    "tools/",
    "tests/tooling/",
    "tests/live/",
    "eval/",
    ".config/",
    "tests/fixtures/",
)


def selected_modules(paths: list[str] | None) -> list[Path]:
    modules = sorted(SUITE.glob("test_*.py"), key=lambda path: (-path.stat().st_size, path.name))
    found = {module.stem for module in modules}
    if missing := TREE_READERS - found:
        raise SystemExit(f"missing mandatory tooling modules: {', '.join(sorted(missing))}")
    if paths is None or any(
        path.lower() == "justfile" or path.startswith(FULL_TOOLING_PREFIXES) for path in paths
    ):
        return modules
    return [module for module in modules if module.stem in TREE_READERS]


def work_items(modules: list[Path]) -> list[tuple[str, list[str]]]:
    early, remaining = [], []
    for path in modules:
        label = str(path.relative_to(REPO_ROOT))
        if path.stem in EARLY_MODULES:
            early.append((label, [str(path)]))
        elif path.stem != "test_dev_check":
            remaining.append((label, [str(path)]))
        else:
            name = "tests.tooling.test_dev_check"
            spec = spec_from_file_location(name, path)
            assert spec is not None and spec.loader is not None
            module = module_from_spec(spec)
            spec.loader.exec_module(module)
            cases = [
                case.id()
                for suite in defaultTestLoader.loadTestsFromModule(module)
                for case in suite
            ]
            early.extend(
                (f"{label} [{index + 1}/{WORKERS}]", cases[index::WORKERS])
                for index in range(WORKERS)
                if cases[index::WORKERS]
            )
    # Interleave mock gate-driver work with the long real-tool modules.
    return early + remaining


def run_cases(cases: list[str], verbose: bool) -> tuple[int, str]:
    command = [sys.executable, "-m", "unittest", *(["-v"] if verbose else []), *cases]
    result = subprocess.run(
        command, cwd=REPO_ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
    )
    return result.returncode, result.stdout


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-v", action="store_true")
    parser.add_argument("--batch", action="store_true")
    args = parser.parse_args(argv)
    paths = batch_changes(REPO_ROOT, os.environ.get("PLAK_BATCH_BASE")) if args.batch else None
    modules = selected_modules(paths)
    failed = []
    with ThreadPoolExecutor(max_workers=WORKERS) as pool:
        running = {
            pool.submit(run_cases, cases, args.v): label for label, cases in work_items(modules)
        }
        for result in as_completed(running):
            label = running[result]
            code, output = result.result()
            print(f"-- {label} (exit {code})\n{output}", end="", flush=True)
            if code:
                failed.append(label)
    if failed:
        print(f"tooling tests failed: {', '.join(failed)}", file=sys.stderr)
        return 1
    print(f"tooling tests passed: {len(modules)} modules")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
