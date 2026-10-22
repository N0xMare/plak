#!/usr/bin/env python3
"""The published shape of the `plak` manifest, and a staged tree that carries it.

`plak/test-hooks` binds the deterministic offline model from the
repository-only `plakous-offline-crash-harness`. Cargo cannot package a regular
dependency that is not on the registry, optional or not, and it strips only
version-less dev-dependencies. So the repository manifest keeps the feature
and its optional harness edge, and the published manifest is that manifest with
both removed: every `cfg(feature = "test-hooks")` site compiles out, and the
workspace lints declare the feature name to `check-cfg` so the packaged crate
builds without an unexpected-cfg warning.

`stage DEST` copies the tracked tree into DEST with the published `plak`
manifest, which is where `cargo package` runs for publication.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))
import vcs_listing  # noqa: E402
PLAK_MANIFEST = Path("bin/plak/Cargo.toml")
HARNESS = "plakous-offline-crash-harness"

# Exact repository text the published shape drops. A manifest edit that moves
# either block fails the transform instead of shipping the edge.
HARNESS_DEPENDENCY = (
    "# `test-hooks` only. The optional edge is what keeps the deterministic offline\n"
    "# model out of every build that does not explicitly ask for it.\n"
    f'{HARNESS} = {{ path = "../../tests/harnesses/offline-crash", optional = true }}\n'
)
TEST_HOOKS_FEATURE = (
    "# Repository gate only: bind the deterministic offline model named by\n"
    "# `PLAK_TEST_SCRIPT` instead of a provider, so the spawned binary's own\n"
    "# lifecycles are testable. Never in `default`, never in a release build.\n"
    "test-hooks = [\n"
    '    "plakous/test-clock",\n'
    f'    "dep:{HARNESS}",\n'
    '    "plakous-hosted/test-support",\n'
    '    "plakous-recursive-subagents-store/commit-trace",\n'
    "]\n"
)


class ShapeError(Exception):
    """The repository manifest no longer has the shape the transform removes."""


def published_plak_manifest(text: str) -> str:
    """Return the `plak` manifest without `test-hooks` and its harness edge."""
    for block in (HARNESS_DEPENDENCY, TEST_HOOKS_FEATURE):
        count = text.count(block)
        if count != 1:
            raise ShapeError(f"{PLAK_MANIFEST}: expected one exact block, found {count}:\n{block}")
        text = text.replace(block, "")
    return text


def tracked_files(root: Path) -> list[str]:
    return [path for path in vcs_listing.listed_files(root) if (root / path).is_file()]


def stage(root: Path, destination: Path) -> None:
    """Copy the working tree's files into DEST with the published plak manifest."""
    if destination.exists():
        raise ShapeError(f"{destination}: already exists")
    for relative in tracked_files(root):
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root / relative, target)
    manifest = destination / PLAK_MANIFEST
    manifest.write_text(published_plak_manifest(manifest.read_text(encoding="utf-8")), encoding="utf-8")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("manifest", help="print the published plak manifest")
    staged = commands.add_parser("stage", help="copy the tree with the published plak manifest")
    staged.add_argument("destination", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "manifest":
            sys.stdout.write(published_plak_manifest((ROOT / PLAK_MANIFEST).read_text(encoding="utf-8")))
        else:
            stage(ROOT, args.destination)
    except ShapeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
