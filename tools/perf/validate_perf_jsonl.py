#!/usr/bin/env python3
"""Validate Plakous performance JSONL structure and portable assertions."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import statistics
from collections import Counter
from pathlib import Path
from typing import Any


UNRATIFIED_BASELINE = "unratified-raw-distribution-v1"
V5_BUILD_FIXTURE = {"name": "isolated-build-closure-matrix", "revision": 5}
V6_BUILD_FIXTURE = {"name": "isolated-build-closure-matrix", "revision": 6}
V7_BUILD_FIXTURE = {"name": "isolated-build-closure-matrix", "revision": 7}
V8_BUILD_FIXTURE = {"name": "isolated-build-closure-matrix", "revision": 8}
V9_BUILD_FIXTURE = {"name": "isolated-build-closure-matrix", "revision": 9}
V5_TARGET_UNIT_PROFILE_SETTINGS: dict[str, dict[str, object]] = {
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
LEGACY_BUILD_PROFILE_CONTRACTS = {
    "minimal": ("supported-public", "plakous", ["--lib"], []),
    "subscriptions": (
        "supported-public",
        "plakous",
        ["--lib"],
        ["native-subscriptions"],
    ),
    "sessions-sqlite": ("supported-public", "plakous", ["--lib"], ["sessions-sqlite"]),
    "openai-platform": ("supported-public", "plakous", ["--lib"], ["openai"]),
    "xai-platform": ("supported-public", "plakous", ["--lib"], ["xai"]),
    "openrouter": ("supported-public", "plakous", ["--lib"], ["openrouter"]),
    "anthropic": ("supported-public", "plakous", ["--lib"], ["anthropic"]),
    "plak": ("supported-public", "plak", ["--bin", "plak"], []),
    "all-providers": (
        "supported-public",
        "plakous",
        ["--lib"],
        ["anthropic", "native-subscriptions", "openai", "openrouter", "xai"],
    ),
    "full": ("supported-public", "plakous", ["--lib"], ["full"]),
    "codex-components": (
        "benchmark-component",
        "plakous",
        ["--lib"],
        ["openai", "plakous-openai/native-auth"],
    ),
    "supergrok-components": (
        "benchmark-component",
        "plakous",
        ["--lib"],
        ["plakous-xai/native-auth", "xai"],
    ),
}
V5_BUILD_PROFILE_CONTRACTS = {
    "minimal": ("supported-public", "plakous", ["--lib"], []),
    "subscriptions": (
        "supported-public",
        "plakous",
        ["--lib"],
        ["native-subscriptions"],
    ),
    "sessions-sqlite": ("supported-public", "plakous", ["--lib"], ["sessions-sqlite"]),
    "plak": ("supported-public", "plak", ["--bin", "plak"], []),
    "agent-wrapper": ("supported-public", "plakous-agent", ["--lib"], []),
    "tools": ("supported-public", "plakous-tools", ["--lib"], []),
    "openai-platform": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["openai"],
    ),
    "xai-platform": ("transitional-facade-baseline", "plakous", ["--lib"], ["xai"]),
    "openrouter": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["openrouter"],
    ),
    "anthropic": ("transitional-facade-baseline", "plakous", ["--lib"], ["anthropic"]),
    "all-providers": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["anthropic", "native-subscriptions", "openai", "openrouter", "xai"],
    ),
    "full": ("transitional-facade-baseline", "plakous", ["--lib"], ["full"]),
    "agent-impl": ("internal-topology-baseline", "plakous-agent-impl", ["--lib"], []),
    "codex-components": (
        "internal-topology-baseline",
        "plakous",
        ["--lib"],
        ["openai", "plakous-openai/native-auth"],
    ),
    "supergrok-components": (
        "internal-topology-baseline",
        "plakous",
        ["--lib"],
        ["plakous-xai/native-auth", "xai"],
    ),
    "dispatch": ("internal-topology-baseline", "plakous-dispatch", ["--lib"], []),
    "attempt": ("internal-topology-baseline", "plakous-attempt", ["--lib"], []),
    "responses-kernel": (
        "internal-topology-baseline",
        "plakous-responses",
        ["--lib"],
        [],
    ),
    "responses-host": (
        "internal-topology-baseline",
        "plakous-responses-dialect",
        ["--lib"],
        [],
    ),
    "messages": ("internal-topology-baseline", "plakous-messages", ["--lib"], []),
    "plak-app": ("product-topology-baseline", "plak-app", ["--lib"], []),
    "plak-lib": ("product-topology-baseline", "plak", ["--lib"], []),
    "paid-platform": (
        "repository-only-topology-baseline",
        "paid-platform",
        ["--lib"],
        [],
    ),
}
V6_BUILD_PROFILE_CONTRACTS = {
    "minimal": ("supported-public", "plakous", ["--lib"], []),
    "subscriptions": (
        "supported-public",
        "plakous",
        ["--lib"],
        ["native-subscriptions"],
    ),
    "sessions-sqlite": ("supported-public", "plakous", ["--lib"], ["sessions-sqlite"]),
    "plak": ("supported-public", "plak", ["--bin", "plak"], []),
    "agent": ("supported-public", "plakous-agent", ["--lib"], []),
    "tools": ("supported-public", "plakous-tools", ["--lib"], []),
    "openai-platform": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["openai"],
    ),
    "xai-platform": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["xai"],
    ),
    "openrouter": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["openrouter"],
    ),
    "anthropic": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["anthropic"],
    ),
    "all-providers": (
        "transitional-facade-baseline",
        "plakous",
        ["--lib"],
        ["anthropic", "native-subscriptions", "openai", "openrouter", "xai"],
    ),
    "full": ("transitional-facade-baseline", "plakous", ["--lib"], ["full"]),
    "codex-components": (
        "internal-topology-baseline",
        "plakous",
        ["--lib"],
        ["openai", "plakous-openai/native-auth"],
    ),
    "supergrok-components": (
        "internal-topology-baseline",
        "plakous",
        ["--lib"],
        ["plakous-xai/native-auth", "xai"],
    ),
    "dispatch": (
        "internal-topology-baseline",
        "plakous-dispatch",
        ["--lib"],
        [],
    ),
    "attempt": (
        "internal-topology-baseline",
        "plakous-attempt",
        ["--lib"],
        [],
    ),
    "responses-kernel": (
        "internal-topology-baseline",
        "plakous-responses",
        ["--lib"],
        [],
    ),
    "responses-host": (
        "internal-topology-baseline",
        "plakous-responses-dialect",
        ["--lib"],
        [],
    ),
    "messages": (
        "internal-topology-baseline",
        "plakous-messages",
        ["--lib"],
        [],
    ),
    "plak-app": ("product-topology-baseline", "plak-app", ["--lib"], []),
    "plak-lib": ("product-topology-baseline", "plak", ["--lib"], []),
    "paid-platform": (
        "repository-only-topology-baseline",
        "paid-platform",
        ["--lib"],
        [],
    ),
}
V7_BUILD_PROFILE_CONTRACTS = {
    profile: contract
    for profile, contract in V6_BUILD_PROFILE_CONTRACTS.items()
    if profile != "dispatch"
}
V8_BUILD_PROFILE_CONTRACTS = {
    profile: contract
    for profile, contract in V7_BUILD_PROFILE_CONTRACTS.items()
    if profile != "plak-app"
}
V9_BUILD_PROFILE_CONTRACTS = {
    profile: contract
    for profile, contract in V8_BUILD_PROFILE_CONTRACTS.items()
    if profile != "paid-platform"
}
CURRENT_BUILD_SCHEMAS = {
    "plakous.perf-build.v5",
    "plakous.perf-build.v6",
    "plakous.perf-build.v7",
    "plakous.perf-build.v8",
    "plakous.perf-build.v9",
}
SUPPORTED_SCHEMAS = {
    "plakous.perf-build.v2",
    "plakous.perf-build.v3",
    "plakous.perf-build.v4",
    "plakous.perf-build.v5",
    "plakous.perf-build.v6",
    "plakous.perf-build.v7",
    "plakous.perf-build.v8",
    "plakous.perf-build.v9",
    "plakous.perf-core.v2",
    "plakous.perf-core.v3",
    "plakous.perf-core.v4",
    "plakous.perf-project.v3",
    "plakous.perf-project.v4",
    "plakous.perf-project.v5",
    "plakous.perf-project.v6",
    "plakous.perf-store.v2",
    "plakous.perf-store.v3",
    "plakous.perf-store.v4",
    "plakous.perf-store.v5",
    "plakous.perf-store.v6",
    "plakous.perf-allocations.v1",
}


class ValidationError(ValueError):
    """One JSONL row violated its declared portable evidence contract."""


def fail(message: str) -> None:
    raise ValidationError(message)


def field(value: dict[str, Any], *path: str) -> Any:
    current: Any = value
    for part in path:
        if not isinstance(current, dict) or part not in current:
            fail(f"missing field: {'.'.join(path)}")
        current = current[part]
    return current


def exact(value: dict[str, Any], expected: Any, *path: str) -> None:
    actual = field(value, *path)
    if actual != expected:
        fail(f"{'.'.join(path)} must be {expected!r}, found {actual!r}")


def nonnegative_int(value: dict[str, Any], *path: str) -> int:
    actual = field(value, *path)
    if isinstance(actual, bool) or not isinstance(actual, int) or actual < 0:
        fail(f"{'.'.join(path)} must be a non-negative integer")
    return actual


def positive_int(value: dict[str, Any], *path: str) -> int:
    actual = nonnegative_int(value, *path)
    if actual == 0:
        fail(f"{'.'.join(path)} must be positive")
    return actual


def tagged_sha256(value: dict[str, Any], *path: str) -> str:
    actual = field(value, *path)
    if (
        not isinstance(actual, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", actual) is None
    ):
        fail(f"{'.'.join(path)} must be a tagged lowercase SHA-256 digest")
    return actual


def stable_host_id(metadata: dict[str, Any]) -> str:
    actual = field(metadata, "host", "host_id")
    rejected = {"localhost", "unknown", "host", "stable-host-id", "<stable-host-id>"}
    if (
        not isinstance(actual, str)
        or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{2,127}", actual) is None
        or actual.lower() in rejected
    ):
        fail("performance evidence requires a stable bounded non-placeholder host ID")
    return actual


def semantic_row_total(value: dict[str, Any], *path: str) -> int:
    rows = field(value, *path)
    if not isinstance(rows, dict):
        fail(f"{'.'.join(path)} must be an object")
    names = (
        "runtime_objects",
        "runtime_object_edges",
        "checkpoints",
        "sessions",
        "branches",
        "create_receipts",
        "commit_receipts",
        "fork_receipts",
        "archive_receipts",
    )
    expected = sum(nonnegative_int(rows, name) for name in names)
    exact(rows, expected, "total_rows_in_semantic_tables")
    return expected


def source_closure(metadata: dict[str, Any], *prefix: str) -> None:
    closure = field(metadata, *prefix, "non_ignored_worktree_content_closure")
    if not isinstance(closure, dict):
        fail("source content closure must be available")
    identity = field(closure, "identity")
    tagged_sha256_identity = isinstance(identity, str) and re.fullmatch(
        r"sha256:[0-9a-f]{64}", identity
    )
    structured_sha256 = (
        isinstance(identity, dict)
        and identity.get("algorithm") == "Sha256"
        and identity.get("domain") == "Blob"
        and identity.get("version") == 1
        and isinstance(identity.get("bytes"), list)
        and len(identity["bytes"]) == 32
        and all(
            isinstance(byte, int) and not isinstance(byte, bool) and 0 <= byte <= 255
            for byte in identity["bytes"]
        )
    )
    if not (tagged_sha256_identity or structured_sha256):
        fail("source content closure identity must be non-empty")
    if nonnegative_int(closure, "file_count") == 0:
        fail("source content closure must cover at least one file")
    nonnegative_int(closure, "content_bytes")
    nonnegative_int(closure, "missing_tracked_files")


def load_rows(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            if not line.strip():
                fail(f"{path}:{line_number}: blank JSONL row")
            try:
                row = json.loads(
                    line,
                    parse_constant=lambda value: fail(
                        f"{path}:{line_number}: non-finite JSON number {value}"
                    ),
                )
            except json.JSONDecodeError as error:
                fail(f"{path}:{line_number}: invalid JSON: {error}")
            if not isinstance(row, dict):
                fail(f"{path}:{line_number}: row must be an object")
            rows.append(row)
    if not rows:
        fail(f"{path}: no JSONL rows")
    schema = field(rows[0], "schema")
    if schema not in SUPPORTED_SCHEMAS:
        fail(f"{path}: unsupported schema {schema!r}")
    for index, row in enumerate(rows, 1):
        if field(row, "schema") != schema:
            fail(f"{path}:{index}: mixed schema")
        if not isinstance(field(row, "kind"), str):
            fail(f"{path}:{index}: kind must be a string")
    if field(rows[0], "kind") != "metadata":
        fail(f"{path}: first row must be metadata")
    if sum(row["kind"] == "metadata" for row in rows) != 1:
        fail(f"{path}: exactly one metadata row is required")
    return rows


def validate_build(rows: list[dict[str, Any]]) -> None:
    metadata = rows[0]
    schema = field(metadata, "schema")
    baseline_identity = field(metadata, "baseline", "identity")
    if schema in CURRENT_BUILD_SCHEMAS:
        if (
            not isinstance(baseline_identity, str)
            or not 1 <= len(baseline_identity) <= 256
            or "\r" in baseline_identity
            or "\n" in baseline_identity
        ):
            fail(f"{schema} baseline identity must be bounded")
    else:
        exact(metadata, UNRATIFIED_BASELINE, "baseline", "identity")
    exact(metadata, None, "baseline", "numeric_regression_gate")
    source_closure(metadata, "git")
    run_identity = field(metadata, "run_identity")
    if schema in {
        "plakous.perf-build.v3",
        "plakous.perf-build.v4",
        "plakous.perf-build.v5",
        "plakous.perf-build.v6",
        "plakous.perf-build.v7",
        "plakous.perf-build.v8",
        "plakous.perf-build.v9",
    }:
        profile = field(metadata, "cargo", "build_profile")
        scope = field(metadata, "cargo", "build_profile_scope")
        if schema in CURRENT_BUILD_SCHEMAS:
            allowed_scopes = {
                "supported-public",
                "transitional-facade-baseline",
                "internal-topology-baseline",
                "product-topology-baseline",
                "repository-only-topology-baseline",
            }
        else:
            allowed_scopes = {"supported-public", "benchmark-component"}
        if scope not in allowed_scopes:
            fail(f"{schema} profile scope is unknown")
        package = field(metadata, "cargo", "package", "name")
        target_selection = field(metadata, "cargo", "target_selection")
        requested_features = field(metadata, "cargo", "requested_features")
        contracts = {
            "plakous.perf-build.v5": V5_BUILD_PROFILE_CONTRACTS,
            "plakous.perf-build.v6": V6_BUILD_PROFILE_CONTRACTS,
            "plakous.perf-build.v7": V7_BUILD_PROFILE_CONTRACTS,
            "plakous.perf-build.v8": V8_BUILD_PROFILE_CONTRACTS,
            "plakous.perf-build.v9": V9_BUILD_PROFILE_CONTRACTS,
        }.get(schema, LEGACY_BUILD_PROFILE_CONTRACTS)
        contract = contracts.get(profile)
        if contract is None:
            fail(f"{schema} profile is unknown")
        expected_scope, expected_package, expected_target, expected_features = contract
        if (scope, package, target_selection, requested_features) != (
            expected_scope,
            expected_package,
            expected_target,
            expected_features,
        ):
            fail(f"{schema} profile contract disagrees with metadata")
        if not isinstance(requested_features, list) or requested_features != sorted(
            requested_features
        ):
            fail("requested features must be a sorted list")
        operation_contracts = {
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
        operation = field(metadata, "cargo", "operation")
        if operation not in operation_contracts:
            fail(f"{schema} operation is unknown")
        if schema in CURRENT_BUILD_SCHEMAS:
            validate_current_build_fixture(metadata)
            validate_current_build_run_identity(metadata)
            validate_resolved_build_closure(metadata)
        if schema in {
            "plakous.perf-build.v4",
            "plakous.perf-build.v5",
            "plakous.perf-build.v6",
            "plakous.perf-build.v7",
            "plakous.perf-build.v8",
            "plakous.perf-build.v9",
        }:
            host_id = field(metadata, "host", "host_id")
            if (
                not isinstance(host_id, str)
                or not 1 <= len(host_id) <= 256
                or "\r" in host_id
                or "\n" in host_id
            ):
                fail(f"{schema} requires a bounded stable host ID")
            exact(
                metadata, operation_contracts[operation], "cargo", "operation_semantics"
            )
    kinds = Counter(row["kind"] for row in rows)
    if set(kinds) - {"metadata", "sample", "dry-run"}:
        fail("perf-build contains an unknown row kind")
    if kinds["dry-run"]:
        if kinds != Counter({"metadata": 1, "dry-run": 1}):
            fail("perf-build dry run must contain metadata plus one dry-run row")
        exact(rows[1], run_identity, "run_identity")
        if schema in {
            "plakous.perf-build.v3",
            "plakous.perf-build.v4",
            "plakous.perf-build.v5",
            "plakous.perf-build.v6",
            "plakous.perf-build.v7",
            "plakous.perf-build.v8",
            "plakous.perf-build.v9",
        }:
            exact(rows[1], profile, "build_profile")
            exact(rows[1], scope, "build_profile_scope")
            if schema in {
                "plakous.perf-build.v4",
                "plakous.perf-build.v5",
                "plakous.perf-build.v6",
                "plakous.perf-build.v7",
                "plakous.perf-build.v8",
                "plakous.perf-build.v9",
            }:
                exact(rows[1], operation, "operation")
                exact(rows[1], operation_contracts[operation], "operation_semantics")
            validate_build_command(rows[1], metadata)
            if schema in CURRENT_BUILD_SCHEMAS:
                exact(rows[1], baseline_identity, "baseline_identity")
        return
    planned_warm = field(metadata, "sample_plan", "warm_samples")
    if isinstance(planned_warm, bool) or not isinstance(planned_warm, int):
        fail("perf-build warm sample count must be an integer")
    samples = [row for row in rows if row["kind"] == "sample"]
    if len(samples) != 1 + planned_warm:
        fail("perf-build sample count disagrees with metadata")
    for index, sample in enumerate(samples):
        exact(sample, 0, "status")
        exact(sample, False, "timed_out")
        exact(sample, run_identity, "run_identity")
        exact(sample, baseline_identity, "baseline_identity")
        exact(sample, index, "sample")
        exact(sample, "cold" if index == 0 else "warm", "temperature")
        exact(sample, index != 0, "target_existed_before")
        if schema in {
            "plakous.perf-build.v3",
            "plakous.perf-build.v4",
            "plakous.perf-build.v5",
            "plakous.perf-build.v6",
            "plakous.perf-build.v7",
            "plakous.perf-build.v8",
            "plakous.perf-build.v9",
        }:
            exact(sample, profile, "build_profile")
            exact(sample, scope, "build_profile_scope")
            if schema in {
                "plakous.perf-build.v4",
                "plakous.perf-build.v5",
                "plakous.perf-build.v6",
                "plakous.perf-build.v7",
                "plakous.perf-build.v8",
                "plakous.perf-build.v9",
            }:
                exact(sample, operation, "operation")
                exact(sample, operation_contracts[operation], "operation_semantics")
            validate_build_command(sample, metadata)
            if schema in CURRENT_BUILD_SCHEMAS:
                validate_current_build_sample_fixture(sample)
        nonnegative_int(sample, "elapsed_ns")
        nonnegative_int(sample, "artifact_bytes")
        nonnegative_int(sample, "artifact_files")


def validate_resolved_build_closure(metadata: dict[str, Any]) -> None:
    """Require exact deterministic Cargo-tree facts in v5 build evidence."""
    cargo = field(metadata, "cargo")
    closure = field(metadata, "cargo", "resolved_feature_closure")
    exact(closure, "cargo tree", "resolver")
    exact(closure, ["normal", "build"], "edge_kinds")
    exact(closure, 1, "format_revision")
    exact(closure, "$WORKSPACE", "workspace_path_token")
    entries = field(closure, "entries")
    if (
        not isinstance(entries, list)
        or not entries
        or not all(isinstance(entry, str) and entry for entry in entries)
        or entries != sorted(set(entries))
    ):
        fail("resolved build closure entries must be a non-empty sorted unique list")
    exact(closure, len(entries), "entry_count")
    encoded = json.dumps(entries, ensure_ascii=True, separators=(",", ":")).encode()
    expected = f"sha256:{hashlib.sha256(encoded).hexdigest()}"
    exact(closure, expected, "entries_sha256")
    tagged_sha256(closure, "raw_stdout_sha256")
    package = field(cargo, "package", "name")
    target = field(cargo, "target")
    features = field(cargo, "requested_features")
    cargo_identity_command = field(metadata, "toolchain", "cargo", "command")
    if (
        not isinstance(cargo_identity_command, list)
        or len(cargo_identity_command) != 2
        or not all(isinstance(value, str) for value in cargo_identity_command)
        or cargo_identity_command[1] != "-Vv"
    ):
        fail("plakous.perf-build.v5 Cargo identity command is invalid")
    expected_command = [
        cargo_identity_command[0],
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
    ]
    if features:
        expected_command.extend(("--features", ",".join(features)))
    expected_command.extend(
        (
            "--target",
            target,
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}|{f}",
        )
    )
    exact(closure, expected_command, "resolver_command")
    if any(entry.endswith(" (*)") or entry.count("|") != 1 for entry in entries):
        fail("resolved build closure entries are not normalized package/feature rows")
    root_entries = [entry for entry in entries if entry.startswith(f"{package} v")]
    if len(root_entries) != 1:
        fail("resolved build closure must contain exactly one normalized package root")
    derived_root_features = sorted(
        feature for feature in root_entries[0].split("|", 1)[1].split(",") if feature
    )
    root_features = field(closure, "resolved_root_features")
    if (
        not isinstance(root_features, list)
        or not all(isinstance(feature, str) and feature for feature in root_features)
        or root_features != sorted(set(root_features))
    ):
        fail("resolved root features must be a sorted unique string list")
    if root_features != derived_root_features:
        fail("resolved root features disagree with the normalized package root")


def validate_current_build_run_identity(metadata: dict[str, Any]) -> None:
    """Recompute a v5/v6 identity from canonical metadata without its identity."""
    schema = field(metadata, "schema")
    actual = tagged_sha256(metadata, "run_identity")
    identity_document = dict(metadata)
    del identity_document["run_identity"]
    encoded = json.dumps(
        identity_document, sort_keys=True, ensure_ascii=True, separators=(",", ":")
    ).encode()
    expected = f"sha256:{hashlib.sha256(encoded).hexdigest()}"
    if actual != expected:
        fail(f"{schema} run identity disagrees with canonical metadata")


def validate_current_build_fixture(metadata: dict[str, Any]) -> None:
    """Pin the producer fixture identity belonging to the build schema."""
    fixture = {
        "plakous.perf-build.v5": V5_BUILD_FIXTURE,
        "plakous.perf-build.v6": V6_BUILD_FIXTURE,
        "plakous.perf-build.v7": V7_BUILD_FIXTURE,
        "plakous.perf-build.v8": V8_BUILD_FIXTURE,
        "plakous.perf-build.v9": V9_BUILD_FIXTURE,
    }.get(field(metadata, "schema"))
    if fixture is None:
        fail("perf-build fixture validation requires schema v5, v6, v7, v8, or v9")
    exact(metadata, fixture, "fixture")


def validate_current_build_sample_fixture(sample: dict[str, Any]) -> None:
    """Require measured rows to name their schema's exact fixture revision."""
    schema = field(sample, "schema")
    fixture = {
        "plakous.perf-build.v5": V5_BUILD_FIXTURE,
        "plakous.perf-build.v6": V6_BUILD_FIXTURE,
        "plakous.perf-build.v7": V7_BUILD_FIXTURE,
        "plakous.perf-build.v8": V8_BUILD_FIXTURE,
        "plakous.perf-build.v9": V9_BUILD_FIXTURE,
    }.get(schema)
    if fixture is None:
        fail(
            "perf-build sample fixture validation requires schema "
            "v5, v6, v7, v8, or v9"
        )
    exact(sample, fixture["revision"], "fixture_revision")


def validate_v5_run_identity(metadata: dict[str, Any]) -> None:
    """Compatibility entry point for immutable v5 evidence tests."""
    validate_current_build_run_identity(metadata)


def validate_v5_fixture(metadata: dict[str, Any]) -> None:
    """Compatibility entry point for immutable v5 evidence tests."""
    metadata = {"schema": "plakous.perf-build.v5", **metadata}
    validate_current_build_fixture(metadata)


def validate_v5_sample_fixture(sample: dict[str, Any]) -> None:
    """Compatibility entry point for immutable v5 evidence tests."""
    sample = {"schema": "plakous.perf-build.v5", **sample}
    validate_current_build_sample_fixture(sample)


def validate_build_command(row: dict[str, Any], metadata: dict[str, Any]) -> None:
    command = field(row, "command")
    if not isinstance(command, list) or not all(
        isinstance(value, str) for value in command
    ):
        fail("perf-build command must be a string list")
    cargo = field(metadata, "cargo")
    schema = field(metadata, "schema")
    if schema in CURRENT_BUILD_SCHEMAS:
        expected = expected_current_build_command(row, metadata)
        if command != expected:
            fail(
                f"{schema} measured Cargo command disagrees with "
                "the exact ordered metadata contract"
            )
        return
    operation = field(cargo, "operation")
    if command.count(operation) != 1 or any(
        other in command for other in {"check", "build"} - {operation}
    ):
        fail("perf-build command has an ambiguous Cargo operation")
    required = {
        field(cargo, "operation"),
        "--locked",
        "--offline",
        "--no-default-features",
        "--package",
        field(cargo, "package", "name"),
        "--target",
        field(cargo, "target"),
        "--profile",
        field(cargo, "cargo_profile"),
    }
    if not required.issubset(command):
        fail("perf-build command disagrees with metadata")
    for selector in field(cargo, "target_selection"):
        if selector not in command:
            fail("perf-build command omitted its selected target")
    features = field(cargo, "requested_features")
    if features:
        if "--features" not in command:
            fail("perf-build command omitted requested features")
        index = command.index("--features")
        if index + 1 >= len(command) or command[index + 1] != ",".join(features):
            fail("perf-build command feature list disagrees with metadata")
    elif "--features" in command:
        fail("perf-build command unexpectedly enables features")


def expected_current_build_command(
    row: dict[str, Any], metadata: dict[str, Any]
) -> list[str]:
    """Reconstruct the complete ordered Cargo command for one v5/v6 row."""
    schema = field(metadata, "schema")
    cargo = field(metadata, "cargo")
    cargo_identity_command = field(metadata, "toolchain", "cargo", "command")
    if (
        not isinstance(cargo_identity_command, list)
        or len(cargo_identity_command) != 2
        or not all(isinstance(value, str) for value in cargo_identity_command)
        or cargo_identity_command[1] != "-Vv"
    ):
        fail(f"{schema} Cargo identity command is invalid")
    cargo_profile = field(cargo, "cargo_profile")
    settings = V5_TARGET_UNIT_PROFILE_SETTINGS.get(cargo_profile)
    if settings is None:
        fail(f"{schema} Cargo profile is unknown")
    exact(cargo, settings, "target_unit_profile_settings")
    target_selection = field(cargo, "target_selection")
    if not isinstance(target_selection, list) or target_selection not in (
        ["--lib"],
        ["--bin", "plak"],
    ):
        fail(f"{schema} selected target is invalid")
    features = field(cargo, "requested_features")
    if (
        not isinstance(features, list)
        or not all(isinstance(feature, str) and feature for feature in features)
        or features != sorted(set(features))
    ):
        fail(f"{schema} requested features are not canonical")
    jobs = positive_int(cargo, "jobs")
    target_directory = field(row, "target_directory")
    if not isinstance(target_directory, str) or not target_directory:
        fail(f"{schema} target directory must be non-empty")
    command = [cargo_identity_command[0]]
    for key, value in settings.items():
        command.extend(
            ("--config", f"profile.{cargo_profile}.{key}={json.dumps(value)}")
        )
    command.extend(
        (
            field(cargo, "operation"),
            "--locked",
            "--offline",
            "--color",
            "never",
            "--manifest-path",
            "Cargo.toml",
            "--package",
            field(cargo, "package", "name"),
            *target_selection,
            "--no-default-features",
        )
    )
    if features:
        command.extend(("--features", ",".join(features)))
    command.extend(
        (
            "--target",
            field(cargo, "target"),
            "--profile",
            cargo_profile,
            "--jobs",
            str(jobs),
            "--target-dir",
            target_directory,
        )
    )
    return command


def expected_v5_build_command(
    row: dict[str, Any], metadata: dict[str, Any]
) -> list[str]:
    """Compatibility entry point for immutable v5 evidence tests."""
    return expected_current_build_command(row, metadata)


def validate_core(rows: list[dict[str, Any]]) -> None:
    metadata = rows[0]
    schema = field(metadata, "schema")
    stable_host_id(metadata)
    exact(metadata, None, "source", "baseline_identity")
    source_closure(metadata, "source")
    counts = Counter(row["kind"] for row in rows)
    expected_turns = len(field(metadata, "history_sweep_turns")) * nonnegative_int(
        metadata, "samples_per_point"
    )
    expected = Counter({"metadata": 1, "turn": expected_turns, "prompt-bound": 1})
    if schema in {"plakous.perf-core.v3", "plakous.perf-core.v4"}:
        expected["event-history-instrumented-sample"] = len(
            field(metadata, "history_sweep_turns")
        )
    if counts != expected:
        fail("perf-core row counts disagree with metadata")
    for row in rows[1:]:
        if row["kind"] == "turn":
            exact(row, "completed", "terminal_outcome")
            exact(row, 1, "measured_task_model_calls")
            exact(row, 1, "model_rounds")
            exact(row, 0, "tool_calls")
            exact(row, None, "allocation_count")
            exact(row, None, "allocated_bytes")
            exact(
                row,
                True,
                "structural_sharing",
                "prepared_request_pre_turn_history_is_exact_persistent_ancestor",
            )
            nonnegative_int(row, "prompt_to_terminal_result_elapsed_ns")
        elif row["kind"] == "prompt-bound":
            exact(row, True, "exact_bound_proved")
            exact(row, "Backpressure", "rejection")
            capacity = nonnegative_int(row, "configured_capacity")
            exact(row, capacity, "admitted_queued_turns")
            exact(row, capacity + 1, "admitted_turns_including_active")
        else:
            if schema not in {"plakous.perf-core.v3", "plakous.perf-core.v4"}:
                fail("instrumented event rows require perf-core v3")
            exact(row, field(metadata, "event_history_instrumented_fixture"), "fixture")
            history_turns = nonnegative_int(row, "history_prefill_turns")
            exact(row, history_turns * 2, "canonical_items_before")
            before = nonnegative_int(row, "pre_turn_cursor_sequence")
            after = nonnegative_int(row, "post_turn_cursor_sequence")
            events = positive_int(row, "projected_events")
            if after - before != events:
                fail("event cursor delta must equal projected event count")
            positive_int(row, "projected_event_serialized_bytes")
            nonnegative_int(row, "event_delivery_and_serialization_elapsed_ns")
            nonnegative_int(row, "snapshot_attach_elapsed_ns")
            nonnegative_int(row, "snapshot_clone_elapsed_ns")
            exact(row, True, "snapshot_clone_value_equal")
            nonnegative_int(row, "snapshot_committed_conversation_items")
            positive_int(row, "snapshot_projection_serialized_bytes")
            exact(row, None, "retained_bytes")
            if schema == "plakous.perf-core.v4":
                event_bytes = positive_int(row, "projected_event_serialized_bytes")
                exact(
                    row,
                    event_bytes // events,
                    "projected_event_serialized_bytes_per_event",
                )
                before_bytes = positive_int(row, "snapshot_projection_serialized_bytes")
                after_bytes = positive_int(
                    row, "post_snapshot_projection_serialized_bytes"
                )
                exact(
                    row,
                    max(0, after_bytes - before_bytes),
                    "snapshot_projection_serialized_growth_bytes",
                )
                external = field(row, "retained_bytes_external_probe")
                if not isinstance(external, str) or not external:
                    fail("perf-core v4 must name its external retained-byte probe")
    if schema in {"plakous.perf-core.v3", "plakous.perf-core.v4"}:
        event_rows = [
            row for row in rows if row["kind"] == "event-history-instrumented-sample"
        ]
        if [row["history_prefill_turns"] for row in event_rows] != field(
            metadata, "history_sweep_turns"
        ):
            fail("event-history rows must exactly follow the declared sweep")


def validate_project(rows: list[dict[str, Any]]) -> None:
    metadata = rows[0]
    schema = field(metadata, "schema")
    stable_host_id(metadata)
    exact(metadata, None, "source", "baseline_identity")
    source_closure(metadata, "source")
    points = len(field(metadata, "history_sweep_items"))
    samples = nonnegative_int(metadata, "samples_per_point")
    counts = Counter(row["kind"] for row in rows)
    expected = Counter(
        {
            "metadata": 1,
            "history-advance": points,
            "history-append-sample": points * samples,
            "sample": points * samples,
        }
    )
    if schema in {"plakous.perf-project.v5", "plakous.perf-project.v6"}:
        expected["family-artifact-growth-sample"] = len(
            field(metadata, "family_artifact_sweep_bytes")
        )
    if counts != expected:
        fail("perf-project row counts disagree with metadata")
    for row in rows[1:]:
        if row["kind"] == "family-artifact-growth-sample":
            if schema not in {"plakous.perf-project.v5", "plakous.perf-project.v6"}:
                fail("family-artifact growth rows require perf-project v5")
            nonnegative_int(row, "opaque_bytes")
            nonnegative_int(row, "serialized_envelope_bytes")
            nonnegative_int(row, "construction_and_integrity_hash_elapsed_ns")
            nonnegative_int(row, "handle_clone_elapsed_ns")
            exact(row, True, "clone_shared_exact_opaque_byte_allocation")
            exact(row, None, "retained_bytes")
        elif row["kind"] == "history-advance":
            exact(row, True, "source_is_exact_persistent_ancestor")
            if field(row, "append_observed"):
                exact(row, True, "all_prior_segment_payloads_shared")
                exact(row, True, "every_append_created_new_history_handle")
        elif row["kind"] == "history-append-sample":
            exact(row, True, "source_is_exact_ancestor")
            exact(row, True, "new_history_handle")
            nonnegative_int(row, "elapsed_ns")
        else:
            exact(row, True, "from_parts", "exact_prefix_allocation_shared")
            exact(row, True, "from_parts", "exact_history_allocation_shared")
            exact(row, True, "for_round", "model_input_identity_equal")
            exact(row, True, "for_round", "request_identity_changed")
            exact(row, True, "for_round", "request_canonical_bytes_changed")
            exact(row, True, "for_round", "exact_prefix_allocation_shared")
            exact(row, True, "for_round", "exact_history_allocation_shared")
            exact(row, True, "full_rebuild", "request_identity_equal")
            exact(row, True, "full_rebuild", "request_canonical_bytes_equal")
            exact(row, False, "full_rebuild", "stable_prefix_allocation_shared")
            exact(row, True, "full_rebuild", "exact_history_allocation_shared")
            exact(row, None, "allocation_count")
            exact(row, None, "allocated_bytes")
            exact(row, None, "transient_full_history_clone_trace")
            if schema in {
                "plakous.perf-project.v4",
                "plakous.perf-project.v5",
                "plakous.perf-project.v6",
            }:
                exact(
                    row,
                    True,
                    "public_handle_clone_probes",
                    "history",
                    "identity_equal",
                )
                exact(
                    row,
                    True,
                    "public_handle_clone_probes",
                    "history",
                    "exact_history_allocation_shared",
                )
                exact(
                    row,
                    True,
                    "public_handle_clone_probes",
                    "model_request",
                    "identity_equal",
                )
                exact(
                    row,
                    True,
                    "public_handle_clone_probes",
                    "model_request",
                    "exact_prefix_allocation_shared",
                )
                exact(
                    row,
                    True,
                    "public_handle_clone_probes",
                    "model_request",
                    "exact_history_allocation_shared",
                )
            exact(
                row,
                True,
                "synthetic_prepared_context",
                "descriptor_binding_matches_authority",
            )
            exact(
                row,
                True,
                "synthetic_prepared_context",
                "request_model_input_identity_equal",
            )
            exact(
                row,
                True,
                "synthetic_prepared_context",
                "active_family_artifact",
                "clone_shared_exact_opaque_byte_allocation",
            )
            if schema == "plakous.perf-project.v6":
                trace = field(row, "separate_reserialization_trace")
                request_bytes = positive_int(trace, "request_serialized_bytes")
                round_bytes = positive_int(trace, "next_round_serialized_bytes")
                exact(trace, request_bytes + round_bytes, "exact_bytes_written_total")
                positive_int(trace, "stable_prefix_canonical_bytes")
                nonnegative_int(trace, "request_serialize_elapsed_ns")
                nonnegative_int(trace, "next_round_serialize_elapsed_ns")
                exact(
                    trace,
                    field(row, "stable_prefix", "identity"),
                    "stable_prefix_identity",
                )
                cache = field(row, "managed_cache_affinity_evidence")
                exact(cache, "measured-synthetic-runtime-affinity", "status")
                exact(cache, True, "prepared_projection_identity_equal")
                exact(trace, field(cache, "identity"), "cache_affinity_identity")

    if schema in {"plakous.perf-project.v5", "plakous.perf-project.v6"}:
        artifact_rows = [
            row for row in rows if row["kind"] == "family-artifact-growth-sample"
        ]
        expected_sizes = field(metadata, "family_artifact_sweep_bytes")
        if [row["opaque_bytes"] for row in artifact_rows] != expected_sizes:
            fail("family-artifact rows must exactly follow the declared sweep")
        if rows[-len(artifact_rows) :] != artifact_rows:
            fail("family-artifact sweep must run after all primary project timing rows")
        for row in artifact_rows:
            if positive_int(row, "serialized_envelope_bytes") <= positive_int(
                row, "opaque_bytes"
            ):
                fail("serialized family-artifact envelope must exceed opaque bytes")
        if schema == "plakous.perf-project.v6":
            for row in artifact_rows[:-1]:
                exact(row, None, "post_primary_tool_schema_trace")
            tool_trace = field(artifact_rows[-1], "post_primary_tool_schema_trace")
            exact(tool_trace, [1, 16, 64, 256], "property_sweep")
            points = field(tool_trace, "points")
            if not isinstance(points, list) or len(points) != 4:
                fail("tool schema trace must contain four declared points")
            if [field(point, "properties") for point in points] != [1, 16, 64, 256]:
                fail("tool schema points must exactly follow the declared sweep")
            for point in points:
                positive_int(point, "schema_serialized_bytes")
                positive_int(point, "valid_arguments_bytes")
                nonnegative_int(point, "compile_elapsed_ns")
                samples = positive_int(point, "validation_samples")
                values = field(point, "validation_elapsed_ns")
                if not isinstance(values, list) or len(values) != samples:
                    fail("tool schema validation samples disagree with metadata")
                if any(
                    isinstance(value, bool) or not isinstance(value, int) or value < 0
                    for value in values
                ):
                    fail("tool schema validation timings must be non-negative integers")


def validate_store(rows: list[dict[str, Any]]) -> None:
    metadata = rows[0]
    schema = field(metadata, "schema")
    stable_host_id(metadata)
    exact(metadata, UNRATIFIED_BASELINE, "baseline_identity")
    source_closure(metadata, "source")
    counts = Counter(row["kind"] for row in rows)
    if counts["dry-run"]:
        if counts != Counter({"metadata": 1, "dry-run": 1}):
            fail("perf-store dry run must contain metadata plus one dry-run row")
        return
    sweep = field(metadata, "turn_sweep")
    expected_commits = 2 * sum(sweep)
    saturation_attempts = nonnegative_int(metadata, "sqlite_actor_saturation_attempts")
    expected = Counter(
        {
            "metadata": 1,
            "commit-sample": expected_commits,
            "summary": 2 * len(sweep),
            "comparison": len(sweep),
            "sqlite-actor-saturation-sample": saturation_attempts,
            "sqlite-actor-saturation-summary": 1,
            "sqlite-observation-schema-proof": 1,
        }
    )
    if schema in {
        "plakous.perf-store.v3",
        "plakous.perf-store.v4",
        "plakous.perf-store.v5",
        "plakous.perf-store.v6",
    }:
        expected["sqlite-restart-memory-probe"] = len(sweep)
    if schema in {"plakous.perf-store.v5", "plakous.perf-store.v6"}:
        expected["sqlite-instrumented-commit-sample"] = sum(sweep)
        expected["sqlite-instrumented-summary"] = len(sweep)
    if counts != expected:
        fail("perf-store row counts disagree with metadata")
    object_limit = nonnegative_int(metadata, "assertions", "delta_object_limit")
    saturation_outcomes: Counter[str] = Counter()
    instrumented_turns: dict[int, list[int]] = {}
    instrumented_summaries: list[int] = []
    for row in rows[1:]:
        kind = row["kind"]
        if kind == "commit-sample":
            if nonnegative_int(row, "new_objects") > object_limit:
                fail("perf-store commit exceeded its portable object bound")
            turn = nonnegative_int(row, "turn")
            exact(row, turn, "branch_revision")
            exact(row, turn + 1, "accepted_semantic_mutation_ordinal")
            nonnegative_int(row, "elapsed_ns")
            if schema in {
                "plakous.perf-store.v3",
                "plakous.perf-store.v4",
                "plakous.perf-store.v5",
                "plakous.perf-store.v6",
            }:
                nonnegative_int(row, "publication_build_elapsed_ns")
                nonnegative_int(row, "publication_clone_elapsed_ns")
        elif kind == "summary":
            turns = nonnegative_int(row, "turns")
            exact(row, turns, "commit_samples")
            exact(row, turns + 1, "accepted_semantic_mutations")
            exact(row, True, "exact_recovery")
            expected_acks = turns + 1 if field(row, "backend") == "sqlite" else None
            exact(row, expected_acks, "sqlite_synchronous_full_commit_acknowledgments")
            exact(row, None, "sqlite_transaction_count")
        elif kind == "comparison":
            for name in (
                "same_final_revision",
                "same_retained_objects",
                "same_delta_objects",
            ):
                exact(row, True, name)
            if schema in {
                "plakous.perf-store.v4",
                "plakous.perf-store.v5",
                "plakous.perf-store.v6",
            }:
                for name in (
                    "retained_conservative_cas_object_and_edge_byte_delta_sqlite_minus_memory",
                    "new_conservative_cas_object_and_edge_byte_delta_sqlite_minus_memory",
                ):
                    value = field(row, name)
                    if isinstance(value, bool) or not isinstance(value, int):
                        fail(f"{name} must be an integer")
                field(row, "byte_equality_not_claimed_reason")
            else:
                for name in (
                    "same_retained_conservative_cas_object_and_edge_bytes",
                    "same_delta_conservative_cas_object_and_edge_bytes",
                ):
                    exact(row, True, name)
        elif kind == "sqlite-actor-saturation-sample":
            outcome = field(row, "public_store_outcome")
            if outcome not in {"ok_create_receipt", "error_backend_busy"}:
                fail("perf-store saturation emitted an unknown typed outcome")
            saturation_outcomes[outcome] += 1
        elif kind == "sqlite-actor-saturation-summary":
            attempts = nonnegative_int(row, "attempts")
            acknowledged = nonnegative_int(row, "durably_acknowledged_create_receipts")
            busy = nonnegative_int(row, "backend_busy_public_outcomes")
            maximum = nonnegative_int(row, "maximum_actor_operation_plus_queued_work")
            if acknowledged > maximum or busy < 1 or acknowledged + busy != attempts:
                fail(
                    "perf-store saturation summary violates the bounded actor invariant"
                )
            if acknowledged != saturation_outcomes["ok_create_receipt"]:
                fail("perf-store saturation receipt count disagrees with sample rows")
            if busy != saturation_outcomes["error_backend_busy"]:
                fail(
                    "perf-store saturation backpressure count disagrees with sample rows"
                )
        elif kind == "sqlite-observation-schema-proof":
            exact(row, 0, "observation_named_schema_object_count")
            exact(row, [], "observation_named_schema_objects")
            exact(row, False, "actual_sql_transaction_trace")
        elif kind == "sqlite-instrumented-commit-sample":
            if schema not in {"plakous.perf-store.v5", "plakous.perf-store.v6"}:
                fail("SQLite instrumented rows require perf-store v5")
            turn = positive_int(row, "turn")
            turns_in_trace = positive_int(row, "turns_in_trace")
            instrumented_turns.setdefault(turns_in_trace, []).append(turn)
            exact(row, turn, "branch_revision")
            exact(
                row,
                nonnegative_int(row, "new_objects"),
                "externally_revalidated_objects",
            )
            positive_int(row, "externally_revalidated_canonical_bytes")
            nonnegative_int(row, "external_object_revalidation_elapsed_ns")
            nonnegative_int(row, "instrumented_commit_elapsed_ns")
            if schema == "plakous.perf-store.v6":
                nonnegative_int(row, "actor_queue_wait_ns")
                exact(row, 1, "sqlite_transaction_begin_count")
                exact(row, 1, "sqlite_transaction_commit_count")
                nonnegative_int(row, "sqlite_transaction_elapsed_ns")
                nonnegative_int(row, "sqlite_commit_call_elapsed_ns")
                exact(
                    row,
                    nonnegative_int(row, "new_objects"),
                    "sqlite_inserted_runtime_objects",
                )
                exact(
                    row,
                    nonnegative_int(row, "new_conservative_cas_object_and_edge_bytes"),
                    "sqlite_inserted_conservative_cas_object_and_edge_bytes",
                )
                for name in (
                    "database_bytes",
                    "wal_bytes",
                    "database_growth_since_create_bytes",
                    "wal_growth_since_create_bytes",
                ):
                    nonnegative_int(row, name)
            semantic_row_total(row, "semantic_table_rows")
            growth = field(row, "semantic_table_row_growth_since_create")
            exact(growth, turn, "checkpoints")
            exact(growth, turn, "commit_receipts")
            nonnegative_int(growth, "runtime_objects")
            nonnegative_int(growth, "runtime_object_edges")
            nonnegative_int(growth, "total_rows_in_semantic_tables")
        elif kind == "sqlite-instrumented-summary":
            if schema not in {"plakous.perf-store.v5", "plakous.perf-store.v6"}:
                fail("SQLite instrumented summaries require perf-store v5")
            turns = positive_int(row, "turns")
            instrumented_summaries.append(turns)
            exact(row, turns, "final_branch_revision")
            exact(row, True, "semantic_table_rows_stable_across_reopen")
            semantic_row_total(row, "semantic_table_rows_before_reopen")
            semantic_row_total(row, "semantic_table_rows_after_reopen")
            exact(
                row,
                field(row, "semantic_table_rows_before_reopen"),
                "semantic_table_rows_after_reopen",
            )
            if schema == "plakous.perf-store.v6":
                nonnegative_int(row, "reopen_ns")
                load = nonnegative_int(row, "latest_checkpoint_load_ns")
                exact(row, load, "latest_checkpoint_load_and_decode_ns")
                scope = field(row, "decode_scope")
                if not isinstance(scope, str) or not scope:
                    fail("perf-store v6 must state its decode measurement scope")
        elif kind == "sqlite-restart-memory-probe":
            exact(row, True, "process_isolated_from_writer")
            nonnegative_int(row, "reopen_ns")
            nonnegative_int(row, "latest_checkpoint_load_ns")
            for name in (
                "rss_before_open_bytes",
                "rss_after_open_bytes",
                "rss_after_latest_load_and_recovery_bytes",
            ):
                value = field(row, name)
                if value is not None and (
                    isinstance(value, bool) or not isinstance(value, int) or value < 0
                ):
                    fail(f"{name} must be null or a non-negative integer")
    if schema in {"plakous.perf-store.v5", "plakous.perf-store.v6"}:
        if instrumented_summaries != sweep:
            fail("SQLite instrumented summaries must exactly follow the declared sweep")
        if set(instrumented_turns) != set(sweep):
            fail("SQLite instrumented traces disagree with the declared sweep")
        for turns in sweep:
            if instrumented_turns[turns] != list(range(1, turns + 1)):
                fail("SQLite instrumented trace turns are incomplete or out of order")


def validate_allocations(rows: list[dict[str, Any]]) -> None:
    metadata = rows[0]
    exact(metadata, "host-qualified-unratified", "qualification", "status")
    exact(metadata, False, "qualification", "portable_numeric_baseline")
    exact(metadata, "startup-inclusive", "qualification", "raw_totals")
    host_id = field(metadata, "host", "id")
    if (
        not isinstance(host_id, str)
        or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{2,127}", host_id) is None
    ):
        fail("allocation evidence requires a stable bounded host ID")
    exact(metadata, "Linux", "host", "os")
    glibc = field(metadata, "host", "glibc")
    if not isinstance(glibc, str) or not glibc.startswith("glibc "):
        fail("allocation evidence must identify glibc")
    tagged_sha256(metadata, "bindings", "source", "identity")
    tagged_sha256(metadata, "bindings", "toolchain", "rustc", "sha256")
    tagged_sha256(metadata, "bindings", "toolchain", "cargo", "sha256")
    tagged_sha256(metadata, "bindings", "memusage", "sha256")
    for benchmark in ("perf_core", "perf_project"):
        tagged_sha256(metadata, "bindings", "binaries", benchmark, "sha256")
        authority = field(metadata, "bindings", "builds", benchmark, "authority")
        if authority not in {"cargo-bench-no-run-json", "caller-supplied-prebuilt"}:
            fail("allocation benchmark build authority is unknown")
    expected_plan = {
        "core-event-round": ("perf_core", "event-round", [10, 100, 1000]),
        "project-round": ("perf_project", "project-round", [10, 100, 1000]),
        "family-artifact": (
            "perf_project",
            "family-artifact",
            [64, 65_536, 1_048_576],
        ),
        "tool-schema": ("perf_project", "tool-schema", [4, 32, 128]),
    }
    iterations = positive_int(metadata, "plan", "iterations_per_measured_process")
    pairs = positive_int(metadata, "plan", "pairs_per_scenario")
    workloads = field(metadata, "plan", "workloads")
    if not isinstance(workloads, dict) or set(workloads) != set(expected_plan):
        fail("allocation workload plan is not the closed workload matrix")
    for name, (benchmark, operation, scales) in expected_plan.items():
        exact(workloads, benchmark, name, "benchmark")
        exact(workloads, operation, name, "operation")
        scenarios = field(workloads, name, "scenarios")
        expected_scenarios = [
            {"name": label, "scale": scale}
            for label, scale in zip(("short", "medium", "long"), scales)
        ]
        if scenarios != expected_scenarios:
            fail("allocation scenario plan disagrees with the closed matrix")

    raw = [row for row in rows if row["kind"] == "raw-sample"]
    derived = [row for row in rows if row["kind"] == "derived-delta"]
    expected_pairs = len(expected_plan) * 3 * pairs
    if Counter(row["kind"] for row in rows) != Counter(
        {
            "metadata": 1,
            "raw-sample": expected_pairs * 2,
            "derived-delta": expected_pairs,
        }
    ):
        fail("allocation evidence row counts disagree with its plan")
    raw_by_pair: dict[str, list[dict[str, Any]]] = {}
    sample_ids: set[str] = set()
    for row in raw:
        pair_id = field(row, "pair_id")
        sample_id = field(row, "sample_id")
        if (
            not isinstance(pair_id, str)
            or not pair_id
            or not isinstance(sample_id, str)
        ):
            fail("allocation sample identities must be non-empty strings")
        if sample_id in sample_ids:
            fail("allocation sample identities must be unique")
        sample_ids.add(sample_id)
        raw_by_pair.setdefault(pair_id, []).append(row)
        workload = field(row, "workload")
        if workload not in expected_plan:
            fail("allocation row names an unknown workload")
        benchmark, operation, scales = expected_plan[workload]
        exact(row, benchmark, "benchmark")
        exact(row, operation, "operation")
        scenario = field(row, "scenario")
        if scenario not in {"short", "medium", "long"}:
            fail("allocation row names an unknown scenario")
        exact(row, scales[("short", "medium", "long").index(scenario)], "scale")
        count = nonnegative_int(row, "iteration_count")
        role = field(row, "pair_role")
        if (role, count) not in {
            ("zero-iteration-startup", 0),
            ("measured-iterations", iterations),
        }:
            fail("allocation raw row has an invalid zero/N role")
        metrics = field(row, "metrics")
        for key in (
            "allocation_calls",
            "heap_total_bytes",
            "heap_peak_bytes",
            "stack_peak_bytes",
        ):
            nonnegative_int(metrics, key)
        call_table = field(metrics, "call_table")
        if not isinstance(call_table, dict) or set(call_table) != {
            "malloc",
            "realloc",
            "calloc",
            "free",
        }:
            fail("allocation call table is incomplete")
        for call in call_table.values():
            nonnegative_int(call, "calls")
            nonnegative_int(call, "memory_bytes")
            exact(call, 0, "failed_calls")
        tagged_sha256(row, "stderr_sha256")
        confirmation = field(row, "probe_confirmation")
        exact(confirmation, benchmark, "bench")
        exact(confirmation, operation, "operation")
        exact(confirmation, count, "iterations")
        exact(confirmation, count, "completed_iterations")

    derived_by_pair = {field(row, "pair_id"): row for row in derived}
    if len(derived_by_pair) != expected_pairs or set(derived_by_pair) != set(
        raw_by_pair
    ):
        fail("allocation derived pairs are incomplete or duplicated")
    for pair_id, samples in raw_by_pair.items():
        if len(samples) != 2:
            fail(
                "allocation pair must contain exactly one zero and one measured sample"
            )
        by_role = {field(row, "pair_role"): row for row in samples}
        if set(by_role) != {"zero-iteration-startup", "measured-iterations"}:
            fail("allocation pair roles are incomplete")
        zero = by_role["zero-iteration-startup"]
        measured = by_role["measured-iterations"]
        for name in ("workload", "benchmark", "operation", "scenario", "scale"):
            exact(measured, field(zero, name), name)
        result = derived_by_pair[pair_id]
        exact(result, field(measured, "workload"), "workload")
        exact(result, field(measured, "scenario"), "scenario")
        exact(result, iterations, "iteration_count")
        exact(result, field(zero, "sample_id"), "zero_sample_id")
        exact(result, field(measured, "sample_id"), "measured_sample_id")
        for metric in ("allocation_calls", "heap_total_bytes", "heap_peak_bytes"):
            delta = field(measured, "metrics", metric) - field(zero, "metrics", metric)
            exact(result, delta, "deltas", metric)
            per_iteration = field(result, "per_iteration", metric)
            if isinstance(per_iteration, bool) or not isinstance(
                per_iteration, (int, float)
            ):
                fail("allocation per-iteration delta must be numeric")
            if per_iteration != delta / iterations:
                fail("allocation per-iteration delta is not recomputable")
        caveat = field(result, "peak_caveat")
        if not isinstance(caveat, str) or "non-additive" not in caveat:
            fail("allocation peak delta must retain its non-additive caveat")


VALIDATORS = {
    "plakous.perf-build.v2": validate_build,
    "plakous.perf-build.v3": validate_build,
    "plakous.perf-build.v4": validate_build,
    "plakous.perf-build.v5": validate_build,
    "plakous.perf-build.v6": validate_build,
    "plakous.perf-build.v7": validate_build,
    "plakous.perf-build.v8": validate_build,
    "plakous.perf-build.v9": validate_build,
    "plakous.perf-core.v2": validate_core,
    "plakous.perf-core.v3": validate_core,
    "plakous.perf-core.v4": validate_core,
    "plakous.perf-project.v3": validate_project,
    "plakous.perf-project.v4": validate_project,
    "plakous.perf-project.v5": validate_project,
    "plakous.perf-project.v6": validate_project,
    "plakous.perf-store.v2": validate_store,
    "plakous.perf-store.v3": validate_store,
    "plakous.perf-store.v4": validate_store,
    "plakous.perf-store.v5": validate_store,
    "plakous.perf-store.v6": validate_store,
    "plakous.perf-allocations.v1": validate_allocations,
}


def validate_build_campaign(
    directory: Path, schema: str, contracts: dict[str, tuple[Any, ...]]
) -> None:
    """Validate one exact comparable measured build cohort."""
    version = schema.rsplit(".", 1)[-1]
    expected_names = {f"{profile}.jsonl" for profile in contracts}
    actual_paths = {path.name: path for path in directory.iterdir() if path.is_file()}
    actual_names = set(actual_paths)
    if actual_names != expected_names:
        missing = sorted(expected_names - actual_names)
        extra = sorted(actual_names - expected_names)
        fail(
            f"{version} campaign filenames disagree: "
            f"missing={missing!r}, extra={extra!r}"
        )
    common_paths = (
        ("baseline", "identity"),
        ("fixture",),
        ("host",),
        ("git",),
        ("toolchain",),
        ("cargo", "target"),
        ("cargo", "cargo_profile"),
        ("cargo", "jobs"),
        ("cargo", "target_unit_profile_settings"),
        ("controlled_environment",),
        ("rustflags",),
        ("removed_environment_names",),
        ("removed_environment_patterns",),
        ("sample_plan",),
    )
    common: dict[tuple[str, ...], Any] = {}
    for profile in sorted(contracts):
        path = actual_paths[f"{profile}.jsonl"]
        rows = load_rows(path)
        if field(rows[0], "schema") != schema:
            fail(f"{version} campaign file is not {version}: {path.name}")
        validate_build(rows)
        metadata = rows[0]
        exact(metadata, profile, "cargo", "build_profile")
        exact(metadata, "build", "cargo", "operation")
        if any(row["kind"] == "dry-run" for row in rows):
            fail(f"{version} campaign requires measured samples: {path.name}")
        for value_path in common_paths:
            value = field(metadata, *value_path)
            if value_path not in common:
                common[value_path] = value
            elif value != common[value_path]:
                fail(
                    f"{version} campaign common field differs: "
                    f"{'.'.join(value_path)} in {path.name}"
                )


def validate_v5_campaign(directory: Path) -> None:
    """Validate the immutable 23-profile pre-consolidation v5 cohort."""
    validate_build_campaign(
        directory, "plakous.perf-build.v5", V5_BUILD_PROFILE_CONTRACTS
    )


def validate_v6_campaign(directory: Path) -> None:
    """Validate the immutable 22-profile post-agent-consolidation v6 cohort."""
    validate_build_campaign(
        directory, "plakous.perf-build.v6", V6_BUILD_PROFILE_CONTRACTS
    )


def validate_v7_campaign(directory: Path) -> None:
    """Validate the immutable 21-profile post-dispatch-consolidation v7 cohort."""
    validate_build_campaign(
        directory, "plakous.perf-build.v7", V7_BUILD_PROFILE_CONTRACTS
    )


def validate_v8_campaign(directory: Path) -> None:
    """Validate the immutable 20-profile post-product-consolidation v8 cohort."""
    validate_build_campaign(
        directory, "plakous.perf-build.v8", V8_BUILD_PROFILE_CONTRACTS
    )


def validate_v9_campaign(directory: Path) -> None:
    """Validate the current 19-profile post-paid-platform-removal v9 cohort."""
    validate_build_campaign(
        directory, "plakous.perf-build.v9", V9_BUILD_PROFILE_CONTRACTS
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="*", type=Path)
    parser.add_argument(
        "--campaign-v5",
        type=Path,
        metavar="DIRECTORY",
        help="validate the exact 23-profile measured perf-build v5 cohort",
    )
    parser.add_argument(
        "--campaign-v6",
        type=Path,
        metavar="DIRECTORY",
        help="validate the exact 22-profile measured perf-build v6 cohort",
    )
    parser.add_argument(
        "--campaign-v7",
        type=Path,
        metavar="DIRECTORY",
        help="validate the exact 21-profile measured perf-build v7 cohort",
    )
    parser.add_argument(
        "--campaign-v8",
        type=Path,
        metavar="DIRECTORY",
        help="validate the exact 20-profile measured perf-build v8 cohort",
    )
    parser.add_argument(
        "--campaign-v9",
        type=Path,
        metavar="DIRECTORY",
        help="validate the exact 19-profile measured perf-build v9 cohort",
    )
    args = parser.parse_args()
    modes = (
        bool(args.paths)
        + bool(args.campaign_v5)
        + bool(args.campaign_v6)
        + bool(args.campaign_v7)
        + bool(args.campaign_v8)
        + bool(args.campaign_v9)
    )
    if modes != 1:
        parser.error("provide paths or exactly one campaign directory")
    try:
        if args.campaign_v5:
            validate_v5_campaign(args.campaign_v5)
            print(
                f"validated {args.campaign_v5}: plakous.perf-build.v5 campaign, "
                f"{len(V5_BUILD_PROFILE_CONTRACTS)} profiles"
            )
        elif args.campaign_v6:
            validate_v6_campaign(args.campaign_v6)
            print(
                f"validated {args.campaign_v6}: plakous.perf-build.v6 campaign, "
                f"{len(V6_BUILD_PROFILE_CONTRACTS)} profiles"
            )
        elif args.campaign_v7:
            validate_v7_campaign(args.campaign_v7)
            print(
                f"validated {args.campaign_v7}: plakous.perf-build.v7 campaign, "
                f"{len(V7_BUILD_PROFILE_CONTRACTS)} profiles"
            )
        elif args.campaign_v8:
            validate_v8_campaign(args.campaign_v8)
            print(
                f"validated {args.campaign_v8}: plakous.perf-build.v8 campaign, "
                f"{len(V8_BUILD_PROFILE_CONTRACTS)} profiles"
            )
        elif args.campaign_v9:
            validate_v9_campaign(args.campaign_v9)
            print(
                f"validated {args.campaign_v9}: plakous.perf-build.v9 campaign, "
                f"{len(V9_BUILD_PROFILE_CONTRACTS)} profiles"
            )
        else:
            for path in args.paths:
                rows = load_rows(path)
                schema = rows[0]["schema"]
                VALIDATORS[schema](rows)
                print(f"validated {path}: {schema}, {len(rows)} rows")
    except (OSError, ValidationError) as error:
        parser.exit(1, f"perf JSONL validation failed: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
