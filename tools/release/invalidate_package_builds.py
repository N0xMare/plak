#!/usr/bin/env python3
"""Invalidate only cohort builds in a warm Cargo package-verification target."""

from __future__ import annotations

import argparse
import os
import re
import shutil
from pathlib import Path


def walk_error(error: OSError) -> None:
    raise error


def invalidate(target: Path, names: set[str]) -> None:
    # Remove the package-owned build directory, not just its fingerprint. Cargo
    # 1.100 puts fingerprints below build/<name>/<hash>/fingerprint; older
    # versions use .fingerprint/<name>-<hash>. Neither hash is an interface.
    if not target.exists():
        return
    for directory, children, _ in os.walk(target, onerror=walk_error):
        parent = Path(directory)
        for child in children[:]:
            owned = parent.name == "build" and child in names
            legacy = parent.name == ".fingerprint" and any(
                child.startswith(name + "-") for name in names
            )
            if owned or legacy:
                shutil.rmtree(parent / child)
                children.remove(child)
            elif parent == target and child == "package":
                # Unpacked archive sources are not build artifacts.
                children.remove(child)

    # Fail closed if a different layout leaves recognizable cohort fingerprints.
    # Inspect both path components and marker filenames (lib-<crate>, etc.).
    spellings = names | {name.replace("-", "_") for name in names}
    cohort = re.compile(r"(?:^|[-_])(?:" + "|".join(map(re.escape, spellings)) + r")(?:$|[-.])")
    for directory, children, files in os.walk(target, onerror=walk_error):
        parent = Path(directory)
        if parent == target and "package" in children:
            children.remove("package")
        relative = parent.relative_to(target)
        if not any("fingerprint" in part for part in relative.parts):
            continue
        if any(cohort.search(part) for part in (*relative.parts, *children, *files)):
            raise SystemExit(
                f"publish: unrecognized cohort fingerprint layout at {parent}; "
                "refusing stale package verification; clear the verification target "
                "and update invalidation for this Cargo layout"
            )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=Path)
    parser.add_argument("order", type=Path)
    args = parser.parse_args()
    invalidate(args.target, {line.split()[0] for line in args.order.read_text().splitlines()})


if __name__ == "__main__":
    main()
