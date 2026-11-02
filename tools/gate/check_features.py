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
Named exclusion profiles independently pin complete external closures. The
manifest security-surface digests are
reviewed pins over the library layers (core, common, engine, protocol,
provider, store, harness, facade) and stay hardcoded below.

`EXCLUDED_CLOSURE_PACKAGES` additionally pins, over the complete external
closure Cargo reports, the packages a named feature profile must not reach.
This is what makes the facade's evaluator decoupling provable: `composition`
buys Code Mode and the JavaScript sidecar and nothing else.

Without `--build`, seven `cargo check --lib` profiles prove feature-conditioned
code: facade none, `full`, `composition`, `claude` and `openai`; hosted none
and `--all-features`. A rustc warning or error fails the run. Every public
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
import shutil
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))

import vcs_listing  # noqa: E402

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
MAX_OPTIONAL_FEATURES = 24
FACADE = "plakous"
PRODUCT_FEATURES = (
    "anthropic",
    "claude",
    "clipboard",
    "codex",
    "grok",
    "images",
    "js",
    "mcp",
    "openai",
    "openrouter",
    "python",
    "tui",
    "web-fetch",
    "xai-api",
)
NAMED_VARIANTS = {
    "default": ("default", ()),
    "headless": ("omit", ("tui", "clipboard")),
    "api": ("omit", ("codex", "grok", "claude")),
    "minimal": ("only", ("tui", "openrouter", "git-cli")),
}
ADMISSION_OMISSIONS = {"tui", "clipboard", "images", "python", "mcp"}


def named_variants(manifest: dict[str, object]) -> list[tuple[str, tuple[str, ...], bool]]:
    """The one named artifact table; only default is distributed."""
    defaults = set((manifest.get("features") or {}).get("default", []))
    profiles = []
    for name, (kind, features) in NAMED_VARIANTS.items():
        selected = set(features) if kind == "only" else defaults - set(features)
        profiles.append(
            (name, () if kind == "default" else tuple(sorted(selected)), kind == "default")
        )
    return profiles


def product_feature_sets(manifest: dict[str, object]) -> list[tuple[str, tuple[str, ...], bool]]:
    """Use the same bounded witness set for product and library packages."""
    return feature_sets(optional_features(manifest), "full" in manifest.get("features", {}))


def check_product_profiles(
    manifests: dict[str, dict[str, object]],
    root: Path = ROOT,
    operation: str = "build",
    target: str | None = None,
    fixture_binary: str | None = None,
    fixture_server: str | None = None,
) -> list[str]:
    """Compile each independent feature-off resolution; warnings fail this gate."""
    if "plak" not in manifests:
        return []
    errors = []
    manifest = manifests["plak"]
    profiles = [profile for profile in named_variants(manifest) if profile[0] != "default"]
    defaults = set(manifest["features"]["default"])
    profiles.append(("admission", tuple(sorted(defaults - ADMISSION_OMISSIONS)), False))
    for name, selected, default in profiles:
        print(f"product {operation}: {name}", flush=True)
        command = cargo_command(operation, selected, default, "plak") + [
            "--lib",
            "--bins",
            "--message-format=json",
        ]
        if target:
            command.extend(["--target", target])
        result = subprocess.run(command, cwd=root, text=True, capture_output=True, check=False)
        diagnostics = compiler_diagnostics(result.stdout)
        for diagnostic in diagnostics:
            sys.stderr.write(diagnostic)
        if result.returncode or diagnostics:
            if not diagnostics:
                sys.stderr.write(result.stderr)
            errors.append(f"plak [{name}]: feature-off compilation failed or emitted warnings")
        elif operation == "build":
            executable = None
            for line in result.stdout.splitlines():
                try:
                    artifact = json.loads(line)
                except ValueError:
                    continue
                if (
                    artifact.get("reason") == "compiler-artifact"
                    and artifact.get("target", {}).get("name") == "plak"
                    and artifact.get("executable")
                ):
                    executable = artifact["executable"]
            if executable is None:
                errors.append(f"plak [{name}]: build reported no executable")
            else:
                errors.extend(product_smoke(executable, selected, name))
                if name == "admission" and fixture_binary and fixture_server:
                    import product_feature_fixture

                    errors.extend(
                        product_feature_fixture.check(fixture_binary, executable, fixture_server)
                    )
                if name in {"api", "minimal"} and fixture_binary:
                    import product_feature_fixture

                    errors.extend(product_feature_fixture.check_route(fixture_binary, executable))
        command = cargo_command("clippy", selected, default, "plak") + ["--lib", "--bins"]
        if target:
            command.extend(["--target", target])
        result = subprocess.run(command + ["--", "-D", "warnings"], cwd=root, check=False)
        if result.returncode:
            errors.append(f"plak [{name}]: feature-off clippy failed")
    print(f"checked {len(profiles)} product feature-off profiles", flush=True)
    return errors


def product_builds(manifest: dict[str, object], target: str | None) -> list[str]:
    """Keep the full test-hooks writer separate from the reduced binaries opening its stores."""
    command = cargo_command("build", ("test-hooks",), True, "plak") + [
        "--bin",
        "plak",
        "--message-format=json",
    ]
    if target:
        command.extend(["--target", target])
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, check=False)
    diagnostics = compiler_diagnostics(result.stdout)
    if result.returncode or diagnostics:
        sys.stderr.write("".join(diagnostics) or result.stderr)
        return ["full test-hooks feature fixture writer failed to build"]
    binaries = [
        row["executable"]
        for line in result.stdout.splitlines()
        if (row := json.loads(line)).get("reason") == "compiler-artifact"
        and row.get("target", {}).get("name") == "plak"
        and row.get("executable")
    ]
    if not binaries:
        return ["full test-hooks fixture writer reported no executable"]
    writer_binary = binaries[-1]
    command = cargo_command("build", (), True, "plakous-mcp-fake-server") + [
        "--bin",
        "mcp-fake-server",
        "--message-format=json",
    ]
    if target:
        command.extend(["--target", target])
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, check=False)
    diagnostics = compiler_diagnostics(result.stdout)
    if result.returncode or diagnostics:
        sys.stderr.write("".join(diagnostics) or result.stderr)
        return ["MCP fixture server failed to build"]
    binaries = [
        row["executable"]
        for line in result.stdout.splitlines()
        if (row := json.loads(line)).get("reason") == "compiler-artifact"
        and row.get("target", {}).get("name") == "mcp-fake-server"
        and row.get("executable")
    ]
    if not binaries:
        return ["MCP fixture server reported no executable"]
    server_binary = binaries[-1]
    with tempfile.TemporaryDirectory(prefix="plak-feature-writer-") as directory:
        writer = str(Path(directory) / "plak")
        shutil.copy2(writer_binary, writer)
        errors = check_product_profiles(
            {"plak": manifest},
            operation="build",
            target=target,
            fixture_binary=writer,
            fixture_server=server_binary,
        )
        selected = tuple(
            sorted(
                set(manifest["features"]["default"]) - ADMISSION_OMISSIONS
                | {"maintainer", "test-hooks"}
            )
        )
        command = cargo_command("build", selected, False, "plak") + [
            "--bin",
            "plak",
            "--message-format=json",
        ]
        if target:
            command.extend(["--target", target])
        result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, check=False)
        diagnostics = compiler_diagnostics(result.stdout)
        if result.returncode or diagnostics:
            sys.stderr.write("".join(diagnostics) or result.stderr)
            return errors + ["reduced test-hooks/maintainer fixture build failed"]
        binaries = [
            row["executable"]
            for line in result.stdout.splitlines()
            if (row := json.loads(line)).get("reason") == "compiler-artifact"
            and row.get("target", {}).get("name") == "plak"
            and row.get("executable")
        ]
        if not binaries:
            return errors + ["reduced test-hooks fixture reported no executable"]
        import product_feature_fixture

        errors.extend(
            product_feature_fixture.check(writer, binaries[-1], server_binary, maintainer=True)
        )
        errors.extend(product_feature_fixture.image_diagnostic(binaries[-1]))
        return errors


def product_smoke(executable: str, selected: tuple[str, ...], profile: str) -> list[str]:
    """Real reduced binaries refuse before credentials, providers or session writes."""
    errors = []
    with tempfile.TemporaryDirectory(prefix="plak-feature-smoke-") as directory:
        environment = dict(os.environ, PLAK_HOME=directory)
        cases = [("version", ["version"], 0, "; features -"), ("help", ["help"], 0, "plak")]
        if "tui" not in selected:
            cases.append(("tui", [], 2, "feature `tui`"))
        if "mcp" not in selected:
            cases.append(("mcp", ["mcp", "list"], 2, "feature `mcp`"))
        if "python" not in selected and "codex" in selected:
            cases.append(
                (
                    "python",
                    [
                        "run",
                        "--account",
                        "codex",
                        "--model",
                        "gpt-6-astra",
                        "--composition",
                        "py-ptc",
                        "--",
                        "hello",
                    ],
                    2,
                    "feature `python`",
                )
            )
            cases.append(
                (
                    "python-jsonl",
                    [
                        "agent",
                        "--account",
                        "codex",
                        "--model",
                        "gpt-6-astra",
                        "--composition",
                        "py-ptc",
                    ],
                    2,
                    "feature `python`",
                )
            )
            cases.append(
                (
                    "sdk",
                    [
                        "run",
                        "--account",
                        "codex",
                        "--model",
                        "gpt-6-astra",
                        "--composition",
                        "py-sdk",
                        "--",
                        "hello",
                    ],
                    2,
                    "`py-sdk` is not included in this build (compiled without feature `python`)",
                )
            )
            cases.append(
                (
                    "sdk-jsonl",
                    [
                        "agent",
                        "--account",
                        "codex",
                        "--model",
                        "gpt-6-astra",
                        "--composition",
                        "py-sdk",
                    ],
                    2,
                    "`py-sdk` is not included in this build (compiled without feature `python`)",
                )
            )
        if "images" not in selected and "codex" in selected:
            cases.append(
                (
                    "images",
                    [
                        "run",
                        "--account",
                        "codex",
                        "--model",
                        "gpt-6-astra",
                        "--image",
                        "missing.png",
                        "--",
                        "hello",
                    ],
                    2,
                    "feature `images`",
                )
            )
        for account in ("codex", "grok", "claude", "openai", "anthropic", "xai-api", "openrouter"):
            if account not in selected:
                arguments = (
                    ["login", account]
                    if account in ("codex", "grok", "claude")
                    else [
                        "run",
                        "--account",
                        account,
                        "--model",
                        "uncompiled-route",
                        "--composition",
                        "direct",
                        "--",
                        "hello",
                    ]
                )
                cases.append((account, arguments, 2, f"feature `{account}`"))
                cases.append(
                    (
                        f"{account}-jsonl",
                        ["agent", "--account", account, "--model", "uncompiled-route"],
                        2,
                        f"feature `{account}`",
                    )
                )
        for kind, arguments, code, expected in cases:
            completed = subprocess.run(
                [executable, *arguments],
                cwd=directory,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
                timeout=30,
            )
            if completed.returncode != code or expected not in completed.stdout + completed.stderr:
                errors.append(
                    f"plak [{profile}] {kind}: expected exit {code} and {expected!r}; got {completed.returncode}: {completed.stdout}{completed.stderr}"
                )
            if kind.endswith("-jsonl"):
                records = [json.loads(line) for line in completed.stdout.splitlines()]
                if [record.get("kind") for record in records] != ["error", "bye"]:
                    errors.append(f"plak [{profile}] {kind} refusal must terminate with error/bye")
            if kind.startswith("sdk") and (Path(directory) / "state").exists():
                errors.append(f"plak [{profile}] SDK admission refusal created state")
        doctor = subprocess.run(
            [executable, "doctor"],
            cwd=directory,
            env=environment,
            text=True,
            capture_output=True,
            check=False,
            timeout=30,
        )
        omitted = sorted(set(PRODUCT_FEATURES) - set(selected))
        expected = f"omitted {', '.join(omitted)}"
        if not any(
            "Build features" in line and expected in line for line in doctor.stdout.splitlines()
        ):
            errors.append(
                f"plak [{profile}] doctor: missing build_features {expected!r}: {doctor.stdout}{doctor.stderr}"
            )
        for feature in ("python", "mcp"):
            if feature in selected or "codex" not in selected:
                continue
            home = Path(directory) / feature
            home.mkdir(mode=0o700)
            (home / "agents").mkdir(mode=0o700)
            config = '[root]\nagent = "root-agent"\n'
            template = 'account = "codex"\nmodel = "gpt-6-astra"\n'
            if feature == "python":
                template += 'composition = "py-ptc"\n'
            else:
                config += '[mcp_servers.example]\ntransport = "stdio"\ncommand = "/bin/false"\n'
                template += 'tools = ["mcp:example"]\n'
            (home / "config.toml").write_text(config)
            (home / "agents/root-agent.toml").write_text(template)
            completed = subprocess.run(
                [executable, "agent"],
                cwd=directory,
                env=dict(environment, PLAK_HOME=str(home)),
                input="",
                text=True,
                capture_output=True,
                check=False,
                timeout=30,
            )
            if completed.returncode != 2 or f"feature `{feature}`" not in completed.stderr:
                errors.append(
                    f"plak [{profile}] configured {feature}: expected typed exit 2; got {completed.returncode}: {completed.stdout}{completed.stderr}"
                )
    return errors


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
    ("plak", "tui", "plakous-hosted/tui"),
    ("plak", "clipboard", "plakous-hosted/clipboard"),
    ("plak", "images", "plakous-hosted/images"),
    ("plak", "js", "plakous-hosted/js"),
    ("plak", "js", "plakous/composition"),
    ("plak", "python", "plakous-hosted/python"),
    ("plak", "python", "plakous/composition-python"),
    ("plak", "mcp", "plakous-hosted/mcp"),
    ("plak", "mcp", "plakous/mcp"),
    ("plak", "codex", "plakous/codex"),
    ("plak", "codex", "plakous-hosted/codex"),
    ("plakous-hosted", "codex", "plakous/codex"),
    ("plak", "grok", "plakous/grok"),
    ("plak", "grok", "plakous-hosted/grok"),
    ("plakous-hosted", "grok", "plakous/grok"),
    ("plak", "claude", "plakous/claude"),
    ("plak", "claude", "plakous-hosted/claude"),
    ("plakous-hosted", "claude", "plakous/claude"),
    ("plak", "openai", "plakous/openai"),
    ("plak", "openai", "plakous-hosted/openai"),
    ("plakous-hosted", "openai", "plakous/openai"),
    ("plak", "anthropic", "plakous/anthropic"),
    ("plak", "anthropic", "plakous-hosted/anthropic"),
    ("plakous-hosted", "anthropic", "plakous/anthropic"),
    ("plak", "xai-api", "plakous/xai"),
    ("plak", "xai-api", "plakous-hosted/xai-api"),
    ("plakous-hosted", "xai-api", "plakous/xai"),
    ("plak", "openrouter", "plakous/openrouter"),
    ("plak", "openrouter", "plakous-hosted/openrouter"),
    ("plakous-hosted", "openrouter", "plakous/openrouter"),
    ("plak", "web-fetch", "plakous-hosted/web-fetch"),
    ("plak", "web-fetch", "plakous/web-fetch"),
    ("plakous-hosted", "js", "plakous/composition"),
    ("plakous-hosted", "python", "plakous/composition-python"),
    ("plakous-hosted", "mcp", "plakous/mcp"),
    ("plakous-hosted", "web-fetch", "plakous/web-fetch"),
    ("plakous", "mcp", "plakous-mcp/client"),
    ("plakous", "test-clock", "plakous-openai?/test-clock"),
    ("plakous", "test-clock", "plakous-openrouter?/test-clock"),
    ("plakous", "test-hooks", "plakous-cell?/test-hooks"),
    ("plak", "test-hooks", "plakous/test-clock"),
    ("plak", "test-hooks", "plakous/test-hooks"),
    ("plakous", "claude", "plakous-anthropic/native-auth"),
    ("plakous", "codex", "plakous-auth/http-refresh"),
    ("plakous", "grok", "plakous-auth/http-refresh"),
    ("plakous", "claude", "plakous-auth/http-refresh"),
    ("plakous", "codex", "plakous-openai/native-auth"),
    ("plakous", "grok", "plakous-xai/native-auth"),
    ("plakous", "sessions-sqlite", "plakous-store/sqlite"),
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
    # `plak`'s repository-gate offline model registers a scripted RuntimeFactory
    # through this edge; the production construction port is always available,
    # but the scripted factory belongs to the hosted test kit.
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
    (
        package,
        "jj" if package == "plak" and feature == "jj-lib" else feature,
        f"{dependency}/{feature}",
    )
    for package, dependencies in {
        "plak": [
            "plakous-hosted",
            "plakous-recursive-subagents",
            "plakous-recursive-subagents-store",
        ],
        "plakous-hosted": ["plakous-workspace", "plakous-recursive-subagents"],
        "plakous-recursive-subagents": ["plakous-workspace"],
        "plakous-recursive-subagents-store": ["plakous-recursive-subagents"],
    }.items()
    for dependency in dependencies
    for feature in ("git-cli", "jj-lib")
)
# Exact reviewed dependency and feature surfaces; path changes require review too.
EXPECTED_SECURITY_SURFACE_DIGESTS = {
    "plakous-py-worker": "12692a8c10a6033ffd8c99be9fd192fb9d76cbd41de571b36aabdc8720bebce5",
    "plakous-js-runtime": "fd22d2bf42bb470f5ed7ec3a852aa0472c1fdafa2ced4641a4a4f42c0a4bdd2a",
    "plakous": "c8ee0fd5d604c74b39b92a6b6f1f600eea7a52a02211f9ea8fcfea5fa343b886",
    "plakous-agent": "da735f6245070e0109928b93c49b8ed1353df48ed5e887b19472bc31d2b32671",
    "plakous-anthropic": "1b02db17b7b8447f9f0abd665616ffbf380c1a6c1e41c192cdc0f075e3cf1c5f",
    "plakous-attempt": "87a0bece47ad17a768ac1eb58a4e6ff9712e6f53008fd524fae3fd4055282f6b",
    "plakous-auth": "b69237dd11b3b34ed0c5d47ced22fb6a1d22cfad9833a9175bd28c9efc8a6042",
    "plakous-chat-completions": "aa09b0d79b7b98c14f6f00f0290ba06cb13cc984ce43459bdd3b375269f8c362",
    "plakous-exchange": "89dc4c8e0cdd7cce6b65b60599e54a385e8ed5bcbc56176c34f2b77d41c91e97",
    "plakous-http": "ddb50db1f8b854c21b80b72732aaeb08956364aec0aac8fc2505544cd085b7e1",
    "plakous-cell": "95f1b945609f1311c36501e26299df396c7cfce8021f1807b5c82e5f174de232",
    "plakous-mcp": "7a863ed2e848f697dce4d643a0d6b581bc0aa962297204865c89a3c277f6c36c",
    "plakous-messages": "30cb4595eb111ef7184ff61bd83fa4351a4fa2401fd2906be0c9409af9a9155b",
    "plakous-model": "04297fb5135a6bec3b2b5838aa8a0f71620162cfa4166529f1e110702fd0a16f",
    "plakous-openai": "4b3068f8a7ff7a3cea79892ccdf561ee1499eb37db69ce045871348ad840c99b",
    "plakous-tool-files": "a683c26a9af2d50b8e2ad05a85920a7e725aa571c267742028b25046521bf037",
    "plakous-tool-plan": "f6b901df7cd208ea7296a9593510ba3ccaa50acdb57b08de497d5f05948d1aff",
    "plakous-tool-process": "fa21e6c7ded66a688062e91814e5ef1127d3ed855f3889fff8c791b9f50cbda8",
    "plakous-tool-search": "96d7fb5cc266fc2475ee59059bbe05e1e80f5f03de7dc046e60b1fcb3a5ba958",
    "plakous-tool-web": "bef13461f221cb892caeda926d304202443ebf67789b6fc554cc13fe5075d9d0",
    "plakous-compose": "67f1d0b46368982e69c5c6500757e5eee5c1a455d24e1ea8217f31a4be1c6716",
    "plakous-prompts": "b5355c6de6d0cc0d848eb5626b2bc7fb264d02ab34faa036453f8e3c2fa0f17d",
    "plakous-openrouter": "fc202257b7a1b3d0586e9d0df778bd1d578c8a6d06d6818e3d5feecc5961246c",
    "plakous-responses": "f565ee25c2e3227c43b269500077732e3fe06c06776e0e3610db0f508249a5ad",
    "plakous-store": "5bd85cf0311ccc058528e1e024fca91d70b14bad82d4327395af1063e66f4973",
    "plakous-tools": "2272a73bb4152af5b5f0c093c8a2b258c4b7e4ff6a64686df8bd70a9a4f7746a",
    "plakous-xai": "4b452bb53d65262dff9cc74e58ac83bb0f89ab6ad2201299eb5a7ed1238b0158",
}


# Named feature profiles and the packages their complete closure must exclude.
# `composition` is the cell client on its own: no credential, HTTP,
# subscription or interpreter machinery for either source language.
EXCLUDED_CLOSURE_PACKAGES = {
    ("plak", ("git-cli", "images", "python", "mcp")): (
        "ratatui",
        "crossterm",
        "syntect",
        "arboard",
        "pulldown-cmark",
    ),
    ("plak", ("git-cli", "tui", "python", "mcp")): ("image",),
    ("plak", ("default",)): ("jj-lib", "gix", "pollster", "aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous", ("full",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous", ("codex",)): (
        "aws-lc-rs",
        "aws-lc-sys",
        "rsa",
        "plakous-xai",
        "plakous-anthropic",
        "monty-pool",
        "htmd",
    ),
    ("plakous", ("grok",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous", ("claude",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous-openai", ("native-auth",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plakous-xai", ("native-auth",)): ("aws-lc-rs", "aws-lc-sys", "rsa"),
    ("plak", ("git-cli",)): ("jj-lib", "gix", "pollster"),
    ("plakous", ("composition",)): (
        "htmd",
        "monty-proto",
        "plakous-auth",
        "plakous-http",
        "plakous-js-runtime",
        "plakous-py-worker",
        "rquickjs",
    ),
    ("plakous", ("composition", "sessions-sqlite")): (
        "plakous-auth",
        "plakous-http",
        "monty-proto",
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
    ("plakous", ("anthropic",)): ("jsonwebtoken",),
}

NAMED_VARIANT_EXCLUSIONS = {
    "headless": ("ratatui", "crossterm", "syntect", "arboard", "pulldown-cmark"),
    "api": ("jsonwebtoken",),
    "minimal": (
        "rmcp",
        "monty-pool",
        "tungstenite",
        "image",
        "htmd",
        "html5ever",
        "jsonwebtoken",
        "aws-lc-sys",
        "plakous-js-eval",
        "plakous-cell",
    ),
}


# Repository-only capabilities of a dependency, keyed by the product and the
# product feature that alone may turn them on. `PLAK_TEST_FAULTS` transport
# fault injection must be inert in every default and release `plak`, and live
# under `test-hooks`, where the live smoke depends on it.
TEST_ONLY_DEPENDENCY_FEATURES = {
    ("plak", "test-hooks"): (
        ("plakous-http", "fault-injection"),
        ("plakous", "test-clock"),
        ("plakous", "test-hooks"),
        ("plakous-openai", "test-clock"),
        ("plakous-openrouter", "test-clock"),
        ("plakous-store", "commit-trace"),
        ("plakous-recursive-subagents-store", "commit-trace"),
        ("plakous-hosted", "test-support"),
    ),
    ("plakous-recursive-subagents-store", "commit-trace"): (("plakous-store", "commit-trace"),),
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
    for package in (
        "plakous",
        "plakous-anthropic",
        "plakous-openai",
        "plakous-openrouter",
        "plakous-xai",
    ):
        for profile in ((), ("full",)) if package == "plakous" else ((),):
            if "test-clock" in resolved_dependency_features(package, profile, package):
                errors.append(
                    f"{package} [{','.join(profile) or 'default'}]: test-clock is test-only"
                )
    for profile in ((), ("full",)):
        if "test-hooks" in resolved_dependency_features("plakous", profile, "plakous"):
            errors.append(f"plakous [{','.join(profile) or 'default'}]: test-hooks is test-only")
    if "test-hooks" in resolved_dependency_features("plakous-cell", (), "plakous-cell"):
        errors.append("plakous-cell [default]: test-hooks is test-only")
    # The Grok and Claude review-anchored clocks reach only offline test
    # builds (facade dev-dependencies): no plak build, `test-hooks` live
    # smoke builds included, may resolve them.
    for profile in PLAK_PROFILES_WITHOUT_ANCHORED_CLOCKS:
        for dependency in ANCHORED_CLOCK_PROVIDERS:
            if "test-clock" in resolved_dependency_features("plak", profile, dependency):
                errors.append(
                    f"plak [{','.join(profile) or 'default'}]: enables {dependency}/test-clock, "
                    "which only offline test builds may enable"
                )
    return errors


ANCHORED_CLOCK_PROVIDERS = ("plakous-anthropic", "plakous-xai")
PLAK_PROFILES_WITHOUT_ANCHORED_CLOCKS = (
    (),
    ("maintainer",),
    ("test-hooks",),
    ("test-hooks", "maintainer"),
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
    pins = dict(EXCLUDED_CLOSURE_PACKAGES)
    manifest = load_workspace_manifests()["plak"]
    for name, selected, default in named_variants(manifest):
        if name in NAMED_VARIANT_EXCLUSIONS:
            pins[("plak", selected)] = NAMED_VARIANT_EXCLUSIONS[name]
    without_python = tuple(sorted(set(manifest["features"]["default"]) - {"python"}))
    pins[("plak", without_python)] = ("monty-pool", "tungstenite")
    for (package, selected), excluded in sorted(pins.items()):
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
                "plakous-recursive-subagents-store",
                "commit-trace",
                "plakous-store/commit-trace",
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
    # The facade and cell deadline seams are hidden the same way as test-clock.
    # plak and plakous-py-worker keep a public test-hooks feature.
    package = manifest.get("package")
    name = package.get("name") if isinstance(package, dict) else None
    if name in {"plakous", "plakous-cell"}:
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

    Facade: no features, `full`, `composition` alone, `claude` alone (one
    native route) and `openai` alone (the API route, including
    `responses_retrieval`'s `cfg(feature = "openai")` paths, without a
    native-auth route beside it). Hosted: no features, and `--all-features`.
    Hosted defaults are compiled with the supported Plak default resolution,
    not as another profile.
    """
    lib = ["--lib", "--message-format=json"]
    if package == FACADE:
        return [
            (name, cargo_command("check", selected, False, package) + lib)
            for name, selected in (
                ("minimal", ()),
                ("full", ("full",)),
                ("composition", ("composition",)),
                ("claude", ("claude",)),
                ("openai", ("openai",)),
            )
        ]
    if package == "plakous-hosted":
        return [
            ("minimal", cargo_command("check", (), False, package) + lib),
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
    """Compile the facade's five representative resolutions."""
    return check_library_profiles(manifests, FACADE, root)


def check_hosted_profiles(manifests: dict[str, dict[str, object]], root: Path = ROOT) -> list[str]:
    """Compile hosted with no features and with every feature, un-unified."""
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
    parser.add_argument("--build", action="store_true", help="cargo-check every profile")
    parser.add_argument(
        "--no-cache", action="store_true", help="query Cargo without reading or writing the cache"
    )
    parser.add_argument(
        "--product-builds",
        action="store_true",
        help="build and lint four distinct feature-off product resolutions",
    )
    parser.add_argument("--target", help="Cargo build target for product variants")
    args = parser.parse_args()

    if args.product_builds:
        import tomllib

        manifest = tomllib.loads((ROOT / "bin/plak/Cargo.toml").read_text())
        errors = product_builds(manifest, args.target)
        for error in errors:
            print(error, file=sys.stderr)
        return int(bool(errors))

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
        if package == "plak":
            profiles[package].extend(
                profile for profile in named_variants(manifests[package]) if profile[0] != "default"
            )
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
        errors.extend(check_facade_profiles(manifests))
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


if __name__ == "__main__":
    raise SystemExit(main())
