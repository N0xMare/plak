#!/usr/bin/env python3
"""The crates.io publication order of a workspace, and registry index lookups.

`order` reads `cargo metadata` in the current directory and prints one
`NAME VERSION` line per publishable member (`publish` not `false`),
dependencies first. Every dependency kind that a packaged manifest keeps
orders the cohort: normal and build dependencies, and dev-dependencies that
carry a version (a path-only dev-dependency is stripped by `cargo package`).
crates.io resolves each of those at upload, so a sibling must be in the index
before the crate that names it.

`indexed INDEX NAME VERSION` exits 0 when the sparse registry index at INDEX
(`https://index.crates.io`, or a `file://` directory with the same layout)
lists NAME at VERSION, and 1 otherwise.
"""

from __future__ import annotations

import argparse
import graphlib
import json
import subprocess
import sys
import urllib.error
import urllib.request
from typing import Any


class PlanError(Exception):
    """The workspace has no valid publication order."""


def publishable(package: dict[str, Any]) -> bool:
    return package.get("publish") != []


def publish_order(metadata: dict[str, Any]) -> list[tuple[str, str]]:
    """Return (name, version) of every publishable member, dependencies first."""
    members = set(metadata["workspace_members"])
    packages = {
        package["name"]: package
        for package in metadata["packages"]
        if package["id"] in members and publishable(package)
    }
    graph: dict[str, set[str]] = {}
    for name, package in packages.items():
        edges = set()
        for dependency in package["dependencies"]:
            if dependency["name"] not in packages or dependency["name"] == name:
                continue
            if dependency.get("kind") == "dev" and dependency.get("req") == "*":
                continue
            edges.add(dependency["name"])
        graph[name] = edges
    sorter = graphlib.TopologicalSorter(graph)
    try:
        sorter.prepare()
    except graphlib.CycleError as error:
        raise PlanError(f"publication dependency cycle: {' -> '.join(error.args[1])}") from error
    order: list[str] = []
    # Ready sets are sorted so the order is deterministic, not just valid.
    while sorter.is_active():
        ready = sorted(sorter.get_ready())
        order.extend(ready)
        sorter.done(*ready)
    return [(name, packages[name]["version"]) for name in order]


def index_path(name: str) -> str:
    """The sparse index file of a crate name (Cargo's registry index layout)."""
    lowered = name.lower()
    if len(lowered) <= 2:
        return f"{len(lowered)}/{lowered}"
    if len(lowered) == 3:
        return f"3/{lowered[0]}/{lowered}"
    return f"{lowered[:2]}/{lowered[2:4]}/{lowered}"


def indexed(index: str, name: str, version: str) -> bool:
    """Whether the index lists NAME at VERSION. An unreadable entry is absent."""
    url = f"{index.rstrip('/')}/{index_path(name)}"
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            body = response.read().decode("utf-8")
    except (urllib.error.URLError, OSError):
        return False
    for line in body.splitlines():
        if line.strip() and json.loads(line).get("vers") == version:
            return True
    return False


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("order", help="print NAME VERSION per publishable member, in order")
    lookup = commands.add_parser("indexed", help="exit 0 when the index lists NAME at VERSION")
    lookup.add_argument("index")
    lookup.add_argument("name")
    lookup.add_argument("version")
    args = parser.parse_args(argv)
    if args.command == "indexed":
        return 0 if indexed(args.index, args.name, args.version) else 1
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        capture_output=True, check=True, text=True,
    ).stdout)
    try:
        for name, version in publish_order(metadata):
            print(name, version)
    except PlanError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
