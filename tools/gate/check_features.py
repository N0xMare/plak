#!/usr/bin/env python3
"""Validate and optionally build linear public feature witnesses per package.

Every workspace package with at least one public optional feature gets its own
closure run: none, each singleton, full minus each feature, default and full
(2n+3 profiles, n capped at 24). Closures are never cross-multiplied
across packages: the layer matrix guarantees a host/bin crate reaches L0
features only through `plakous/*`, so the facade's own run already covers the
L0 side and each higher crate's run covers its side.

The expected closure is walked from the package's own feature table over the
workspace's normal dependency edges (from `cargo metadata`, no hardcoded edge
map) and compared to `cargo tree`. All packages use the same witness scheme.
Named exclusion profiles independently pin complete external closures.

`EXCLUDED_CLOSURE_PACKAGES` additionally pins, over the complete external
closure Cargo reports, the packages a named feature profile must not reach.
This is what makes the facade's evaluator decoupling provable: `composition`
buys Code Mode and the JavaScript sidecar and nothing else.

Normally, thirteen `cargo check --lib` profiles prove feature-conditioned
code: eleven facade resolutions and hosted without optional features and
with `--all-features`. A rustc warning or error fails the run. Every public
feature still has its closure witness.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))

import vcs_listing  # noqa: E402

from plakous_policy import (  # noqa: E402
    ABOVE_FACADE_LAYERS,
    Workspace,
    blank_rust_non_code,
    load_workspace,
)

ROOT = Path(__file__).resolve().parents[2]
MAX_OPTIONAL_FEATURES = 24
FACADE = "plakous"


def cache_path(root: Path = ROOT) -> Path:
    return root / (os.environ.get("CARGO_TARGET_DIR") or "target") / "feature-closure-cache.json"


def cache_digest(root: Path = ROOT) -> str:
    """Hash Cargo's inputs, including the tracked manifest inventory."""
    version = subprocess.check_output(["cargo", "--version"], cwd=root, text=True)
    tracked = "\0".join(vcs_listing.listed_files(root))
    paths = {Path(name) for name in tracked.split("\0") if name and Path(name).name == "Cargo.toml"}
    paths.update(
        Path(name)
        for name in tracked.split("\0")
        if Path(name).parent.name == ".cargo" and Path(name).name.startswith("config")
    )
    paths.update(path.relative_to(root) for path in root.glob(".cargo/config*") if path.is_file())
    paths.update(
        map(
            Path,
            (
                "Cargo.lock",
                "rust-toolchain.toml",
                "tools/gate/check_features.py",
                "tools/gate/plakous_policy.py",
            ),
        )
    )
    # Metadata contains absolute manifest paths, so it cannot cross checkouts.
    digest = hashlib.sha256(json.dumps([str(root.resolve()), version]).encode())
    for path in sorted(paths):
        content = (root / path).read_bytes() if (root / path).is_file() else None
        digest.update(
            json.dumps(
                [str(path), None if content is None else hashlib.sha256(content).hexdigest()]
            ).encode()
        )
    return digest.hexdigest()


class CargoOutputCache:
    """Memoize successful Cargo queries, never validation results."""

    def __init__(self, path: Path, digest: str):
        self.path = path
        self.digest = digest
        self.outputs: dict[str, str] = {}
        self.hit = False
        self.dirty = False
        try:
            cached = json.loads(path.read_text())
            outputs = cached["outputs"]
            if (
                cached["digest"] == digest
                and isinstance(outputs, dict)
                and all(
                    isinstance(key, str) and isinstance(value, str)
                    for key, value in outputs.items()
                )
                and cached["checksum"] == self.checksum(outputs)
            ):
                self.outputs = outputs
                self.hit = True
        except (OSError, ValueError, KeyError, TypeError):
            pass

    @staticmethod
    def checksum(outputs: dict[str, str]) -> str:
        return hashlib.sha256(json.dumps(outputs, sort_keys=True).encode()).hexdigest()

    def output(self, command: list[str], root: Path, capture: bool = False) -> str:
        key = json.dumps([str(root.resolve()), command])
        if key not in self.outputs:
            self.outputs[key] = run_cargo_output(command, root, capture)
            self.dirty = True
        return self.outputs[key]

    def save(self) -> None:
        if self.hit and not self.dirty:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w", dir=self.path.parent, delete=False
            ) as stream:
                temporary = Path(stream.name)
                json.dump(
                    {
                        "digest": self.digest,
                        "outputs": self.outputs,
                        "checksum": self.checksum(self.outputs),
                    },
                    stream,
                )
            os.replace(temporary, self.path)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)


CARGO_OUTPUTS: CargoOutputCache | None = None


def run_cargo_output(command: list[str], root: Path, capture: bool = False) -> str:
    if not capture:
        return subprocess.check_output(command, cwd=root, text=True)
    # Suppress Cargo's concurrent package-cache lock noise unless a query fails.
    result = subprocess.run(command, cwd=root, text=True, capture_output=True, check=False)
    if result.returncode:
        sys.stderr.write(result.stderr)
        result.check_returncode()
    return result.stdout


def cargo_output(command: list[str], root: Path = ROOT, capture: bool = False) -> str:
    if CARGO_OUTPUTS is None:
        return run_cargo_output(command, root, capture)
    return CARGO_OUTPUTS.output(command, root, capture)


def cached_workspace(root: Path = ROOT) -> Workspace:
    graph = json.loads(
        cargo_output(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"],
            root,
        )
    )
    return load_workspace(root, graph)


REQUIRED_DEPENDENCY_FEATURE_EDGES = {
    ("plakous", "mcp", "plakous-mcp/client"),
    ("plakous", "test-support", "plakous-openai?/test-support"),
    ("plakous", "test-support", "plakous-openrouter?/test-clock"),
    ("plakous", "test-support", "plakous-cell?/test-hooks"),
    ("plakous", "test-support", "plakous-agent/test-hooks"),
    ("plakd", "test-support", "plakous/test-support"),
    ("plakous", "claude", "plakous-anthropic/native-auth"),
    ("plakous", "codex", "plakous-auth/http-refresh"),
    ("plakous", "grok", "plakous-auth/http-refresh"),
    ("plakous", "claude", "plakous-auth/http-refresh"),
    ("plakous", "codex", "plakous-openai/native-auth"),
    ("plakous", "grok", "plakous-xai/native-auth"),
    ("plakous", "sessions-sqlite", "plakous-store/sqlite"),
    ("plakous", "gateway", "plakous-auth/gateway"),
    ("plakous", "gateway", "plakous-http/gateway-client"),
    ("plakous", "gateway", "plakous-http/gateway-server"),
    ("plakous-auth", "gateway", "plakous-http/gateway-client"),
    # Local fetch is independent of provider-hosted tools and composition.
    ("plakous", "web-fetch", "plakous-tool-web/fetch"),
    ("plakous", "anthropic", "plakous-tool-web/image-gen"),
    ("plakous", "codex", "plakous-tool-web/image-gen"),
    ("plakous", "grok", "plakous-tool-web/image-gen"),
    ("plakous", "claude", "plakous-tool-web/image-gen"),
    ("plakous", "openai", "plakous-tool-web/image-gen"),
    ("plakous", "openrouter", "plakous-tool-web/image-gen"),
    ("plakous", "xai", "plakous-tool-web/image-gen"),
    ("plakous-anthropic", "native-auth", "plakous-auth/http-refresh"),
    ("plakous-openai", "native-auth", "plakous-auth/http-refresh"),
    ("plakous-openai", "native-auth", "plakous-auth/jwt"),
    ("plakous-xai", "native-auth", "plakous-auth/http-refresh"),
    ("plakous-xai", "native-auth", "plakous-auth/jwt"),
    # `FakeVcs` and the real-repository fixtures the tree's tests drive live in
    # `plakous-workspace`; `plakous-hosted`'s own test-support turns them on.
    # `MemoryStore` is the hosted session-record double `plakous-hosted`'s own
    # tests drive; it lives with the port it implements.
    ("plakous-hosted", "test-support", "plakous-recursive-subagents/test-support"),
    # `plakd`'s repository-gate offline model registers a scripted RuntimeFactory
    # through this edge; the production construction port is always available,
    # but the scripted factory belongs to the hosted test kit.
    ("plakd", "test-support", "plakous-hosted/test-support"),
    ("plakd", "test-support", "plakous-recursive-subagents-store/test-support"),
    # Content-free checkpoint classes are a repository-only tracing seam.
    ("plakous-store", "test-support", "plakous-agent/test-hooks"),
    (
        "plakous-recursive-subagents-store",
        "test-support",
        "plakous-store/test-support",
    ),
    # The repository harness's extra enrolled Code Mode target. Forwarded
    # unchanged through the facade; each hop is pinned so the enrolment cannot
    # arrive by accident.
    ("plakous-hosted", "test-support", "plakous-workspace/test-support"),
    # The tree's tests mint real copies through `FakeVcs`, and the store's
    # conformance suite runs over the tree's own `MemoryTreeStore`.
    ("plakous-recursive-subagents", "test-support", "plakous-host-protocol/test-support"),
    ("plakous-recursive-subagents", "test-support", "plakous-workspace/test-support"),
    (
        "plakous-recursive-subagents-store",
        "test-support",
        "plakous-recursive-subagents/test-support",
    ),
}
# JJ is optional in the host and product; Git is an unconditional dependency
# feature there and remains selectable in the workspace and tree crates.
REQUIRED_DEPENDENCY_FEATURE_EDGES.update(
    (
        package,
        "jj" if package == "plakd" and feature == "jj-lib" else feature,
        f"{dependency}/{feature}",
    )
    for package, dependencies in {
        "plakd": [
            "plakous-hosted",
            "plakous-recursive-subagents",
            "plakous-recursive-subagents-store",
        ],
        "plakous-hosted": ["plakous-workspace", "plakous-recursive-subagents"],
        "plakous-recursive-subagents": ["plakous-workspace"],
        "plakous-recursive-subagents-store": ["plakous-recursive-subagents"],
    }.items()
    for dependency in dependencies
    for feature in (
        ("jj-lib",) if package in {"plakd", "plakous-hosted"} else ("git-cli", "jj-lib")
    )
)
# Named feature profiles and the packages their complete closure must exclude.
# `composition` is the cell client on its own: no credential, HTTP,
# subscription or interpreter machinery for either source language.
REPOSITORY_ONLY_PACKAGES = (
    "plakous-offline-crash-harness",
    "plakous-test-support",
    "plakous-mcp-fake-server",
)
EXCLUDED_CLOSURE_PACKAGES = {
    ("plakd", ("default",)): (
        "micro-monty",
        "plakous-js-runtime",
        "rquickjs",
        "jj-lib",
        "gix",
        "pollster",
        "aws-lc-rs",
        "aws-lc-sys",
        "rsa",
        "ratatui",
        "crossterm",
        "syntect",
        "arboard",
        "pulldown-cmark",
        *REPOSITORY_ONLY_PACKAGES,
    ),
    ("plakd", ("maintainer",)): REPOSITORY_ONLY_PACKAGES,
    ("plakous", ("full",)): ("aws-lc-rs", "aws-lc-sys", "micro-monty", "rsa"),
    ("plakous", ("codex",)): (
        "aws-lc-rs",
        "aws-lc-sys",
        "rsa",
        "plakous-xai",
        "plakous-anthropic",
        "micro-monty",
        "htmd",
    ),
    ("plakous", ("grok",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous", ("claude",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous-openai", ("native-auth",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous-xai", ("native-auth",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakd", ()): ("tungstenite", "tokio-tungstenite"),
    ("plakous", ("composition",)): (
        "htmd",
        "micro-monty",
        "plakous-auth",
        "plakous-http",
        "plakous-js-runtime",
        "plakous-py-worker",
        "rquickjs",
    ),
    ("plakous", ("composition", "sessions-sqlite")): (
        "plakous-auth",
        "plakous-http",
        "micro-monty",
    ),
    # Credential-store, MCP and Claude closures never acquire JWT crypto.
    ("plakous", ("auth",)): ("jsonwebtoken", "plakous-openai", "plakous-xai"),
    ("plakous", ("openai",)): ("htmd", "jsonwebtoken"),
    ("plakous", ("web-fetch",)): (
        "plakous-openai",
        "plakous-xai",
        "plakous-anthropic",
        "plakous-openrouter",
    ),
    ("plakous", ("mcp",)): ("jsonwebtoken",),
    # The gateway client and server core link no provider and no JWT crypto.
    ("plakous", ("gateway",)): (
        "jsonwebtoken",
        "htmd",
        "plakous-anthropic",
        "plakous-openai",
        "plakous-openrouter",
        "plakous-xai",
    ),
    ("plakous", ("anthropic",)): ("jsonwebtoken",),
}

# Repository-only capabilities of a dependency, keyed by the product and the
# product feature that alone may turn them on. `PLAK_TEST_FAULTS` transport
# fault injection must be inert in every default and release `plakd`, and live
# under `plakd/test-support`, where the live smoke depends on it.
TEST_ONLY_DEPENDENCY_FEATURES = {
    ("plakd", "test-support"): (
        ("plakous-http", "fault-injection"),
        ("plakous", "test-support"),
        ("plakous-agent", "test-hooks"),
        ("plakous-openai", "test-support"),
        ("plakous-openrouter", "test-clock"),
        ("plakous-store", "test-support"),
        ("plakous-recursive-subagents-store", "test-support"),
        ("plakous-hosted", "test-support"),
    ),
    ("plakous-recursive-subagents-store", "test-support"): (("plakous-store", "test-support"),),
}


def resolved_dependency_features(
    package: str, product_features: tuple[str, ...], dependency: str
) -> set[str]:
    """Features Cargo enables on `dependency` in a normal build of `package`."""
    command = cargo_command("tree", product_features, True, package)
    command.extend(["-e", "normal,build", "--prefix", "none", "--format", "{p} {f}"])
    output = cargo_output(command)
    features: set[str] = set()
    for line in output.splitlines():
        if line.split(" ", maxsplit=1)[0] != dependency or ") " not in line:
            continue
        listed = line.split(") ", maxsplit=1)[1].replace("(*)", "").strip()
        features.update(feature for feature in listed.split(",") if feature)
    return features


def validate_test_only_dependency_features() -> list[str]:
    """A default build never enables a test-only dependency feature; its gate does."""
    errors: list[str] = []
    for (package, gate), pins in sorted(TEST_ONLY_DEPENDENCY_FEATURES.items()):
        for dependency, feature in pins:
            profiles = ((), ("maintainer",)) if package == "plakd" else ((),)
            for profile in profiles:
                if feature in resolved_dependency_features(package, profile, dependency):
                    name = ",".join(profile) or "default"
                    errors.append(
                        f"{package} [{name}]: enables {dependency}/{feature}, which only "
                        f"`{gate}` may enable"
                    )
            if feature not in resolved_dependency_features(package, (gate,), dependency):
                errors.append(f"{package} [{gate}]: no longer enables {dependency}/{feature}")
    for package in (
        "plakous-anthropic",
        "plakous-openrouter",
        "plakous-xai",
    ):
        if "test-clock" in resolved_dependency_features(package, (), package):
            errors.append(f"{package} [default]: test-clock is test-only")
    for package, profiles in (
        ("plakous", ((), ("full",))),
        ("plakous-openai", ((),)),
        ("plakous-tools", ((),)),
    ):
        for profile in profiles:
            if "test-support" in resolved_dependency_features(package, profile, package):
                errors.append(
                    f"{package} [{','.join(profile) or 'default'}]: test-support is test-only"
                )
    for package in ("plakous-cell", "plakous-agent"):
        if "test-hooks" in resolved_dependency_features(package, (), package):
            errors.append(f"{package} [default]: test-hooks is test-only")
    # The Grok and Claude review-anchored clocks reach only offline test
    # builds (facade dev-dependencies): no plakd build, `test-support` live
    # smoke builds included, may resolve them.
    for profile in PLAK_PROFILES_WITHOUT_ANCHORED_CLOCKS:
        for dependency in ANCHORED_CLOCK_PROVIDERS:
            if "test-clock" in resolved_dependency_features("plakd", profile, dependency):
                errors.append(
                    f"plakd [{','.join(profile) or 'default'}]: enables {dependency}/test-clock, "
                    "which only offline test builds may enable"
                )
    return errors


ANCHORED_CLOCK_PROVIDERS = ("plakous-anthropic", "plakous-xai")
PLAK_PROFILES_WITHOUT_ANCHORED_CLOCKS = (
    (),
    ("maintainer",),
    ("test-support",),
    ("test-support", "maintainer"),
)


def full_closure(package: str, selected: tuple[str, ...]) -> set[str]:
    """Return every package Cargo builds for one no-default feature profile."""
    command = cargo_command("tree", selected, False, package)
    command.extend(["-e", "normal,build", "--prefix", "none", "--format", "{p}"])
    output = cargo_output(command)
    return {line.split(" ", maxsplit=1)[0] for line in output.splitlines() if line.strip()}


def validate_excluded_closures() -> list[str]:
    """Check that each named profile's complete closure omits its excluded packages."""
    errors: list[str] = []
    for (package, selected), excluded in sorted(EXCLUDED_CLOSURE_PACKAGES.items()):
        reached = full_closure(package, selected)
        present = sorted(name for name in excluded if name in reached)
        if present:
            errors.append(
                f"{package} [{'+'.join(selected)}]: closure reaches {present!r}, "
                "which this profile must not link"
            )
    return errors


def load_workspace_manifests(root: Path = ROOT) -> dict[str, dict[str, object]]:
    """Load every workspace member manifest by the path Cargo reports."""
    return {name: package.manifest for name, package in cached_workspace(root).packages.items()}


def dependency_package_name(name: str, value: object) -> str:
    """Resolve a dependency table key to its Cargo package name."""
    if isinstance(value, dict) and isinstance(value.get("package"), str):
        return value["package"]
    return name


def dependency_tables(
    manifest: dict[str, object],
) -> list[tuple[str, str | None, dict[str, object]]]:
    """Return every root and target-specific Cargo dependency table."""
    tables: list[tuple[str, str | None, dict[str, object]]] = []
    section_kinds = (
        ("dependencies", "normal"),
        ("dev-dependencies", "dev"),
        ("build-dependencies", "build"),
    )
    for section, kind in section_kinds:
        values = manifest.get(section, {})
        if isinstance(values, dict):
            tables.append((kind, None, values))
    targets = manifest.get("target", {})
    if isinstance(targets, dict):
        for target, target_value in sorted(targets.items()):
            if not isinstance(target, str) or not isinstance(target_value, dict):
                continue
            for section, kind in section_kinds:
                values = target_value.get(section, {})
                if isinstance(values, dict):
                    tables.append((kind, target, values))
    return tables


def uuid_feature_findings(
    manifest: dict[str, object],
    inherited: dict[str, object],
    sources: dict[str, str],
) -> list[str]:
    """Require local UUID generation features, not another member's unification."""
    declared: set[str] = set()
    aliases = {"uuid"}
    for _kind, _target, table in dependency_tables(manifest):
        for alias, value in table.items():
            local = value if isinstance(value, dict) else {}
            shared = inherited.get(alias, {}) if local.get("workspace") else {}
            shared = shared if isinstance(shared, dict) else {}
            if dependency_package_name(alias, {**shared, **local}) != "uuid":
                continue
            aliases.add(alias.replace("-", "_"))
            declared.update(shared.get("features", []))
            declared.update(local.get("features", []))

    errors: list[str] = []
    for path, source in sorted(sources.items()):
        code = blank_rust_non_code(source)
        names = {"Uuid"}
        for alias in aliases:
            names.update(
                re.findall(
                    rf"\b{re.escape(alias)}\s*::\s*Uuid\s+as\s+(\w+)",
                    code,
                )
            )
        constructors = set(
            re.findall(
                rf"\b(?:{'|'.join(map(re.escape, sorted(names)))})\s*::\s*"
                r"(new_v[1-8]|now_v[167])\b",
                code,
            )
        )
        for constructor in sorted(constructors):
            feature = constructor.rsplit("_", 1)[1]
            if feature not in declared:
                errors.append(
                    f"{path}: UUID constructor {constructor} requires uuid feature "
                    f"{feature} in this package"
                )
    return errors


def validate_uuid_features(workspace: Workspace) -> list[str]:
    """Inspect each package's Rust targets, including test-only constructors."""
    inherited = workspace.root_manifest.get("workspace", {}).get("dependencies", {})
    errors: list[str] = []
    for package in workspace.packages.values():
        root = package.manifest_path.parent
        paths = {
            path
            for directory in ("src", "tests", "benches", "examples")
            for path in (root / directory).rglob("*.rs")
        }
        if (root / "build.rs").is_file():
            paths.add(root / "build.rs")
        sources = {path.relative_to(root).as_posix(): path.read_text() for path in paths}
        errors.extend(
            f"{package.name}: {finding}"
            for finding in uuid_feature_findings(package.manifest, inherited, sources)
        )
    return errors


def direct_workspace_edges(
    manifests: dict[str, dict[str, object]],
) -> dict[str, set[str]]:
    """Extract direct, normal, nonoptional internal dependency edges."""
    packages = set(manifests)
    result: dict[str, set[str]] = {}
    for package, manifest in manifests.items():
        dependency_items = [
            (name, value)
            for kind, _target, table in dependency_tables(manifest)
            if kind == "normal"
            for name, value in table.items()
        ]
        result[package] = {
            dependency_package_name(name, value)
            for name, value in dependency_items
            if dependency_package_name(name, value) in packages
            and not (isinstance(value, dict) and value.get("optional") is True)
        }
    return result


def dependency_feature_edges(
    manifests: dict[str, dict[str, object]],
) -> set[tuple[str, str, str]]:
    """Extract explicit feature forwarding across internal dependencies."""
    packages = set(manifests)
    result: set[tuple[str, str, str]] = set()
    for package, manifest in manifests.items():
        features = manifest.get("features", {})
        if not isinstance(features, dict):
            continue
        dependency_packages = {
            name: dependency_package_name(name, value)
            for _kind, _target, table in dependency_tables(manifest)
            for name, value in table.items()
        }
        for feature, edges in features.items():
            if not isinstance(feature, str) or not isinstance(edges, list):
                continue
            for edge in edges:
                if not isinstance(edge, str) or "/" not in edge:
                    continue
                dependency = edge.split("/", 1)[0].removesuffix("?")
                if dependency_packages.get(dependency) in packages:
                    result.add((package, feature, edge))
    return result


def validate_feature_contracts(
    manifests: dict[str, dict[str, object]],
    layers: dict[str, str] | None = None,
) -> list[str]:
    """Check required feature forwarding and layer forwarding rules.

    `layers` (package -> layer) activates the host/bin
    rule: a feature of a host/bin package forwards only to `plakous/<feature>`
    or to another host package, never to a `plakous-<x>/<feature>`.
    """
    errors: list[str] = []
    actual_feature_edges = dependency_feature_edges(manifests)
    if actual_feature_edges != REQUIRED_DEPENDENCY_FEATURE_EDGES:
        errors.append(
            "dependency feature-forwarding edges "
            f"{sorted(actual_feature_edges)!r}, expected "
            f"{sorted(REQUIRED_DEPENDENCY_FEATURE_EDGES)!r}"
        )
    if layers is not None:
        for package, feature, edge in sorted(actual_feature_edges):
            if layers.get(package) not in ABOVE_FACADE_LAYERS:
                continue
            target = edge.split("/", 1)[0].removesuffix("?")
            # The sole repository-only host -> store exception shares the
            # process tracer; the manifest gate is pinned by check_workspace.
            if (package, feature, edge) == (
                "plakous-recursive-subagents-store",
                "test-support",
                "plakous-store/test-support",
            ):
                continue
            if target != "plakous" and layers.get(target) != "host":
                errors.append(
                    f"{package}: feature edge {edge!r} forwards below the facade; host/bin "
                    "features forward only to plakous/<feature> or a host crate"
                )
    return errors


def optional_features(manifest: dict[str, object]) -> list[str]:
    """Return a package's public optional features (`default` and `full` aside)."""
    features = manifest.get("features", {})
    if not isinstance(features, dict):
        return []
    hidden = {"default", "full", "test-clock"}
    # Test-only library seams are hidden like test-clock. Product and worker
    # fixtures keep their witnesses.
    package = manifest.get("package")
    name = package.get("name") if isinstance(package, dict) else None
    if name in {"plakous", "plakous-tools"}:
        hidden.add("test-support")
    if name in {"plakous-cell", "plakous-agent"}:
        hidden.add("test-hooks")
    return sorted(feature for feature in features if feature not in hidden)


def workspace_package_names(root: Path = ROOT) -> set[str]:
    """Derive the complete internal package inventory from locked metadata."""
    output = cargo_output(
        [
            "cargo",
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--no-deps",
        ],
        root,
    )
    metadata = json.loads(output)
    packages = {package["id"]: package["name"] for package in metadata["packages"]}
    return {packages[package_id] for package_id in metadata["workspace_members"]}


def feature_sets(
    features: list[str], has_full: bool = True
) -> list[tuple[str, tuple[str, ...], bool]]:
    """Return exactly 2n+3 witnesses without multiplying package selections."""
    profiles: list[tuple[str, tuple[str, ...], bool]] = [("none", (), False)]
    profiles.extend((feature, (feature,), False) for feature in features)
    profiles.extend(
        (f"full-minus-{feature}", tuple(other for other in features if other != feature), False)
        for feature in features
    )
    profiles.append(("default", (), True))
    profiles.append(("full", ("full",) if has_full else tuple(features), False))
    return profiles


def closure_packages(manifests: dict[str, dict[str, object]]) -> list[str]:
    """Return the packages that get their own closure run, in name order."""
    return sorted(name for name, manifest in manifests.items() if optional_features(manifest))


def _dependency_declarations(manifest: dict[str, object]) -> dict[str, dict[str, object]]:
    """Return alias -> normal dependency declaration (all targets, normalized)."""
    declarations: dict[str, dict[str, object]] = {}
    for kind, _target, table in dependency_tables(manifest):
        if kind != "normal":
            continue
        for alias, value in table.items():
            declaration = {"version": value} if isinstance(value, str) else dict(value)
            declaration["package"] = dependency_package_name(alias, value)
            declarations.setdefault(alias, declaration)
    return declarations


def expected_closure(
    root: str,
    selected: tuple[str, ...],
    default: bool,
    manifests: dict[str, dict[str, object]],
) -> set[str]:
    """Walk one package's feature selection over the workspace dependency graph.

    Cargo's unification for a single selected package: the root's selected
    features (plus `default` when enabled) are resolved through its feature
    table; `dep:` edges and `pkg/feature` edges activate optional internal
    dependencies; each included dependency then contributes its non-optional
    normal internal edges, the features its edge requests, and its `default`
    unless the edge disables it. Weak `pkg?/feature` edges enable the feature
    only once `pkg` is included. External packages are ignored.
    """
    internal = set(manifests)
    declarations = {
        name: _dependency_declarations(manifest) for name, manifest in manifests.items()
    }
    feature_tables = {
        name: (manifest.get("features", {}) if isinstance(manifest.get("features"), dict) else {})
        for name, manifest in manifests.items()
    }
    included: set[str] = {root}
    enabled: dict[str, set[str]] = {name: set() for name in manifests}
    weak: list[tuple[str, str, str]] = []
    pending: list[tuple[str, str]] = []

    def include(alias_owner: str, alias: str) -> None:
        declaration = declarations[alias_owner].get(alias)
        if declaration is None:
            return
        package = str(declaration["package"])
        if package not in internal:
            return
        first = package not in included
        included.add(package)
        if first:
            for other_alias, other in declarations[package].items():
                if other.get("optional") is not True:
                    include(package, other_alias)
        if (
            declaration.get("default-features", True) is not False
            and "default" in feature_tables[package]
        ):
            pending.append((package, "default"))
        for feature in declaration.get("features", []) or []:
            pending.append((package, str(feature)))

    def enable(package: str, feature: str) -> None:
        if feature in enabled[package]:
            return
        table = feature_tables[package]
        if feature not in table:
            # An implicit optional-dependency feature (`optional = true` without `dep:`).
            include(package, feature)
            enabled[package].add(feature)
            return
        enabled[package].add(feature)
        for edge in table[feature]:
            if not isinstance(edge, str):
                continue
            if edge.startswith("dep:"):
                include(package, edge.removeprefix("dep:"))
            elif "/" in edge:
                alias, _, dependency_feature = edge.partition("/")
                if alias.endswith("?"):
                    weak.append((package, alias.removesuffix("?"), dependency_feature))
                else:
                    include(package, alias)
                    target = declarations[package].get(alias)
                    if target is not None and str(target["package"]) in internal:
                        pending.append((str(target["package"]), dependency_feature))
            else:
                pending.append((package, edge))

    for alias, declaration in declarations[root].items():
        if declaration.get("optional") is not True:
            include(root, alias)
    if default and "default" in feature_tables[root]:
        pending.append((root, "default"))
    if selected == ("full",):
        pending.append((root, "full"))
    else:
        pending.extend((root, feature) for feature in selected)

    while pending or weak:
        while pending:
            package, feature = pending.pop()
            enable(package, feature)
        remaining: list[tuple[str, str, str]] = []
        for package, alias, dependency_feature in weak:
            target = declarations[package].get(alias)
            target_package = None if target is None else str(target["package"])
            if target_package in included:
                pending.append((target_package, dependency_feature))
            else:
                remaining.append((package, alias, dependency_feature))
        if len(remaining) == len(weak) and not pending:
            break
        weak = remaining
    return included


def cargo_command(
    subcommand: str,
    selected: tuple[str, ...],
    default: bool,
    package: str = "plakous",
) -> list[str]:
    """Build one locked Cargo command for a package feature profile."""
    command = ["cargo", subcommand, "--locked", "--offline", "-p", package]
    if not default:
        command.append("--no-default-features")
    if selected:
        command.extend(["--features", ",".join(selected)])
    return command


def compiler_diagnostics(stdout: str) -> list[str]:
    """Return the rendered rustc warnings and errors in Cargo's JSON messages."""
    rendered = []
    for line in stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if not isinstance(message, dict) or message.get("reason") != "compiler-message":
            continue
        diagnostic = message.get("message") or {}
        if diagnostic.get("level") in {"warning", "error"} and not macos_unwind_size_note(
            diagnostic
        ):
            rendered.append(diagnostic.get("rendered") or diagnostic.get("message") or "")
    return rendered


def macos_unwind_size_note(diagnostic: dict[str, object]) -> bool:
    """Whether this warning carries only Apple ld's compact-unwind size note.

    rustc forwards the linker's whole stderr through the `linker_messages`
    lint, which `-D warnings` does not deny. The note reports an unwind-table
    encoding limit of a large debug binary, not a defect in the code; a
    warning holding any other linker line still fails.
    """
    code = diagnostic.get("code")
    lint = code.get("code") if isinstance(code, dict) else None
    if diagnostic.get("level") != "warning" or lint != "linker_messages":
        return False
    text = str(diagnostic.get("message") or "").removeprefix("linker stderr: ")
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    return bool(lines) and all(
        "__eh_frame section too large" in line and "compact unwind" in line for line in lines
    )


def library_compile_commands(package: str) -> list[tuple[str, list[str]]]:
    """The compile resolutions for one library, named and as Cargo arguments.

    The facade's API profile compiles the gateway with API routes but no
    native auth. The xAI singleton additionally exercises the Responses
    imports that only apply when neither OpenAI nor Codex is enabled.
    """
    lib = ["--lib", "--message-format=json"]
    if package == FACADE:
        return [
            (name, cargo_command("check", selected, False, package) + lib)
            for name, selected in (
                ("none", ()),
                ("full", ("full",)),
                ("composition", ("composition",)),
                ("claude", ("claude",)),
                ("openai", ("openai",)),
                ("gateway", ("gateway",)),
                (
                    "api",
                    ("openai", "xai", "anthropic", "openrouter", "gateway", "sessions-sqlite"),
                ),
                ("xai", ("xai",)),
                ("openrouter", ("openrouter",)),
                ("mcp", ("mcp",)),
                ("web-fetch", ("web-fetch",)),
            )
        ]
    if package == "plakous-hosted":
        return [
            ("no-optional-features", cargo_command("check", (), False, package) + lib),
            (
                "all-features",
                [
                    "cargo",
                    "check",
                    "--locked",
                    "--offline",
                    "-p",
                    package,
                    "--all-features",
                    *lib,
                ],
            ),
        ]
    return []


def check_facade_profiles(manifests: dict[str, dict[str, object]], root: Path = ROOT) -> list[str]:
    """Compile the facade's standalone feature profiles."""
    return check_library_profiles(manifests, FACADE, root)


def check_hosted_profiles(manifests: dict[str, dict[str, object]], root: Path = ROOT) -> list[str]:
    """Compile hosted without optional features and with all features."""
    return check_library_profiles(manifests, "plakous-hosted", root)


def check_library_profiles(
    manifests: dict[str, dict[str, object]], package: str, root: Path = ROOT
) -> list[str]:
    """`cargo check` each library profile; any rustc diagnostic fails.

    Closures prove dependency edges, not that the code compiles: a facade
    feature alone can name a module only another feature compiles. Cargo's
    fingerprints keep an unchanged profile at a no-op.
    """
    if package not in manifests:
        return [f"{package}: package missing from workspace metadata"]
    errors = []
    started = time.monotonic()
    profiles = library_compile_commands(package)
    for name, command in profiles:
        result = subprocess.run(command, cwd=root, text=True, capture_output=True, check=False)
        diagnostics = compiler_diagnostics(result.stdout)
        for diagnostic in diagnostics:
            sys.stderr.write(diagnostic)
        if result.returncode:
            if not diagnostics:
                sys.stderr.write(result.stderr)
            errors.append(f"{package} [{name}]: cargo check failed")
        elif diagnostics:
            errors.append(f"{package} [{name}]: cargo check reported {len(diagnostics)} warning(s)")
    print(
        f"checked {len(profiles)} {package} feature profiles in {time.monotonic() - started:.0f}s",
        flush=True,
    )
    return errors


def closure(
    package: str, selected: tuple[str, ...], default: bool, internal_packages: set[str]
) -> set[str]:
    """Return the normal dependency closure reported by Cargo tree."""
    command = cargo_command("tree", selected, default, package)
    command.extend(["-e", "normal", "--prefix", "none", "--format", "{p}"])
    output = cargo_output(command, capture=True)
    return {
        name
        for line in output.splitlines()
        if (name := line.split(" ", maxsplit=1)[0]) in internal_packages
    }


def main() -> int:
    """Check every package's feature closures and optionally compile each profile."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--build", action="store_true", help="cargo-check every closure witness")
    parser.add_argument(
        "--no-cache", action="store_true", help="query Cargo without reading or writing the cache"
    )
    parser.add_argument(
        "--library-packages",
        nargs="*",
        default=None,
        help="worker-only library compile scope; closure validation remains workspace-wide",
    )
    args = parser.parse_args()

    global CARGO_OUTPUTS
    CARGO_OUTPUTS = None
    if args.build or args.no_cache:
        print("feature closure cache: bypass (--build or --no-cache)")
    else:
        digest = cache_digest()
        CARGO_OUTPUTS = CargoOutputCache(cache_path(), digest)
        print(f"feature closure cache: {'hit' if CARGO_OUTPUTS.hit else 'miss'}", flush=True)

    workspace = cached_workspace(ROOT)
    manifests = {name: package.manifest for name, package in workspace.packages.items()}
    layers = {name: package.layer for name, package in workspace.packages.items()}
    internal_packages = set(manifests)
    errors = validate_feature_contracts(manifests, layers)
    errors.extend(validate_uuid_features(workspace))
    errors.extend(validate_excluded_closures())
    errors.extend(validate_test_only_dependency_features())
    errors.extend(validate_test_hook_methods(ROOT))
    validated = 0
    packages_run = 0
    profiles: dict[str, list[tuple[str, tuple[str, ...], bool]]] = {}
    for package in closure_packages(manifests):
        features = optional_features(manifests[package])
        if len(features) > MAX_OPTIONAL_FEATURES:
            errors.append(
                f"{package}: {len(features)} public features exceed the closure cap of "
                f"{MAX_OPTIONAL_FEATURES}; split the package or amend spec 06 with a reviewed limit"
            )
            continue
        has_full = "full" in (manifests[package].get("features") or {})
        profiles[package] = list(feature_sets(features, has_full))
    # Cache misses query concurrently; Cargo's package-cache lock caps useful
    # concurrency (measured 105 s at four workers, 109 s at ten).
    queries = [
        (package, selected, default)
        for package, sets in profiles.items()
        for _, selected, default in sets
    ]
    with ThreadPoolExecutor(max_workers=4) as pool:
        closures = dict(
            zip(
                queries,
                pool.map(lambda query: closure(*query, internal_packages), queries),
            )
        )
    for package, sets in profiles.items():
        packages_run += 1
        for name, selected, default in sets:
            actual = closures[(package, selected, default)]
            expected = expected_closure(package, selected, default, manifests)
            validated += 1
            if actual != expected:
                errors.append(
                    f"{package} [{name}]: dependency closure {sorted(actual)!r}, "
                    f"expected {sorted(expected)!r}"
                )
            if args.build:
                result = subprocess.run(
                    cargo_command("check", selected, default, package),
                    cwd=ROOT,
                    check=False,
                )
                if result.returncode != 0:
                    errors.append(f"{package} [{name}]: cargo check failed")
        if args.build:
            result = subprocess.run(
                ["cargo", "check", "--locked", "--offline", "-p", package, "--all-features"],
                cwd=ROOT,
                check=False,
            )
            if result.returncode != 0:
                errors.append(f"{package} [all-features]: cargo check failed")
    if CARGO_OUTPUTS is not None:
        CARGO_OUTPUTS.save()
        CARGO_OUTPUTS = None
    # --build already cargo-checks every closure witness.
    if not args.build:
        if args.library_packages is None or FACADE in args.library_packages:
            errors.extend(check_facade_profiles(manifests))
        if args.library_packages is None or "plakous-hosted" in args.library_packages:
            errors.extend(check_hosted_profiles(manifests))
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    action = "built and validated" if args.build else "validated"
    print(
        f"{action} {validated} feature closures across {packages_run} packages "
        "and the reviewed named closure-exclusion pins"
    )
    return 0


def validate_test_hook_methods(root: Path) -> list[str]:
    """Keep internal job-retention test seams out of normal library builds."""
    checks = (
        ("crates/core/agent/src/runtime/build.rs", "retain_jobs_for_test", "test-hooks"),
        ("crates/plakous/src/sessions.rs", "with_retained_jobs_for_test", "test-support"),
    )
    errors = []
    for relative, method, feature in checks:
        source = (root / relative).read_text()
        declaration = source.find(f"pub fn {method}")
        attributes = source[:declaration].splitlines()[-8:] if declaration >= 0 else []
        if not any(line.strip() == f'#[cfg(feature = "{feature}")]' for line in attributes):
            errors.append(f"{relative}: {method} must be gated by the {feature} feature")
    return errors


if __name__ == "__main__":
    raise SystemExit(main())
