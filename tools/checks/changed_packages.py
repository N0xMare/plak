#!/usr/bin/env python3
"""Map the working-tree diff against HEAD to the workspace packages it touches.

`scripts/dev-check.sh --fast` lints and tests only these packages (and, for
tests, their reverse dependencies). A file belongs to the package whose
directory is its longest prefix. A change to a workspace-wide Cargo input
(the root manifest, the lockfile, toolchain, lint or nextest config) touches
every package. Files outside any package, such as docs and Python tooling,
touch none.

Usage:
  changed_packages.py            # one package name per line, or `--workspace`
  changed_packages.py PATH...    # map the given repository-relative paths
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path, PurePosixPath

sys.path.insert(0, str(Path(__file__).resolve().parent))
import vcs_listing  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = "--workspace"

# Inputs that change how every package builds, lints or tests.
WORKSPACE_INPUTS = {
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rust-toolchain",
    "clippy.toml",
    "rustfmt.toml",
    ".config/nextest.toml",
}
WORKSPACE_PREFIXES = (".cargo/",)


def package_dirs(root: Path = REPO_ROOT) -> dict[str, str]:
    """Repository-relative package directory -> package name."""
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    dirs = {}
    for package in json.loads(metadata.stdout)["packages"]:
        directory = Path(package["manifest_path"]).parent.relative_to(root)
        dirs[directory.as_posix()] = package["name"]
    return dirs


def changed_paths(root: Path = REPO_ROOT, base: str | None = None) -> list[str]:
    return vcs_listing.changed_paths(root, base)


def touched_packages(paths: list[str], dirs: dict[str, str]) -> list[str]:
    """Package names touched by `paths`, or `[WORKSPACE]` for a workspace input."""
    touched: set[str] = set()
    for path in paths:
        if path in WORKSPACE_INPUTS or path.startswith(WORKSPACE_PREFIXES):
            return [WORKSPACE]
        owner = None
        for parent in PurePosixPath(path).parents:
            owner = dirs.get(parent.as_posix())
            if owner is not None:
                break
        if owner is not None:
            touched.add(owner)
    return sorted(touched)


def main(argv: list[str]) -> int:
    paths = argv or changed_paths()
    for name in touched_packages(paths, package_dirs()):
        print(name)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
