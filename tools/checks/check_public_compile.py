#!/usr/bin/env python3
"""Run archive-friendly positive and negative public compile-contract pins."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def contract_environment() -> dict[str, str]:
    """Keep fixture feature graphs separate from workspace and other targets."""
    environment = os.environ.copy()
    target = environment.get("CARGO_BUILD_TARGET")
    if not target:
        version = subprocess.check_output(["rustc", "-vV"], text=True)
        target = next(line.removeprefix("host: ") for line in version.splitlines()
                      if line.startswith("host: "))
    base = Path(environment.get("CARGO_TARGET_DIR", ROOT / "target"))
    base = base.resolve()
    # cargo-sweep descends into nested target dirs. A gitignored sibling survives
    # both lane sweeps and checkout cleanup without sharing another lane's graph.
    cache = Path(environment.get(
        "PLAK_PUBLIC_CONTRACTS_DIR", base.parent / ".public-contracts" / base.name,
    ))
    environment["CARGO_TARGET_DIR"] = str(cache.resolve() / target)
    return environment


SUPPORTED_API_FIXTURES = (
    ("plakous-agent", "plakous-agent", Path("tests/fixtures/agent-consumer/Cargo.toml")),
    ("plakous-facade", "plakous", Path("tests/fixtures/facade-consumer/Cargo.toml")),
    ("plakous-model", "plakous-model", Path("tests/fixtures/model-consumer/Cargo.toml")),
    ("plakous-tools", "plakous-tools", Path("tests/fixtures/tools-consumer/Cargo.toml")),
)
EXPECTED_SUPPORTED_PACKAGES = ("plakous-agent", "plakous", "plakous-model", "plakous-tools")
# One workflow fixture per embedding deployment that is not the broad inventory.
# `facade-consumer` above is the complete supported surface and still reaches
# the Python evaluator; this one pins that the JavaScript-only workflow compiles
# from a manifest with no `[patch]` table at all.
EMBEDDING_WORKFLOW_FIXTURES = (
    ("js-embedder", Path("tests/fixtures/js-embedder-consumer/Cargo.toml")),
)
# Each embedding example, the exact facade features it selects, and whether it
# runs to completion offline. `provider_direct` runs, says what it needs, and
# exits: with no key file there is nothing to do, because it discovers no
# credential. `worker_js_sidecar` takes the built worker as an explicit
# argument, so the gate compiles it and the operator runs it against a worker.
EMBEDDING_EXAMPLES = (
    ("cancellation_shutdown", "", True),
    ("direct_custom_tool", "sessions-sqlite", True),
    ("durable_session", "sessions-sqlite", True),
    ("provider_direct", "openrouter", True),
    ("worker_js_sidecar", "composition,sessions-sqlite", False),
)
SUPPORTED_INVENTORY_SOURCES = tuple(
    source
    for _, _, manifest in SUPPORTED_API_FIXTURES
    for source in sorted((ROOT / manifest.parent / "src").rglob("*.rs"))
)
# The five embedding examples are external consumer workflows and must stay
# test-support free. The repository scripted quickstart is an integration test.
EMBEDDING_SOURCES = tuple(
    source
    for _, manifest in EMBEDDING_WORKFLOW_FIXTURES
    for source in sorted((ROOT / manifest.parent / "src").rglob("*.rs"))
) + tuple(
    source
    for source in sorted((ROOT / "crates/plakous/examples").rglob("*.rs"))
)
NAMEABILITY_POLICY_SOURCES = (
    SUPPORTED_INVENTORY_SOURCES
    + EMBEDDING_SOURCES
    + (ROOT / "crates/plakous/tests/public_facade.rs",)
)
RUNTIME_ONLY_TOOL_NAMES = (
    "AuthorizedToolInvocation",
    "ClaimedToolInvocation",
    "EffectDispatchIntent",
    "PendingToolApproval",
    "TokioToolTaskOwner",
    "TokioToolTimer",
    "ToolRuntimeConfig",
    "ToolScheduler",
    "ToolSchedulerStatus",
    "ToolTask",
    "ToolTaskDrain",
    "ToolTaskFuture",
    "ToolTaskHandle",
    "ToolTaskJoin",
    "ToolTaskOwner",
    "ToolTaskOwnerStatus",
    "ToolTaskStatus",
    "ToolTimer",
    "ToolTimerFuture",
    "ToolAuthorization",
    "ToolDispatchObserver",
    "ToolDispatchObserverFuture",
    "ToolDispatchReport",
    "ToolDispatchState",
    "ToolExecutionControl",
    "ToolRuntime",
)

SUPPORTED_POSITIVE = tuple(
    (
        f"positive-{label}",
        (
            "cargo",
            "check",
            "--manifest-path",
            str(manifest),
            "--locked",
            "--offline",
        ),
    )
    for label, _, manifest in SUPPORTED_API_FIXTURES
) + tuple(
    (
        f"positive-{label}",
        ("cargo", "check", "--manifest-path", str(manifest), "--locked", "--offline"),
    )
    for label, manifest in EMBEDDING_WORKFLOW_FIXTURES
)


def example_command(verb: str, name: str, features: str) -> tuple[str, ...]:
    """Build one locked, offline Cargo command for an embedding example."""
    command = [
        "cargo",
        verb,
        "--locked",
        "--offline",
        "-p",
        "plakous",
        "--no-default-features",
        "--example",
        name,
    ]
    if features:
        command.extend(["--features", features])
    return tuple(command)


EXAMPLE_POSITIVE = tuple(
    (
        f"positive-example-build-{name.replace('_', '-')}",
        example_command("build", name, features),
    )
    for name, features, _ in EMBEDDING_EXAMPLES
) + tuple(
    (
        f"positive-example-run-{name.replace('_', '-')}",
        example_command("run", name, features),
    )
    for name, features, runs in EMBEDDING_EXAMPLES
    if runs
)
LOCK_PREFLIGHT = tuple(
    (
        f"locked-preflight-{label}",
        (
            "cargo",
            "metadata",
            "--manifest-path",
            str(manifest),
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--no-deps",
        ),
    )
    for label, _, manifest in SUPPORTED_API_FIXTURES
) + tuple(
    (
        f"locked-preflight-{label}",
        (
            "cargo",
            "metadata",
            "--manifest-path",
            str(manifest),
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--no-deps",
        ),
    )
    for label, manifest in EMBEDDING_WORKFLOW_FIXTURES
) + (
    (
        "locked-preflight-authority-negative",
        (
            "cargo",
            "metadata",
            "--manifest-path",
            "tests/fixtures/authority-negative/Cargo.toml",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--no-deps",
        ),
    ),
)
FETCH_FIXTURES = tuple(
    (
        label.replace("locked-preflight-", "fetch-", 1),
        ("cargo", "fetch", "--manifest-path", command[3], "--locked"),
    )
    for label, command in LOCK_PREFLIGHT
)
DEPENDENCY_POSITIVE = (
    (
        "positive-js-embedder-lifecycle",
        (
            "cargo",
            "run",
            "--manifest-path",
            "tests/fixtures/js-embedder-consumer/Cargo.toml",
            "--locked",
            "--offline",
        ),
    ),
    (
        "positive-facade-lifecycle",
        (
            "cargo",
            "run",
            "--manifest-path",
            "tests/fixtures/facade-consumer/Cargo.toml",
            "--locked",
            "--offline",
        ),
    ),
    (
        "positive-authority-negative-dependencies",
        (
            "cargo",
            "check",
            "--manifest-path",
            "tests/fixtures/authority-negative/Cargo.toml",
            "--lib",
            "--locked",
            "--offline",
        ),
    ),
)
# The two consumer manifests spec 06 and the embedding guide document on their
# own. Every other positive selects a feature set that also pulls a session
# backend or the whole product closure, so a consumer that selects only
# `composition`, or only `composition-python`, was never type-checked. `check`
# rather than `build`: the question is whether the crate compiles under the
# feature gates, not whether it links.
CONSUMER_MANIFEST_POSITIVE = tuple(
    (
        f"positive-consumer-manifest-{feature}",
        (
            "cargo",
            "check",
            "--locked",
            "--offline",
            "-p",
            "plakous",
            "--no-default-features",
            "--features",
            feature,
        ),
    )
    for feature in ("composition", "composition-python")
)
DOCUMENTATION_NEGATIVE = (
    (
        "negative-curated-doctests",
        ("cargo", "test", "--locked", "--offline", "--doc", "-p", "plakous-agent", "-p", "plakous", "--all-features"),
    ),
)
AUTHORITY_NEGATIVE_BINS = (
    "tools_runtime",
    "tools_internal_process_dispatch",
    "facade_tools_runtime",
    "agent_tool_runtime",
    "facade_checkpoint",
    "component_checkpoint",
    "store_authority",
    "driver_authority",
    "checkpoint_codec",
    "driver_endpoint",
    "issued_invocation",
    "terminal_result",
    "credential_bytes",
    "family_transport",
    "standard_descriptor",
    "release_evidence",
    "standard_identity",
    "standard_registration",
    "standard_configuration",
    "family_artifact_sealing",
    "anthropic_transport",
    "openrouter_model_lane",
    "xai_platform_authority",
    "xai_platform_grant",
    "xai_platform_bearer",
    "local_composition_session",
)
AUTHORITY_NEGATIVE_IMPORTS = {
    "tools_runtime": "use plakous_tools::{ToolRuntime, ToolScheduler};",
    "tools_internal_process_dispatch": (
        "use plakous_tools::internal::{exec_command, run_terminal_cmd, write_stdin};"
    ),
    "facade_tools_runtime": "use plakous::tools::{ToolRuntime, ToolScheduler};",
    "agent_tool_runtime": "use plakous_agent::{ToolRuntime, ToolScheduler};",
    "facade_checkpoint": "use plakous::agent::RuntimeCheckpoint;",
    "component_checkpoint": "use plakous_agent::RuntimeCheckpoint;",
    "store_authority": "use plakous::agent::SessionStore;",
    "driver_authority": "use plakous::agent::DurableSessionRuntime;",
    "checkpoint_codec": "use plakous::agent::RuntimeCheckpointCodec;",
    "driver_endpoint": "use plakous::agent::AgentSessionEndpoint;",
    "issued_invocation": "use plakous::tools::ToolInvocation;",
    "terminal_result": "use plakous::agent::TurnResult;",
    "credential_bytes": "use plakous::auth::SecretString;",
    "family_transport": "use plakous::responses::ResponsesAttemptHost;",
    "standard_descriptor": "use plakous::model::StandardIntegrationAuthority;",
    "release_evidence": "use plakous::model::StandardIntegrationAuthority;",
    "standard_identity": "use plakous::model::StandardIntegrationAuthority;",
    "standard_registration": "use plakous::model::StandardIntegrationAuthority;",
    "standard_configuration": "use plakous::model::StandardIntegrationAuthority;",
    "family_artifact_sealing": "use plakous::model::SealedFamilyArtifactCandidate;",
    "anthropic_transport": "use plakous::anthropic::AnthropicMessagesTransport;",
    "openrouter_model_lane": "use plakous::openrouter::OpenRouterLanguageModel;",
    "xai_platform_authority": "use plakous::xai::XaiPlatformApiKeyAuthority;",
    "xai_platform_grant": "use plakous::xai::XaiPlatformCredentialGrant;",
    "xai_platform_bearer": "use plakous::xai::XaiPlatformApiKey;",
    "local_composition_session": "use plakous::agent::LocalCompositionSession;",
}
AUTHORITY_NEGATIVE_MARKERS = {
    binary: statement.rsplit("::", maxsplit=1)[-1]
    .rstrip(";")
    .split(",", maxsplit=1)[0]
    .strip("{}")
    for binary, statement in AUTHORITY_NEGATIVE_IMPORTS.items()
}
AUTHORITY_NEGATIVE_MARKERS["credential_bytes"] = "expose"
AUTHORITY_NEGATIVE_SOURCES = {
    binary: f"{statement}\n\nfn main() {{}}\n"
    for binary, statement in AUTHORITY_NEGATIVE_IMPORTS.items()
}
AUTHORITY_NEGATIVE_SOURCES["issued_invocation"] = """use plakous::tools::ToolInvocation;

fn main() {
    let _ = ToolInvocation {};
}
"""
AUTHORITY_NEGATIVE_SOURCES["terminal_result"] = """use plakous::agent::TurnResult;

fn main() {
    let _ = TurnResult {};
}
"""
AUTHORITY_NEGATIVE_SOURCES["credential_bytes"] = """use plakous::auth::SecretString;

fn main() {
    let secret = SecretString::from("not-a-real-secret");
    let _ = secret.expose();
}
"""
for binary, argument, operation in (
    ("xai_platform_authority", "XaiPlatformApiKeyAuthority", "for_key_generation(&[])"),
    ("xai_platform_grant", "XaiPlatformCredentialGrant", "execution_authority()"),
    ("xai_platform_bearer", "XaiPlatformApiKey", "bearer()"),
):
    AUTHORITY_NEGATIVE_SOURCES[binary] = (
        f"{AUTHORITY_NEGATIVE_IMPORTS[binary]}\n\n"
        f"fn forbidden(value: &{argument}) {{\n    let _ = value.{operation};\n}}\n\nfn main() {{}}\n"
    )
    AUTHORITY_NEGATIVE_MARKERS[binary] = operation.split("(", maxsplit=1)[0]
AUTHORITY_NEGATIVE_ERROR_CODES = {
    binary: "E0432" for binary in AUTHORITY_NEGATIVE_BINS
}
AUTHORITY_NEGATIVE_ERROR_CODES["component_checkpoint"] = "E0603"
AUTHORITY_NEGATIVE_ERROR_CODES["issued_invocation"] = None
AUTHORITY_NEGATIVE_ERROR_CODES["terminal_result"] = None
AUTHORITY_NEGATIVE_ERROR_CODES["credential_bytes"] = "E0599"
AUTHORITY_NEGATIVE_ERROR_MESSAGES = {
    "issued_invocation": (
        "cannot construct `ToolInvocation` with struct literal syntax due to private fields"
    ),
    "terminal_result": (
        "cannot construct `TurnResult` with struct literal syntax due to private fields"
    ),
}
AUTHORITY_NEGATIVE_ERROR_LINES = {
    binary: 1 for binary in AUTHORITY_NEGATIVE_BINS
}
AUTHORITY_NEGATIVE_ERROR_LINES["issued_invocation"] = 4
AUTHORITY_NEGATIVE_ERROR_LINES["terminal_result"] = 4
AUTHORITY_NEGATIVE_ERROR_LINES["credential_bytes"] = 5
for binary in ("xai_platform_authority", "xai_platform_grant", "xai_platform_bearer"):
    AUTHORITY_NEGATIVE_ERROR_CODES[binary] = "E0624"
    AUTHORITY_NEGATIVE_ERROR_LINES[binary] = 4
AUTHORITY_NEGATIVE = tuple(
    (
        f"negative-authority-{binary.replace('_', '-')}",
        (
            "cargo",
            "check",
            "--manifest-path",
            "tests/fixtures/authority-negative/Cargo.toml",
            "--bin",
            binary,
            "--locked",
            "--offline",
            "--message-format=json",
        ),
    )
    for binary in AUTHORITY_NEGATIVE_BINS
)


def validate_supported_inventory_policy() -> bool:
    """Fail closed when the explicit supported-package inventory drifts."""
    packages = tuple(package for _, package, _ in SUPPORTED_API_FIXTURES)
    if packages != EXPECTED_SUPPORTED_PACKAGES:
        print(
            f"error: supported API fixtures must be exactly {EXPECTED_SUPPORTED_PACKAGES!r}",
            file=sys.stderr,
        )
        return False
    for source in NAMEABILITY_POLICY_SOURCES:
        text = source.read_text(encoding="utf-8")
        for name in RUNTIME_ONLY_TOOL_NAMES:
            if name in text:
                print(
                    f"error: runtime-only {name} is present in policy-pinned source {source}",
                    file=sys.stderr,
                )
                return False
    example_dir = ROOT / "crates/plakous/examples"
    declared_examples = {name for name, _, _ in EMBEDDING_EXAMPLES}
    actual_examples = {
        source.stem
        for source in example_dir.glob("*.rs")
        }
    if actual_examples != declared_examples:
        print(
            "error: embedding example inventory drifted: "
            f"{sorted(actual_examples)!r}, expected {sorted(declared_examples)!r}",
            file=sys.stderr,
        )
        return False
    for source in EMBEDDING_SOURCES:
        if "plakous_test_support" in source.read_text(encoding="utf-8"):
            print(
                f"error: {source} depends on a repository test-support crate; "
                "an external consumer cannot copy it",
                file=sys.stderr,
            )
            return False
    for _label, manifest in EMBEDDING_WORKFLOW_FIXTURES:
        if "[patch" in (ROOT / manifest).read_text(encoding="utf-8"):
            print(
                f"error: {manifest} carries a [patch] table; the embedding "
                "workflow fixtures pin that a consumer needs none",
                file=sys.stderr,
            )
            return False
    negative_source_dir = ROOT / "tests/fixtures/authority-negative/src/bin"
    actual_negative_bins = {source.stem for source in negative_source_dir.glob("*.rs")}
    if actual_negative_bins != set(AUTHORITY_NEGATIVE_BINS):
        print(
            "error: authority-negative fixture inventory drifted: "
            f"{sorted(actual_negative_bins)!r}, expected {sorted(AUTHORITY_NEGATIVE_BINS)!r}",
            file=sys.stderr,
        )
        return False
    for binary, expected in AUTHORITY_NEGATIVE_SOURCES.items():
        source = negative_source_dir / f"{binary}.rs"
        if source.read_text(encoding="utf-8") != expected:
            print(
                f"error: authority-negative fixture content drifted: {source}",
                file=sys.stderr,
            )
            return False
    return True


def has_authority_diagnostic(output: str, binary: str) -> bool:
    """Require the exact rustc error at the fixture's pinned authority span."""
    expected_file = f"src/bin/{binary}.rs"
    expected_line_number = AUTHORITY_NEGATIVE_ERROR_LINES[binary]
    expected_line = AUTHORITY_NEGATIVE_SOURCES[binary].splitlines()[
        expected_line_number - 1
    ]
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = message.get("message", {})
        code = diagnostic.get("code") or {}
        if code.get("code") != AUTHORITY_NEGATIVE_ERROR_CODES[binary]:
            continue
        expected_message = AUTHORITY_NEGATIVE_ERROR_MESSAGES.get(binary)
        if expected_message is not None and diagnostic.get("message") != expected_message:
            continue
        for span in diagnostic.get("spans", []):
            if (
                not span.get("is_primary")
                or span.get("line_start") != expected_line_number
            ):
                continue
            if not span.get("file_name", "").endswith(expected_file):
                continue
            source_lines = span.get("text", [])
            if len(source_lines) != 1 or source_lines[0].get("text") != expected_line:
                continue
            return True
    return False


def compiler_output_for_display(output: str) -> str:
    """Render rustc diagnostics without dumping Cargo's JSON artifacts."""
    displayed: list[str] = []
    for line in output.splitlines(keepends=True):
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            displayed.append(line)
            continue
        if message.get("reason") == "compiler-message":
            rendered = message.get("message", {}).get("rendered")
            if isinstance(rendered, str):
                displayed.append(rendered)
    return "".join(displayed)


def batch_passing_commands(
    pins: tuple[tuple[str, tuple[str, ...]], ...],
) -> tuple[tuple[tuple[str, ...], tuple[str, ...]], ...]:
    """A lifecycle run also proves its identical compile pin, not other features."""
    batches: dict[tuple[str, ...], tuple[list[str], tuple[str, ...]]] = {}
    strength = {"check": 0, "build": 1, "run": 2}
    for label, command in pins:
        verb = command[1]
        key = (command[0], *command[2:]) if verb in strength else command
        if key not in batches:
            batches[key] = ([label], command)
            continue
        labels, previous = batches[key]
        labels.append(label)
        if strength[verb] > strength[previous[1]]:
            batches[key] = (labels, command)
    return tuple((tuple(labels), command) for labels, command in batches.values())


def diagnostics_for_binary(output: str, binary: str) -> str:
    """Attribute batch diagnostics to their Cargo target, not a dependency."""
    records = []
    for line in output.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = record.get("target", {})
        if (
            record.get("reason") == "compiler-message"
            and target.get("name") == binary
            and "bin" in target.get("kind", [])
        ):
            records.append(line)
    return "\n".join(records)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--positive-only", action="store_true")
    selection.add_argument("--negative-only", action="store_true")
    selection.add_argument(
        "--fetch", action="store_true",
        help="prefetch every locked fixture's dependencies online without compiling",
    )
    parser.add_argument(
        "--only",
        action="append",
        metavar="LABEL",
        help="run only the pins whose label contains this text; repeatable",
    )
    parser.add_argument(
        "--archive-dir",
        type=Path,
        help="optionally write one complete log per compile pin for evidence archiving",
    )
    args = parser.parse_args()
    if args.fetch and args.only:
        parser.error("--fetch prefetches every fixture; --only is not supported")

    if not validate_supported_inventory_policy():
        return 1
    positives = (
        LOCK_PREFLIGHT
        + SUPPORTED_POSITIVE
        + EXAMPLE_POSITIVE
        + DEPENDENCY_POSITIVE
        + CONSUMER_MANIFEST_POSITIVE
    )
    passing_commands = (
        FETCH_FIXTURES
        if args.fetch
        else LOCK_PREFLIGHT + DOCUMENTATION_NEGATIVE
        if args.negative_only
        else positives
        if args.positive_only
        else positives + DOCUMENTATION_NEGATIVE
    )
    failing_pairs = (
        tuple(zip(AUTHORITY_NEGATIVE, AUTHORITY_NEGATIVE_BINS, strict=True))
        if not (args.positive_only or args.fetch)
        else ()
    )
    if args.only:
        selected = tuple(args.only)

        def wanted(label: str) -> bool:
            return any(fragment in label for fragment in selected)

        passing_commands = tuple(
            pin for pin in passing_commands if wanted(pin[0])
        )
        failing_pairs = tuple(pair for pair in failing_pairs if wanted(pair[0][0]))
        if not passing_commands and not failing_pairs:
            print(f"error: --only {selected} matched no pin", file=sys.stderr)
            return 1
    if args.archive_dir:
        args.archive_dir.mkdir(parents=True, exist_ok=True)
    cargo_environment = os.environ.copy() if args.fetch else contract_environment()

    for labels, command in batch_passing_commands(passing_commands):
        header = f"== {', '.join(labels)} ==\n$ {' '.join(command)}\n"
        print(header, end="", flush=True)
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=cargo_environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        if not all(label.startswith("locked-preflight-") for label in labels) or result.returncode:
            print(result.stdout, end="")
        log = header + result.stdout + f"exit_code={result.returncode}\n"
        if args.archive_dir:
            for label in labels:
                (args.archive_dir / f"{label}.log").write_text(log, encoding="utf-8")
        if result.returncode:
            return result.returncode
    if failing_pairs:
        command = (
            "cargo", "check", "--manifest-path",
            "tests/fixtures/authority-negative/Cargo.toml", "--locked", "--offline",
            "--keep-going", "--message-format=json",
            *(arg for _, binary in failing_pairs for arg in ("--bin", binary)),
        )
        header = f"== authority-negative batch (expected failures) ==\n$ {' '.join(command)}\n"
        print(header, end="", flush=True)
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=cargo_environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        print(compiler_output_for_display(result.stdout), end="")
        valid = result.returncode != 0
        if not valid:
            print("error: authority-negative batch unexpectedly compiled", file=sys.stderr)
        for (label, _), binary in failing_pairs:
            output = diagnostics_for_binary(result.stdout, binary)
            if args.archive_dir:
                log = (
                    f"== {label} (expected failure) ==\n"
                    + header + result.stdout + f"exit_code={result.returncode}\n"
                )
                (args.archive_dir / f"{label}.log").write_text(log, encoding="utf-8")
            if not has_authority_diagnostic(output, binary):
                print(
                    f"error: {label} compiled or failed without its exact rustc JSON diagnostic "
                    f"at the pinned authority marker {AUTHORITY_NEGATIVE_MARKERS[binary]!r}",
                    file=sys.stderr,
                )
                valid = False
        if args.archive_dir:
            log = header + result.stdout + f"exit_code={result.returncode}\n"
            (args.archive_dir / "authority-negative-batch.log").write_text(log, encoding="utf-8")
        if not valid:
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
