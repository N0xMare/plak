#!/usr/bin/env python3
"""Emit process-isolated glibc allocation evidence for Plakous perf probes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any, Iterable


SCHEMA = "plakous.perf-allocations.v1"
MEMUSAGE = Path("/usr/bin/memusage")
HOST_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{2,127}\Z")
SUMMARY_RE = re.compile(
    r"Memory usage summary:\s*heap total:\s*([0-9][0-9,]*)\s*,\s*"
    r"heap peak:\s*([0-9][0-9,]*)\s*,\s*stack peak:\s*([0-9][0-9,]*)"
)
CALL_RE = re.compile(
    r"^\s*(malloc|realloc|calloc)\s*\|\s*([0-9][0-9,]*)\s+"
    r"([0-9][0-9,]*)\s+([0-9][0-9,]*)(?:\s+\([^\n]*\))?\s*$",
    re.MULTILINE,
)
FREE_RE = re.compile(
    r"^\s*free\s*\|\s*([0-9][0-9,]*)\s+([0-9][0-9,]*)\s*$", re.MULTILINE
)
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
WORKLOADS = {
    "core-event-round": ("perf_core", "event-round", (("short", 10), ("medium", 100), ("long", 1_000))),
    "project-round": ("perf_project", "project-round", (("short", 10), ("medium", 100), ("long", 1_000))),
    "family-artifact": ("perf_project", "family-artifact", (("short", 64), ("medium", 65_536), ("long", 1_048_576))),
    "tool-schema": ("perf_project", "tool-schema", (("short", 4), ("medium", 32), ("long", 128))),
}


class ProbeError(RuntimeError):
    """The allocation workload could not preserve its evidence contract."""


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


def command(arguments: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
    return subprocess.run(arguments, check=True, text=True, capture_output=True, **kwargs)


def parse_memusage(stderr: str) -> dict[str, Any]:
    plain = ANSI_RE.sub("", stderr)
    summaries = list(SUMMARY_RE.finditer(plain))
    if len(summaries) != 1:
        raise ProbeError(f"expected exactly one memusage summary, found {len(summaries)}")
    calls: dict[str, dict[str, int]] = {}
    for match in CALL_RE.finditer(plain):
        name = match.group(1)
        if name in calls:
            raise ProbeError(f"duplicate memusage {name} row")
        calls[name] = {
            "calls": _integer(match.group(2)),
            "memory_bytes": _integer(match.group(3)),
            "failed_calls": _integer(match.group(4)),
        }
    frees = list(FREE_RE.finditer(plain))
    if len(frees) != 1:
        raise ProbeError(f"expected exactly one memusage free row, found {len(frees)}")
    calls["free"] = {
        "calls": _integer(frees[0].group(1)),
        "memory_bytes": _integer(frees[0].group(2)),
        "failed_calls": 0,
    }
    expected = {"malloc", "realloc", "calloc", "free"}
    if set(calls) != expected:
        raise ProbeError(f"incomplete memusage call table: {sorted(calls)}")
    if any(row["failed_calls"] for row in calls.values()):
        raise ProbeError("memusage reported failed allocation calls")
    summary = summaries[0]
    return {
        "allocation_calls": sum(calls[name]["calls"] for name in ("malloc", "realloc", "calloc")),
        "heap_total_bytes": _integer(summary.group(1)),
        "heap_peak_bytes": _integer(summary.group(2)),
        "stack_peak_bytes": _integer(summary.group(3)),
        "call_table": calls,
        "summary_text": summary.group(0),
    }


def _integer(value: str) -> int:
    return int(value.replace(",", ""))


def stable_host_id(environment: dict[str, str]) -> str:
    value = environment.get("PLAKOUS_PERF_HOST_ID", "")
    rejected = {"localhost", "unknown", "host", "stable-host-id", "<stable-host-id>"}
    if not HOST_RE.fullmatch(value) or value.lower() in rejected:
        raise ProbeError("PLAKOUS_PERF_HOST_ID must be a stable, non-placeholder host identifier")
    return value


def require_platform() -> None:
    if platform.system() != "Linux":
        raise ProbeError("allocation evidence requires Linux")
    try:
        libc = os.confstr("CS_GNU_LIBC_VERSION")
    except (AttributeError, OSError, ValueError):
        libc = None
    if not libc or not libc.startswith("glibc "):
        raise ProbeError("allocation evidence requires glibc")
    try:
        mode = MEMUSAGE.stat().st_mode
    except FileNotFoundError as error:
        raise ProbeError("/usr/bin/memusage is required") from error
    if not stat.S_ISREG(mode) or not os.access(MEMUSAGE, os.X_OK):
        raise ProbeError("/usr/bin/memusage must be a regular executable")


def discover_benchmark(root: Path, name: str) -> tuple[Path, list[str]]:
    arguments = [
        "cargo", "bench", "--locked", "-p", "plakous", "--bench", name,
        "--no-run", "--message-format=json",
    ]
    result = command(arguments, cwd=root)
    matches: list[Path] = []
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = message.get("target", {})
        if (
            message.get("reason") == "compiler-artifact"
            and target.get("name") == name
            and "bench" in target.get("kind", [])
            and isinstance(message.get("executable"), str)
        ):
            matches.append(Path(message["executable"]))
    unique = list(dict.fromkeys(path.resolve() for path in matches))
    if len(unique) != 1 or not unique[0].is_file():
        raise ProbeError(f"cargo did not identify exactly one {name} bench executable")
    return unique[0], arguments


def source_binding(root: Path) -> dict[str, Any]:
    listed = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        check=True,
        capture_output=True,
    ).stdout.split(b"\0")
    paths = sorted(item for item in listed if item)
    digest = hashlib.sha256()
    for encoded in paths:
        path = root / os.fsdecode(encoded)
        before = path.lstat()
        if not stat.S_ISREG(before.st_mode):
            raise ProbeError(f"source closure requires regular files: {path}")
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            opened = os.fstat(descriptor)
            identity = lambda value: (
                value.st_dev,
                value.st_ino,
                value.st_mode,
                value.st_size,
                value.st_mtime_ns,
            )
            if identity(before) != identity(opened):
                raise ProbeError(f"source changed before hashing: {path}")
            chunks = []
            while chunk := os.read(descriptor, 1024 * 1024):
                chunks.append(chunk)
            payload = b"".join(chunks)
            after = os.fstat(descriptor)
        finally:
            os.close(descriptor)
        if identity(before) != identity(after) or identity(path.lstat()) != identity(before):
            raise ProbeError(f"source changed while hashing: {path}")
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return {
        "identity": f"sha256:{digest.hexdigest()}",
        "scope": "tracked-plus-non-ignored-untracked-regular-files",
        "file_count": len(paths),
    }


def snapshot_executable(source: Path, destination: Path) -> dict[str, Any]:
    source = source.absolute()
    try:
        descriptor = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except OSError as error:
        raise ProbeError(f"cannot open benchmark executable: {source}") from error
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_mode & 0o111 == 0:
            raise ProbeError(f"benchmark is not a regular executable: {source}")
        with os.fdopen(os.dup(descriptor), "rb") as input_stream, destination.open(
            "xb"
        ) as output_stream:
            shutil.copyfileobj(input_stream, output_stream, 1024 * 1024)
        destination.chmod(0o500)
        after = os.fstat(descriptor)
        identity = lambda value: (
            value.st_dev,
            value.st_ino,
            value.st_mode,
            value.st_size,
            value.st_mtime_ns,
        )
        if identity(before) != identity(after):
            raise ProbeError(f"benchmark changed while snapshotting: {source}")
    finally:
        os.close(descriptor)
    snapshot_hash = sha256(destination)
    return {"supplied_or_discovered_path": str(source), "sha256": snapshot_hash}


def probe_arguments(binary: Path, operation: str, scale: int, iterations: int) -> list[str]:
    return [str(binary), "--isolated-probe", operation, str(scale), str(iterations)]


def measure(binary: Path, workload: str, benchmark: str, operation: str, scenario: str, scale: int, iterations: int,
            pair_id: str, sample_id: str) -> dict[str, Any]:
    probe = probe_arguments(binary, operation, scale, iterations)
    arguments = [str(MEMUSAGE), "--", *probe]
    result = subprocess.run(arguments, text=True, capture_output=True, check=False, env={})
    if result.returncode != 0:
        raise ProbeError(f"{benchmark} allocation probe exited {result.returncode}: {result.stderr[-500:]}")
    try:
        confirmations = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    except json.JSONDecodeError as error:
        raise ProbeError(f"{benchmark} allocation probe emitted invalid JSON") from error
    if len(confirmations) != 1:
        raise ProbeError(f"{benchmark} allocation probe must emit exactly one confirmation")
    confirmation = confirmations[0]
    expected_scale_key = "history_turns" if benchmark == "perf_core" else "scale"
    expected_confirmation = {
        "schema": "plakous.perf-isolated-workload.v1",
        "bench": benchmark,
        "operation": operation,
        expected_scale_key: scale,
        "iterations": iterations,
        "completed_iterations": iterations,
    }
    if any(confirmation.get(key) != value for key, value in expected_confirmation.items()):
        raise ProbeError(f"{benchmark} allocation probe confirmation does not match request")
    metrics = parse_memusage(result.stderr)
    row = {
        "schema": SCHEMA,
        "kind": "raw-sample",
        "sample_id": sample_id,
        "pair_id": pair_id,
        "workload": workload,
        "benchmark": benchmark,
        "operation": operation,
        "scenario": scenario,
        "scale": scale,
        "iteration_count": iterations,
        "pair_role": "zero-iteration-startup" if iterations == 0 else "measured-iterations",
        "scope": "startup-inclusive process total from one isolated benchmark invocation",
        "command": ["/usr/bin/memusage", "--", binary.name, *probe[1:]],
        "metrics": metrics,
        "stderr_sha256": f"sha256:{hashlib.sha256(result.stderr.encode()).hexdigest()}",
        "probe_confirmation": confirmation,
    }
    validate_row(row)
    return row


def delta_row(zero: dict[str, Any], measured: dict[str, Any]) -> dict[str, Any]:
    count = measured["iteration_count"]
    if count <= 0 or zero["iteration_count"] != 0:
        raise ProbeError("derived delta requires a zero/N pair")
    delta = {
        key: measured["metrics"][key] - zero["metrics"][key]
        for key in ("allocation_calls", "heap_total_bytes", "heap_peak_bytes")
    }
    row = {
        "schema": SCHEMA,
        "kind": "derived-delta",
        "pair_id": measured["pair_id"],
        "workload": measured["workload"],
        "benchmark": measured["benchmark"],
        "operation": measured["operation"],
        "scenario": measured["scenario"],
        "scale": measured["scale"],
        "iteration_count": count,
        "zero_sample_id": zero["sample_id"],
        "measured_sample_id": measured["sample_id"],
        "derivation": "measured startup-inclusive total minus paired zero-iteration startup-inclusive total",
        "deltas": delta,
        "per_iteration": {key: value / count for key, value in delta.items()},
        "qualification": "host-qualified-unratified-derived-observation",
        "peak_caveat": "heap peak is non-additive; its signed difference is descriptive, not incremental retained memory",
    }
    validate_row(row)
    return row


def _exact(value: dict[str, Any], keys: Iterable[str], label: str) -> None:
    expected = set(keys)
    if set(value) != expected:
        raise ProbeError(f"invalid {label} keys: expected {sorted(expected)}, got {sorted(value)}")


def validate_row(row: dict[str, Any]) -> None:
    if row.get("schema") != SCHEMA:
        raise ProbeError("invalid allocation schema")
    kind = row.get("kind")
    if kind == "metadata":
        _exact(row, ("schema", "kind", "qualification", "host", "measurement", "plan", "bindings"), kind)
        _exact(row["qualification"], ("status", "portable_numeric_baseline", "raw_totals"), "qualification")
        _exact(row["host"], ("id", "os", "arch", "kernel", "glibc"), "host")
        _exact(row["measurement"], ("tool", "process_isolation", "delta_method", "failed_allocations"), "measurement")
        _exact(row["bindings"], ("source", "toolchain", "memusage", "binaries", "builds"), "bindings")
    elif kind == "raw-sample":
        _exact(row, ("schema", "kind", "sample_id", "pair_id", "workload", "benchmark", "operation", "scenario", "scale", "iteration_count", "pair_role", "scope", "command", "metrics", "stderr_sha256", "probe_confirmation"), kind)
        _exact(row["metrics"], ("allocation_calls", "heap_total_bytes", "heap_peak_bytes", "stack_peak_bytes", "call_table", "summary_text"), "metrics")
        if row["scenario"] not in {"short", "medium", "long"} or row["iteration_count"] < 0:
            raise ProbeError("invalid raw sample scenario or iteration count")
    elif kind == "derived-delta":
        _exact(row, ("schema", "kind", "pair_id", "workload", "benchmark", "operation", "scenario", "scale", "iteration_count", "zero_sample_id", "measured_sample_id", "derivation", "deltas", "per_iteration", "qualification", "peak_caveat"), kind)
        keys = ("allocation_calls", "heap_total_bytes", "heap_peak_bytes")
        _exact(row["deltas"], keys, "deltas")
        _exact(row["per_iteration"], keys, "per_iteration")
    else:
        raise ProbeError(f"unknown allocation row kind: {kind!r}")
    json.dumps(row, allow_nan=False)


def metadata(root: Path, host_id: str, binaries: dict[str, Any], builds: dict[str, Any], iterations: int, samples: int) -> dict[str, Any]:
    rustc = Path(shutil.which("rustc") or "")
    cargo = Path(shutil.which("cargo") or "")
    if not rustc.is_file() or not cargo.is_file():
        raise ProbeError("cargo and rustc are required for toolchain binding")
    row = {
        "schema": SCHEMA,
        "kind": "metadata",
        "qualification": {
            "status": "host-qualified-unratified",
            "portable_numeric_baseline": False,
            "raw_totals": "startup-inclusive",
        },
        "host": {
            "id": host_id,
            "os": platform.system(),
            "arch": platform.machine(),
            "kernel": platform.release(),
            "glibc": os.confstr("CS_GNU_LIBC_VERSION"),
        },
        "measurement": {
            "tool": "/usr/bin/memusage",
            "process_isolation": "one new process per raw zero/N sample",
            "delta_method": "paired N-iteration total minus zero-iteration total at identical history size",
            "failed_allocations": "rejected",
        },
        "plan": {
            "workloads": {name: {"benchmark": bench, "operation": operation, "scenarios": [{"name": label, "scale": scale} for label, scale in scenarios]} for name, (bench, operation, scenarios) in WORKLOADS.items()},
            "iterations_per_measured_process": iterations,
            "pairs_per_scenario": samples,
        },
        "bindings": {
            "source": source_binding(root),
            "toolchain": {
                "rustc": {"version": command([str(rustc), "-Vv"]).stdout.strip(), "sha256": sha256(rustc.resolve())},
                "cargo": {"version": command([str(cargo), "-V"]).stdout.strip(), "sha256": sha256(cargo.resolve())},
            },
            "memusage": {"path": str(MEMUSAGE), "sha256": sha256(MEMUSAGE)},
            "binaries": binaries,
            "builds": builds,
        },
    }
    validate_row(row)
    return row


def parse_args(arguments: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--perf-core", type=Path)
    parser.add_argument("--perf-project", type=Path)
    parser.add_argument("--iterations", type=int, default=8)
    parser.add_argument("--samples", type=int, default=3)
    values = parser.parse_args(arguments)
    if not 1 <= values.iterations <= 10_000 or not 1 <= values.samples <= 100:
        parser.error("iterations must be 1..10000 and samples must be 1..100")
    return values


def run(arguments: list[str] | None = None) -> list[dict[str, Any]]:
    args = parse_args(arguments)
    require_platform()
    host_id = stable_host_id(os.environ)
    root = Path(__file__).resolve().parents[2]
    supplied = {"perf_core": args.perf_core, "perf_project": args.perf_project}
    discovered: dict[str, Path] = {}
    builds: dict[str, Any] = {}
    for name, path in supplied.items():
        if path is None:
            discovered[name], build = discover_benchmark(root, name)
            builds[name] = {"authority": "cargo-bench-no-run-json", "command": build}
        else:
            discovered[name] = path
            builds[name] = {"authority": "caller-supplied-prebuilt", "command": None}
    rows: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="plakous-perf-allocations-") as temporary:
        directory = Path(temporary)
        snapshots: dict[str, Path] = {}
        bindings: dict[str, Any] = {}
        for name, path in discovered.items():
            snapshot = directory / name
            bindings[name] = snapshot_executable(path, snapshot)
            snapshots[name] = snapshot
        rows.append(metadata(root, host_id, bindings, builds, args.iterations, args.samples))
        for workload, (benchmark, operation, scenarios) in WORKLOADS.items():
            for scenario, history in scenarios:
                for sample in range(args.samples):
                    pair_id = f"{workload}-{scenario}-{sample}-{uuid.uuid4().hex}"
                    zero = measure(snapshots[benchmark], workload, benchmark, operation, scenario, history, 0, pair_id, f"{pair_id}-zero")
                    measured = measure(snapshots[benchmark], workload, benchmark, operation, scenario, history, args.iterations, pair_id, f"{pair_id}-n")
                    rows.extend((zero, measured, delta_row(zero, measured)))
    return rows


def main() -> int:
    try:
        for row in run():
            print(json.dumps(row, sort_keys=True, separators=(",", ":"), allow_nan=False))
    except (ProbeError, OSError, subprocess.SubprocessError) as error:
        print(f"perf_allocations: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
