#!/usr/bin/env python3
"""Run the `tests/tooling` unittest modules a few at a time.

Three modules take two thirds of the suite's five minutes one after another, so
the modules run as separate `python3 -m unittest` processes, largest file
first. Each module's output is printed whole when it finishes, and the run
fails if any module fails. Four at a time keeps the load light: a few modules
assert process-cleanup deadlines against the wall clock.

Usage: run_tooling_tests.py [-v]
"""

from __future__ import annotations

import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SUITE = REPO_ROOT / "tests" / "tooling"
WORKERS = 4


def run_module(path: Path, verbose: bool) -> tuple[Path, int, str]:
    command = [sys.executable, "-m", "unittest", *(["-v"] if verbose else []), str(path)]
    result = subprocess.run(
        command, cwd=REPO_ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
    )
    return path, result.returncode, result.stdout


def main(argv: list[str]) -> int:
    verbose = "-v" in argv
    modules = sorted(SUITE.glob("test_*.py"), key=lambda path: -path.stat().st_size)
    failed = []
    with ThreadPoolExecutor(max_workers=WORKERS) as pool:
        for path, code, output in pool.map(lambda path: run_module(path, verbose), modules):
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
