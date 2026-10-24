#!/usr/bin/env python3
"""Run one Cargo command offline first, then online only if it must.

Usage: offline_first.py CARGO_ARGS...

The command runs with `--offline` first. When that fails because a cached
sparse-index entry has no version matching a requirement ("failed to select a
version for ..."), the named crates' index entries are refreshed online with
`cargo info` and the offline command is retried once, which avoids an online
run of the whole step for a stale entry. Any other failure, or a retry that
still fails, runs the command online once.

The refresh runs in an empty directory outside every Cargo workspace: inside
the staged tree, `cargo info` answers a crate the workspace patches (such as
`monty-typeshed`) from its local path and never reads the registry entry.
"""

from __future__ import annotations

import re
import subprocess
import sys
import tempfile
from collections.abc import Callable, Sequence
from pathlib import Path

# `failed to select a version for the requirement `name = "^1.2.0"`` and
# `failed to select a version for `name` which could resolve this conflict`.
VERSION_MISMATCH = re.compile(
    r"failed to select a version for (?:the requirement )?`([A-Za-z][A-Za-z0-9_-]*)(?: = \"[^\"]*\")?`"
)

Runner = Callable[..., subprocess.CompletedProcess[str]]


def stale_index_names(stderr: str) -> list[str]:
    """Crates whose cached index entry had no version matching a requirement."""
    return sorted(set(VERSION_MISMATCH.findall(stderr)))


def run_offline(command: Sequence[str], runner: Runner) -> subprocess.CompletedProcess[str]:
    result = runner([*command, "--offline"], stderr=subprocess.PIPE, text=True)
    sys.stderr.write(result.stderr or "")
    return result


def outside_workspace_base() -> str:
    """A temporary base with no `Cargo.toml` above it, so Cargo finds no workspace."""
    for base in (tempfile.gettempdir(), "/tmp"):
        path = Path(base).resolve()
        if path.is_dir() and not any((parent / "Cargo.toml").exists()
                                     for parent in (path, *path.parents)):
            return str(path)
    raise RuntimeError("no temporary directory outside a Cargo workspace")


def refresh_index_entries(names: Sequence[str], runner: Runner) -> bool:
    """Read each crate's registry entry online from outside any workspace."""
    with tempfile.TemporaryDirectory(prefix="plak-index-refresh.",
                                     dir=outside_workspace_base()) as empty:
        return all(
            runner(["cargo", "info", "--quiet", "--", name], cwd=empty,
                   stdout=subprocess.DEVNULL).returncode == 0
            for name in names
        )


def run(command: Sequence[str], runner: Runner = subprocess.run) -> int:
    offline = run_offline(command, runner)
    if offline.returncode == 0:
        return 0
    names = stale_index_names(offline.stderr or "")
    if names:
        print(f"offline resolution found stale index entries; refreshing {', '.join(names)}",
              file=sys.stderr)
        if refresh_index_entries(names, runner) and run_offline(command, runner).returncode == 0:
            print("offline retry succeeded after the index refresh", file=sys.stderr)
            return 0
    return runner(list(command)).returncode


if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] != "cargo":
        print("usage: offline_first.py cargo ARGS...", file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(run(sys.argv[1:]))
