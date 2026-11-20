#!/usr/bin/env python3
"""Run the `tests/tooling` unittest modules a few at a time.

Three modules take two thirds of the suite's five minutes one after another, so
the modules run as separate `python3 -m unittest` processes, largest file
first. Each module's output is printed whole when it finishes, and the run
fails if any module fails. Four at a time keeps the load light: a few modules
assert process-cleanup deadlines against the wall clock.

In batch mode, changes outside the tooling inputs only run the tree readers.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from changed_packages import batch_changes

REPO_ROOT = Path(__file__).resolve().parents[2]
SUITE = REPO_ROOT / "tests" / "tooling"
WORKERS = 4
TREE_READERS = {
    "test_configuration_placement",
    "test_host_client",
    "test_nextest_lanes",
    "test_pinned_callback_ports",
    "test_policy_validators",
    "test_portability_fence",
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


def run_module(path: Path, verbose: bool) -> tuple[Path, int, str]:
    command = [sys.executable, "-m", "unittest", *(["-v"] if verbose else []), str(path)]
    result = subprocess.run(
        command, cwd=REPO_ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
    )
    return path, result.returncode, result.stdout


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-v", action="store_true")
    parser.add_argument("--batch", action="store_true")
    args = parser.parse_args(argv)
    paths = batch_changes(REPO_ROOT, os.environ.get("PLAK_BATCH_BASE")) if args.batch else None
    modules = selected_modules(paths)
    failed = []
    with ThreadPoolExecutor(max_workers=WORKERS) as pool:
        for path, code, output in pool.map(lambda path: run_module(path, args.v), modules):
            print(f"-- {path.relative_to(REPO_ROOT)} (exit {code})\n{output}", end="", flush=True)
            if code:
                failed.append(path.name)
    if failed:
        print(f"tooling tests failed: {', '.join(failed)}", file=sys.stderr)
        return 1
    print(f"tooling tests passed: {len(modules)} modules")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
