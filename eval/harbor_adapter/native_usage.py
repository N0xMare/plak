"""Content-free canonical usage from frozen vendor artifacts.

The extractor reads only harvested files and never writes the source. It does
not parse transcript content beyond typed usage events or ATIF metric blocks.
An absent bucket stays unknown unless the pinned native event explicitly
supplied zero, or Harbor's pinned OpenCode converter omitted a zero metric.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from harbor_adapter.usage import parse_pmux_json, parse_pmux_jsonl, UsageError


def _count(value: object) -> int | None:
    return value if type(value) is int and value >= 0 else None


def _digest(path: Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def _object(value: object) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


@dataclass(frozen=True)
class NativeUsage:
    harness: str
    input_tokens: int | None
    cache_read_tokens: int | None
    cache_write_tokens: int | None
    output_tokens: int | None
    reasoning_tokens: int | None
    status: str
    missing_buckets: tuple[str, ...]
    source: str | None
    source_sha256: str | None
    schema: str
    usage_records: int

    def as_dict(self) -> dict[str, Any]:
        return {
            "schemaVersion": 1, "harness": self.harness,
            "input_tokens": self.input_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "output_tokens": self.output_tokens,
            "reasoning_tokens": self.reasoning_tokens,
            "status": self.status, "missing_buckets": list(self.missing_buckets),
            "source": self.source, "source_sha256": self.source_sha256,
            "source_schema": self.schema, "usage_records": self.usage_records,
        }


def _make(harness: str, counts: dict[str, int | None], path: Path | None,
          schema: str, records: int, *, partial: bool = False) -> NativeUsage:
    required = ("input", "cache_read", "cache_write", "output")
    missing = tuple(name for name in required if counts.get(name) is None)
    output, reasoning = counts.get("output"), counts.get("reasoning")
    if output is not None and reasoning is not None and reasoning > output:
        partial = True
    status = "missing" if records == 0 else "partial" if missing or partial else "complete"
    return NativeUsage(
        harness, counts.get("input"), counts.get("cache_read"),
        counts.get("cache_write"), counts.get("output"), counts.get("reasoning"),
        status, missing, str(path) if path is not None and path.is_file() else None,
        _digest(path) if path is not None else None, schema, records,
    )


def _sum_records(rows: list[dict[str, int | None]], key: str) -> int | None:
    if not rows or any(row.get(key) is None for row in rows):
        return None
    return sum(row[key] for row in rows if row[key] is not None)


def _pi(agent_dir: Path) -> NativeUsage:
    path = agent_dir / "pi.txt"
    rows: list[dict[str, int | None]] = []
    partial = False
    terminal = False
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        lines = []
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            if line.lstrip().startswith("{"):
                partial = True
            continue
        if not isinstance(event, dict):
            continue
        if event.get("type") == "agent_end":
            terminal = True
        if event.get("type") != "message_end":
            continue
        message = _object(event.get("message"))
        if message.get("role") != "assistant":
            continue
        usage = _object(message.get("usage"))
        row = {"remainder": _count(usage.get("input")),
               "cache_read": _count(usage.get("cacheRead")),
               "cache_write": _count(usage.get("cacheWrite")),
               "output": _count(usage.get("output")),
               "reasoning": _count(usage.get("reasoning"))}
        if any(row[k] is None for k in ("remainder", "cache_read", "cache_write", "output")):
            partial = True
        rows.append(row)
    remainder = _sum_records(rows, "remainder")
    read = _sum_records(rows, "cache_read")
    write = _sum_records(rows, "cache_write")
    counts = {"input": remainder + read + write if None not in (remainder, read, write) else None,
              "cache_read": read, "cache_write": write,
              "output": _sum_records(rows, "output"),
              "reasoning": _sum_records(rows, "reasoning")}
    return _make("pi", counts, path, "pi.message_end.usage", len(rows),
                 partial=partial or not terminal)


def _opencode(agent_dir: Path) -> NativeUsage:
    path = agent_dir / "opencode.txt"
    rows: dict[tuple[str, str], dict[str, int | None]] = {}
    partial = False
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        lines = []
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            if line.lstrip().startswith("{"):
                partial = True
            continue
        if not isinstance(event, dict) or event.get("type") != "step_finish":
            continue
        part = _object(event.get("part"))
        identity = (str(part.get("sessionID") or ""), str(part.get("id") or ""))
        if not all(identity):
            partial = True
            continue
        tokens = _object(part.get("tokens"))
        cache = _object(tokens.get("cache"))
        # OpenCode 1.18.30 stores visible output separately from reasoning;
        # its provider output and own pricing include both. A missing
        # reasoning field cannot certify the canonical completion total.
        visible_output = _count(tokens.get("output"))
        reasoning = _count(tokens.get("reasoning"))
        row = {"remainder": _count(tokens.get("input")),
               "cache_read": _count(cache.get("read")),
               "cache_write": _count(cache.get("write")),
               "output": visible_output + reasoning
               if visible_output is not None and reasoning is not None else None,
               "reasoning": reasoning}
        if any(row[k] is None for k in ("remainder", "cache_read", "cache_write", "output")):
            partial = True
        if identity in rows and rows[identity] != row:
            partial = True
        rows[identity] = row
    values = list(rows.values())
    remainder = _sum_records(values, "remainder")
    read = _sum_records(values, "cache_read")
    write = _sum_records(values, "cache_write")
    counts = {"input": remainder + read + write if None not in (remainder, read, write) else None,
              "cache_read": read, "cache_write": write,
              "output": _sum_records(values, "output"),
              "reasoning": _sum_records(values, "reasoning")}
    return _make("opencode", counts, path, "opencode.step_finish.part.tokens.text_plus_reasoning",
                 len(values), partial=partial)


def _trajectory(agent_dir: Path, harness: str) -> NativeUsage:
    path = agent_dir / "trajectory.json"
    try:
        body = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        body = {}
    fm = _object(_object(body).get("final_metrics"))
    valid_schema = _object(body).get("schema_version") == "ATIF-v1.7"
    extra = _object(fm.get("extra"))
    write_key = ("total_cache_creation_input_tokens" if harness == "grok-build"
                 else "total_cache_write_input_tokens")
    reason_key = "total_reasoning_tokens" if harness == "grok-build" else "reasoning_output_tokens"
    counts = {"input": _count(fm.get("total_prompt_tokens")),
              "cache_read": _count(fm.get("total_cached_tokens")),
              "cache_write": _count(extra.get(write_key)),
              "output": _count(fm.get("total_completion_tokens")),
              "reasoning": _count(extra.get(reason_key))}
    # ATIF's optional cached field may be omitted for a real zero. The pinned
    # converter preserves it in `extra` for Grok, but Codex has no such proof.
    if harness == "grok-build" and counts["cache_read"] is None:
        counts["cache_read"] = _count(extra.get("total_cache_read_input_tokens"))
    return _make(harness, counts, path, "ATIF-v1.7.final_metrics" if valid_schema else "unknown.ATIF",
                 1 if fm else 0, partial=not valid_schema)


def extract_agent_usage(agent_dir: Path, harness: str) -> NativeUsage:
    """Read one agent archive. Unknown fields remain None, including absent zeroes."""
    if harness == "pi":
        return _pi(agent_dir)
    if harness == "pmux":
        return _pmux(agent_dir)
    if harness == "opencode":
        native = _opencode(agent_dir)
        if native.usage_records:
            return native
        return _opencode_trajectory(agent_dir)
    if harness in ("codex", "grok-build"):
        return _trajectory(agent_dir, harness)
    return _make(harness, {}, None, "unknown", 0)


def _pmux(agent_dir: Path) -> NativeUsage:
    structured = agent_dir / "pmux-stdout.txt"
    try:
        totals = parse_pmux_json(structured.read_text(encoding="utf-8"))
    except (OSError, UsageError, ValueError):
        totals = parse_pmux_json("{}")
    source = structured
    schema = "pmux.StatefulResult.usage.combined"
    fallback_used = False
    if not totals.resolved:
        transcripts = sorted((agent_dir / "claude-transcripts").glob("*.jsonl"))
        # One raw transcript can be bound by one SHA. Several transcripts may
        # repeat the same message IDs; without a session join, keep unresolved.
        if len(transcripts) == 1:
            try:
                candidate = parse_pmux_jsonl(transcripts[0].read_text(encoding="utf-8"))
            except (OSError, UsageError, ValueError):
                candidate = totals
            if candidate.resolved:
                totals = candidate
                source = transcripts[0]
                schema = "claude.assistant.message.usage.jsonl"
                fallback_used = True
    counts = {"input": totals.n_input_tokens, "cache_read": totals.n_cache_tokens,
              "cache_write": totals.n_cache_write_tokens,
              "output": totals.n_output_tokens,
              "reasoning": totals.n_reasoning_tokens}
    return _make("pmux", counts, source, schema,
                 1 if totals.n_input_tokens is not None or totals.n_output_tokens is not None else 0,
                 partial=not totals.resolved or fallback_used)


def _opencode_trajectory(agent_dir: Path) -> NativeUsage:
    path = agent_dir / "trajectory.json"
    try:
        body = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        body = {}
    fm = _object(_object(body).get("final_metrics"))
    valid_schema = _object(body).get("schema_version") == "ATIF-v1.7"
    steps = _object(body).get("steps")
    metrics = [_object(_object(step).get("metrics")) for step in steps] if isinstance(steps, list) else []
    metrics = [item for item in metrics if item]
    writes = [_count(_object(item.get("extra")).get("cache_write_tokens")) for item in metrics]
    # The pinned Harbor converter omits cache_write_tokens only when its value
    # is zero, for steps with a metrics object. A write-only step has no metrics
    # in that converter, so trajectory-only evidence remains partial.
    write = sum(value or 0 for value in writes) if metrics else None
    # The pinned Harbor converter sums OpenCode's visible tokens.output into
    # total_completion_tokens and only preserves nonzero reasoning on some
    # step metrics. It cannot reconstruct a complete canonical output total.
    counts = {"input": _count(fm.get("total_prompt_tokens")),
              "cache_read": _count(fm.get("total_cached_tokens")),
              "cache_write": write,
              "output": None,
              "reasoning": None}
    if counts["input"] is not None and write is not None:
        counts["input"] += write
    return _make("opencode", counts, path,
                 "ATIF-v1.7.steps.metrics" if valid_schema else "unknown.ATIF",
                 len(metrics), partial=True)
