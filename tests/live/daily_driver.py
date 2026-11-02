#!/usr/bin/env python3
"""Summarize local daily-driver logs read-only, without printing private text."""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from collections import Counter
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path
from typing import Iterable

import live

ANSI = re.compile(r"\x1b\[[0-9;]*m")
LINE = re.compile(r"^(\d{4}-\d{2}-\d{2})T\S+\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$")
TARGET = re.compile(r"\b((?:plak|plakous)[a-zA-Z0-9_:.]*):\s")
FIELDS = re.compile(r'\b([a-z_]+)=(?:"([^"\n]*)"|([^\s}]+))')
BUILD_PATH = re.compile(r"(?:^|/)maintainer/([0-9a-f]{12,64})/plak(?:\s|\"|$)")
VERSION = re.compile(r"\bplak (?:version[: ]+)?[0-9][^\n]*?\bbinary ([0-9a-f]{12,64})\b")
PROVIDERS = frozenset({"codex", "xai", "grok", "claude", "native", "openrouter",
                       "openai", "anthropic", "xai-api"})
MODELS = frozenset(route.model for route in live.ROUTES.values())
CAUSES = frozenset({"http_status", "connect", "record_rejected_in_transit",
                   "transport_after_send", "stream_interrupted"})


@dataclass
class Build:
    build_id: str
    binary_id: str | None = None
    snapshot: bool = False
    days: set[str] = field(default_factory=set)
    bindings: dict[tuple[str, str], set[str]] = field(default_factory=dict)
    turns: set[str] = field(default_factory=set)
    problems: Counter = field(default_factory=Counter)
    capture: Counter = field(default_factory=Counter)
    retries: Counter = field(default_factory=Counter)
    reloads: int = 0
    carries: int = 0
    recovered: int = 0
    abandoned: int = 0

    def routes(self, child: bool) -> dict[str, int]:
        counts = Counter()
        for (route, session), _dates in self.bindings.items():
            if bool(re.search(r"\.c[1-9][0-9]*$", session)) == child:
                counts[route] += 1
        return dict(sorted(counts.items()))


def fields(text: str) -> dict[str, str]:
    return {match[1]: match[2] if match[2] is not None else match[3]
            for match in FIELDS.finditer(text)}


def snapshot_binary_id(snapshot: str) -> str | None:
    if not re.fullmatch(r"[0-9a-f]{12,64}", snapshot):
        return None
    executable = Path.home() / ".local/lib/plak/maintainer" / snapshot / "plak"
    try:
        digest = hashlib.sha256()
        with executable.open("rb") as source:
            for block in iter(lambda: source.read(64 * 1024), b""):
                digest.update(block)
        return digest.hexdigest()[:12]
    except OSError:
        return None


def summarize(lines: Iterable[str], since: str | None = None) -> list[Build]:
    if since is not None:
        since = date.fromisoformat(since).isoformat()
    builds: dict[str, Build] = {}
    snapshot_ids: dict[str, str | None] = {}
    current = "unknown"
    for raw in lines:
        match = LINE.match(ANSI.sub("", raw).rstrip("\n"))
        if not match:
            continue
        day, level, body = match.groups()
        try:
            date.fromisoformat(day)
        except ValueError:
            continue
        target_match = TARGET.search(body)
        target = target_match[1] if target_match else None
        message = body[target_match.end():] if target_match else body
        reload = target == "plak.reload" and "exec installed plak" in message
        version = VERSION.search(message) if not reload else None
        startup = fields(message).get("binary") if target == "plak.start" else None
        if startup and re.fullmatch(r"[0-9a-f]{12}", startup):
            current = next((key for key, build in builds.items() if build.binary_id == startup), startup)
            builds.setdefault(current, Build(current, binary_id=startup))
        elif version:
            previous = builds.get(current)
            if previous is not None and previous.snapshot and previous.binary_id in {None, version[1]}:
                previous.binary_id = version[1]
            else:
                current = version[1]
                builds.setdefault(current, Build(current, binary_id=current))
        # A reload belongs to the outgoing build; its subsequent lines are the
        # new build. Boundaries before --since still establish that attribution.
        if since is None or day >= since:
            build = builds.setdefault(current, Build(current))
            if target != "plak.capture":
                build.days.add(day)
            if level in {"WARN", "ERROR"} and target:
                counts = build.capture if target == "plak.capture" else build.problems
                counts[(level, target)] += 1
            for span in re.finditer(r"\bsession_runtime_binding\{([^}]*)\}", body):
                binding = fields(span[1])
                session = binding.get("session_id")
                if not session:
                    continue
                provider, model = binding.get("provider"), binding.get("model")
                provider = provider if provider in PROVIDERS else "unknown"
                model = model if model in MODELS else "unknown"
                build.bindings.setdefault((f"{provider}/{model}", session), set()).add(day)
            values = fields(message)
            if target == "plak.retry":
                cause = values.get("cause", values.get("cause_kind", "unknown"))
                build.retries[cause if cause in CAUSES else "unknown"] += 1
            if re.match(r"turn\.lifecycle\b", message) and values.get("phase") == "admitted":
                if turn := values.get("turn_id"):
                    build.turns.add(turn)
            if re.match(r"(?:turn|session)\.recovered\b", message):
                build.recovered += 1
            if re.match(r"turn\.abandoned\b", message):
                build.abandoned += 1
            if reload:
                build.reloads += 1
                build.carries += values.get("carried") == "true"
        if reload:
            installed = BUILD_PATH.search(message)
            current = installed[1] if installed else "unknown"
            if installed:
                if current not in snapshot_ids:
                    snapshot_ids[current] = snapshot_binary_id(current)
                builds.setdefault(current, Build(current, binary_id=snapshot_ids[current], snapshot=True))
    return [build for build in builds.values()
            if build.days or build.bindings or build.problems or build.capture or build.reloads]


def counts_text(counts: Counter) -> str:
    return "; ".join(f"{' '.join(key) if isinstance(key, tuple) else key}={count}"
                     for key, count in sorted(counts.items())) or "0 observed"


def render(builds: list[Build]) -> str:
    lines = ["# Daily-driver evidence", "",
             "| Build reference | Binary ID | Active days | Root routes | Turns admitted | Reloads / carries | Recovered / abandoned |",
             "| --- | --- | ---: | --- | ---: | --- | --- |"]
    for build in builds:
        routes = ", ".join(build.routes(False)) or "none observed"
        lines.append(f"| {build.build_id} | {build.binary_id or 'unknown'} | {len(build.days)} | {routes} | "
                     f"{len(build.turns) if build.turns else 'n/a'} | "
                     f"{build.reloads} / {build.carries} | {build.recovered} / {build.abandoned} |")
    lines += ["", "| Build | Provider/model | Session bindings |",
              "| --- | --- | --- |"]
    for build in builds:
        roots, children = build.routes(False), build.routes(True)
        for route in sorted(roots.keys() | children.keys()):
            lines.append(f"| {build.build_id} | {route} | root={roots.get(route, 0)}, child={children.get(route, 0)} |")
    lines += ["", "| Build | WARN/ERROR by target (excluding capture) | Capture noise | Retries by cause |",
              "| --- | --- | --- | --- |"]
    for build in builds:
        lines.append(f"| {build.build_id} | {counts_text(build.problems)} | "
                     f"{counts_text(build.capture)} | {counts_text(build.retries)} |")
    lines += ["", "Counts are observed log evidence, not canonical totals. Active days exclude capture-only noise.",
              "Build attribution prefers plak.start binary IDs, then chronological reload/version boundaries.",
              "Snapshot binaries still installed are hashed read-only for their 12-hex binary ID; missing snapshots",
              "and pre-boundary use stay unknown. Concurrent old processes cannot be separated without startup IDs,",
              "and maintainer directory hashes identify three-executable snapshots, not plak binary IDs.",
              "Root routes are",
              "provider/model pairs, not proof of effort, protocol or composition. Bindings deduplicate",
              "session IDs within each build/route; repeated span lines are not additional sessions.",
              "Turns are n/a without admitted lifecycle records; absent recovery/carry records do not prove zero.",
              "Only registered provider/model names, stable retry causes, target names, build hashes and counts",
              "are printed. No session IDs, prompts, paths, request details, credentials or environment are printed."]
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--since", type=date.fromisoformat, help="inclusive ISO date (YYYY-MM-DD)")
    parser.add_argument("--log", type=Path, default=Path.home() / ".plak" / "logs" / "plak.log")
    args = parser.parse_args(argv)
    try:
        with args.log.open(errors="replace") as source:
            report = summarize(source, args.since.isoformat() if args.since else None)
    except OSError:
        print("daily-driver: log unavailable (no path or private details printed)", file=sys.stderr)
        return 2
    print(render(report), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
