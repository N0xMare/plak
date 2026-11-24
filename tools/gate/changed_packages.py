#!/usr/bin/env python3
"""Map a working-tree or batch diff to workspace packages and test selections.

`tools/gate/dev-check.sh --fast` selects these packages for its parallel lane
and their reverse dependencies for clippy. Structural checks still cover
the workspace.
A file belongs to the package whose
directory is its longest prefix. A change to a workspace-wide Cargo input
(the root manifest, the lockfile, toolchain, lint or nextest config) touches
every package, as does any path under vendor/. A file outside every package
that a package's tests read (PACKAGE_INPUTS) touches that package. The
batch filter adds consumers of runtime binaries beyond Cargo's graph. Other
files outside any package, such as docs and Python tooling, touch none.

Usage:
  changed_packages.py            # one package name per line, or `--workspace`
  changed_packages.py PATH...    # map the given repository-relative paths
  changed_packages.py --dependents PACKAGE...  # conservative workspace rdeps
  changed_packages.py --batch-filter          # nextest filter, using PLAK_BATCH_BASE
"""

from __future__ import annotations

import json
import os
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
    ".config/nextest-install.toml",
    "tools/gate/test-env.sh",
    "tools/gate/test-processes.py",
    "tools/gate/test-tmpdir.sh",
    "tools/gate/dev-tools.pins",
}
WORKSPACE_PREFIXES = (".cargo/", "vendor/")
# A fixture crate is a normal package input; loose shared fixtures are not.
SHARED_TEST_PREFIX = "tests/fixtures/"
# Files and directories outside every package that the named package's tests
# read: plakd compares its config key table and reference config with these.
PACKAGE_INPUTS = {
    ".config/install-predecessor": "plakd",
    "docs/spec/07-config-reference.md": "plakd",
    "docs/reference-config": "plakd",
}

# These binaries and protocols can affect tests without a Cargo dependency
# edge. Keep the consumer list aligned with the fixture env injection in
# dev-check.sh (PLAK_PY_HOST, PLAK_JS_HOST, PLAK_MCP_SERVER, PLAK_TEST_BINARY).
CELL_WORKER_CONSUMERS = ("plakd", "plakous", "plakous-hosted")
RUNTIME_EDGES = {
    "plakous-py-worker": CELL_WORKER_CONSUMERS,
    "plakous-js-runtime": CELL_WORKER_CONSUMERS,
    "plakous-cell": CELL_WORKER_CONSUMERS,
    "plakd": ("plakous-offline-crash-harness",),
    "plakous-mcp-fake-server": ("plakous-mcp", "plakous-hosted", "plakd"),
}


def workspace_packages(root: Path = REPO_ROOT) -> list[dict]:
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(metadata.stdout)["packages"]


def package_dirs(root: Path = REPO_ROOT, packages: list[dict] | None = None) -> dict[str, str]:
    """Repository-relative package directory -> package name."""
    if packages is None:
        packages = workspace_packages(root)
    return {
        Path(package["manifest_path"]).parent.relative_to(root).as_posix(): package["name"]
        for package in packages
    }


def changed_paths(root: Path = REPO_ROOT, base: str | None = None) -> list[str]:
    return vcs_listing.changed_paths(root, base or os.environ.get("PLAK_FAST_BASE"))


def batch_changes(root: Path, base: str | None) -> list[str] | None:
    """Return known changed paths or None when no comparison establishes a scope."""
    if not base:
        return None
    # Ignored regression seeds left by previous runs are not batch inputs.
    return vcs_listing.changed_paths(root, base, include_ignored_seeds=False)


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
            owner = dirs.get(parent.as_posix())
            if owner is not None:
                break
        if owner is None:
            if path.startswith(SHARED_TEST_PREFIX):
                return [WORKSPACE]
            for parent in (PurePosixPath(path), *PurePosixPath(path).parents):
                owner = PACKAGE_INPUTS.get(parent.as_posix())
                if owner is not None:
                    break
        if owner is not None:
            touched.add(owner)
    return sorted(touched)


def batch_filter(touched: list[str], packages: list[dict]) -> str:
    if touched == [WORKSPACE]:
        return "all()"
    cargo_scope = reverse_dependencies(touched, packages)
    runtime_scope: set[str] = set()
    pending = list(cargo_scope)
    while pending:
        for consumer in RUNTIME_EDGES.get(pending.pop(), ()):
            if consumer not in runtime_scope:
                runtime_scope.add(consumer)
                pending.append(consumer)
    extra = sorted(runtime_scope.difference(touched))
    terms = [f"rdeps(={name})" for name in touched] + [f"package(={name})" for name in extra]
    return " | ".join(terms) or "none()"


def main(argv: list[str]) -> int:
    if argv[:1] == ["--dependents"]:
        try:
            names = reverse_dependencies(argv[1:], workspace_packages())
        except ValueError as error:
            raise SystemExit(str(error)) from error
        print("\n".join(names))
        return 0
    base = os.environ.get("PLAK_BATCH_BASE")
    if argv == ["--batch-filter"]:
        paths = batch_changes(REPO_ROOT, base)
        if paths is None:
            print("all()")
            return 0
        packages = workspace_packages()
        print(batch_filter(touched_packages(paths, package_dirs(packages=packages)), packages))
        return 0
    paths = argv or changed_paths()
    for name in touched_packages(paths, package_dirs()):
        print(name)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
