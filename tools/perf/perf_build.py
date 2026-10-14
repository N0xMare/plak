#!/usr/bin/env python3
"""Locked, offline, isolated JSONL workload for the named build matrix."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

try:
    import resource
except ImportError:  # pragma: no cover - Python has no resource module on Windows.
    resource = None  # type: ignore[assignment]


ROOT = Path(__file__).resolve().parents[2]
SCHEMA = "plakous.perf-build.v9"
FIXTURE = {"name": "isolated-build-closure-matrix", "revision": 9}
SUPPORTED_PUBLIC_BUILD_PROFILES = {
    "minimal": ("plakous", ("--lib",), ()),
    "subscriptions": ("plakous", ("--lib",), ("native-subscriptions",)),
    "sessions-sqlite": ("plakous", ("--lib",), ("sessions-sqlite",)),
    "plak": ("plak", ("--bin", "plak"), ()),
    "agent": ("plakous-agent", ("--lib",), ()),
    "tools": ("plakous-tools", ("--lib",), ()),
}
# These feature closures expose transitional whole-integration facade aliases.
# Measuring them does not promote those aliases into the supported inventory.
TRANSITIONAL_FACADE_BUILD_PROFILES = {
    "openai-platform": ("plakous", ("--lib",), ("openai",)),
    "xai-platform": ("plakous", ("--lib",), ("xai",)),
    "openrouter": ("plakous", ("--lib",), ("openrouter",)),
    "anthropic": ("plakous", ("--lib",), ("anthropic",)),
    "all-providers": (
        "plakous",
        ("--lib",),
        ("anthropic", "native-subscriptions", "openai", "openrouter", "xai"),
    ),
    "full": ("plakous", ("--lib",), ("full",)),
}
# The public facade has one combined native-subscriptions feature. These two
# benchmark-only component closures measure one provider integration at a time;
# neither claims a supported facade configuration.
COMPONENT_BUILD_PROFILES = {
    "codex-components": (
        "plakous",
        ("--lib",),
        ("openai", "plakous-openai/native-auth"),
    ),
    "supergrok-components": (
        "plakous",
        ("--lib",),
        ("plakous-xai/native-auth", "xai"),
    ),
}
# These direct package closures are evidence about the current package graph,
# not supported direct-consumer configurations. They intentionally pin the
# retained implementation package boundaries named by the crate-boundary handoff.
INTERNAL_TOPOLOGY_BUILD_PROFILES = {
    **COMPONENT_BUILD_PROFILES,
    "attempt": ("plakous-attempt", ("--lib",), ()),
    "responses-kernel": ("plakous-responses", ("--lib",), ()),
    "responses-host": ("plakous-responses-dialect", ("--lib",), ()),
    "messages": ("plakous-messages", ("--lib",), ()),
}
PRODUCT_TOPOLOGY_BUILD_PROFILES = {
    "plak-lib": ("plak", ("--lib",), ()),
}
BUILD_PROFILES = {
    **SUPPORTED_PUBLIC_BUILD_PROFILES,
    **TRANSITIONAL_FACADE_BUILD_PROFILES,
    **INTERNAL_TOPOLOGY_BUILD_PROFILES,
    **PRODUCT_TOPOLOGY_BUILD_PROFILES,
}
CARGO_OPERATION_SEMANTICS = {
    "check": {
        "workload": "cargo-check",
        "selected_target_linked": False,
        "scope": "type-check and compile dependencies without linking the selected package target",
    },
    "build": {
        "workload": "cargo-build-link",
        "selected_target_linked": True,
        "scope": "compile and link the selected package target",
    },
}
TARGET_UNIT_PROFILE_SETTINGS: dict[str, dict[str, object]] = {
    "dev": {
        "opt-level": 0,
        "debug": 2,
        "debug-assertions": True,
        "overflow-checks": True,
        "lto": False,
        "codegen-units": 256,
        "panic": "unwind",
        "incremental": True,
        "rpath": False,
        "strip": False,
        "split-debuginfo": "off",
    },
    "release": {
        "opt-level": 3,
        "debug": 0,
        "debug-assertions": False,
        "overflow-checks": False,
        "lto": "thin",
        "codegen-units": 1,
        "panic": "unwind",
        "incremental": False,
        "rpath": False,
        "strip": "debuginfo",
        "split-debuginfo": "off",
    },
}
RUSTFLAGS: tuple[str, ...] = ()
RUSTFLAGS_SEPARATOR = "\x1f"
TOOL_TIMEOUT_SECONDS = 60
TOOL_STDOUT_LIMIT_BYTES = 4 * 1024 * 1024
OUTPUT_TAIL_BYTES = 8192
HELPER_GRACE_SECONDS = 15
REMOVED_ENVIRONMENT_PATTERNS = [
    "CARGO_PROFILE_*",
    "CARGO_TARGET_*_RUSTFLAGS",
]


def sha256_bytes(value: bytes) -> str:
    """Return a tagged SHA-256 identity."""
    return f"sha256:{hashlib.sha256(value).hexdigest()}"


def file_identity(path: Path) -> dict[str, object]:
    """Return a repository-relative content identity for one input file."""
    value = path.read_bytes()
    return {
        "path": path.relative_to(ROOT).as_posix(),
        "bytes": len(value),
        "sha256": sha256_bytes(value),
    }


def terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    """Kill a bounded command and its descendants where the host supports it."""
    if hasattr(os, "killpg"):
        try:
            os.killpg(process.pid, signal.SIGKILL)
            return
        except ProcessLookupError:
            return
    process.kill()


def file_tail(stream: Any, limit: int) -> bytes:
    """Read at most the final limit bytes from a seekable temporary file."""
    size = stream.tell()
    stream.seek(max(0, size - limit))
    return stream.read(limit)


def run_captured(
    command: list[str],
    *,
    cwd: Path,
    timeout_seconds: int,
    env: dict[str, str] | None = None,
    stdout_limit_bytes: int = TOOL_STDOUT_LIMIT_BYTES,
    tail_bytes: int = OUTPUT_TAIL_BYTES,
) -> dict[str, object]:
    """Run a command with a timeout and bounded retained output."""
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        started = time.perf_counter_ns()
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=env,
                stdout=stdout,
                stderr=stderr,
                start_new_session=True,
            )
        except OSError as error:
            return {
                "returncode": 127,
                "timed_out": False,
                "elapsed_ns": time.perf_counter_ns() - started,
                "stdout": b"",
                "stdout_complete": True,
                "stderr_tail": str(error).encode(),
            }
        timed_out = False
        try:
            returncode = process.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
            terminate_process_group(process)
            returncode = process.wait()
        elapsed_ns = time.perf_counter_ns() - started
        stdout_size = stdout.tell()
        stderr_tail = file_tail(stderr, tail_bytes)
        if stdout_size <= stdout_limit_bytes:
            stdout.seek(0)
            stdout_value = stdout.read(stdout_limit_bytes)
            stdout_complete = True
        else:
            stdout_value = file_tail(stdout, tail_bytes)
            stdout_complete = False
        return {
            "returncode": returncode,
            "timed_out": timed_out,
            "elapsed_ns": elapsed_ns,
            "stdout": stdout_value,
            "stdout_complete": stdout_complete,
            "stdout_bytes": stdout_size,
            "stderr_tail": stderr_tail,
        }


def checked_stdout(command: list[str], *, env: dict[str, str] | None = None) -> bytes:
    """Return complete bounded stdout or fail with a bounded diagnostic."""
    result = run_captured(
        command,
        cwd=ROOT,
        timeout_seconds=TOOL_TIMEOUT_SECONDS,
        env=env,
    )
    if result["timed_out"]:
        raise RuntimeError(f"timed out after {TOOL_TIMEOUT_SECONDS}s: {command!r}")
    if result["returncode"] != 0:
        tail = bytes(result["stderr_tail"]).decode(errors="replace")
        raise RuntimeError(
            f"command failed ({result['returncode']}): {command!r}: {tail}"
        )
    if not result["stdout_complete"]:
        raise RuntimeError(
            f"command stdout exceeded {TOOL_STDOUT_LIMIT_BYTES} bytes: {command!r}"
        )
    return bytes(result["stdout"])


def checked_stdout_sha256(command: list[str]) -> str:
    """Hash arbitrary bounded-runtime stdout without retaining it in memory."""
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            returncode = process.wait(timeout=TOOL_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            terminate_process_group(process)
            process.wait()
            raise RuntimeError(
                f"timed out after {TOOL_TIMEOUT_SECONDS}s: {command!r}"
            ) from None
        if returncode != 0:
            tail = file_tail(stderr, OUTPUT_TAIL_BYTES).decode(errors="replace")
            raise RuntimeError(f"command failed ({returncode}): {command!r}: {tail}")
        digest = hashlib.sha256()
        stdout.seek(0)
        while chunk := stdout.read(1024 * 1024):
            digest.update(chunk)
        return f"sha256:{digest.hexdigest()}"


def executable(name: str) -> str:
    """Resolve the exact executable used by the workload."""
    resolved = shutil.which(name)
    if resolved is None:
        raise RuntimeError(f"required executable is unavailable: {name}")
    # Do not dereference rustup proxy symlinks: their basename selects cargo or
    # rustc behavior, while the symlink target itself is the rustup executable.
    return str(Path(resolved).absolute())


def rustc_identity(rustc: str) -> dict[str, object]:
    """Preserve the complete bounded verbose rustc identity."""
    command = [rustc, "-Vv"]
    stdout = checked_stdout(command)
    verbose = stdout.decode(errors="strict").rstrip("\n")
    fields = {
        key.strip(): value.strip()
        for line in verbose.splitlines()
        if ":" in line
        for key, value in (line.split(":", 1),)
    }
    if not fields.get("host") or not fields.get("LLVM version"):
        raise RuntimeError("rustc -Vv did not report both host and LLVM version")
    return {
        "command": command,
        "verbose_version": verbose,
        "stdout_bytes": len(stdout),
        "stdout_limit_bytes": TOOL_STDOUT_LIMIT_BYTES,
        "host": fields["host"],
        "llvm_version": fields["LLVM version"],
    }


def cargo_identity(cargo: str) -> dict[str, object]:
    """Return the complete bounded verbose Cargo identity."""
    command = [cargo, "-Vv"]
    stdout = checked_stdout(command)
    return {
        "command": command,
        "verbose_version": stdout.decode(errors="strict").rstrip("\n"),
        "stdout_bytes": len(stdout),
        "stdout_limit_bytes": TOOL_STDOUT_LIMIT_BYTES,
    }


def cargo_environment(
    rustc: str, cargo_profile: str
) -> tuple[dict[str, str], list[str]]:
    """Return the explicitly controlled Cargo environment and ambient removals."""
    controlled = {
        "CARGO_ENCODED_RUSTFLAGS": RUSTFLAGS_SEPARATOR.join(RUSTFLAGS),
        "CARGO_INCREMENTAL": "1"
        if TARGET_UNIT_PROFILE_SETTINGS[cargo_profile]["incremental"]
        else "0",
        "CARGO_TERM_COLOR": "never",
        "RUSTC": rustc,
        "RUSTC_WORKSPACE_WRAPPER": "",
        "RUSTC_WRAPPER": "",
    }
    removed = [
        "CARGO_BUILD_JOBS",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
    ]
    return controlled, removed


def sanitized_environment(
    controlled: dict[str, str], removed: list[str]
) -> dict[str, str]:
    """Apply the recorded Cargo environment policy to the ambient environment."""
    env = os.environ.copy()
    dynamic_removals = [
        key
        for key in env
        if key.startswith("CARGO_PROFILE_")
        or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS"))
    ]
    for key in (*removed, *dynamic_removals):
        env.pop(key, None)
    env.update(controlled)
    return env


def profile_config_arguments(cargo_profile: str) -> list[str]:
    """Encode every controlled Cargo profile setting as command-line config."""
    arguments: list[str] = []
    for key, value in TARGET_UNIT_PROFILE_SETTINGS[cargo_profile].items():
        arguments.extend(
            ("--config", f"profile.{cargo_profile}.{key}={json.dumps(value)}")
        )
    return arguments


def feature_arguments(features: tuple[str, ...]) -> list[str]:
    """Encode exact requested facade features."""
    return ["--features", ",".join(features)] if features else []


def build_profile_scope(profile: str) -> str:
    """Classify one named closure without implying internal package support."""
    if profile in SUPPORTED_PUBLIC_BUILD_PROFILES:
        return "supported-public"
    if profile in TRANSITIONAL_FACADE_BUILD_PROFILES:
        return "transitional-facade-baseline"
    if profile in INTERNAL_TOPOLOGY_BUILD_PROFILES:
        return "internal-topology-baseline"
    if profile in PRODUCT_TOPOLOGY_BUILD_PROFILES:
        return "product-topology-baseline"
    raise KeyError(profile)


def host_identity(host_id: str) -> dict[str, object]:
    """Bind host-sensitive evidence to a caller-managed stable machine identity."""
    return {
        "host_id": host_id,
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "cpu_count": os.cpu_count(),
    }


def cargo_command(
    cargo: str,
    operation: str,
    cargo_profile: str,
    package: str,
    package_target: tuple[str, ...],
    features: tuple[str, ...],
    target: str,
    target_directory: Path,
    jobs: int,
) -> list[str]:
    """Construct one exact locked, offline, isolated package-target command."""
    return [
        cargo,
        *profile_config_arguments(cargo_profile),
        operation,
        "--locked",
        "--offline",
        "--color",
        "never",
        "--manifest-path",
        "Cargo.toml",
        "--package",
        package,
        *package_target,
        "--no-default-features",
        *feature_arguments(features),
        "--target",
        target,
        "--profile",
        cargo_profile,
        "--jobs",
        str(jobs),
        "--target-dir",
        str(target_directory),
    ]


def closure_provenance(
    cargo: str,
    package: str,
    features: tuple[str, ...],
    target: str,
    env: dict[str, str],
) -> dict[str, object]:
    """Resolve and retain the exact normal/build package-feature closure."""
    command = [
        cargo,
        "tree",
        "--locked",
        "--offline",
        "--color",
        "never",
        "--manifest-path",
        "Cargo.toml",
        "--package",
        package,
        "--no-default-features",
        *feature_arguments(features),
        "--target",
        target,
        "--edges",
        "normal,build",
        "--prefix",
        "none",
        "--format",
        "{p}|{f}",
    ]
    raw = checked_stdout(command, env=env)
    normalized = sorted(
        {
            line.removesuffix(" (*)").replace(str(ROOT), "$WORKSPACE")
            for line in raw.decode(errors="strict").splitlines()
            if line
        }
    )
    encoded = json.dumps(normalized, ensure_ascii=True, separators=(",", ":")).encode()
    root_entries = [line for line in normalized if line.startswith(f"{package} v")]
    if len(root_entries) != 1:
        raise RuntimeError(f"resolved closure has {len(root_entries)} {package} roots")
    return {
        "resolver": "cargo tree",
        "resolver_command": command,
        "edge_kinds": ["normal", "build"],
        "format_revision": 1,
        "workspace_path_token": "$WORKSPACE",
        "entries": normalized,
        "entry_count": len(normalized),
        "entries_sha256": sha256_bytes(encoded),
        "raw_stdout_sha256": sha256_bytes(raw),
        "resolved_root_features": sorted(
            feature
            for feature in root_entries[0].split("|", 1)[1].split(",")
            if feature
        ),
    }


def cargo_package_and_inputs(
    cargo: str, package_name: str, target: str, env: dict[str, str]
) -> tuple[dict[str, object], list[dict[str, object]]]:
    """Return exact selected-package identity and content provenance inputs."""
    command = [
        cargo,
        "metadata",
        "--locked",
        "--offline",
        "--format-version",
        "1",
        "--no-deps",
        "--filter-platform",
        target,
        "--manifest-path",
        "Cargo.toml",
    ]
    document = json.loads(checked_stdout(command, env=env))
    selected = [
        package for package in document["packages"] if package["name"] == package_name
    ]
    if len(selected) != 1:
        raise RuntimeError(
            f"cargo metadata found {len(selected)} {package_name} packages"
        )
    package = selected[0]
    manifest_path = Path(package["manifest_path"]).resolve()
    paths = {
        ROOT / "Cargo.toml",
        ROOT / "Cargo.lock",
        ROOT / "rust-toolchain.toml",
        manifest_path,
    }
    cargo_config = ROOT / ".cargo" / "config.toml"
    if cargo_config.exists():
        paths.add(cargo_config)
    paths.update(
        Path(item["manifest_path"])
        for item in document["packages"]
        if Path(item["manifest_path"]).is_relative_to(ROOT)
    )
    inputs = [file_identity(path) for path in sorted(paths)]
    identity = {
        "name": package["name"],
        "version": package["version"],
        "cargo_package_id": package["id"].replace(str(ROOT), "$WORKSPACE"),
        "manifest_path": manifest_path.relative_to(ROOT).as_posix(),
        "metadata_command": command,
    }
    return identity, inputs


def git_identity() -> dict[str, object]:
    """Return the commit plus current non-ignored source-content identity."""
    git = executable("git")
    commit = checked_stdout([git, "rev-parse", "--verify", "HEAD"]).decode().strip()
    status = checked_stdout(
        [git, "status", "--porcelain=v1", "--untracked-files=normal"]
    )
    return {
        "commit": commit,
        "dirty": bool(status),
        "status_sha256": sha256_bytes(status),
        "tracked_patch_sha256": checked_stdout_sha256(
            [git, "diff", "--binary", "HEAD"]
        ),
        "non_ignored_worktree_content_closure": worktree_content_closure(git),
    }


def worktree_content_closure(git: str) -> dict[str, object]:
    """Hash current tracked and non-ignored untracked source bytes."""
    listing = checked_stdout(
        [git, "ls-files", "-z", "--cached", "--others", "--exclude-standard"]
    )
    framed = bytearray()
    file_count = 0
    content_bytes = 0
    missing_tracked_files = 0
    for raw_path in filter(None, listing.split(b"\0")):
        path = ROOT / os.fsdecode(raw_path)
        framed.extend(len(raw_path).to_bytes(8, "big"))
        framed.extend(raw_path)
        try:
            content = path.read_bytes()
        except FileNotFoundError:
            framed.extend(b"M")
            missing_tracked_files += 1
        else:
            framed.extend(b"F")
            framed.extend(len(content).to_bytes(8, "big"))
            framed.extend(content)
            content_bytes += len(content)
        file_count += 1
    return {
        "identity": sha256_bytes(bytes(framed)),
        "file_count": file_count,
        "content_bytes": content_bytes,
        "missing_tracked_files": missing_tracked_files,
        "rule": "ordered git ls-files cached plus non-ignored untracked paths, framed with each current file content or a missing marker",
    }


def artifact_inventory(target: Path) -> dict[str, object]:
    """Return regular, non-symlink artifact bytes in the isolated target."""
    total = 0
    files = 0
    if target.exists():
        for path in target.rglob("*"):
            if path.is_file() and not path.is_symlink():
                total += path.stat().st_size
                files += 1
    return {"artifact_bytes": total, "artifact_files": files}


def normalized_rss(usage: Any) -> tuple[int | None, str]:
    """Normalize ru_maxrss only where the operating-system unit is documented."""
    system = platform.system()
    if resource is None:
        return None, "resource module unavailable; RSS omitted"
    if system == "Linux":
        return int(usage.ru_maxrss), "Linux native KiB"
    if system == "Darwin":
        return (int(usage.ru_maxrss) + 1023) // 1024, "Darwin bytes rounded up to KiB"
    return None, f"{system} ru_maxrss units not encoded; RSS omitted"


def helper_sample(payload: dict[str, Any]) -> dict[str, object]:
    """Measure exactly one Cargo child from a fresh Python helper process."""
    target = Path(payload["target_directory"])
    existed_before = target.exists()
    expected = payload["expected_target_state"]
    if existed_before != (expected == "present"):
        return {
            "schema": SCHEMA,
            "kind": "sample",
            "sample": payload["sample"],
            "temperature": payload["temperature"],
            "status": 125,
            "error": f"target was {'present' if existed_before else 'absent'}; expected {expected}",
            "target_existed_before": existed_before,
        }
    env = sanitized_environment(
        payload["controlled_environment"], payload["removed_environment"]
    )
    before = resource.getrusage(resource.RUSAGE_CHILDREN) if resource else None
    result = run_captured(
        payload["command"],
        cwd=ROOT,
        timeout_seconds=payload["timeout_seconds"],
        env=env,
        tail_bytes=payload["output_tail_bytes"],
    )
    after = resource.getrusage(resource.RUSAGE_CHILDREN) if resource else None
    rss, rss_normalization = normalized_rss(after) if after else (None, "unavailable")
    return {
        "schema": SCHEMA,
        "kind": "sample",
        "fixture_revision": FIXTURE["revision"],
        "run_identity": payload["run_identity"],
        "baseline_identity": payload["baseline_identity"],
        "sample": payload["sample"],
        "temperature": payload["temperature"],
        "build_profile": payload["build_profile"],
        "build_profile_scope": payload["build_profile_scope"],
        "operation": payload["operation"],
        "operation_semantics": payload["operation_semantics"],
        "cargo_profile": payload["cargo_profile"],
        "target": payload["target"],
        "command": payload["command"],
        "target_directory": str(target),
        "target_existed_before": existed_before,
        "elapsed_ns": result["elapsed_ns"],
        "rusage_children_maxrss_kib": rss,
        "rusage_children_maxrss_normalization": rss_normalization,
        "rusage_children_before_maxrss_native": int(before.ru_maxrss)
        if before
        else None,
        **artifact_inventory(target),
        "status": 124 if result["timed_out"] else result["returncode"],
        "process_returncode": result["returncode"],
        "timed_out": result["timed_out"],
        "stdout_tail": bytes(result["stdout"])[-payload["output_tail_bytes"] :].decode(
            errors="replace"
        ),
        "stdout_complete": result["stdout_complete"],
        "stderr_tail": bytes(result["stderr_tail"]).decode(errors="replace"),
        "output_tail_bytes": payload["output_tail_bytes"],
    }


def invoke_helper(payload: dict[str, object]) -> dict[str, object]:
    """Run one private helper and decode its single bounded JSON result."""
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--_sample-helper",
        json.dumps(payload),
    ]
    result = run_captured(
        command,
        cwd=ROOT,
        timeout_seconds=int(payload["timeout_seconds"]) + HELPER_GRACE_SECONDS,
        stdout_limit_bytes=256 * 1024,
    )
    if (
        result["timed_out"]
        or result["returncode"] != 0
        or not result["stdout_complete"]
    ):
        return {
            "schema": SCHEMA,
            "kind": "sample",
            "sample": payload["sample"],
            "temperature": payload["temperature"],
            "status": 125,
            "error": "sample helper failed or exceeded its bound",
            "helper_timed_out": result["timed_out"],
            "helper_returncode": result["returncode"],
            "helper_stderr_tail": bytes(result["stderr_tail"]).decode(errors="replace"),
        }
    try:
        return json.loads(bytes(result["stdout"]))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        return {
            "schema": SCHEMA,
            "kind": "sample",
            "sample": payload["sample"],
            "temperature": payload["temperature"],
            "status": 125,
            "error": f"invalid sample helper result: {error}",
            "helper_stderr_tail": bytes(result["stderr_tail"]).decode(errors="replace"),
        }


def parse_arguments() -> argparse.Namespace:
    """Parse the public perf-build workload arguments."""
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--build-profile",
        "--facade-feature-profile",
        dest="build_profile",
        choices=sorted(BUILD_PROFILES),
        default="minimal",
    )
    parser.add_argument(
        "--cargo-profile", choices=sorted(TARGET_UNIT_PROFILE_SETTINGS), default="dev"
    )
    parser.add_argument(
        "--operation", choices=tuple(CARGO_OPERATION_SEMANTICS), default="check"
    )
    parser.add_argument("--host-id", default=os.environ.get("PLAKOUS_PERF_HOST_ID"))
    parser.add_argument(
        "--target", help="explicit Rust target triple; defaults to rustc host"
    )
    parser.add_argument("--jobs", type=int, default=max(1, os.cpu_count() or 1))
    parser.add_argument("--warm-samples", type=int, default=2)
    parser.add_argument("--timeout-seconds", type=int, default=900)
    parser.add_argument("--baseline-identity", default="unratified-raw-distribution-v1")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    if not args.host_id:
        parser.error("--host-id or PLAKOUS_PERF_HOST_ID is required")
    if len(args.host_id) > 256 or "\n" in args.host_id or "\r" in args.host_id:
        parser.error("--host-id must be 1-256 characters without newlines")
    if not 0 <= args.warm_samples <= 20:
        parser.error("--warm-samples must be between 0 and 20")
    if not 1 <= args.timeout_seconds <= 3600:
        parser.error("--timeout-seconds must be between 1 and 3600")
    if not 1 <= args.jobs <= 512:
        parser.error("--jobs must be between 1 and 512")
    if (
        not 1 <= len(args.baseline_identity) <= 256
        or "\n" in args.baseline_identity
        or "\r" in args.baseline_identity
    ):
        parser.error("--baseline-identity must be 1-256 characters without newlines")
    if args.target and (
        len(args.target) > 128 or not re.fullmatch(r"[A-Za-z0-9_.-]+", args.target)
    ):
        parser.error("--target must be a target triple of at most 128 safe characters")
    return args


def main() -> int:
    """Emit raw metadata and cold/warm sample JSON Lines."""
    args = parse_arguments()
    cargo = executable("cargo")
    rustc = executable("rustc")
    rustc_data = rustc_identity(rustc)
    target = args.target or rustc_data["host"]
    package_name, package_target, features = BUILD_PROFILES[args.build_profile]
    profile_scope = build_profile_scope(args.build_profile)
    operation_semantics = CARGO_OPERATION_SEMANTICS[args.operation]
    controlled, removed = cargo_environment(rustc, args.cargo_profile)
    env = sanitized_environment(controlled, removed)
    package, provenance_inputs = cargo_package_and_inputs(
        cargo, package_name, target, env
    )
    closure = closure_provenance(cargo, package_name, features, target, env)
    metadata: dict[str, object] = {
        "schema": SCHEMA,
        "kind": "metadata",
        "fixture": FIXTURE,
        "git": git_identity(),
        "baseline": {
            "identity": args.baseline_identity,
            "role": "caller-supplied comparator identity; unratified-raw-distribution-v1 means no ratified comparator is established",
            "numeric_regression_gate": None,
        },
        "host": host_identity(args.host_id),
        "toolchain": {
            "rustc": rustc_data,
            "cargo": cargo_identity(cargo),
            "python": {
                "executable": str(Path(sys.executable).absolute()),
                "implementation": platform.python_implementation(),
                "version": platform.python_version(),
                "full_version": sys.version,
            },
        },
        "cargo": {
            "package": package,
            "operation": args.operation,
            "operation_semantics": operation_semantics,
            "target_selection": list(package_target),
            "target": target,
            "build_profile": args.build_profile,
            "build_profile_scope": profile_scope,
            "requested_features": list(features),
            "default_features": False,
            "resolved_feature_closure": closure,
            "cargo_profile": args.cargo_profile,
            "target_unit_profile_settings": TARGET_UNIT_PROFILE_SETTINGS[
                args.cargo_profile
            ],
            "profile_settings_scope": "explicit profile.<name> settings for target units",
            "host_build_unit_scope": {
                "units": "build scripts, proc macros, and their dependencies",
                "control": "Cargo build-override/default semantics; no profile.<name>.build-override values are forced",
                "limitation": "host-unit profile settings may depend on the recorded Cargo version and Cargo configuration outside the repository",
            },
            "jobs": args.jobs,
            "locked": True,
            "offline": True,
            "provenance_inputs": provenance_inputs,
        },
        "rustflags": {
            "source": "CARGO_ENCODED_RUSTFLAGS",
            "values": list(RUSTFLAGS),
            "encoded": controlled["CARGO_ENCODED_RUSTFLAGS"],
            "separator": "U+001F",
            "ambient_rustflags_removed": True,
        },
        "controlled_environment": controlled,
        "removed_environment_names": removed,
        "removed_environment_patterns": REMOVED_ENVIRONMENT_PATTERNS,
        "sample_plan": {
            "cold_samples": 1,
            "warm_samples": args.warm_samples,
            "sequence": ["cold", *("warm" for _ in range(args.warm_samples))],
            "target_isolation": "one unique temporary target directory per cold-plus-warm series; never the workspace target",
            "cold_reset": "target path must not exist before the cold Cargo command",
            "warm_reset": "reuse the cold sample's target; the workload performs no cargo clean, deletion, or source mutation between samples",
            "warm_condition": "warm samples run only after a successful cold sample",
            "rss_reset": "every Cargo command runs as the sole child of a fresh Python helper",
        },
        "measurement": {
            "elapsed": {
                "clock": "time.perf_counter_ns",
                "unit": "ns",
                "scope": "Cargo child",
            },
            "rss": {
                "field": "rusage_children_maxrss_kib",
                "source": "fresh helper getrusage(RUSAGE_CHILDREN).ru_maxrss",
                "unit": "KiB",
                "scope": "kernel maximum over waited-for Cargo descendants; not simultaneous aggregate tree RSS",
                "non_linux_limitation": "Darwin bytes are rounded up to KiB; other operating systems emit null",
            },
            "artifacts": "regular non-symlink bytes under the isolated target after each command",
            "output": "raw JSON Lines; no machine-specific pass threshold",
            "timeout_seconds": args.timeout_seconds,
            "output_tail_bytes": OUTPUT_TAIL_BYTES,
        },
    }
    run_identity_document = json.dumps(
        metadata, sort_keys=True, ensure_ascii=True, separators=(",", ":")
    ).encode()
    metadata["run_identity"] = sha256_bytes(run_identity_document)
    print(json.dumps(metadata, sort_keys=True), flush=True)

    with tempfile.TemporaryDirectory(prefix="plakous-perf-build-v7-") as directory:
        target_directory = Path(directory) / "target"
        command = cargo_command(
            cargo,
            args.operation,
            args.cargo_profile,
            package_name,
            package_target,
            features,
            target,
            target_directory,
            args.jobs,
        )
        common: dict[str, object] = {
            "command": command,
            "target_directory": str(target_directory),
            "timeout_seconds": args.timeout_seconds,
            "output_tail_bytes": OUTPUT_TAIL_BYTES,
            "controlled_environment": controlled,
            "removed_environment": removed,
            "run_identity": metadata["run_identity"],
            "baseline_identity": args.baseline_identity,
            "build_profile": args.build_profile,
            "build_profile_scope": profile_scope,
            "operation": args.operation,
            "operation_semantics": operation_semantics,
            "cargo_profile": args.cargo_profile,
            "target": target,
        }
        if args.dry_run:
            print(
                json.dumps(
                    {
                        "schema": SCHEMA,
                        "kind": "dry-run",
                        "run_identity": metadata["run_identity"],
                        "baseline_identity": args.baseline_identity,
                        "build_profile": args.build_profile,
                        "build_profile_scope": profile_scope,
                        "operation": args.operation,
                        "operation_semantics": operation_semantics,
                        "command": command,
                        "target_directory": str(target_directory),
                        "cwd": str(ROOT),
                        "controlled_environment": controlled,
                        "removed_environment_names": removed,
                        "removed_environment_patterns": REMOVED_ENVIRONMENT_PATTERNS,
                        "target_existed_before": target_directory.exists(),
                    },
                    sort_keys=True,
                ),
                flush=True,
            )
            return 0

        samples: list[dict[str, object]] = []
        cold = invoke_helper(
            {
                **common,
                "sample": 0,
                "temperature": "cold",
                "expected_target_state": "absent",
            }
        )
        samples.append(cold)
        print(json.dumps(cold, sort_keys=True), flush=True)
        if cold.get("status") == 0:
            for sample_number in range(1, args.warm_samples + 1):
                warm = invoke_helper(
                    {
                        **common,
                        "sample": sample_number,
                        "temperature": "warm",
                        "expected_target_state": "present",
                    }
                )
                samples.append(warm)
                print(json.dumps(warm, sort_keys=True), flush=True)
        return 0 if all(sample.get("status") == 0 for sample in samples) else 1


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--_sample-helper":
        print(
            json.dumps(helper_sample(json.loads(sys.argv[2])), sort_keys=True),
            flush=True,
        )
        raise SystemExit(0)
    raise SystemExit(main())
