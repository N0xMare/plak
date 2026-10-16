#!/usr/bin/env python3
"""Validate and optionally build every public feature subset, package by package.

Every workspace package with at least one public optional feature gets its own
closure run: 2^n subsets under `--no-default-features` (n capped at 10), plus
`default` and, where defined, `full`. Closures are never cross-multiplied
across packages: the layer matrix guarantees a host/bin crate reaches L0
features only through `plakous/*`, so the facade's own run already covers the
L0 side and each higher crate's run covers its side.

The expected closure is walked from the package's own feature table over the
workspace's normal dependency edges (from `cargo metadata`, no hardcoded edge
map) and compared to `cargo tree`. The manifest security-surface digests are
reviewed pins over the library layers (core, common, engine, protocol,
provider, store, harness, facade) and stay hardcoded below.

`EXCLUDED_CLOSURE_PACKAGES` additionally pins, over the complete external
closure Cargo reports, the packages a named feature profile must not reach.
This is what makes the facade's evaluator decoupling provable: `composition`
buys Code Mode and the JavaScript sidecar and nothing else.

Without `--build`, the facade library is also `cargo check`ed with no features
and with each public feature alone; a rustc warning or error fails the run.
"""

from __future__ import annotations

import argparse
import hashlib
import itertools
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

from plakous_policy import (  # noqa: E402
    ABOVE_FACADE_LAYERS,
    BELOW_FACADE_LAYERS,
    L0_LAYERS,
    Workspace,
    load_workspace,
)
from check_workspace import _blank_rust_non_code  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
# Layers whose manifests carry a reviewed security-surface digest.
DIGEST_LAYERS = L0_LAYERS + BELOW_FACADE_LAYERS
MAX_OPTIONAL_FEATURES = 10
FACADE = "plakous"


def cache_path(root: Path = ROOT) -> Path:
    return root / (os.environ.get("CARGO_TARGET_DIR") or "target") / "feature-closure-cache.json"


def cache_digest(root: Path = ROOT) -> str | None:
    """Hash Cargo's inputs, including the tracked manifest inventory."""
    version = subprocess.check_output(["cargo", "--version"], cwd=root, text=True)
    try:
        tracked = subprocess.check_output(
            ["git", "ls-files", "-z"], cwd=root, text=True, stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return None
    paths = {Path(name) for name in tracked.split("\0") if name and Path(name).name == "Cargo.toml"}
    paths.update(Path(name) for name in tracked.split("\0")
                 if Path(name).parent.name == ".cargo" and Path(name).name.startswith("config"))
    paths.update(path.relative_to(root) for path in root.glob(".cargo/config*") if path.is_file())
    paths.update(map(Path, (
        "Cargo.lock", "rust-toolchain.toml", "tools/checks/check_features.py",
        "tools/checks/plakous_policy.py",
    )))
    # Metadata contains absolute manifest paths, so it cannot cross checkouts.
    digest = hashlib.sha256(json.dumps([str(root.resolve()), version]).encode())
    for path in sorted(paths):
        content = (root / path).read_bytes() if (root / path).is_file() else None
        digest.update(json.dumps([str(path), None if content is None else hashlib.sha256(content).hexdigest()]).encode())
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
            if (cached["digest"] == digest and isinstance(outputs, dict)
                    and all(isinstance(key, str) and isinstance(value, str) for key, value in outputs.items())
                    and cached["checksum"] == self.checksum(outputs)):
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
            with tempfile.NamedTemporaryFile(mode="w", dir=self.path.parent, delete=False) as stream:
                temporary = Path(stream.name)
                json.dump({"digest": self.digest, "outputs": self.outputs,
                           "checksum": self.checksum(self.outputs)}, stream)
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
    graph = json.loads(cargo_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"], root
    ))
    return load_workspace(root, graph)


REQUIRED_DEPENDENCY_FEATURE_EDGES = {
    ("plakous", "native-subscriptions", "plakous-anthropic/native-auth"),
    ("plakous", "native-subscriptions", "plakous-auth/http-refresh"),
    ("plakous", "native-subscriptions", "plakous-openai/native-auth"),
    ("plakous", "native-subscriptions", "plakous-xai/native-auth"),
    ("plakous", "sessions-sqlite", "plakous-store/sqlite"),
    # Every facade feature that links plakous-http builds the Plak-run
    # `fetch_url` and `image_gen`; composition-only closures stay HTTP-free.
    ("plakous", "anthropic", "plakous-tool-web/fetch"),
    ("plakous", "mcp", "plakous-tool-web/fetch"),
    ("plakous", "native-subscriptions", "plakous-tool-web/fetch"),
    ("plakous", "openai", "plakous-tool-web/fetch"),
    ("plakous", "openrouter", "plakous-tool-web/fetch"),
    ("plakous", "xai", "plakous-tool-web/fetch"),
    ("plakous", "anthropic", "plakous-tool-web/image-gen"),
    ("plakous", "mcp", "plakous-tool-web/image-gen"),
    ("plakous", "native-subscriptions", "plakous-tool-web/image-gen"),
    ("plakous", "openai", "plakous-tool-web/image-gen"),
    ("plakous", "openrouter", "plakous-tool-web/image-gen"),
    ("plakous", "xai", "plakous-tool-web/image-gen"),
    ("plakous-anthropic", "native-auth", "plakous-auth/http-refresh"),
    ("plakous-openai", "native-auth", "plakous-auth/http-refresh"),
    ("plakous-xai", "native-auth", "plakous-auth/http-refresh"),
    # `FakeVcs` and the real-repository fixtures the tree's tests drive live in
    # `plakous-workspace`; `plakous-hosted`'s own test-support turns them on.
    # `MemoryStore` is the hosted session-record double `plakous-hosted`'s own
    # tests drive; it lives with the port it implements.
    ("plakous-hosted", "test-support", "plakous-recursive-subagents/test-support"),
    # `plak`'s repository-gate offline model reaches the host's injection point
    # through exactly this edge; without it `test_injection` is compiled out and
    # the scripted binding would be inert.
    ("plak", "test-hooks", "plakous-hosted/test-support"),
    ("plak", "test-hooks", "plakous-recursive-subagents-store/commit-trace"),
    (
        "plakous-recursive-subagents-store",
        "commit-trace",
        "plakous-store/commit-trace",
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
# Native workspace engines are independent build capabilities. Every host edge
# forwards explicitly; no default-enabled transitive edge may defeat selection.
REQUIRED_DEPENDENCY_FEATURE_EDGES.update(
    (package, feature, f"{dependency}/{feature}")
    for package, dependencies in {
        "plak": ["plakous-hosted", "plakous-recursive-subagents", "plakous-recursive-subagents-store"],
        "plakous-hosted": ["plakous-workspace", "plakous-recursive-subagents"],
        "plakous-recursive-subagents": ["plakous-workspace"],
        "plakous-recursive-subagents-store": ["plakous-recursive-subagents"],
    }.items()
    for dependency in dependencies
    for feature in ("git-cli", "jj-lib")
)
# Exact reviewed dependency and feature surfaces; path changes require review too.
EXPECTED_SECURITY_SURFACE_DIGESTS = {
    "plakous-py-worker": "a57158273317e3c1c0ebe404e30d6e81f3c7074a51c9b713ad946093bcee9bda",
    "plakous-js-runtime": "6c68fa7e92379b21f826b9a64729008865573b5cd9dd0439cea8d765ae10e99e",
    "plakous": "62afe3ab30f43575c1894a9e4c5b25fab9346601dda987c0e1617c7fc5d57d3e",
    "plakous-agent": "bd86d54cede49bb93150a0461f2c803b79525872d1fe243715261f9f0d6ef8e8",
    "plakous-anthropic": "b24186b9bde9a727d28a12bc90cb6919922118e77f3d048ab3d29a994a0598fa",
    "plakous-attempt": "6fa8c59b329b4de7ae6eb497bf4b3e2d93f916551c682c2f1a803a506ee65992",
    "plakous-auth": "5f45056d97e40ee2e4281e6b625a1d9a95e464d30d4c24126fb7fb2e95835c2a",
    "plakous-code-mode": "6126837839f8d7f0647eaff50f357f144940d5eca849a54805f70102d4de3cf9",
    "plakous-chat-completions": "d8085531aeadc39ac48ea5ce25f914f731529d5c4ca323fe7d7d976f96b96288",
    "plakous-exchange": "e0f496fedf43d21bf110457d61e96f7f99dbec9d337ff0770fab2afe9530b129",
    "plakous-http": "509477319f7c960b67654006da39d7b5ccf281b2f656d9229ba0dd22b1e99e57",
    "plakous-js-eval": "97fac4b3c7bddcf38ecc29fde3c4c946cc8b060042e2778beb7ee4e2d053aecf",
    "plakous-py-eval": "9762b4ca38230836eaee81639f677cba2bf7f7af708d8a9c6aaf9cddd43beed2",
    "plakous-mcp": "ed1fc19e0a46a24becdcb0731cddce8ecdd38cc038dd4234c614301000ed20c3",
    "plakous-messages": "5d7e6faa7134e308d22117482f06e3a11bc4faf667eb13356d9589e43db36b70",
    "plakous-model": "ad51d57d372a8bceb2696f96a4e87f7ac31071c9690f074c41559cb2677ad78d",
    "plakous-openai": "aef3a24707b0e4300ab292d142475be6eb564105b36bdc75ca064f270e63063e",
    "plakous-tool-files": "032d5367ef52db60dd8a5f798aecb2517aa5464787cb962d743b18d48f2dc9b0",
    "plakous-tool-plan": "97d99c53c448bc26d4bfb53ef555578d2c2185a98579a6df934ea3ce7a245da4",
    "plakous-tool-process": "6fa78529b74a6ed25c559f877278b23022a2203cd66f0ac393a9c3d0d8b5d790",
    "plakous-tool-search": "e453f2e948e74d7821e2a7b78233456b3c7bb55826dccea8d2a5aaf2be507b64",
    "plakous-tool-web": "081a9ce7813fda8c13b44d7ba618002a1dbe28938def7858d38d9427e9438143",
    "plakous-compose": "8ed7e3a1a5f408bad755197a1fee3b880b3ec6819660b9c916f09f88b5d34afc",
    "plakous-prompts": "aa45fde552eb9f54ab312678a0d812513ab2e3a5ff7407b56eb5b109c2d80bb9",
    "plakous-openrouter": "33b6e9c72fc590afb426dba9a6bacf36d352a16bb7729531ccc63eaccf04763a",
    "plakous-responses": "b87aaaa7f0fd6f8b28a1df608553ebc9a4f359da3078f605f9152ef4f37b9d3d",
    "plakous-responses-dialect": "db14ebd84b815ab6f9b60041c49224cebabe859a3bf56a3787dad7209ce94c8a",
    "plakous-store": "c23f61cbedc0d1f35c7eb983f164580f83b9147f8213b5fe403b142c1027e25e",
    "plakous-tools": "105550b8044a8418f497b8d4d4f1b0adf2b772c181b9b4c8aef19fa6fd56f5d1",
    "plakous-xai": "e13ddd5d34ea121b503e82d00d2696ba8c96e798927172626c01c00e8dadb1f5",
}


# Named feature profiles and the packages their complete closure must exclude.
# `composition` is the JavaScript evaluator capability on its own: no
# credential, HTTP, subscription, or Python machinery. `tungstenite` is reached
# only through `plakous-py-eval` -> `monty-pool` -> `tokio-tungstenite`, so its
# absence is the load-bearing evidence that a JavaScript-only embedder does not
# pull in the Monty closure.
EXCLUDED_CLOSURE_PACKAGES = {
    ("plakous", ("composition",)): (
        "htmd",
        "monty-pool",
        "plakous-auth",
        "plakous-http",
        "plakous-js-runtime",
        "plakous-py-eval",
        "rquickjs",
        "tokio-tungstenite",
        "tungstenite",
    ),
    ("plakous", ("composition", "sessions-sqlite")): (
        "plakous-auth",
        "plakous-http",
        "plakous-py-eval",
        "tungstenite",
    ),
    ("plakous", ("composition-python",)): (
        "plakous-auth",
        "plakous-http",
    ),
}


# Repository-only capabilities of a dependency, keyed by the product and the
# product feature that alone may turn them on. `PLAK_TEST_FAULTS` transport
# fault injection must be inert in every default and release `plak`, and live
# under `test-hooks`, where the live smoke depends on it.
TEST_ONLY_DEPENDENCY_FEATURES = {
    ("plak", "test-hooks"): (
        ("plakous-http", "fault-injection"),
        ("plakous-store", "commit-trace"),
        ("plakous-recursive-subagents-store", "commit-trace"),
    ),
    ("plakous-recursive-subagents-store", "commit-trace"): (
        ("plakous-store", "commit-trace"),
    ),
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
            profiles = ((), ("maintainer",)) if package == "plak" else ((),)
            for profile in profiles:
                if feature in resolved_dependency_features(package, profile, dependency):
                    name = ",".join(profile) or "default"
                    errors.append(
                        f"{package} [{name}]: enables {dependency}/{feature}, which only "
                        f"`{gate}` may enable"
                    )
            if feature not in resolved_dependency_features(package, (gate,), dependency):
                errors.append(f"{package} [{gate}]: no longer enables {dependency}/{feature}")
    return errors


def full_closure(package: str, selected: tuple[str, ...]) -> set[str]:
    """Return every package Cargo builds for one no-default feature profile."""
    command = cargo_command("tree", selected, False, package)
    command.extend(["-e", "normal", "--prefix", "none", "--format", "{p}"])
    output = cargo_output(command)
    return {
        line.split(" ", maxsplit=1)[0] for line in output.splitlines() if line.strip()
    }


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
    return {
        name: package.manifest for name, package in cached_workspace(root).packages.items()
    }


def digest_packages(workspace: Workspace) -> set[str]:
    """Return the packages whose manifest surface carries a reviewed digest."""
    return {package.name for package in workspace.in_layers(*DIGEST_LAYERS)}


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
        code = _blank_rust_non_code(source)
        names = {"Uuid"}
        for alias in aliases:
            names.update(re.findall(
                rf"\b{re.escape(alias)}\s*::\s*Uuid\s+as\s+(\w+)", code,
            ))
        constructors = set(re.findall(
            rf"\b(?:{'|'.join(map(re.escape, sorted(names)))})\s*::\s*"
            r"(new_v[1-8]|now_v[167])\b", code,
        ))
        for constructor in sorted(constructors):
            feature = constructor.rsplit("_", 1)[1]
            if feature not in declared:
                errors.append(f"{path}: UUID constructor {constructor} requires uuid feature "
                              f"{feature} in this package")
    return errors


def validate_uuid_features(workspace: Workspace) -> list[str]:
    """Inspect each package's Rust targets, including test-only constructors."""
    inherited = workspace.root_manifest.get("workspace", {}).get("dependencies", {})
    errors: list[str] = []
    for package in workspace.packages.values():
        root = package.manifest_path.parent
        paths = {path for directory in ("src", "tests", "benches", "examples")
                 for path in (root / directory).rglob("*.rs")}
        if (root / "build.rs").is_file():
            paths.add(root / "build.rs")
        sources = {path.relative_to(root).as_posix(): path.read_text() for path in paths}
        errors.extend(f"{package.name}: {finding}" for finding in
                      uuid_feature_findings(package.manifest, inherited, sources))
    return errors


def _normalized_dependency(value: object) -> object:
    """Normalize one Cargo dependency declaration without losing capabilities."""
    if isinstance(value, str):
        return {"version": value}
    if not isinstance(value, dict):
        return value
    return {
        key: sorted(item) if key == "features" and isinstance(item, list) else item
        for key, item in sorted(value.items())
    }


def security_surface_digest(manifest: dict[str, object]) -> str:
    """Digest the exact feature and all-target dependency/capability surface."""
    dependencies = [
        {
            "alias": alias,
            "kind": kind,
            "target": target,
            "value": _normalized_dependency(value),
        }
        for kind, target, table in dependency_tables(manifest)
        for alias, value in sorted(table.items())
    ]
    features_value = manifest.get("features", {})
    features = (
        {
            name: sorted(value) if isinstance(value, list) else value
            for name, value in sorted(features_value.items())
        }
        if isinstance(features_value, dict)
        else features_value
    )
    encoded = json.dumps(
        {"dependencies": dependencies, "features": features},
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


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
    digest_names: set[str] | None = None,
    layers: dict[str, str] | None = None,
) -> list[str]:
    """Check the reviewed digests, feature forwarding, and layer forwarding rules.

    `digest_names` selects the manifests that must carry a digest (default:
    every manifest given). `layers` (package -> layer) activates the host/bin
    rule: a feature of a host/bin package forwards only to `plakous/<feature>`
    or to another host package, never to a `plakous-<x>/<feature>`.
    """
    errors: list[str] = []
    digest_names = set(manifests) if digest_names is None else digest_names
    for package in sorted(digest_names):
        expected_digest = EXPECTED_SECURITY_SURFACE_DIGESTS.get(package)
        actual_digest = security_surface_digest(manifests[package])
        if expected_digest is None or actual_digest != expected_digest:
            errors.append(
                f"{package}: manifest dependency/feature security surface changed "
                f"({actual_digest}, expected {expected_digest})"
            )
    stale = sorted(set(EXPECTED_SECURITY_SURFACE_DIGESTS) - digest_names)
    if stale:
        errors.append(f"security surface digests pin packages outside the digest layers: {stale!r}")
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
                "plakous-recursive-subagents-store", "commit-trace", "plakous-store/commit-trace"
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
    return sorted(name for name in features if name not in {"default", "full"})


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
    """Return all no-default subsets plus the explicit default and full aliases."""
    profiles: list[tuple[str, tuple[str, ...], bool]] = []
    for size in range(len(features) + 1):
        for selected in itertools.combinations(features, size):
            profiles.append(("+".join(selected) or "minimal", selected, False))
    profiles.append(("default", (), True))
    if has_full:
        profiles.append(("full", ("full",), False))
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
    declarations = {name: _dependency_declarations(manifest) for name, manifest in manifests.items()}
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
        if declaration.get("default-features", True) is not False and "default" in feature_tables[package]:
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


def facade_profiles(manifests: dict[str, dict[str, object]]) -> list[tuple[str, ...]]:
    """Return the facade's empty feature set and each public feature alone."""
    return [()] + [(feature,) for feature in optional_features(manifests[FACADE])]


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
        if diagnostic.get("level") in {"warning", "error"}:
            rendered.append(diagnostic.get("rendered") or diagnostic.get("message") or "")
    return rendered


def check_facade_profiles(manifests: dict[str, dict[str, object]], root: Path = ROOT) -> list[str]:
    """`cargo check` the facade library per profile; any rustc diagnostic fails.

    Closures prove dependency edges, not that the code compiles: a facade
    feature alone can name a module only another feature compiles. Cargo's
    fingerprints keep an unchanged profile at a no-op.
    """
    if FACADE not in manifests:
        return [f"{FACADE}: package missing from workspace metadata"]
    errors = []
    started = time.monotonic()
    profiles = facade_profiles(manifests)
    for selected in profiles:
        name = "+".join(selected) or "minimal"
        command = cargo_command("check", selected, False) + ["--lib", "--message-format=json"]
        result = subprocess.run(command, cwd=root, text=True, capture_output=True, check=False)
        diagnostics = compiler_diagnostics(result.stdout)
        for diagnostic in diagnostics:
            sys.stderr.write(diagnostic)
        if result.returncode:
            if not diagnostics:
                sys.stderr.write(result.stderr)
            errors.append(f"{FACADE} [{name}]: cargo check failed")
        elif diagnostics:
            errors.append(f"{FACADE} [{name}]: cargo check reported {len(diagnostics)} warning(s)")
    print(f"checked {len(profiles)} facade feature profiles in {time.monotonic() - started:.0f}s",
          flush=True)
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
    parser.add_argument("--build", action="store_true", help="cargo-check every profile")
    parser.add_argument("--no-cache", action="store_true", help="query Cargo without reading or writing the cache")
    args = parser.parse_args()

    global CARGO_OUTPUTS
    CARGO_OUTPUTS = None
    if args.build or args.no_cache:
        print("feature closure cache: bypass (--build or --no-cache)")
    else:
        digest = cache_digest()
        if digest is None:
            print("feature closure cache: bypass (git ls-files failed; not a git checkout)", flush=True)
        else:
            CARGO_OUTPUTS = CargoOutputCache(cache_path(), digest)
            print(f"feature closure cache: {'hit' if CARGO_OUTPUTS.hit else 'miss'}", flush=True)

    workspace = cached_workspace(ROOT)
    manifests = {name: package.manifest for name, package in workspace.packages.items()}
    layers = {name: package.layer for name, package in workspace.packages.items()}
    internal_packages = set(manifests)
    errors = validate_feature_contracts(manifests, digest_packages(workspace), layers)
    errors.extend(validate_uuid_features(workspace))
    errors.extend(validate_excluded_closures())
    errors.extend(validate_test_only_dependency_features())
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
    # --build already checks every facade subset.
    if not args.build:
        errors.extend(check_facade_profiles(manifests))
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    action = "built and validated" if args.build else "validated"
    print(
        f"{action} {validated} feature closures across {packages_run} packages "
        f"and {len(EXCLUDED_CLOSURE_PACKAGES)} closure-exclusion profiles"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
