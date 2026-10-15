#!/usr/bin/env python3
"""Run the opt-in real-product SQLite commit sweep."""
import argparse
import os
from pathlib import Path
import subprocess


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shapes", default="final_only,direct_tool_then_final")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()
    if args.jobs < 1 or args.repeat < 1:
        parser.error("--jobs and --repeat must be positive")
    selector = Path(__file__).resolve().parent / "test-tmpdir.sh"
    scratch = Path(subprocess.run(["bash", str(selector)], check=True,
                                  capture_output=True, text=True).stdout.strip())
    home = scratch / "plak-isolated-home"
    home.mkdir(mode=0o700, exist_ok=True)
    env = dict(os.environ, TMPDIR=str(scratch), PLAK_HOME=str(home),
               PLAK_CRASH_SWEEP_SHAPES=args.shapes,
               PLAK_CRASH_SWEEP_JOBS=str(args.jobs),
               PLAK_CRASH_SWEEP_REPEAT=str(args.repeat),
               PLAK_CRASH_SWEEP_STRICT=str(int(args.strict)))
    return subprocess.call([
        "cargo", "test", "--locked", "-p", "plak", "--all-features",
        "--test", "integration", "crash_sweep::full_sweep", "--",
        "--ignored", "--exact", "--nocapture",
    ], env=env)


if __name__ == "__main__":
    raise SystemExit(main())
