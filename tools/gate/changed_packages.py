#!/usr/bin/env python3
"""Map the working-tree diff against HEAD to the workspace packages it touches.

`tools/gate/dev-check.sh --fast` selects these packages and their reverse
dependencies for its parallel test lane and worker clippy. Structural checks
still cover the workspace.
A file belongs to the package whose
directory is its longest prefix. A change to a workspace-wide Cargo input
(the root manifest, the lockfile, toolchain, lint or nextest config) touches
every package, as does any path under vendor/. A file outside every package
that a package's tests read (PACKAGE_INPUTS) touches that package. Other
files outside any package, such as docs and Python tooling, touch none.

Usage:
  changed_packages.py            # one package name per line, or `--workspace`
  changed_packages.py PATH...    # map the given repository-relative paths
  changed_packages.py --dependents PACKAGE...  # conservative workspace rdeps
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
    ".clippy.toml",
    ".rustfmt.toml",
    ".config/nextest.toml",
}
WORKSPACE_PREFIXES = (".cargo/", "vendor/")
# Files and directories outside every package that the named package's tests
# read: plak compares its config key table, template schema and reference
# config with these and loads every scenario home.
PACKAGE_INPUTS = {
    "docs/spec/07-config-reference.md": "plak",
    "docs/reference-config": "plak",
    "tests/fixtures/config-scenarios": "plak",
}


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


def reverse_dependencies(names: list[str], packages: list[dict]) -> list[str]:
    """Include all dependency kinds and targets, even inactive optional edges."""
    known = {package["name"] for package in packages}
    missing = set(names) - known
    if missing:
        raise ValueError(f"unknown workspace packages: {', '.join(sorted(missing))}")
    selected = set(names)
    while True:
        expanded = selected | {
            package["name"]
            for package in packages
            if any(dependency["name"] in selected for dependency in package["dependencies"])
        }
        if expanded == selected:
            return sorted(selected)
        selected = expanded


def touched_packages(paths: list[str], dirs: dict[str, str]) -> list[str]:
    """Package names touched by `paths`, or `[WORKSPACE]` for a workspace input."""
    touched: set[str] = set()
    for path in paths:
        if path in WORKSPACE_INPUTS or path.startswith(WORKSPACE_PREFIXES):
            return [WORKSPACE]
        owner = None
        for parent in (PurePosixPath(path), *PurePosixPath(path).parents):
            owner = dirs.get(parent.as_posix()) or PACKAGE_INPUTS.get(parent.as_posix())
            if owner is not None:
                break
        if owner is not None:
            touched.add(owner)
    return sorted(touched)


def main(argv: list[str]) -> int:
    if argv[:1] == ["--dependents"]:
        metadata = subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
            cwd=REPO_ROOT,
            text=True,
        )
        try:
            names = reverse_dependencies(argv[1:], json.loads(metadata)["packages"])
        except ValueError as error:
            raise SystemExit(str(error)) from error
        print("\n".join(names))
        return 0
    paths = argv or changed_paths()
    for name in touched_packages(paths, package_dirs()):
        print(name)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
