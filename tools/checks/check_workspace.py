#!/usr/bin/env python3
"""Validate the Plak application's Plakous workspace boundaries.

Layer facts are data, not tables in this file: every member manifest carries
`[package.metadata.plakous] layer/class[/neutral]` and the root `Cargo.toml`
carries `[workspace.metadata.plakous]` (layer matrix, edge exceptions, layout
templates, legal classes, host/bin production dependencies, facade features,
module rules). `tools/checks/plakous_policy.py` loads both from `cargo metadata`.
From that the checker derives the manifest inventory and paths, the internal
edge map, the package classification, the neutral-core set, the security scan
set (every host/bin package), and the facade-only symbol rule.

What stays hardcoded here, on purpose: the security pins (environment reads,
process commands and imports, macro allowlists, forbidden identifiers), the
approved `plakous::` symbols per host/bin package, the neutral-core provider
name exemptions, the retired-source pins, the consumer fixture inventories, the
rquickjs / tracing linkage pins, and the tracked-file hygiene patterns. Every path-bearing pin is keyed by package
name plus a package-relative path, so a package move never edits them.
"""

from __future__ import annotations

import fnmatch
import hashlib
import os
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tomllib
from collections.abc import Iterable
from functools import lru_cache
from pathlib import Path
from typing import Any, NamedTuple

sys.path.insert(0, str(Path(__file__).resolve().parent))

from plakous_policy import (  # noqa: E402
    ABOVE_FACADE_LAYERS,
    PACKAGE_CLASSES,
    Package,
    Workspace,
    edge_permitted,
    expand_layout_template,
    layout_matches,
    load_workspace,
)

ROOT = Path(__file__).resolve().parents[2]
# The root `[workspace.package]` values every package carries, by inheritance
# for a member and spelled out for a standalone fixture workspace.
WORKSPACE_LICENSE = "MIT"
WORKSPACE_REPOSITORY = "https://github.com/N0xMare/plak"

# The supported library packages' external compile-positive fixtures. The
# dependency paths are derived from the workspace at load time; only the
# file inventories and the requested facade features are pinned here.
SUPPORTED_COMPILE_FIXTURE_FILES = {
    "agent-consumer": frozenset(
        {
            ".cargo/config.toml",
            "Cargo.lock",
            "Cargo.toml",
            "src/control.rs",
            "src/main.rs",
            "src/observation.rs",
            "src/profiles.rs",
        }
    ),
    "facade-consumer": frozenset(
        {
            ".cargo/config.toml",
            "Cargo.lock",
            "Cargo.toml",
            "src/configuration.rs",
            "src/control_observation.rs",
            "src/custom_model.rs",
            "src/custom_tool.rs",
            "src/lifecycle.rs",
            "src/lifecycle_configuration.rs",
            "src/local_composition.rs",
            "src/main.rs",
            "src/providers.rs",
            "src/sessions.rs",
        }
    ),
    "js-embedder-consumer": frozenset(
        {".cargo/config.toml", "Cargo.lock", "Cargo.toml", "src/main.rs"}
    ),
    "model-consumer": frozenset({".cargo/config.toml", "Cargo.lock", "Cargo.toml", "src/main.rs"}),
    "tools-consumer": frozenset({".cargo/config.toml", "Cargo.lock", "Cargo.toml", "src/main.rs"}),
}
# Each supported compile fixture, as one closed contract: the workspace packages
# it depends on, the facade features it selects and the extra utility
# dependencies it pins. These are per fixture on purpose: `facade-consumer` is
# the broad public inventory and still reaches the Python evaluator, while
# `js-embedder-consumer` is the JavaScript-only embedding workflow.
SUPPORTED_COMPILE_FIXTURES = {
    "agent-consumer": {
        "description": (
            "Compile fixture: an out-of-workspace consumer of the `plakous-agent` core crate."
        ),
        "dependencies": ("plakous-agent",),
        "facade-features": (),
        "extra-dependencies": {},
    },
    "facade-consumer": {
        "description": (
            "Compile fixture: an out-of-workspace consumer of the `plakous` facade with its "
            "provider features."
        ),
        "dependencies": ("plakous",),
        "facade-features": (
            "anthropic",
            "native-subscriptions",
            "openai",
            "openrouter",
            "sessions-sqlite",
            "xai",
        ),
        "extra-dependencies": {
            "futures-util": "0.3.31",
            "serde_json": "1.0",
            "tempfile": "3.23",
            "tokio": {
                "version": "1.48",
                "features": ["rt-multi-thread", "macros", "sync", "time"],
            },
            "uuid": "1",
        },
    },
    "js-embedder-consumer": {
        "description": (
            "Compile fixture: an out-of-workspace consumer embedding the JavaScript evaluator "
            "through the `composition` feature."
        ),
        "dependencies": ("plakous",),
        "facade-features": ("composition", "sessions-sqlite"),
        "extra-dependencies": {
            "tempfile": "3.23",
            "tokio": {
                "version": "1.48",
                "features": ["rt-multi-thread", "macros", "sync", "time"],
            },
        },
    },
    "model-consumer": {
        "description": (
            "Compile fixture: an out-of-workspace consumer of the `plakous-model` core crate."
        ),
        "dependencies": ("plakous-model",),
        "facade-features": (),
        "extra-dependencies": {},
    },
    "tools-consumer": {
        "description": (
            "Compile fixture: an out-of-workspace consumer of the `plakous-model` and "
            "`plakous-tools` core crates."
        ),
        "dependencies": ("plakous-model", "plakous-tools"),
        "facade-features": (),
        "extra-dependencies": {},
    },
}
SUPPORTED_COMPILE_FIXTURE_DEPENDENCIES = {
    name: contract["dependencies"] for name, contract in SUPPORTED_COMPILE_FIXTURES.items()
}


def fixture_name_for(package: Package) -> str:
    """Return the primary consumer fixture directory name for a supported package."""
    return "facade-consumer" if package.layer == "facade" else f"{package.short}-consumer"


def supported_compile_fixtures(workspace: Workspace) -> dict[str, dict[str, Any]]:
    """Build the exact fixture contracts with dependency paths derived from the tree."""
    contracts: dict[str, dict[str, Any]] = {}
    for fixture_name, contract in SUPPORTED_COMPILE_FIXTURES.items():
        fixture_root = f"tests/fixtures/{fixture_name}"
        # These fixtures are standalone workspaces, so they cannot inherit
        # `[workspace.package]`. The shared fields are therefore spelled out and
        # pinned here, byte for byte, against the root manifest's own values.
        manifest: dict[str, Any] = {
            "package": {
                "name": f"plakous-{fixture_name}",
                "version": "0.0.0",
                "edition": "2024",
                "rust-version": "1.99.0",
                "description": contract["description"],
                "license": WORKSPACE_LICENSE,
                "repository": WORKSPACE_REPOSITORY,
                "readme": "../../../README.md",
                "publish": False,
                "metadata": {"plakous": {"layer": "test", "class": "repository-only"}},
            },
            "dependencies": {},
            "workspace": {},
        }
        for dependency in contract["dependencies"]:
            package = workspace.packages[dependency]
            entry: dict[str, Any] = {
                "version": "0.1.0",
                "path": os.path.relpath(package.directory, fixture_root),
            }
            if package.layer == "facade":
                entry["features"] = list(contract["facade-features"])
            manifest["dependencies"][dependency] = entry
        manifest["dependencies"].update(contract["extra-dependencies"])
        contracts[fixture_name] = {
            "manifest": manifest,
            "files": SUPPORTED_COMPILE_FIXTURE_FILES[fixture_name],
        }
    return contracts


# Provider-identifying dependency name parts a neutral package may never link.
FORBIDDEN_NEUTRAL_DEPENDENCY_PARTS = (
    "anthropic",
    "async-openai",
    "gemini",
    "google-generative",
    "openai",
    "openrouter",
    "xai",
)

# Retired packages that must never come back under any path.
RETIRED_PACKAGES = ("plak-compose-host",)

# OpenRouter is a Responses dialect, never a peer protocol stack.
RETIRED_SOURCES = {
    "plakous-openrouter": (
        "src/attempt.rs",
        "src/client.rs",
        "src/project.rs",
        "src/request.rs",
        "src/response.rs",
        "src/stream.rs",
    ),
}

# A test-only cfg predicate: `cfg(test)` or `cfg(all(test, ...))`.
_CFG_TEST_PREDICATE = r"cfg\s*\(\s*(?:test|all\s*\(\s*test\s*,[^()]*\))\s*\)"
# Attribute bodies may span lines (e.g. multi-line `#[allow(..., reason = "...")]`).
_CFG_ATTR = r"#\s*\[[^\]]*\]\s*"
CFG_TEST_BLOCK_ITEM = re.compile(
    rf"#\s*\[\s*{_CFG_TEST_PREDICATE}\s*\]\s*"
    rf"(?:{_CFG_ATTR})*"
    r"(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:(?:async|unsafe)\s+)*"
    r"(?:mod\s+(?:r#)?[A-Za-z_][A-Za-z0-9_]*|"
    r"fn\s+(?:r#)?[A-Za-z_][A-Za-z0-9_]*[^;{]*|"
    r"impl\b[^;{]*|trait\b[^;{]*|enum\b[^;{]*|struct\b[^;{]*)\s*\{",
    re.MULTILINE,
)
CFG_TEST_SEMICOLON_ITEM = re.compile(
    rf"#\s*\[\s*{_CFG_TEST_PREDICATE}\s*\]\s*"
    rf"(?:{_CFG_ATTR})*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:const|static|type|use|mod)\b[^;]*;",
    re.MULTILINE,
)

# An out-of-line `#[cfg(test)] mod name;` declaration. The file behind it is a
# whole-file test body: nothing in it is compiled into a production build, so it
# is skipped for every production rule instead of being scanned as shipping
# code. This mirrors the whole-file rule the density meter applies through
# `is_test_only_file_name` in check_prod_loc.py.
CFG_TEST_MODULE_DECLARATION = re.compile(
    rf"#\s*\[\s*{_CFG_TEST_PREDICATE}\s*\]\s*"
    rf"(?:{_CFG_ATTR})*"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*;",
    re.MULTILINE,
)

# ---------------------------------------------------------------------------
# Security pins. Hardcoded by design; keyed by (package, package-relative path)
# so a package move re-keys nothing and a file move re-keys one row.
# ---------------------------------------------------------------------------
FORBIDDEN_CLI_SYMBOLS = {
    "CodexAuthBroker",
    "CodexLoginStore",
    "LoginCommand",
    "OpenCodeXaiLoginStore",
}
FORBIDDEN_CLI_STRINGS = {
    "--api-key",
    "--base-url",
    "--endpoint",
    "--origin",
    "--token",
}
FORBIDDEN_CLI_FOREIGN_FUNCTIONS = {
    "__secure_getenv",
    "execl",
    "execle",
    "execlp",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "getenv",
    "popen",
    "posix_spawn",
    "posix_spawnp",
    "secure_getenv",
    "system",
}
FORBIDDEN_PRODUCT_SESSION_AUTHORITY_IDENTIFIERS = {
    "CompactStats",
    "CompactionThresholds",
    "compaction_summary",
    "context_window_for_model",
    "estimate_tokens",
    "estimate_transcript_tokens",
    "fork_session",
    "hot_swap_model",
    "keep_from_index",
    "local_structure_summary",
    "maybe_compact_session",
    "model_facing_lines",
    "product_compaction",
}
SourceKey = tuple[str, str]
EXPECTED_PRODUCT_ENVIRONMENT_CALLS: dict[SourceKey, list[tuple[str, tuple[tuple[str, str], ...]]]] = {
    # Maintainer/test-hooks private inference capture, explicitly selected per process.
    ("plak", "src/commands/exchange_capture.rs"): [
        ("var_os", (("string", "PLAK_TEST_EXCHANGE_CAPTURE_DIR"),)),
    ],
    ("plak", "src/diagnostics.rs"): [
        ("var_os", (("string", "PLAK_LOG"),)),
        ("var_os", (("string", "OTEL_EXPORTER_OTLP_ENDPOINT"),)),
    ],
    ("plak", "src/lib.rs"): [
        ("var_os", (("string", "XDG_RUNTIME_DIR"),)),
        ("var_os", (("string", "NO_COLOR"),)),
    ],
    # Config edit selects the operator's VISUAL/EDITOR shell command.
    ("plak", "src/commands/support.rs"): [
        ("var", (("identifier", "name"),)),
    ],
    # `test-hooks` only, and never in `default` or a release build: the
    # repository gate's offline model selector. Naming the model rather than a
    # bare flag keeps the selection an explicit binding fact.
    ("plak", "src/test_hooks.rs"): [
        ("var", (("string", "PLAK_TEST_MODEL"),)),
        ("var_os", (("string", "PLAK_TEST_ASSISTANT_DONE_RELEASE"),)),
        ("var_os", (("string", "PLAK_TEST_MODEL"),)),
        ("var_os", (("string", "PLAK_TEST_TUI_PILOT"),)),
        ("var_os", (("string", "PLAK_TEST_SCRIPT"),)),
        ("var_os", (("string", "PLAK_TEST_EVIDENCE"),)),
        ("var_os", (("string", "PLAK_TEST_PROVIDER_RELEASE"),)),
    ],
    # cfg(test)-only process-test seam: the PTY latency budgets scale with an
    # explicit operator knob instead of flaking under nextest load.
    ("plak", "src/test_support.rs"): [
        ("var", (("string", "PLAK_TEST_TIME_SCALE"),)),
    ],
    # The one-shot CLI face collapses a tool's lifecycle to one line on
    # completion. `PLAK_VERBOSE` restores the per-tool progress lines for a
    # reader who wants them; nothing else is read.
    ("plak", "src/commands/run/face.rs"): [
        ("var_os", (("string", "PLAK_VERBOSE"),)),
    ],
    # `/reload` resolves its own argv[0] on PATH the way the shell did.
    ("plak", "src/build_identity.rs"): [
        ("var_os", (("string", "PATH"),)),
    ],
    ("plak", "src/oauth_ui.rs"): [
        ("var_os", (("string", "SSH_CONNECTION"),)),
        ("var_os", (("string", "SSH_CLIENT"),)),
        ("var_os", (("string", "SSH_TTY"),)),
    ],
    ("plak", "src/sidecars/python_bundle.rs"): [
        ("var_os", (("identifier", "EXPLICIT_ENV"),)),
    ],
    ("plak", "src/sidecars/js_host.rs"): [
        ("var_os", (("identifier", "EXPLICIT_ENV"),)),
    ],
    # an `on_mint` hook runs with `env_clear()`, so the only
    # host variables that reach a template-authored shell command are these
    # two — never an exported `*_API_KEY`. `EMAIL` is only tested for
    # presence, so Plak's own git steps add a fallback identity when it is unset.
    ("plakous-workspace", "src/occupancy_vcs.rs"): [
        ("var", (("string", "PATH"),)),
        ("var", (("string", "HOME"),)),
        ("var_os", (("string", "EMAIL"),)),
    ],
    ("plak", "src/home/paths.rs"): [
        ("var_os", (("string", "PLAK_HOME"),)),
        ("var_os", (("string", "HOME"),)),
        ("var_os", (("string", "XDG_STATE_HOME"),)),
        ("var_os", (("string", "HOME"),)),
        ("var_os", (("string", "HOME"),)),
        ("var_os", (("string", "PLAK_TEST_TMPDIR"),)),
    ],
    ("plak", "src/home/secrets.rs"): [
        ("var_os", (("identifier", "name"),)),
        ("var", (("identifier", "name"),)),
        ("var_os", (("identifier", "name"),)),
        ("var", (("identifier", "name"),)),
        ("var_os", (("identifier", "name"),)),
    ],
    ("plakous-workspace", "src/workspace.rs"): [
        ("var_os", (("string", "HOME"),)),
    ],
    ("plakous-workspace", "src/git_redirect_fence.rs"): [
        ("var_os", (("string", "PATH"),)),
        ("var_os", (("string", "PATH"),)),
    ],
}
EXPECTED_PRODUCT_GENERATED_MACROS: dict[SourceKey, tuple[str, ...]] = {
    # Compile-time repository location restricts the private capture root.
    ("plak", "src/commands/exchange_capture.rs"): ("env",),
    # Spec 02 §3.2: the help pages are one-level static text, so they are static
    # files under `bin/plak/help/` included at compile time — three global
    # fragments (head, maintainer verbs, tail) and one page per verb. The
    # renderer supplies only what a page cannot state for itself.
    ("plak", "src/cli/render.rs"): ("include_str",) * 23,
    # The three provider homes are the built-in Agents profiles. Each home's
    # four role files are compiled into the binary so a user with no
    # `~/.plak/profiles` still has codex, claude, and grok.
    ("plak", "src/profiles.rs"): ("include_str",) * 12,
}
EXPECTED_PRODUCT_PROCESS_COMMANDS: dict[SourceKey, list[tuple[str, tuple[tuple[str, str], ...]]]] = {
    ("plak", "src/terminal_application.rs"): [
        ("std::process::Command", (("string", "open"),)),
        ("std::process::Command", (("string", "xdg-open"),)),
        ("std::process::Command", (("string", "rundll32"),)),
    ],
    # `/reload` execs the installed `plak` its own argv[0] resolves to.
    ("plak", "src/lib.rs"): [
        ("std::process::Command", (("punct", "&"), ("identifier", "executable"))),
    ],
    ("plak", "src/commands/support.rs"): [
        ("std::process::Command", (("string", "/bin/sh"),)),
    ],
    # Carry-forward asks the installed `plak` for its reload verdict (the
    # destination probe, then the bare state-key fallback) and has a kept
    # maintainer snapshot export a session its own state holds.
    ("plak", "src/carry.rs"): [
        ("std::process::Command", (("punct", "&"), ("identifier", "executable"))),
        ("std::process::Command", (("identifier", "executable"),)),
        ("std::process::Command", (("identifier", "snapshot"),)),
    ],
    ("plak", "src/oauth_ui.rs"): [
        ("Command", (("identifier", "prog"),)),
    ],
    # Behind `test-support`: real-repository fixtures for the crates whose
    # tests drive a copy. Pinned like every other invocation in this package.
    ("plakous-workspace", "src/vcs_fixtures.rs"): [
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "jj"),)),
        ("Command", (("string", "jj"),)),
        ("Command", (("string", "jj"),)),
        ("Command", (("string", "jj"),)),
        ("Command", (("string", "jj"),)),
        ("Command", (("string", "jj"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
        ("Command", (("string", "git"),)),
    ],
    ("plakous-workspace", "src/occupancy_vcs.rs"): [
        # `sh -c` runs a template's `on_mint` hook (with an empty environment).
        ("Command", (("string", "sh"),)),
        ("Command", (("string", "git"),)),
    ],
}
EXPECTED_PRODUCT_PROCESS_IMPORTS: dict[SourceKey, set[str]] = {
    ("plak", "src/main.rs"): {"ExitCode"},
    ("plak", "src/lib.rs"): {"ExitCode"},
    ("plak", "src/oauth_ui.rs"): {"Command", "Stdio"},
    ("plakous-workspace", "src/occupancy_vcs.rs"): {"Command", "Output"},
    ("plakous-workspace", "src/vcs_fixtures.rs"): {"Command"},
}
# Macros any host/bin production source may invoke.
ALLOWED_CLI_MACROS = {
    "assert",  # compile-time const budget ties (e.g. turn TTL vs catalog)
    "eprintln",
    "format",
    "info_span",  # tracing boundary spans; typed fields only
    "matches",
    "unreachable",
    "vec",
    "write",
    "writeln",
}
# Per-source additions to ALLOWED_CLI_MACROS.
ALLOWED_CLI_MACROS_BY_SOURCE: dict[SourceKey, set[str]] = {
    # Fold integrity refusals: fixed diagnostics and the affected agent id only.
    ("plakous-recursive-subagents", "src/tree/candidates.rs"): {"error"},
    ("plakous-recursive-subagents", "src/tree/workspaces.rs"): {"error"},
    # Private capture diagnostics use closed metadata, never request bodies.
    ("plak", "src/commands/exchange_capture.rs"): {"env", "warn"},
    # A published export whose directory sync failed is kept and logged.
    ("plak", "src/carry.rs"): {"warn"},
    ("plakous-hosted", "src/mcp_binding.rs"): {"warn"},
    # `test-hooks` only: scripted offline model plans for child fixtures.
    ("plak", "src/test_hooks.rs"): {"json"},
    ("plak", "src/build_identity.rs"): {"cfg"},
    ("plak", "src/tui/launch_intro.rs"): {"cfg"},
    # The `test-support`-only persistence benchmark checks that its final
    # reload retained every transcript line it timed.
    ("plak", "src/store/benchmark.rs"): {"assert_eq"},
    # A `#[cfg(test)]`-only module of the host crate: it cannot be an
    # integration test because the projection it exercises is
    # `pub(crate)`. Nothing here ships in the CLI binary.
    ("plak", "src/topology_resources_projection.rs"): {"assert_eq"},
    ("plak", "src/commands/session_delete_tests.rs"): {"assert_eq", "assert_ne"},
    # Read-only child history projects its bounded JSONL response at the CLI boundary.
    ("plak", "src/commands/session/read_only.rs"): {"json"},
    # `/reload` logs the exec of the installed build before replacing itself.
    ("plak", "src/lib.rs"): {"eprint", "info", "join"},
    # The help pages; see EXPECTED_PRODUCT_GENERATED_MACROS.
    ("plak", "src/cli/render.rs"): {"include_str"},
    ("plak", "src/profiles.rs"): {"include_str"},
    ("plak", "src/commands/application.rs"): {"select"},
    ("plak", "src/commands/secret_input.rs"): {"select"},
    # The SIGTERM/SIGHUP watcher thread waits on two signal streams at once.
    ("plak", "src/signals.rs"): {"select"},
    ("plakous-hosted", "src/runtime/turn_drive/mod.rs"): {"pin", "select"},
    ("plakous-hosted", "src/runtime/network_watch.rs"): {"select"},
    ("plakous-hosted", "src/runtime/turn_drive/admission.rs"): {"thread_local"},
    ("plakous-hosted", "src/interrupt.rs"): {"pin", "select"},
    # The product half of the session record keeps the one-entry store cache.
    ("plak", "src/store/sessions.rs"): {"thread_local"},
    # The highlighter cache is thread-local because syntect parse/highlight
    # state is mutable and TUI rendering may occur on more than one thread.
    ("plak", "src/tui/highlight.rs"): {"thread_local"},
    ("plak", "src/tui/activity.rs"): {"thread_local"},
    ("plakous-host-protocol", "src/jsonl.rs"): {"json"},
    ("plakous-hosted", "src/session_log.rs"): {"thread_local"},
    # Canonical bounded result-context requests for the durable operation journal.
    ("plakous-hosted", "src/agent/result_context.rs"): {"json"},
    # Explicit unreachable operation in the feature-gated hosted test fixture.
    ("plakous-hosted", "src/runtime/test_kit.rs"): {"panic"},
    ("plakous-hosted", "src/sessions.rs"): {"thread_local"},
    ("plak", "src/store/mod.rs"): {"params"},
    ("plak", "src/store/views.rs"): {"params"},
    ("plak", "src/store/tree.rs"): {"params"},
    ("plakous-recursive-subagents-store", "src/database.rs"): {"params"},
    ("plakous-recursive-subagents-store", "src/rows.rs"): {"params"},
    ("plakous-recursive-subagents", "src/epoch.rs"): {"json"},
    ("plakous-recursive-subagents", "src/hub.rs"): {"error", "info", "pin", "select", "warn"},
    ("plakous-recursive-subagents", "src/tools.rs"): {"json", "thread_local", "select"},
    ("plakous-recursive-subagents", "src/tree/mail.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tree/mod.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tree/projection.rs"): {"json"},
    # Static WAL-checkpoint contention warning; no SQL, paths, or session data.
    ("plak", "src/telemetry_store.rs"): {"params", "warn"},
    ("plak", "src/telemetry_sink.rs"): {"warn"},
    # A refused tree publish (the store's compare-and-swap found another
    # writer) is reported through the tracing boundary, like the
    # credential-refresh warning above; typed fields only.
    ("plakous-hosted", "src/agent/children.rs"): {"error", "pin", "select"},
    # Background recovery reports only typed child ids and closed recovery
    # dispositions after the pass has settled; it never logs prompt content.
    ("plakous-hosted", "src/agent/recovery.rs"): {"info"},
    # Each model-request retry logs its closed notice fields and a bounded
    # detail, which may carry the provider's credential-filtered error
    # message; never request or response bodies.
    ("plakous-hosted", "src/runtime/events.rs"): {"warn"},
    # A wake the session store refused logs its cause class, never content.
    ("plakous-hosted", "src/runtime/host_loop/wake.rs"): {"warn"},
    ("plak", "src/sidecars/python_bundle.rs"): {"cfg"},
    # The sidecar's stdio loop multiplexes the client channel and the evaluator.
    # Presentation-only invariants and the per-frame tracing span.
    ("plak", "src/tui/tree_navigator.rs"): {"debug_assert_eq"},
    ("plak", "src/tui/ui/render.rs"): {"debug_span"},
}
# Rust keywords that may legally precede `!` without naming a macro (`if !x`).
_NOT_MACRO_KEYWORDS = {"else", "if", "in", "match", "return", "while"}

# The facade symbols a host/bin production source may import, per package.
# Today every scanned package shares one approved list; a per-package narrowing
# is a one-row edit here.
_SHARED_PLAKOUS_SYMBOLS = {
    "openai": {
        "CredentialAuthorityId", "CredentialReference", "EndpointOrigin", "OpenAiApiKey",
        "OpenAiApiKeyGenerationGrant", "OpenAiApiModel", "OpenAiApiModelStatus",
        "OpenAiAuthError", "OpenAiListedModel", "OpenAiPlatformModel",
        "OpenAiPlatformModelBuilder", "OpenAiPlatformModelFacts", "PlatformCredentialSource",
        "PlatformProjectPolicy", "PlatformStoragePolicy", "StaticPlatformCredentialSource",
        "accept_openai_listed_models", "list_openai_api_models", "openai_api_bundled_listing",
        "openai_api_key_fingerprint", "openai_api_models",
    },
    "anthropic": {
        "AnthropicApiKey", "AnthropicApiKeySource", "AnthropicLanguageModel", "AnthropicError",
        "AnthropicErrorKind", "AnthropicErrorPhase", "claude_max_output_tokens",
        "CLAUDE_ENROLLMENTS", "AnthropicApiModel", "AnthropicApiModelStatus",
        "AnthropicListedModel", "accept_anthropic_listed_models", "anthropic_api_bundled_listing",
        "anthropic_api_models", "list_anthropic_api_models",
    },
    "xai": {
        "XaiApiModel", "XaiApiModelError", "XaiApiModelStatus", "XaiListedModel",
        "XaiPlatformApiKey", "XaiPlatformApiKeyAuthority", "XaiPlatformApiKeySource",
        "XaiPlatformBuildError", "XaiPlatformCredentialGrant", "XaiPlatformModel",
        "accept_xai_listed_models", "grok_max_output_tokens", "list_xai_api_models",
        "xai_api_bundled_listing", "xai_api_key_fingerprint", "xai_api_models",
        "xai_platform_selection", "xai_platform_selection_for", "XAI_PLATFORM_GROK_46_MODEL",
        "XAI_PLATFORM_GROK_46_CONTEXT_TOKENS", "XAI_PLATFORM_MAX_OUTPUT_TOKENS",
    },
    "exchange_observation": {
        "ConnectionUse", "ExchangeObserver", "ExchangeObservation", "ExchangeTerminal",
    },
    "openrouter": {
        "OPENROUTER_CHAT_PROTOCOL_ID",
        "OPENROUTER_MESSAGES_PROTOCOL_ID",
        "OPENROUTER_RESPONSES_PROTOCOL_ID",
        "OpenRouterApiKey",
        "OpenRouterApiKeyGenerationGrant",
        "OpenRouterApiKeySource",
        "OpenRouterAuthError",
        "OpenRouterCatalogCredentialRequest",
        "OpenRouterChatClient",
        "OpenRouterChatCredentialRequest",
        "OpenRouterChatEndpointCandidate",
        "OpenRouterChatModelEvidence",
        "OpenRouterReasoningFacts",
        "OpenRouterToolChoiceModes",
        "OpenRouterCredentialBinding",
        "OpenRouterCredentialRequest",
        "OpenRouterKeyUsage",
        "OpenRouterKeyUsageError",  # Host renders typed 401/403 refusals with safe source labels.
        "OpenRouterMessagesCredentialRequest",
        "OpenRouterRetentionPolicy",
        "OpenRouterRetrievalCredentialRequest",
        "StaticOpenRouterApiKeySource",
    },
    "agent": {
        "ViewRequest",
        # A child's attachment snapshot and tail, named by the hosted child
        # observer that serves both the overlay and the agent transcript.
        "AgentViewSnapshot",
        "EventSubscription",
        "AbandonedTurnClaim",
        "AbandonedTurnReport",
        "AbandonmentCause",
        "Agent",
        "EffectDecisionRef",
        "EffectDecisionResolution",
        "AgentBuilder",
        "AgentConfig",
        "AgentError",
        "AgentErrorKind",
        "AgentEvent",
        "AgentEventKind",
        "AgentRequestPolicy",
        "codex_minimal_local_code_mode_sidecar",
        "CapabilityRequirement",
        "CachePreferenceStrategy",
        "CommandAckDisposition",
        "CommandId",
        "ContextOverflowRemedy",
        "ContextSourceCatalog",
        "ContextSourceResolver",
        "ContextSourceSelector",
        "ContinuationPreferenceStrategy",
        "Digest",
        "DigestDomain",
        "GenerationLimits",
        "HarnessProfileId",
        "HarnessProfileProvenance",
        "HarnessProfileSpec",
        "HarnessProfileSpecBuilder",
        "InstructionCompositionStrategy",
        "LocalCompositionAttachment",
        "LocalCompositionEngineArtifact",
        "LocalCompositionProfile",
        # The host's re-resolution hook for a model past its admitted window.
        "ModelReresolutionError",
        "ModelReresolutionOperation",
        "ModelReresolver",
        # A Content view carries a response completion only as its kind; the
        # host settles the response's reasoning there.
        "NormalizedModelEventKind",
        "ProductExtensionEvidence",
        "ProductExtensionLimits",
        "ProfileError",
        "ProfileSemanticVersion",
        "ReasoningEffort",
        "ReasoningPolicy",
        "ReasoningPreferenceStrategy",
        "RecoveredTurnHandle",
        "RegisteredContextSource",
        "ReresolvedModel",
        "ResolvedContextSourceFact",
        "Turn",
        "TurnControl",
        # Owned child startup inspects the original typed rejection acknowledgement.
        "TurnCommand",
        "CancellationStatus",
        # The host must tell a cancel refused because the turn is gone from a
        # cancel refused while it is still running; only the closed kind says
        # which (plakous-hosted `interrupt::CancelRefusal`).
        "ControlError",
        "ControlErrorKind",
        "TurnEventError",
        "TurnResult",
        "TurnResultHandle",
        "TurnResultOutcome",
        "TurnSubmission",
        "TurnUsage",
        "RecoveredTurnHandle",
        "CompactionEventOutcome",
        "CompactionEventReason",
        "CompactionRefusalReason",
        # A committed compaction reports which rung ran and what it cost; the
        # hosted note and the `compact.finished` record render both.
        "CompactionRung",
        "CompactionSummary",
        "DurabilityAttemptKind",
        "DurabilityAttemptOutcome",
        "TelemetryContext",
        "TelemetryEmitter",
        "TelemetryObservation",
        "TelemetryObservationKind",
        "TelemetrySink",
        "TelemetrySinkError",
        "TelemetryTerminalStatus",
        "ChildProcessKind",
        "ChildProcessExit",
        "CredentialProviderLabel",
        "CredentialRefreshDetail",
        "CredentialRefreshFailureClass",
        "CredentialRefreshFailureReason",
        "CredentialRefreshPhase",
        "HttpStatusClass",
        "ToolCallObservation",
        "ToolCallerClass",
        "ToolCallerLineageView",
        "ToolExposureStrategy",
        "compile_explicit_profile",
        "compile_explicit_runtime_profile",
        "attach_local_composition",
    },
    "application": {
        "ApplicationExecutor",
        "ApplicationExecutorError",
        "TurnDriveError",
        "TurnDriveOutcome",
        "run_once_until_signal",
    },
    "model": {
        "Clock",
        "ToolResultKind",
        "TurnOrigin",
        "SystemClock",
        "Actor",
        "AgentId",
        "AttemptId",
        "BlockStart",
        "BlockStarted",
        "BranchId",
        "CallId",
        "OutputLocation",
        "ReasoningDelta",
        "ReasoningVisibility",
        "ReasoningVisibilityBound",
        "Timestamp",
        "UsageUpdate",
        "CapacityRule",
        "CachePolicy",
        "Capability",
        "CapabilityRequirement",
        "CheckpointId",
        "ContentBlock",
        "Digest",
        "DigestDomain",
        "EffectiveProviderConfiguration",
        "IntegrationClass",
        "EffectiveProviderConfigurationId",
        "ConfiguredLanguageModel",
        "ConfiguredTool",
        "ContextAdmissionRequirement",
        "ContextAnchor",
        "ContextCapacityFailure",
        "ContextEvidenceClass",
        "ContextNonGuaranteeDisposition",
        "ContextCapacity",
        "ContextInputParts",
        "ContextMeasurement",
        "ContextStatus",
        "ContextStatusKind",
        "context_occupancy_tokens",
        "CountBound",
        "ConversationItem",
        "CompiledProviderConfiguration",
        "FactSource",
        "FinishReason",
        "HistorySegment",  # carried in the session export document
        "HistorySegmentKind",  # carried in the session export document
        "EntryId",  # bounded carried checkpoint reconstruction
        "Provenance",  # bounded carried checkpoint reconstruction
        "TextBlock",  # bounded carried checkpoint reconstruction
        "ToolResultPart",  # readable text retained in the inert carry archive
        "SafeDiagnostic",  # bounded provider refusal details for host presentation
        "GenerationLimits",
        "LanguageModel",
        "LocalDurabilityDisposition",
        "ModelAdapterProfileId",
        "ModelError",
        "ModelErrorKind",
        "ModelErrorContext",
        # a quota refusal fills the attempt's resets_at from
        # the provider's own retry disposition.
        "ModelRetryability",
        "ModelErrorPhase",
        "ModelHttpStatus",
        "ModelRetryRisk",
        "CancellationToken",
        "ReachabilityProbe",
        "ModelRetryCause",
        "ModelRetry",  # transient, redaction-safe per-agent retry logging hook
        "ModelRetryLoss",
        "ModelRetryDeclineReason",  # closed telemetry spelling
        "ModelRetryPhase",
        # The hosted xAI freshness check distinguishes stale receipts from
        # invalid or unavailable authority before exact session reopen.
        "RuntimeModelAuthorityError",
        # Typed projection notices are presentation acknowledgements, not replay authority.
        "ModelDiagnostic",
        "ModelEvent",
        "ModelId",
        "ModelProviderRequestId",
        "ModelResponse",
        "ServedBy",
        "OutputBoundDisposition",
        "OutputPolicy",
        "OutputReservation",
        "PortableToolSchemaV2",
        "ParallelToolDisposition",
        "ProcessingTierDisposition",
        "ProviderBehaviorConfiguration",
        "ProviderProfileRevisionId",
        "ProviderServiceTierId",
        "ProviderStorageDisposition",
        "ProtocolId",
        "ReasoningEffort",
        "ReasoningEffortDisposition",
        "ReasoningIncludeDisposition",
        "ReasoningPolicy",
        "ReasoningSummaryDisposition",
        "ReplayDisposition",
        "ResponseStateDisposition",
        "RetentionDisposition",
        "RouteId",
        "SafeLimit",
        "SessionId",
        "SessionLineage",
        "Support",
        "SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS",
        "SEMANTIC_COMPACTION_SUMMARY_TOKENS",
        "SelectedProviderConfiguration",
        "SelectedProviderConfigurationId",
        "SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO",
        "SUBSCRIPTION_BALANCE_PREPAID_CREDITS",
        "SubscriptionUsageBalance",
        "SubscriptionUsagePeriod",
        "SubscriptionUsageRead",
        "SubscriptionUsageSnapshot",
        "SubscriptionUsageCredentialState",
        "SubscriptionUsageUnavailable",
        "ToolChoice",
        "ToolCallerDisposition",
        "ToolCallerLineage",
        "ToolConcurrency",
        "ToolDefinition",
        "ToolEffect",
        "ToolExecutionDisposition",
        "ToolInputContract",
        "ToolName",
        "ToolOutputContract",
        "ToolPresentationMode",
        "Timestamp",
        "TurnId",
        "Usage",
        # The leaf crate owns the shared wall clock and the unframed sha256/hex
        # primitives, so the product stops hand-rolling either one.
        "hex_lower",
        "now_unix_millis",
        "now_unix_secs",
        "sha256_bytes",
        "sha256_hex",
    },
    "sessions": {
        "AgentSessions",
        # The change feed and its bounded reads (spec 01 §5.11): the one
        # projector's only inputs (spec 02 §2.6).
        "feed",
        "FeedBranchTransition",
        "FeedCall",
        "FeedCallOutcome",
        "FeedCallRequest",
        "FeedCallTarget",
        "FeedCallerLineage",
        "FeedCancelCause",
        "FeedChange",
        "FeedCommandKind",
        "FeedCompactPass",
        "FeedCursor",
        "FeedFact",
        "FeedHeadKind",
        "FeedHeader",
        "FeedItemsOutcome",
        "FeedItemsRequest",
        "FeedLocator",
        "FeedOperationKind",
        "FeedOperationState",
        "FeedPage",
        "FeedPageRequest",
        "FeedRecord",
        "FeedSeq",
        "FeedTerminalOutcome",
        "FeedTerminalRequest",
        "FeedTurnOutcome",
        "FeedWorkflowKind",
        "FeedWorkflowOutcome",
        "RUNTIME_FORMAT_EPOCH",
        "RUNTIME_STORE_SCHEMA_VERSION",
        "CreateSessionOutcome",
        "CreateSessionRequest",
        "ForkSessionOutcome",
        "ForkSessionRequest",
        "AgentSessionsError",
        "AgentSessionsErrorKind",
        "ArchiveSessionReport",
        "AbandonCompactionOutcome",
        "AbandonCompactionRequest",
        "CompactReason",
        "CompactSessionOutcome",
        "CompactSessionRequest",
        "ContextQuery",
        "ContextStatusRequest",
        "DeleteSessionReport",
        "OpenSessionOutcome",
        "OpenSessionRequest",
        "OpenedSession",
        "PortableSessionHistory",
        "PruneHistoryOutcome",
        "RebindSessionOutcome",
        "RebindSessionRequest",
        "RecoverSessionOutcome",
        "RecoverSessionRequest",
        "RecoverEffectEvidenceOutcome",
        "RecoverEffectEvidenceRequest",
        "RecoveryRequiredReport",
        "SessionEffectEvidence",
        "SessionRecoveryAction",
        "SessionBackend",
        "SessionBindingReport",
        # Bounded, redacted call-detail read for the transcript item expansion.
        "SessionCallDetailOutcome",
        # Typed, redacted read of committed history for one agent's transcript view.
        "SessionTranscriptItem",
        "SessionTranscriptPart",
        "SessionTranscriptTail",
        "SessionTranscriptSource",
        "SessionPersistenceShape",
        "SessionStoreAdmission",
        "SessionStoreBound",
        "SessionStoreBoundUsage",
        "SessionStorePressure",
        "SessionStoreUsage",
        "SessionLocator",
        "SessionOperationId",
        "SessionRuntimeBinding",
        "isolated_tool_scheduler",
        "SessionContextStatus",
        "SessionContextStatusKind",
        "SessionTargetBinding",
        "SESSION_HISTORY_VISIBILITY_REVISION",
        "SessionHistoryCursor",
        "SessionHistoryDirection",
        "SessionHistoryItem",
        "SessionHistoryPageOutcome",
        "SessionHistoryPageRequest",
        "SessionHistoryVisibility",
    },
    "subscriptions": {
        "CompatibilityNotice",
        "CompatibilityStatus",
        "FacadeAuthStatus",
        "FacadeCompatibilityStatus",
        "NativeAuthStatus",
        "ForcedRefresh",
        "NativeLoginCompletion",
        "NativeLoginMode",
        "NativeModelCatalog",
        "NativeModelSelector",
        "NativeModelSelectorError",
        "NativeModelVisibility",
        "NativeProvider",
        "NativeSubscriptionError",
        "NativeSubscriptions",
        "NativeToolMode",
        "PendingNativeLogin",
        "ResolvedProvider",
        "SubscriptionInferenceScope",
        "SubscriptionPaths",
        "SubscriptionRequestClass",
        "SubscriptionRequestGate",
        "SubscriptionRequestLimits",
        "SystemSubscriptionGateClock",
        "TurnScopedSubscriptionRequestGate",
        "native_provider_route_selection",
        "install_credential_telemetry",
    },
    "tools": {
        "ToolPreempt",
        "AlwaysAllowPolicy",
        # A template's `mcp:<id>` names parse through the facade's server id.
        "McpServerName",
        # Hosted-retrieval wrappers bind through the facade's web module.
        "web",
        "RetrievalPort",
        "WebTool",
        "ExecutionBackendError",
        "JobConfig",
        "JobHost",
        "JobHostCapacity",
        "JobOutputRoot",
        "JobOwner",
        "JobStatus",
        "JobView",
        "JobStopReason",
        "JoinedProcessSessions",
        "plan_restoration_note",
        # The composition owns the evaluator-worker port; the product supplies
        # its implementation through plakous-compose.
        "SidecarLocator",
        "HostProcessOptions",
        "EffectDecisionAuditId",
        "CodingPresentation",
        "CodexNativeLocalCodeModeV1",
        "CodexMinimalLocalCodeModeV1",
        # One resolved outer/nested tool namespace per construction (B8).
        "CompositionRuntime",
        "ResolvedNamespaces",
        "FrozenToolRegistry",
        "InMemoryPlanState",
        "MenuBinding",
        "MenuEntry",
        "MenuShape",
        # The bound a process tool cuts its own output at. The CLI face names
        # it when it tells a user their tool output was truncated, so the
        # number on screen is the number the tool used.
        "PROCESS_OBSERVATION_BYTES",
        "PROCESS_TOOL_PROJECTION_REVISION",
        "PlanStateIdentity",
        "ProcessSessionTools",
        "PRODUCT_JAVASCRIPT_RUN_NAME_V1",
        "PRODUCT_JAVASCRIPT_WAIT_NAME_V1",
        "PYTHON_PTC_RUN_NAME_V1",
        "PYTHON_PTC_WAIT_NAME_V1",
        "ProcessToolProjectionKind",
        "PromptPin",
        "ProviderModelProjectionIdentityV2",
        "ProductJavaScriptCompositionDescriptorV1",
        "ProductJavaScriptCompositionV1",
        "PythonPtcDescriptorV1",
        "PythonPtcPresentationV1",
        "PythonPtcV1",
        "ReadOnlyToolPolicy",
        "RegisteredToolId",
        "Tool",
        "ToolContext",
        "ToolExecutor",
        "ToolExecutorBindingIdentity",
        "ToolExecutorId",
        "ToolFuture",
        "ToolInvocation",
        "ToolOutcome",
        "ToolPack",
        "CodingPresentationExt",
        "CompiledSurface",
        "PromptContribution",
        "PromptDigest",
        "Earned",
        "MenuEntry",
        "PROCESS_PROJECTION_FAMILY",
        "PassthroughPack",
        "PresentationRoute",
        "PresentationRouteTuple",
        "PresentationRoutes",
        "ProjectionCandidateAdmission",
        "compile",
        "compile_over",
        "drifted_pins",
        "menu_names",
        "register_packs",
        "standard_coding_menus",
        "standard_coding_packs",
        "validate_process_presentation_candidate_v2",
        "ToolProjectionLoweringV2",
        "ToolPolicy",
        "ToolPolicyDecision",
        "ToolPolicyDenial",
        "ToolPolicyFuture",
        "ToolPolicyRequest",
        "ToolPolicyRevision",
        "ToolProjectionMap",
        "ToolRegistration",
        "ToolRegistryBuilder",
        "ToolRegistryLimits",
        "ToolRuntimeError",
        "ToolRuntimeErrorKind",
        "WorkspacePolicy",
        "process_presentation_candidate_v2",
        "process_presentation_candidate_with_extensions_v2",
    },
    "profiles": {
        "claude",
        "codex",
        "grok",
        "neutral_coding",
    },
    # `plakous-prompts` re-exported by the facade: the instruction text, its
    # assembler and its vocabulary, and bounded AGENTS.md discovery. The tool
    # descriptions the product still owns are not here -- they travel with the
    # topology pack and live in `crates/orchestration/recursive-subagents/src/text/`.
    "prompts": {
        "bases",
        "identity",
        "instructions",
        "pins",
        "plan",
        "ptc",
        "vocabulary",
        "AgentTier",
        "ASSIGNMENT_CONTRIBUTION_ID",
        "Composition",
        "PLAK_PRODUCT_ENVIRONMENT_TEMPLATE",
        "PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER",
        "PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE",
        "PLAK_PRODUCT_TOPOLOGY_AGREEMENTS",
        "PLAK_PRODUCT_VCS_POLICY",
        "ProductAgreementPack",
        "TOPOLOGY_CONTRIBUTION_ID",
        "WireDialect",
        "discover_instruction_sources",
        "openrouter_direct_base_instructions",
        "target_instructions",
    },
}
ALLOWED_PLAKOUS_SYMBOLS_BY_PACKAGE: dict[str, dict[str, set[str]]] = {
    "plak": _SHARED_PLAKOUS_SYMBOLS,
    "plakous-host-protocol": _SHARED_PLAKOUS_SYMBOLS,
    "plakous-hosted": {
        **_SHARED_PLAKOUS_SYMBOLS,
        # Hidden, unsupported preview-minimization seam; not a product API.
        "tools": _SHARED_PLAKOUS_SYMBOLS["tools"] | {"preview_redaction"},
    },
    "plakous-workspace": _SHARED_PLAKOUS_SYMBOLS,
    "plakous-recursive-subagents": _SHARED_PLAKOUS_SYMBOLS,
    "plakous-recursive-subagents-store": _SHARED_PLAKOUS_SYMBOLS,
}
ALLOWED_PLAKOUS_SYMBOLS_BY_SOURCE: dict[SourceKey, dict[str, set[str]]] = {
    ("plakous-hosted", "src/agent/build.rs"): {
        "sessions": {"internal"},
        # A Direct extension's registry owner names its MCP server.
        "tools": {"ToolOwnerClass"},
    },
    ("plakous-hosted", "src/api_runtime.rs"): {"tools": {"RegisteredToolIdentity"}},
    # MCP binding: the facade's MCP client, a host environment for the pool's
    # service processes, and the named-secret value type.
    ("plakous-hosted", "src/mcp_binding.rs"): {
        "mcp": {
            "HeaderName", "HeaderValue", "McpError", "McpHttpEndpoint", "McpHttpSecret",
            "McpHttpSecretResolver", "McpPool", "McpSecretResolver", "McpServerSpec",
            "McpToolPack", "McpToolSelection", "SensitiveBytes", "ServiceSecretValue",
        },
        "model": {"RunId"},
        "tools": {
            "EnvironmentVariableName", "ExecutionEnvironment", "ExecutionEnvironmentConfig",
            "ExecutionEnvironmentScope",
        },
    },
    # The untrusted-data paragraph an MCP tool earns.
    ("plakous-hosted", "src/tools.rs"): {
        "prompts": {"UNTRUSTED_TOOL_DATA_AGREEMENTS", "UNTRUSTED_TOOL_DATA_CONTRIBUTION_ID"},
    },
    # Binding reads each route's release-owned hosted retrieval table.
    ("plakous-hosted", "src/tool_plan.rs"): {
        "model": {
            "HostedRetrievalSupport", "HostedRetrievalTable", "HostedRetrievalTool",
            "HostedRetrievalUnsupported",
        },
        "openai": {"CODEX_HOSTED_RETRIEVAL", "OPENAI_API_HOSTED_RETRIEVAL"},
        "anthropic": {"ANTHROPIC_API_HOSTED_RETRIEVAL", "CLAUDE_OAUTH_HOSTED_RETRIEVAL"},
        "xai": {"SUPERGROK_HOSTED_RETRIEVAL", "XAI_API_HOSTED_RETRIEVAL"},
        "openrouter": {"OPENROUTER_HOSTED_RETRIEVAL"},
        # The deferred port the session's executor connects to, and the
        # bounds a planned fetch_url runs under.
        "tools": {
            "ExecutionOperationControl", "FetchUrlLimits", "RetrievalError",
            "RetrievalErrorKind", "RetrievalFuture", "RetrievalKind", "RetrievalRequest",
        },
    },
    # The Anthropic, OpenAI and xAI API executors connect their side-request ports.
    ("plakous-hosted", "src/platform.rs"): {
        "anthropic": {"AnthropicRetrievalPort"},
        "openai": {"OpenAiRetrievalPort", "lowest_responses_effort"},
        "xai": {"XaiRetrievalPort"},
    },
    # The OpenRouter executor connects its side-request port.
    ("plakous-hosted", "src/openrouter.rs"): {
        "openrouter": {"OpenRouterRetrieval", "OpenRouterRetrievalPort"},
    },
    # The frozen [tools.fetch_url] table read into the tool's bounds.
    ("plakous-hosted", "src/frozen_tables.rs"): {"tools": {"FetchUrlLimits"}},
    # The compile-time check that the host release timeout covers the store deadline.
    ("plakous-hosted", "src/lib.rs"): {"agent": {"DEFAULT_STORE_OPERATION_TIMEOUT"}},
    ("plak", "src/commands/reload_rehearsal.rs"): {
        "sessions": {"rehearsal", "inspect_sqlite", "OfflineReopenOutcome"},
    },
}
PROVIDER_NAMES = {"anthropic", "gemini", "openai", "openrouter", "xai"}
# Plak's own account spellings. They are not transport identities the way
# `openai` is, but a neutral package that names one carries a closed roster of
# the product's accounts inside it. Check every neutral package except the
# explicit source exemptions below.
PRODUCT_ACCOUNT_NAMES = {"claude", "codex", "grok", "supergrok"}
# Core account names are allowed only at the explicitly listed source sites.
PRODUCT_ACCOUNT_NAME_WAIVERS: set[str] = set()
# Where a neutral package still names one of Plak's accounts. Each of these is
# *identity*, not behaviour: a vocabulary a model is shown, a durable lowering
# recorded in a persisted projection, or an enrolled-candidate table. None of
# them can move above the core, because the core is what compares them.
PRODUCT_ACCOUNT_NAME_EXEMPTIONS: dict[SourceKey, set[str]] = {
    # `CodingPresentation::{Codex, Grok, Claude}` is the model vocabulary --
    # which tool dialect a model is shown. It is irreducibly named after the
    # vendor whose dialect it reproduces.
    ("plakous-tools", "src/coding_presentation.rs"): {"codex", "grok", "claude"},
    # `ToolProjectionLoweringV2::{CodexExecCommand, GrokRunTerminalCommand,
    # ClaudeBash, ...}` is durable: a persisted projection descriptor records
    # the lowering identity, so the variant names are wire format. Lowering and
    # raising match those identities to produce each vocabulary's shapes.
    ("plakous-tools", "src/exposure/projection.rs"): {"codex", "grok", "claude"},
    ("plakous-tools", "src/exposure/lowering.rs"): {"claude"},
    ("plakous-tools", "src/exposure/raising.rs"): {"claude"},
    ("plakous-tools", "src/exposure/raising/claude.rs"): {"claude"},
    # The Codex patch envelope's canonical name and the Claude `Bash`
    # observation bound -- both compared on reopen.
    ("plakous-tools", "src/capabilities.rs"): {"codex", "grok", "claude"},
    # The Code Mode driver-marker identity strings.
    ("plakous-tools", "src/composition_markers.rs"): {"codex"},
}

NEUTRAL_PROVIDER_NAME_EXEMPTIONS: dict[SourceKey, set[str]] = {}
# Neutral core packages that must not link the tracing/otel stacks.
TRACING_FREE_PACKAGES = ("plakous-agent", "plakous-model")
PRODUCTION_TARGET_KINDS = {
    "bin",
    "cdylib",
    "custom-build",
    "dylib",
    "lib",
    "proc-macro",
    "rlib",
    "staticlib",
}
RAW_STRING_START = re.compile(r'(?:br|r)(?P<hashes>#{0,255})"')
RUST_CHARACTER = re.compile(
    r"(?:b)?'(?:\\(?:.|u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2})|[^'\\\n])'"
)
RUST_IDENTIFIER = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*")


class RustToken(NamedTuple):
    """One significant Rust token; comments and test-only modules are absent."""

    kind: str
    value: str


@lru_cache(maxsize=256)
def _blank_rust_non_code(source: str) -> str:
    """Blank comments and literals while preserving offsets and newlines."""
    masked = list(source)

    def blank(start: int, end: int) -> None:
        for index in range(start, end):
            if masked[index] not in "\r\n":
                masked[index] = " "

    index = 0
    while index < len(source):
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            end = len(source) if end == -1 else end
            blank(index, end)
            index = end
            continue
        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(source) and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank(index, end)
            index = end
            continue

        raw = RAW_STRING_START.match(source, index)
        if raw is not None:
            terminator = '"' + raw.group("hashes")
            end = source.find(terminator, raw.end())
            end = len(source) if end == -1 else end + len(terminator)
            blank(index, end)
            index = end
            continue

        prefix = 2 if source.startswith(('b"', 'c"'), index) else 1
        if source[index : index + prefix].endswith('"'):
            end = index + prefix
            escaped = False
            while end < len(source):
                character = source[end]
                end += 1
                if character == '"' and not escaped:
                    break
                if character == "\\" and not escaped:
                    escaped = True
                else:
                    escaped = False
            blank(index, end)
            index = end
            continue

        character = RUST_CHARACTER.match(source, index)
        if character is not None:
            end = character.end()
            blank(index, end)
            index = end
            continue
        index += 1
    return "".join(masked)


def production_rust_source(source: str) -> str:
    """Remove cfg(test) modules without truncating later production items."""
    masked = _blank_rust_non_code(source)
    spans: list[tuple[int, int]] = []
    cursor = 0
    while match := CFG_TEST_BLOCK_ITEM.search(masked, cursor):
        opening = match.end() - 1
        depth = 0
        closing = len(masked)
        for index in range(opening, len(masked)):
            if masked[index] == "{":
                depth += 1
            elif masked[index] == "}":
                depth -= 1
                if depth == 0:
                    closing = index + 1
                    break
        spans.append((match.start(), closing))
        cursor = closing
    for match in CFG_TEST_SEMICOLON_ITEM.finditer(masked):
        if not any(start <= match.start() < end for start, end in spans):
            spans.append((match.start(), match.end()))

    production = list(source)
    for start, end in spans:
        for index in range(start, end):
            if production[index] not in "\r\n":
                production[index] = " "
    return "".join(production)


def _decode_rust_string(value: str) -> str:
    """Decode the escape forms relevant to policy-bearing Rust strings."""
    decoded: list[str] = []
    index = 0
    escapes = {"0": "\0", "n": "\n", "r": "\r", "t": "\t", "\\": "\\", '"': '"'}
    while index < len(value):
        if value[index] != "\\":
            decoded.append(value[index])
            index += 1
            continue
        index += 1
        if index >= len(value):
            decoded.append("\\")
            break
        escaped = value[index]
        index += 1
        if escaped in escapes:
            decoded.append(escapes[escaped])
        elif escaped == "x" and re.fullmatch(r"[0-9A-Fa-f]{2}", value[index : index + 2]):
            decoded.append(chr(int(value[index : index + 2], 16)))
            index += 2
        elif escaped == "u" and index < len(value) and value[index] == "{":
            closing = value.find("}", index + 1)
            digits = value[index + 1 : closing].replace("_", "") if closing != -1 else ""
            if closing != -1 and re.fullmatch(r"[0-9A-Fa-f]{1,6}", digits):
                decoded.append(chr(int(digits, 16)))
                index = closing + 1
            else:
                decoded.extend(("\\u", value[index:]))
                break
        elif escaped in "\r\n":
            if escaped == "\r" and index < len(value) and value[index] == "\n":
                index += 1
            while index < len(value) and value[index] in " \t\r\n":
                index += 1
        else:
            decoded.extend(("\\", escaped))
    return "".join(decoded)


def rust_tokens(source: str) -> list[RustToken]:
    """Lex policy-significant Rust without treating comments/literals as code."""
    # Several policy passes inspect the same source; callers still get their
    # own mutable list. Caches are bounded, process-local and exact-content keyed.
    return list(_cached_rust_tokens(source))


@lru_cache(maxsize=256)
def _cached_rust_tokens(source: str) -> tuple[RustToken, ...]:
    source = production_rust_source(source)
    tokens: list[RustToken] = []
    index = 0
    while index < len(source):
        if source[index].isspace():
            index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            index = len(source) if end == -1 else end
            continue
        if source.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            continue

        raw = RAW_STRING_START.match(source, index)
        if raw is not None:
            terminator = '"' + raw.group("hashes")
            content_start = raw.end()
            closing = source.find(terminator, content_start)
            if closing == -1:
                tokens.append(RustToken("string", source[content_start:]))
                break
            tokens.append(RustToken("string", source[content_start:closing]))
            index = closing + len(terminator)
            continue

        prefix = 2 if source.startswith(('b"', 'c"'), index) else 1
        if source[index : index + prefix].endswith('"'):
            content_start = index + prefix
            end = content_start
            escaped = False
            while end < len(source):
                character = source[end]
                if character == '"' and not escaped:
                    break
                if character == "\\" and not escaped:
                    escaped = True
                else:
                    escaped = False
                end += 1
            tokens.append(
                RustToken("string", _decode_rust_string(source[content_start:end]))
            )
            index = min(end + 1, len(source))
            continue

        character = RUST_CHARACTER.match(source, index)
        if character is not None:
            index = character.end()
            continue
        identifier = RUST_IDENTIFIER.match(source, index)
        if identifier is not None:
            value = identifier.group(0).removeprefix("r#")
            tokens.append(RustToken("identifier", value))
            index = identifier.end()
            continue
        if source.startswith("::", index):
            tokens.append(RustToken("punct", "::"))
            index += 2
            continue
        tokens.append(RustToken("punct", source[index]))
        index += 1
    return tuple(tokens)


def _concat_values(tokens: list[RustToken]) -> set[str]:
    """Evaluate literal-only concat! invocations, including nested concat! calls."""

    def parse(index: int) -> tuple[str, int] | None:
        if (
            index + 2 >= len(tokens)
            or tokens[index] != RustToken("identifier", "concat")
            or tokens[index + 1].value != "!"
            or tokens[index + 2].value not in {"(", "[", "{"}
        ):
            return None
        closing = {"(": ")", "[": "]", "{": "}"}[tokens[index + 2].value]
        cursor = index + 3
        parts: list[str] = []
        expect_value = True
        while cursor < len(tokens):
            if tokens[cursor].value == closing:
                return ("".join(parts), cursor + 1) if not expect_value or not parts else None
            if not expect_value:
                if tokens[cursor].value != ",":
                    return None
                cursor += 1
                expect_value = True
                continue
            if tokens[cursor].kind == "string":
                parts.append(tokens[cursor].value)
                cursor += 1
            else:
                nested = parse(cursor)
                if nested is None:
                    return None
                value, cursor = nested
                parts.append(value)
            expect_value = False
        return None

    values: set[str] = set()
    for index, token in enumerate(tokens):
        if token == RustToken("identifier", "concat"):
            parsed = parse(index)
            if parsed is not None:
                values.add(parsed[0])
    return values


def _significant_strings(tokens: list[RustToken]) -> set[str]:
    values = {token.value for token in tokens if token.kind == "string"} | _concat_values(tokens)
    for start, token in enumerate(tokens):
        if token.value != "[":
            continue
        cursor = start + 1
        parts: list[str] = []
        expect_value = True
        while cursor < len(tokens) and tokens[cursor].value != "]":
            if expect_value and tokens[cursor].kind == "string":
                parts.append(tokens[cursor].value)
                expect_value = False
            elif not expect_value and tokens[cursor].value == ",":
                expect_value = True
            else:
                parts = []
                break
            cursor += 1
        if (
            parts
            and cursor + 4 < len(tokens)
            and tokens[cursor].value == "]"
            and tokens[cursor + 1].value == "."
            and tokens[cursor + 2] == RustToken("identifier", "concat")
            and tokens[cursor + 3].value == "("
            and tokens[cursor + 4].value == ")"
        ):
            values.add("".join(parts))
    return values


def _use_statement_spans(tokens: list[RustToken]) -> list[tuple[int, int]]:
    """Half-open `[start, end)` token spans of every `use` statement's tree.

    `start` is the token after `use`; `end` is the terminating `;`.
    """
    spans: list[tuple[int, int]] = []
    index = 0
    while index < len(tokens):
        if tokens[index] != RustToken("identifier", "use"):
            index += 1
            continue
        end = index + 1
        depths = {"(": 0, "[": 0, "{": 0}
        while end < len(tokens):
            value = tokens[end].value
            if value in depths:
                depths[value] += 1
            elif value in {')', ']', '}'}:
                opening = {')': '(', ']': '[', '}': '{'}[value]
                depths[opening] = max(0, depths[opening] - 1)
            elif value == ";" and not any(depths.values()):
                break
            end += 1
        spans.append((index + 1, end))
        index = end + 1
    return spans


def _use_statements(tokens: list[RustToken]) -> list[list[RustToken]]:
    return [tokens[start:end] for start, end in _use_statement_spans(tokens)]


def _split_top_level(tokens: list[RustToken]) -> list[list[RustToken]]:
    """Split a brace group's body on its own top-level commas."""
    groups: list[list[RustToken]] = []
    current: list[RustToken] = []
    depth = 0
    for token in tokens:
        if token.value == "{":
            depth += 1
        elif token.value == "}":
            depth = max(0, depth - 1)
        if token.value == "," and depth == 0:
            if current:
                groups.append(current)
            current = []
            continue
        current.append(token)
    if current:
        groups.append(current)
    return groups


def _use_tree_paths(statement: list[RustToken]) -> list[list[str]]:
    """Expand one `use` tree into the full path of every leaf it imports.

    `use crate::{tui::X, shell::Y}` is two paths, not one flattened path: the
    flattened form let a second brace member name anything the first member's
    prefix allowed. An `as` alias ends the path; a glob leaf retains `*`.
    """

    def expand(tokens: list[RustToken], prefix: list[str]) -> list[list[str]]:
        segments = list(prefix)
        for index, token in enumerate(tokens):
            if token.value == "{":
                depth = 1
                end = index + 1
                while end < len(tokens) and depth:
                    if tokens[end].value == "{":
                        depth += 1
                    elif tokens[end].value == "}":
                        depth -= 1
                    end += 1
                paths: list[list[str]] = []
                for group in _split_top_level(tokens[index + 1 : end - 1]):
                    paths.extend(expand(group, segments))
                return paths
            if token.value == "*":
                segments.append("*")
                break
            if token.kind == "identifier":
                if token.value == "as":
                    break
                segments.append(token.value)
        return [segments] if segments else []

    return expand(statement, [])


def _path_root_identifiers(tokens: list[RustToken], spans: list[tuple[int, int]]) -> set[str]:
    """Every identifier used as the root of a `::` path outside a `use` tree.

    This is what catches a bare `plakous_hosted::surface_protocol::x()` in a body,
    which no `use` statement declares.
    """
    roots: set[str] = set()
    for index, token in enumerate(tokens):
        if token.kind != "identifier":
            continue
        if any(start <= index < end for start, end in spans):
            continue
        if index + 1 >= len(tokens) or tokens[index + 1].value != "::":
            continue
        if index > 0 and tokens[index - 1].value == "::":
            continue
        roots.add(token.value)
    return roots


def _identifier_values(tokens: list[RustToken]) -> list[str]:
    return [token.value for token in tokens if token.kind == "identifier"]


def _contains_identifier_pair(tokens: list[RustToken], first: str, second: str) -> bool:
    identifiers = _identifier_values(tokens)
    return any(
        identifiers[index : index + 2] == [first, second]
        for index in range(len(identifiers) - 1)
    )


def _call_names(tokens: list[RustToken]) -> list[str]:
    return [
        token.value
        for index, token in enumerate(tokens[:-1])
        if token.kind == "identifier" and tokens[index + 1].value == "("
    ]


def _foreign_function_names(tokens: list[RustToken]) -> set[str]:
    """Return function and link names declared by explicit foreign blocks."""
    names: set[str] = set()
    for index, token in enumerate(tokens):
        if token != RustToken("identifier", "extern"):
            continue
        cursor = index + 1
        if cursor < len(tokens) and tokens[cursor].kind == "string":
            cursor += 1
        if cursor >= len(tokens) or tokens[cursor].value != "{":
            continue
        depth = 1
        cursor += 1
        while cursor < len(tokens) and depth:
            if tokens[cursor].value == "{":
                depth += 1
            elif tokens[cursor].value == "}":
                depth -= 1
            elif (
                depth == 1
                and tokens[cursor] == RustToken("identifier", "fn")
                and cursor + 1 < len(tokens)
                and tokens[cursor + 1].kind == "identifier"
            ):
                names.add(tokens[cursor + 1].value)
            elif (
                depth == 1
                and tokens[cursor] == RustToken("identifier", "link_name")
                and cursor + 2 < len(tokens)
                and tokens[cursor + 1].value == "="
                and tokens[cursor + 2].kind == "string"
            ):
                names.add(tokens[cursor + 2].value)
            cursor += 1
    return names


def _unapproved_plakous_symbols(
    tokens: list[RustToken], allowed_symbols: dict[str, set[str]]
) -> set[str]:
    """Find facade symbols outside the package's approved `plakous::` surface."""
    unapproved: set[str] = set()
    for statement in _use_statements(tokens):
        for names in _use_tree_paths(statement):
            if not names or names[0] != "plakous":
                continue
            if len(names) < 2 or names[1] not in allowed_symbols:
                unapproved.add("::".join(names[:2]))
                continue
            module = names[1]
            unapproved.update(
                f"{module}::{name}"
                for name in names[2:]
                if name not in allowed_symbols[module]
            )

    for index in range(len(tokens) - 2):
        if not (
            tokens[index] == RustToken("identifier", "plakous")
            and tokens[index + 1].value == "::"
            and tokens[index + 2].kind == "identifier"
        ):
            continue
        module = tokens[index + 2].value
        if module == "VERSION":
            continue
        if module not in allowed_symbols:
            unapproved.add(module)
            continue
        if (
            index + 4 < len(tokens)
            and tokens[index + 3].value == "::"
            and tokens[index + 4].kind == "identifier"
            and tokens[index + 4].value not in allowed_symbols[module]
        ):
            unapproved.add(f"{module}::{tokens[index + 4].value}")
    return unapproved


def _environment_calls(tokens: list[RustToken]) -> list[tuple[str, list[RustToken]]]:
    """Extract direct std::env module calls and their top-level argument tokens."""
    calls: list[tuple[str, list[RustToken]]] = []
    for index in range(2, len(tokens) - 1):
        name = tokens[index]
        if (
            name.kind != "identifier"
            or name.value not in {"var", "var_os", "vars", "vars_os"}
            or tokens[index - 1].value != "::"
            or tokens[index - 2] != RustToken("identifier", "env")
            or tokens[index + 1].value != "("
        ):
            continue
        depth = 1
        cursor = index + 2
        while cursor < len(tokens) and depth:
            if tokens[cursor].value == "(":
                depth += 1
            elif tokens[cursor].value == ")":
                depth -= 1
            cursor += 1
        calls.append((name.value, tokens[index + 2 : cursor - 1]))
    return calls


def _environment_path_names(tokens: list[RustToken]) -> set[str]:
    return {
        tokens[index].value
        for index in range(2, len(tokens))
        if tokens[index].kind == "identifier"
        and tokens[index].value in {"var", "var_os", "vars", "vars_os"}
        and tokens[index - 1].value == "::"
        and tokens[index - 2] == RustToken("identifier", "env")
    }


def _uninvoiced_environment_readers(tokens: list[RustToken]) -> set[str]:
    """Find std::env reader paths used as function items instead of exact calls."""
    return {
        tokens[index].value
        for index in range(2, len(tokens))
        if tokens[index].kind == "identifier"
        and tokens[index].value in {"var", "var_os", "vars", "vars_os"}
        and tokens[index - 1].value == "::"
        and tokens[index - 2] == RustToken("identifier", "env")
        and (index + 1 >= len(tokens) or tokens[index + 1].value != "(")
    }


def _process_imports(tokens: list[RustToken]) -> set[str]:
    """Return names imported directly from `std::process`."""
    imported: set[str] = set()
    for statement in _use_statements(tokens):
        names = _identifier_values(statement)
        if names[:2] == ["std", "process"]:
            imported.update(names[2:])
            if any(token.value in {"*", "as"} for token in statement):
                imported.add("*")
    return imported


def _call_arguments(tokens: list[RustToken], opening: int) -> tuple[RustToken, ...]:
    """Return tokens within one parenthesized call."""
    depth = 1
    cursor = opening + 1
    while cursor < len(tokens) and depth:
        if tokens[cursor].value == "(":
            depth += 1
        elif tokens[cursor].value == ")":
            depth -= 1
        cursor += 1
    return tuple(tokens[opening + 1 : cursor - 1])


def _process_command_calls(
    tokens: list[RustToken],
) -> list[tuple[str, tuple[tuple[str, str], ...]]]:
    """Return exact constructors backed by `std::process::Command`."""
    calls: list[tuple[str, tuple[tuple[str, str], ...]]] = []
    imports_command = "Command" in _process_imports(tokens)
    for index in range(len(tokens) - 3):
        if not (
            tokens[index] == RustToken("identifier", "Command")
            and tokens[index + 1].value == "::"
            and tokens[index + 2] == RustToken("identifier", "new")
            and tokens[index + 3].value == "("
        ):
            continue
        qualified = (
            index >= 4
            and tokens[index - 4] == RustToken("identifier", "std")
            and tokens[index - 3].value == "::"
            and tokens[index - 2] == RustToken("identifier", "process")
            and tokens[index - 1].value == "::"
        )
        if not qualified and not imports_command:
            continue
        owner = "std::process::Command" if qualified else "Command"
        arguments = tuple(
            (token.kind, token.value)
            for token in _call_arguments(tokens, index + 3)
        )
        calls.append((owner, arguments))
    return calls


def validate_product_session_authority_sources(
    package: str, sources: dict[str, str]
) -> list[str]:
    """Forbid known Plak-owned canonical context, compaction, and fork seams."""
    errors: list[str] = []
    for path, source in sources.items():
        path = f"{package}:{path}"
        identifiers = set(_identifier_values(rust_tokens(source)))
        forbidden = sorted(identifiers & FORBIDDEN_PRODUCT_SESSION_AUTHORITY_IDENTIFIERS)
        if forbidden:
            errors.append(
                f"{package}:{path}: Plak contains competing session authority identifiers: {forbidden!r}"
            )
    return errors


# Where the product home may be discovered from the process environment.
#
# `PlakPaths::from_env()` reads `$PLAK_HOME`/XDG: it is a composition-root act,
# and a library that performs it has decided for its caller which home the
# session belongs to. Only `bin/plak`, the composition root, may discover
# the product home; library callers must supply it explicitly.
PRODUCT_HOME_DISCOVERY: str = "from_env"
PRODUCT_HOME_DISCOVERY_ALLOWED_PREFIXES: tuple[tuple[str, str], ...] = (("plak", ""),)


def validate_product_home_discovery(package: str, sources: dict[str, str]) -> list[str]:
    """No library may discover the product home for its caller."""
    errors: list[str] = []
    for path, source in sources.items():
        if "PlakPaths::from_env" not in source:
            continue
        allowed = any(
            package == name and (prefix == "" or path.startswith(prefix))
            for name, prefix in PRODUCT_HOME_DISCOVERY_ALLOWED_PREFIXES
        )
        if not allowed:
            errors.append(
                f"{package}:{path}: PlakPaths::from_env is composition-root only; "
                "the product home is a fact a caller states, not one a library discovers"
            )
    return errors


def validate_cli_security_sources(package: str, sources: dict[str, str]) -> list[str]:
    """Enforce one host/bin package's exact process, environment, and credential-free surface.

    `sources` is keyed by package-relative path; every pin table is keyed by
    `(package, path)`.
    """
    errors: list[str] = []
    allowed_symbols = ALLOWED_PLAKOUS_SYMBOLS_BY_PACKAGE.get(package, {})
    tokens_by_file = {path: rust_tokens(source) for path, source in sources.items()}
    all_tokens = [token for tokens in tokens_by_file.values() for token in tokens]
    identifiers = set(_identifier_values(all_tokens))
    strings = set().union(*(_significant_strings(tokens) for tokens in tokens_by_file.values()))

    for symbol in sorted(FORBIDDEN_CLI_SYMBOLS & identifiers):
        errors.append(f"plak CLI contains forbidden delegated-login symbol: {symbol}")
    generated_names = {"include", "include_bytes", "include_str", "macro_rules"}
    for path, tokens in tokens_by_file.items():
        expected_pattern = EXPECTED_PRODUCT_GENERATED_MACROS.get((package, path), ())
        pattern_names = set(expected_pattern)
        actual_pattern = sorted(
            token.value
            for index, token in enumerate(tokens[:-1])
            if token.kind == "identifier"
            and token.value in pattern_names
            and tokens[index + 1].value == "!"
        )
        if actual_pattern != sorted(expected_pattern):
            errors.append(
                f"{package}:{path}: bundled source macro pattern {actual_pattern!r}, "
                f"expected {sorted(expected_pattern)!r}"
            )
        actual_generated = sorted(
            token.value
            for index, token in enumerate(tokens[:-1])
            if token.kind == "identifier"
            and token.value in generated_names
            and tokens[index + 1].value == "!"
        )
        expected_generated = sorted(
            macro
            for macro in EXPECTED_PRODUCT_GENERATED_MACROS.get((package, path), ())
            if macro in generated_names
        )
        if actual_generated != expected_generated:
            errors.append(
                f"{package}:{path}: source-generation macros {actual_generated!r}, "
                f"expected {expected_generated!r}"
            )
    for path, tokens in tokens_by_file.items():
        process_imports = _process_imports(tokens)
        expected_imports = EXPECTED_PRODUCT_PROCESS_IMPORTS.get((package, path), set())
        if process_imports != expected_imports:
            errors.append(
                f"{package}:{path}: std::process imports {sorted(process_imports)!r}, "
                f"expected {sorted(expected_imports)!r}"
            )
        for statement in _use_statements(tokens):
            names = _identifier_values(statement)
            if "env" in names:
                if names != ["std", "env"] or any(
                    token.value in {"*", "as"} for token in statement
                ):
                    errors.append("plak CLI must import std::env as a module, not ambient readers")
            if "Command" in names and "plakous" in names:
                errors.append("plak CLI imports a forbidden Command process capability")

        process_calls = _process_command_calls(tokens)
        expected_calls = EXPECTED_PRODUCT_PROCESS_COMMANDS.get((package, path), [])
        if process_calls != expected_calls:
            errors.append(
                f"{package}:{path}: process command constructors {process_calls!r}, "
                f"expected {expected_calls!r}"
            )

    for value in sorted(FORBIDDEN_CLI_STRINGS & strings):
        errors.append(f"plak CLI contains forbidden delegated-login option: {value}")
    for value in strings:
        lowered = value.lower().replace("\\", "/")
        if re.search(r"(?:^|/)\.(?:codex)(?:/|$)|(?:^|/)opencode(?:/|$)", lowered):
            errors.append("plak CLI contains forbidden delegated-login storage/owner locator")
            break
    if any(
        re.search(r"(?:^|/)proc/+self/+environ(?:$|/)", value.lower())
        for value in strings
    ):
        errors.append("plak CLI contains forbidden /proc/self/environ access")

    for path, tokens in tokens_by_file.items():
        source_symbols = ALLOWED_PLAKOUS_SYMBOLS_BY_SOURCE.get((package, path), {})
        file_symbols = {
            module: allowed_symbols.get(module, set()) | source_symbols.get(module, set())
            for module in allowed_symbols.keys() | source_symbols.keys()
        }
        unapproved_plakous = _unapproved_plakous_symbols(tokens, file_symbols)
        if unapproved_plakous:
            errors.append(
                f"{package}:{path}: plak CLI uses unapproved plakous facade symbols: "
                f"{sorted(unapproved_plakous)!r}"
            )
        foreign = _foreign_function_names(tokens) & FORBIDDEN_CLI_FOREIGN_FUNCTIONS
        if foreign:
            errors.append(
                f"{package}:{path}: plak CLI declares forbidden foreign environment/process functions: "
                f"{sorted(foreign)!r}"
            )
        environment_calls = _environment_calls(tokens)
        environment_paths = _environment_path_names(tokens)
        function_items = _uninvoiced_environment_readers(tokens)
        if function_items:
            errors.append(
                f"{package}:{path}: plak CLI contains environment reader function items: "
                f"{sorted(function_items)!r}"
            )
        if environment_paths & {"vars", "vars_os"}:
            errors.append(f"{package}:{path}: plak CLI contains ambient environment iteration")
        expected_generated = EXPECTED_PRODUCT_GENERATED_MACROS.get((package, path), ())
        if "env" not in expected_generated and any(
            token == RustToken("identifier", macro)
            and index + 1 < len(tokens)
            and tokens[index + 1].value == "!"
            for index, token in enumerate(tokens)
            for macro in ("env", "option_env")
        ):
            errors.append(f"{package}:{path}: plak CLI contains compile-time environment access")
        actual = [
            (name, tuple((token.kind, token.value) for token in arguments))
            for name, arguments in environment_calls
            if name in {"var", "var_os"}
        ]
        expected = EXPECTED_PRODUCT_ENVIRONMENT_CALLS.get((package, path), [])
        if actual != expected:
            errors.append(
                f"{package}:{path}: environment reader calls {actual!r}, expected {expected!r}"
            )

        allowed_macros = ALLOWED_CLI_MACROS | ALLOWED_CLI_MACROS_BY_SOURCE.get(
            (package, path), set()
        )
        macro_names = {
            token.value
            for index, token in enumerate(tokens[:-2])
            if token.kind == "identifier"
            and token.value not in _NOT_MACRO_KEYWORDS
            and tokens[index + 1].value == "!"
            and tokens[index + 2].value in {"(", "[", "{"}
        }
        for macro in sorted(macro_names - allowed_macros):
            errors.append(f"{package}:{path}: unapproved macro invocation cannot cross CLI boundary: {macro}!")

    return errors


def discover_rust_files(
    repository: Path,
    targets: list[tuple[Path, bool]],
) -> tuple[list[Path], list[str]]:
    """Discover Rust production sources without following or hiding symlinks."""
    repository = Path(os.path.abspath(repository))
    files: set[Path] = set()
    module_discovery_sources: set[Path] = set()
    errors: list[str] = []
    directories: set[Path] = set()

    def relative(path: Path) -> str | None:
        try:
            return path.relative_to(repository).as_posix()
        except ValueError:
            return None

    def has_symlink(path: Path) -> bool:
        try:
            rel = path.relative_to(repository)
        except ValueError:
            return False
        cursor = repository
        for part in rel.parts:
            cursor /= part
            if cursor.is_symlink():
                return True
        return False

    for target_value, exact_only in targets:
        target_path = target_value if target_value.is_absolute() else repository / target_value
        target = Path(os.path.abspath(target_path))
        label = relative(target) or str(target)
        if relative(target) is None:
            errors.append(f"production Rust target escapes repository: {label}")
            continue
        if has_symlink(target):
            errors.append(f"production Rust target traverses a symbolic link: {label}")
            continue
        if not target.is_file():
            errors.append(f"production Rust target is not a regular file: {label}")
            continue
        if exact_only:
            files.add(target)
            module_discovery_sources.add(target)
        else:
            files.add(target)
            module_discovery_sources.add(target)
            directories.add(target.parent)

    for directory in sorted(directories, key=lambda item: item.as_posix()):
        for current, dirnames, filenames in os.walk(directory, followlinks=False):
            current_path = Path(current)
            retained: list[str] = []
            for name in sorted(dirnames):
                child = current_path / name
                if child.is_symlink():
                    errors.append(
                        "production Rust source directory is a symbolic link: "
                        + (relative(child) or str(child))
                    )
                else:
                    retained.append(name)
            dirnames[:] = retained
            for name in sorted(filenames):
                if not name.endswith(".rs"):
                    continue
                path = current_path / name
                label = relative(path) or str(path)
                if path.is_symlink():
                    errors.append(
                        f"production Rust source is a symbolic link: {label}"
                    )
                elif path.is_file():
                    files.add(path)
                else:
                    errors.append(
                        f"production Rust source is not a regular file: {label}"
                    )

    pending = list(files)
    inspected: set[Path] = set()

    def resolve_explicit_target(source: Path, path_value: str, kind: str) -> None:
        if not path_value or "\0" in path_value or "\\" in path_value:
            errors.append(
                f"production Rust source has invalid {kind} target: {relative(source)}"
            )
            return
        target = Path(os.path.abspath(source.parent / path_value))
        label = relative(target) or str(target)
        if relative(target) is None:
            errors.append(f"production Rust {kind} target escapes repository: {label}")
        elif has_symlink(target):
            errors.append(
                f"production Rust {kind} target traverses a symbolic link: {label}"
            )
        elif not target.is_file():
            errors.append(f"production Rust {kind} target is not a regular file: {label}")
        else:
            newly_discoverable = target not in module_discovery_sources
            files.add(target)
            module_discovery_sources.add(target)
            if newly_discoverable:
                inspected.discard(target)
            if target not in inspected or newly_discoverable:
                pending.append(target)

    while pending:
        source = pending.pop()
        if source in inspected:
            continue
        inspected.add(source)
        try:
            source_text = source.read_text(encoding="utf-8")
        except OSError as error:
            errors.append(f"cannot inspect production Rust source {relative(source)}: {error}")
            continue
        has_explicit_source_target = (
            re.search(r"#\s*\[\s*path\s*=", source_text) is not None
            or "cfg_attr" in source_text
            or re.search(r"\binclude\s*!", source_text) is not None
        )
        if source not in module_discovery_sources and not has_explicit_source_target:
            continue
        tokens = rust_tokens(source_text)
        for index in range(len(tokens) - 2) if source in module_discovery_sources else ():
            if not (
                tokens[index] == RustToken("identifier", "mod")
                and tokens[index + 1].kind == "identifier"
                and tokens[index + 2].value == ";"
            ):
                continue
            module = tokens[index + 1].value
            candidates = [source.parent / f"{module}.rs", source.parent / module / "mod.rs"]
            if source.name not in {"lib.rs", "main.rs", "mod.rs", "build.rs"}:
                candidates.extend(
                    (
                        source.parent / source.stem / f"{module}.rs",
                        source.parent / source.stem / module / "mod.rs",
                    )
                )
            for candidate in candidates:
                target = Path(os.path.abspath(candidate))
                if not target.exists() and not target.is_symlink():
                    continue
                label = relative(target) or str(target)
                if relative(target) is None:
                    errors.append(
                        f"production Rust module target escapes repository: {label}"
                    )
                elif has_symlink(target):
                    errors.append(
                        f"production Rust module target traverses a symbolic link: {label}"
                    )
                elif not target.is_file():
                    errors.append(
                        f"production Rust module target is not a regular file: {label}"
                    )
                else:
                    newly_discoverable = target not in module_discovery_sources
                    files.add(target)
                    module_discovery_sources.add(target)
                    if newly_discoverable:
                        inspected.discard(target)
                    if target not in inspected or newly_discoverable:
                        pending.append(target)
                break
        for index in range(len(tokens) - 4):
            if not (
                tokens[index].value == "#"
                and tokens[index + 1].value == "["
                and tokens[index + 2] == RustToken("identifier", "path")
                and tokens[index + 3].value == "="
                and tokens[index + 4].kind == "string"
            ):
                continue
            resolve_explicit_target(source, tokens[index + 4].value, "#[path]")

        for index in range(len(tokens) - 2):
            if not (
                tokens[index] == RustToken("identifier", "cfg_attr")
                and tokens[index + 1].value == "("
            ):
                continue
            cursor = index + 2
            depth = 1
            while cursor < len(tokens) and depth:
                if tokens[cursor].value == "(":
                    depth += 1
                elif tokens[cursor].value == ")":
                    depth -= 1
                elif (
                    tokens[cursor] == RustToken("identifier", "path")
                    and cursor + 1 < len(tokens)
                    and tokens[cursor + 1].value == "="
                ):
                    if (
                        cursor + 2 >= len(tokens)
                        or tokens[cursor + 2].kind != "string"
                    ):
                        errors.append(
                            "production Rust cfg_attr(path) target must be a direct "
                            f"string literal: {relative(source)}"
                        )
                    else:
                        resolve_explicit_target(
                            source,
                            tokens[cursor + 2].value,
                            "cfg_attr(path)",
                        )
                cursor += 1

        for index in range(len(tokens) - 3):
            if not (
                tokens[index] == RustToken("identifier", "include")
                and tokens[index + 1].value == "!"
                and tokens[index + 2].value in {"(", "[", "{"}
            ):
                continue
            closing = {"(": ")", "[": "]", "{": "}"}[tokens[index + 2].value]
            cursor = index + 3
            if cursor >= len(tokens) or tokens[cursor].kind != "string":
                errors.append(
                    "production Rust include! target must be a direct string literal: "
                    f"{relative(source)}"
                )
                continue
            path_value = tokens[cursor].value
            cursor += 1
            if cursor < len(tokens) and tokens[cursor].value == ",":
                cursor += 1
            if cursor >= len(tokens) or tokens[cursor].value != closing:
                errors.append(
                    "production Rust include! target must contain exactly one string "
                    f"literal: {relative(source)}"
                )
                continue
            resolve_explicit_target(source, path_value, "include!")
    return sorted(files, key=lambda item: item.as_posix()), sorted(set(errors))


def cfg_test_module_files(sources: Iterable[Path]) -> set[Path]:
    """Resolve the files behind out-of-line `#[cfg(test)] mod name;` items.

    Such a file is a whole-file test body, so production rules (macros, process
    commands and imports, environment reads, provider names in core, ...) must
    not scan it. Only the declaration form is trusted: an inline
    `#[cfg(test)] mod name { ... }` is already blanked by
    `production_rust_source`, and a `#[path]`-redirected module keeps its own
    explicit handling.
    """
    excluded: set[Path] = set()
    for source in sources:
        try:
            text = source.read_text(encoding="utf-8")
        except OSError:
            continue
        masked = _blank_rust_non_code(text)
        for match in CFG_TEST_MODULE_DECLARATION.finditer(masked):
            module = match.group("name")
            bases = [source.parent]
            if source.name not in {"lib.rs", "main.rs", "mod.rs", "build.rs"}:
                bases.append(source.parent / source.stem)
            for base in bases:
                excluded.add(Path(os.path.abspath(base / f"{module}.rs")))
                directory = Path(os.path.abspath(base / module))
                root_module = directory / "mod.rs"
                if not root_module.is_file():
                    continue
                # The directory form carries its own submodules; every one of
                # them is reachable only through the cfg(test) root.
                excluded.add(root_module)
                for current, _dirnames, filenames in os.walk(
                    directory, followlinks=False
                ):
                    for name in filenames:
                        if name.endswith(".rs"):
                            excluded.add(Path(os.path.abspath(Path(current) / name)))
    return excluded


def production_rust_sources(
    package: dict[str, Any], repository: Path = ROOT
) -> tuple[list[Path], list[str]]:
    """Resolve every Cargo production/custom-build target and adjacent module."""
    targets: list[tuple[Path, bool]] = []
    for target in package.get("targets", []):
        kinds = set(target.get("kind", []))
        if not kinds & PRODUCTION_TARGET_KINDS:
            continue
        source = Path(target["src_path"])
        targets.append((source, kinds == {"custom-build"}))
    sources, errors = discover_rust_files(repository, targets)
    test_only = cfg_test_module_files(sources)
    sources = [
        source
        for source in sources
        if source not in test_only
        and "product_crash_tests" not in source.parts
        and source.name
        not in {
            "product_terminal_e2e_tests.rs",
            "product_compaction_terminal_e2e_tests.rs",
        }
    ]
    return sources, errors


def provider_names_in_rust(source: str, names: Iterable[str] = ()) -> set[str]:
    """Find provider identities in production code, excluding comments and tests.

    `names` defaults to `PROVIDER_NAMES`; a caller passes the wider set that
    also carries the product account spellings.
    """
    checked = set(names) or set(PROVIDER_NAMES)
    tokens = rust_tokens(source)
    found = {value.lower() for value in _identifier_values(tokens) if value.lower() in checked}
    for value in _significant_strings(tokens):
        found.update(
            provider
            for provider in checked
            if re.search(rf"(?<![a-z0-9]){re.escape(provider)}(?![a-z0-9])", value.lower())
        )
    return found


def dependency_names(package: dict[str, Any]) -> set[str]:
    """Return every direct Cargo dependency regardless of dependency kind."""
    return {dependency["name"] for dependency in package.get("dependencies", [])}


def production_dependency_names(package: dict[str, Any]) -> set[str]:
    """Return direct normal dependencies, excluding test-only and build edges."""
    return {
        dependency["name"]
        for dependency in package.get("dependencies", [])
        if dependency.get("kind") is None
    }


def validate_supported_compile_fixture(
    fixture_name: str,
    manifest: dict[str, Any],
    files: set[str],
    expected_contract: dict[str, Any],
    inventory_errors: tuple[str, ...] = (),
) -> list[str]:
    """Require one exact, isolated external supported-surface fixture."""
    errors = [f"{fixture_name}: {error}" for error in inventory_errors]
    expected_manifest = expected_contract["manifest"]
    if manifest != expected_manifest:
        errors.append(
            f"{fixture_name}: normalized manifest differs from the supported fixture contract"
        )
    expected_files = expected_contract["files"]
    if files != expected_files:
        errors.append(
            f"{fixture_name}: non-target files {sorted(files)!r}, "
            f"expected {sorted(expected_files)!r}"
        )
    return errors


def non_target_file_inventory(root: Path) -> tuple[set[str], tuple[str, ...]]:
    """Return regular fixture files and fail-closed filesystem-type errors."""
    files: set[str] = set()
    errors: list[str] = []
    root_mode = root.lstat().st_mode
    if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
        errors.append("fixture root must be a regular directory, not a symlink")
        return files, tuple(errors)

    for directory, directories, names in os.walk(root):
        directory_path = Path(directory)
        traversable_directories: list[str] = []
        for name in directories:
            path = directory_path / name
            relative = path.relative_to(root).as_posix()
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                errors.append(f"symlink directory is forbidden: {relative}")
            elif not stat.S_ISDIR(mode):
                errors.append(f"non-directory traversal entry is forbidden: {relative}")
            elif name != "target":
                traversable_directories.append(name)
        # Inspect symlinks named `target` before pruning ordinary Cargo output.
        directories[:] = traversable_directories

        for name in names:
            path = directory_path / name
            relative = path.relative_to(root).as_posix()
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                errors.append(f"symlink file is forbidden: {relative}")
            elif not stat.S_ISREG(mode):
                errors.append(f"non-regular file is forbidden: {relative}")
            else:
                files.add(relative)
    return files, tuple(errors)


def validate_local_production_lints(
    workspace_manifests: dict[str, dict[str, Any]],
) -> list[str]:
    """Require the repository's unsafe prohibition for every local production crate."""
    errors = []
    for name, manifest in workspace_manifests.items():
        if name == "plakous-js-eval":
            unsafe_code = manifest.get("lints", {}).get("rust", {}).get("unsafe_code")
            if unsafe_code != "forbid":
                errors.append(
                    'plakous-js-eval: unsafe_code must be explicitly "forbid"'
                )
            continue
        if manifest.get("lints") != {"workspace": True}:
            errors.append(f"{name}: workspace lint inheritance is required")
    return errors


def validate_single_dependency(
    label: str,
    actual: set[str],
    expected: str,
) -> list[str]:
    """Require an application-style consumer to depend on only one facade."""
    if actual == {expected}:
        return []
    return [f"{label}: direct dependencies {sorted(actual)!r}, expected [{expected!r}]"]


# ---------------------------------------------------------------------------
# Policy-derived validators. Each is pure over a loaded Workspace so the unit
# tests can feed mutated workspaces without touching the tree.
# ---------------------------------------------------------------------------


def validate_package_metadata(workspace: Workspace) -> list[str]:
    """Require a known layer and a class the layer admits on every package."""
    errors: list[str] = []
    policy = workspace.policy
    for package in sorted(workspace.packages.values(), key=lambda item: item.name):
        if package.layer not in policy.layers:
            errors.append(f"{package.name}: unknown layer {package.layer!r}")
            continue
        if package.package_class not in PACKAGE_CLASSES:
            errors.append(f"{package.name}: unknown class {package.package_class!r}")
        elif package.package_class not in policy.classes.get(package.layer, ()):
            errors.append(
                f"{package.name}: class {package.package_class!r} is not legal for layer "
                f"{package.layer!r} (allowed {sorted(policy.classes.get(package.layer, ()))!r})"
            )
    return errors


def package_classifications(workspace: Workspace) -> dict[str, set[str]]:
    """Return class -> package names, derived from the manifests."""
    classes: dict[str, set[str]] = {name: set() for name in PACKAGE_CLASSES}
    for package in workspace.packages.values():
        classes.setdefault(package.package_class, set()).add(package.name)
    return classes


def validate_layout(workspace: Workspace) -> list[str]:
    """Require every package directory to satisfy its layer's layout templates."""
    errors: list[str] = []
    for package in sorted(workspace.packages.values(), key=lambda item: item.name):
        if package.layer not in workspace.policy.layout:
            continue
        if not layout_matches(workspace.policy, package):
            expected = [
                expand_layout_template(template, package)
                for template in workspace.policy.layout[package.layer]
            ]
            errors.append(
                f"{package.name}: manifest directory {package.directory!r} does not match the "
                f"{package.layer!r} layout {expected!r}"
            )
    return errors


def internal_production_edges(workspace: Workspace) -> dict[str, set[str]]:
    """Return package -> direct normal internal dependencies (optional included)."""
    return {
        name: package.production_dependency_names() & set(workspace.packages)
        for name, package in workspace.packages.items()
    }


def validate_layer_edges(workspace: Workspace) -> list[str]:
    """Admit only edges the layer matrix or an explicit exception allows."""
    errors: list[str] = []
    policy = workspace.policy
    edges = internal_production_edges(workspace)
    for source_name in sorted(edges):
        source = workspace.packages[source_name]
        for target_name in sorted(edges[source_name]):
            target = workspace.packages[target_name]
            if not edge_permitted(policy, source, target):
                errors.append(
                    f"{source_name} ({source.layer}) -> {target_name} ({target.layer}): edge is "
                    f"outside the layer matrix and has no edge-exception"
                )
    for source_name, targets in sorted(policy.edge_exceptions.items()):
        if source_name not in workspace.packages:
            errors.append(f"edge-exception names unknown package {source_name!r}")
            continue
        for target_name in targets:
            if target_name not in workspace.packages:
                errors.append(
                    f"edge-exception {source_name} -> {target_name} names an unknown package"
                )
            elif target_name not in edges[source_name]:
                errors.append(
                    f"edge-exception {source_name} -> {target_name} is stale: no such edge"
                )
    return errors


def validate_harness_pack_independence(workspace: Workspace) -> list[str]:
    """Keep the named harness packages free of every other harness package.

    `harness -> harness` exists so a composition crate may compose packs.
    Instruction text may not: a prompt describes the agent, and a tool's own
    text travels with its pack (spec 02 6.3).
    """
    errors: list[str] = []
    edges = internal_production_edges(workspace)
    for source_name in workspace.policy.harness_pack_independent:
        source = workspace.packages.get(source_name)
        if source is None:
            errors.append(f"harness-pack-independent names unknown package {source_name!r}")
            continue
        if source.layer != "harness":
            errors.append(
                f"harness-pack-independent names {source_name}, which is layer {source.layer!r}"
            )
            continue
        for target_name in sorted(edges.get(source_name, set())):
            if workspace.packages[target_name].layer == "harness":
                errors.append(
                    f"{source_name} -> {target_name}: instruction text may not depend on a "
                    "harness pack or composition crate"
                )
    return errors


def validate_production_dependencies(workspace: Workspace) -> list[str]:
    """Pin the exact direct production dependency set of every host/bin package."""
    errors: list[str] = []
    pinned = workspace.policy.production_dependencies
    for package in workspace.in_layers(*ABOVE_FACADE_LAYERS):
        expected = pinned.get(package.name)
        if expected is None:
            errors.append(
                f"{package.name}: layer {package.layer!r} packages need a "
                "[workspace.metadata.plakous.production-dependencies] row"
            )
            continue
        actual = package.production_dependency_names()
        if actual != set(expected):
            errors.append(
                f"{package.name}: direct dependencies {sorted(actual)!r}, expected "
                f"{sorted(expected)!r}"
            )
    for name in sorted(pinned):
        package = workspace.packages.get(name)
        if package is None:
            errors.append(f"production-dependencies row names unknown package {name!r}")
        elif package.layer not in ABOVE_FACADE_LAYERS:
            errors.append(
                f"production-dependencies row for {name!r} (layer {package.layer!r}) is not "
                "a host/bin package"
            )
    return errors


def validate_facade_features(workspace: Workspace) -> list[str]:
    """Require each host/bin facade edge to enable exactly the pinned features."""
    errors: list[str] = []
    pinned = workspace.policy.facade_features
    for package in workspace.in_layers(*ABOVE_FACADE_LAYERS):
        facade_edges = [
            dependency
            for dependency in package.metadata.get("dependencies", [])
            if dependency["name"] == "plakous" and dependency.get("kind") is None
        ]
        expected = pinned.get(package.name)
        if not facade_edges:
            if expected is not None:
                errors.append(f"{package.name}: facade-features row but no plakous dependency")
            continue
        if expected is None:
            errors.append(
                f"{package.name}: depends on plakous without a "
                "[workspace.metadata.plakous.facade-features] row"
            )
            continue
        if len(facade_edges) != 1:
            errors.append(f"{package.name} must have one exact plakous dependency edge")
            continue
        edge = facade_edges[0]
        if edge.get("uses_default_features") is not False or set(edge.get("features") or []) != set(
            expected
        ):
            errors.append(
                f"{package.name} must enable exactly plakous/{{{', '.join(sorted(expected))}}} "
                "with dependency defaults disabled"
            )
    for name in sorted(pinned):
        if name not in workspace.packages:
            errors.append(f"facade-features row names unknown package {name!r}")
    return errors


def validate_neutral_dependencies(workspace: Workspace) -> list[str]:
    """Forbid provider-identifying dependencies on every `neutral = true` package."""
    errors: list[str] = []
    for package in sorted(workspace.packages.values(), key=lambda item: item.name):
        if not package.neutral:
            continue
        for name in sorted(package.dependency_names()):
            normalized = name.lower()
            if any(part in normalized for part in FORBIDDEN_NEUTRAL_DEPENDENCY_PARTS):
                errors.append(f"{package.name}: provider dependency forbidden: {normalized}")
    return errors


def validate_neutral_sources(package_name: str, sources: dict[str, str]) -> list[str]:
    """Forbid provider and product-account names in a neutral package's sources."""
    errors: list[str] = []
    checked = set(PROVIDER_NAMES)
    if package_name not in PRODUCT_ACCOUNT_NAME_WAIVERS:
        checked |= PRODUCT_ACCOUNT_NAMES
    for path, contents in sorted(sources.items()):
        allowed = NEUTRAL_PROVIDER_NAME_EXEMPTIONS.get((package_name, path), set())
        allowed = allowed | PRODUCT_ACCOUNT_NAME_EXEMPTIONS.get((package_name, path), set())
        for provider in sorted(provider_names_in_rust(contents, checked) - allowed):
            errors.append(f"{package_name}:{path}: provider name {provider!r} in neutral package")
    return errors


def _component_crate_paths(tokens: list[RustToken]) -> set[str]:
    """Return every `plakous_<x>` crate named as a path root, `use` root, or extern crate."""
    crates: set[str] = set()

    def is_component(token: RustToken) -> bool:
        return token.kind == "identifier" and token.value.startswith("plakous_")

    for index, token in enumerate(tokens):
        if not is_component(token):
            continue
        preceded_by_path = index > 0 and tokens[index - 1].value == "::"
        followed_by_path = index + 1 < len(tokens) and tokens[index + 1].value == "::"
        extern_crate = (
            index >= 2
            and tokens[index - 2] == RustToken("identifier", "extern")
            and tokens[index - 1] == RustToken("identifier", "crate")
        )
        if (followed_by_path and not preceded_by_path) or extern_crate:
            crates.add(token.value.replace("_", "-"))
    for statement in _use_statements(tokens):
        for index, token in enumerate(statement):
            if not is_component(token):
                continue
            # A leading segment of the use tree: statement root or a `{`-group
            # member directly under the root (`use {plakous_x::y, ...}`).
            if index == 0 or statement[index - 1].value in {"{", ","} and (
                index < 2 or statement[index - 2].value != "::"
            ):
                crates.add(token.value.replace("_", "-"))
    return crates


def validate_facade_only_sources(
    package_name: str, sources: dict[str, str], permitted: Iterable[str] = ()
) -> list[str]:
    """Host/bin production sources never spell `plakous_<x>::`; the facade is the surface."""
    errors: list[str] = []
    permitted_crates = set(permitted)
    for path, contents in sorted(sources.items()):
        crates = _component_crate_paths(rust_tokens(contents)) - permitted_crates
        if crates:
            errors.append(
                f"{package_name}:{path}: names Plakous component crates {sorted(crates)!r}; "
                "host/bin sources reach L0 only through `plakous`"
            )
    return errors


def _crate_pattern_matches(pattern: str, crate: str) -> bool:
    """Match a `forbid-crates` glob against a crate name, `_`/`-` normalized."""
    return fnmatch.fnmatchcase(crate.replace("_", "-"), pattern.replace("_", "-"))


def validate_module_rules(
    package: Package, sources: dict[str, str], rules: dict[str, dict[str, Any]]
) -> list[str]:
    """Apply `[workspace.metadata.plakous.modules.<package>]` per file.

    A rule for module `m` covers every production source under `src/m/`. Such a
    file may `use` std/core/alloc, its own external dependencies, the paths
    listed in `may-use`, and nothing matching `forbid-crates` unless it is
    listed in `allow-crates`; any other `crate::` path is an error. Inactive
    until the module directory exists.
    """
    errors: list[str] = []
    for module, rule in sorted(rules.items()):
        prefix = f"src/{module}/"
        allowed_crates = rule.get("allow-crates", [])

        def forbidden(crate: str) -> bool:
            if any(_crate_pattern_matches(pattern, crate) for pattern in allowed_crates):
                return False
            return any(_crate_pattern_matches(pattern, crate) for pattern in rule["forbid-crates"])

        for path, contents in sorted(sources.items()):
            if not path.startswith(prefix):
                continue
            tokens = rust_tokens(contents)
            spans = _use_statement_spans(tokens)
            for statement in _use_statements(tokens):
                refused_roots: set[str] = set()
                for names in _use_tree_paths(statement):
                    if not names:
                        continue
                    root = names[0]
                    if root in {"std", "core", "alloc", "super", "self"}:
                        continue
                    if root == "crate":
                        full = "::".join(names)
                        if not any(
                            full == allowed or full.startswith(allowed + "::")
                            for allowed in rule["may-use"]
                        ):
                            errors.append(
                                f"{package.name}:{path}: module {module!r} may not use {full!r}"
                            )
                        continue
                    if forbidden(root) and root not in refused_roots:
                        refused_roots.add(root)
                        errors.append(
                            f"{package.name}:{path}: module {module!r} may not use crate {root!r}"
                        )
            for root in sorted(_path_root_identifiers(tokens, spans)):
                crate = root.replace("_", "-")
                if forbidden(crate):
                    errors.append(
                        f"{package.name}:{path}: module {module!r} names forbidden crate {crate!r}"
                    )
    return errors


def package_sources(package: Package, repository: Path = ROOT) -> tuple[dict[str, str], list[str]]:
    """Return the package's production sources keyed by package-relative path."""
    sources, errors = production_rust_sources(package.metadata, repository)
    base = (repository / package.directory).resolve()
    by_path = {
        Path(os.path.relpath(source.resolve(), base)).as_posix(): source.read_text(
            encoding="utf-8"
        )
        for source in sources
    }
    return by_path, errors


def cargo_tree_output(root: Path, spec: list[str], errors: list[str]) -> str | None:
    """Fail closed with Cargo's diagnostic rather than treating failure as absence."""
    command = ["cargo", "tree", "--offline", *spec]
    result = subprocess.run(
        command, cwd=root, text=True, capture_output=True, check=False,
    )
    if result.returncode != 0:
        errors.append(
            f"{shlex.join(command)} failed (exit {result.returncode}): "
            f"{result.stderr.strip() or 'no stderr'}"
        )
        return None
    return result.stdout


def rquickjs_reaches_plak_jointly(root: Path, errors: list[str]) -> bool | None:
    """Ask Cargo whether `plak` links rquickjs when built beside the sidecar."""
    output = cargo_tree_output(
        root,
        [
            "-e",
            "features",
            "-i",
            "rquickjs",
            "-p",
            "plak",
            "-p",
            "plakous-js-runtime",
            "-p",
            "plakous-py-worker",
        ],
        errors,
    )
    if output is None:
        return None
    return re.search(r"(?m)^[\s│├└─]*plak v\d", output) is not None


def rquickjs_linkage_errors(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    for spec, must_link, message in (
        (["-p", "plakous", "--all-features"], False,
         "plakous must not link rquickjs under any feature; Code Mode uses the sidecar"),
        (["-p", "plak"], False,
         "plak must not link rquickjs; Code Mode uses plakous-js-host"),
        (["-p", "plakous-js-runtime"], True, "plakous-js-runtime must link rquickjs"),
    ):
        # An inverse query errors when rquickjs is absent from a selected graph.
        output = cargo_tree_output(root, [*spec, "--prefix", "none", "--format", "{p}"], errors)
        if output is not None and bool(re.search(r"(?m)^rquickjs v", output)) != must_link:
            errors.append(message)
    if rquickjs_reaches_plak_jointly(root, errors):
        errors.append("building both workers alongside plak must not link rquickjs into plak")
    return errors


# Symbols that appear only when the QuickJS engine itself is linked. The Rust
# markers are v0-mangled crate names (`<length><crate>`) and carry both
# supported platforms; the C markers are the engine's own entry points, and
# Mach-O prefixes those with an underscore while ELF does not, so both
# spellings are listed. Plakous' own `RquickjsCodeModeIntegration` type and
# `rquickjs_sidecar_v1` constructor carry "rquickjs" in their names and are the
# sidecar *client*, so a substring match on the crate name proves nothing.
RQUICKJS_ENGINE_SYMBOL_MARKERS = (
    "13rquickjs_core",
    "12rquickjs_sys",
    "_JS_NewRuntime",
    "_JS_NewContext",
    "_JS_FreeRuntime",
    "JS_NewRuntime",
    "JS_NewContext",
    "JS_FreeRuntime",
)
# The three executables Plak's Cargo-native build produces.
PRODUCT_EXECUTABLES = ("plak", "plakous-js-host", "monty")

# Length-prefixed crate identifiers in legacy and v0 Rust symbols. Adapter
# names (json_to_monty, monty_proto's client) are not interpreter code.
MONTY_RUNTIME_SYMBOLS = re.compile(
    r"(?<![0-9])(?:5monty|11monty_alloc|19monty_type_checking|11monty_proto6worker)(?=[0-9A-Z_]|$)"
)


def symbol_tool() -> list[str] | None:
    """Return the symbol lister to use, preferring the LLVM one when present."""
    for candidate in (["llvm-nm"], ["nm"]):
        if shutil.which(candidate[0]) is not None:
            return candidate
    return None


def built_product_artifacts(root: Path) -> dict[str, Path]:
    """Return the release product executables, which is the product build.

    Only `target/release` counts: `target/debug` holds gate builds of a
    different feature selection (`--all-features`, including `test-hooks`).
    The product parent is `cargo build --release --locked -p plak`, whose facade
    features are the explicit set `bin/plak` declares.
    """
    return {
        name: candidate
        for name in PRODUCT_EXECUTABLES
        if (candidate := root / "target" / "release" / name).is_file()
    }


def engine_symbol_markers(symbol_output: str) -> set[str]:
    """Return the QuickJS engine markers present in one symbol listing."""
    return {
        marker for marker in RQUICKJS_ENGINE_SYMBOL_MARKERS if marker in symbol_output
    }


def artifact_linkage_findings(
    symbols: dict[str, str | None], digests: dict[str, str], tool_name: str = "nm"
) -> list[str]:
    """Apply the linkage rules to listed symbols and digests.

    `symbols` maps each product executable to its symbol listing, or to `None`
    when the listing failed. This is the whole decision, kept free of the
    filesystem so it can be exercised on fabricated listings.
    """
    errors: list[str] = []
    for name in sorted(symbols):
        output = symbols[name]
        if output is None:
            errors.append(f"{name}: could not list symbols with {tool_name}")
            continue
        found = engine_symbol_markers(output)
        monty_runtime = MONTY_RUNTIME_SYMBOLS.search(output) is not None
        if name != "monty" and monty_runtime:
            errors.append(f"{name} links the Monty runtime; interpreter must remain in the worker")
        if name == "monty" and not monty_runtime:
            errors.append("monty does not link the Monty runtime; cannot verify isolation")
        if name == "plak" and found:
            errors.append(
                f"plak links the QuickJS engine ({sorted(found)!r}); the product "
                "evaluates JavaScript only in the plakous-js-host worker"
            )
        if name == "plakous-js-host" and not found:
            errors.append(
                "plakous-js-host does not link the QuickJS engine; the worker is "
                "the only place the product evaluates JavaScript"
            )
        if name == "monty" and found:
            errors.append(f"monty links the QuickJS engine ({sorted(found)!r})")
    if len(set(digests.values())) != len(digests):
        errors.append(
            f"the product executables are not separate artifacts: {digests!r}"
        )
    return errors


def newest_tracked_source_mtime(root: Path) -> float | None:
    """Return the newest mtime among tracked Rust sources and Cargo metadata.

    `Cargo.lock` is matched by the same listing, so a dependency change dates
    the tree just as a source edit does. Returns `None` when `git` cannot be
    asked, in which case staleness is simply not decided.
    """
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs", "*Cargo.toml", "Cargo.lock"],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    newest: float | None = None
    for relative in result.stdout.split("\0"):
        if not relative:
            continue
        try:
            stamp = (root / relative).stat().st_mtime
        except OSError:
            continue
        if newest is None or stamp > newest:
            newest = stamp
    return newest


def stale_release_artifacts(root: Path, artifacts: dict[str, Path]) -> list[str]:
    """Return the release executables older than the source they must witness."""
    newest = newest_tracked_source_mtime(root)
    if newest is None:
        return []
    return sorted(
        name for name, path in artifacts.items() if path.stat().st_mtime < newest
    )


def artifact_linkage_errors(root: Path = ROOT) -> tuple[list[str], str]:
    """Check the built artifacts, not the manifests, for engine linkage.

    Cargo feature names say what *should* link; this reads what *did*. The
    product binary must contain no QuickJS engine symbol, the JavaScript worker
    must contain them, and all three executables must be separate files. The
    note names the three digests so a build receipt can be matched against the
    exact artifacts this run read.
    """
    artifacts = built_product_artifacts(root)
    missing = [name for name in PRODUCT_EXECUTABLES if name not in artifacts]
    if missing:
        return (
            [],
            f"artifact linkage not checked: release {', '.join(missing)} is not built",
        )
    stale = stale_release_artifacts(root, artifacts)
    if stale:
        return (
            [],
            "artifact linkage not checked: stale release build; "
            f"{', '.join(stale)} is older than Cargo.lock or the newest tracked source",
        )
    return verify_artifact_linkage(artifacts)


def verify_artifact_linkage(artifacts: dict[str, Path]) -> tuple[list[str], str]:
    """Mandatory linkage proof for a freshly built product on either platform."""
    missing = [name for name in PRODUCT_EXECUTABLES if name not in artifacts or not artifacts[name].is_file()]
    if missing:
        return [f"missing product artifacts: {', '.join(missing)}"], "artifact linkage refused"
    tool = symbol_tool()
    if tool is None:
        return ["artifact linkage requires nm or llvm-nm on PATH"], "artifact linkage refused"

    def symbol_listing(path: Path) -> str | None:
        result = subprocess.run(
            [*tool, str(path)], text=True, capture_output=True, check=False
        )
        return None if result.returncode != 0 else result.stdout

    digests = {
        name: hashlib.sha256(path.read_bytes()).hexdigest()
        for name, path in sorted(artifacts.items())
    }
    symbols = {name: symbol_listing(path) for name, path in sorted(artifacts.items())}
    errors = artifact_linkage_findings(symbols, digests, tool[0])
    receipt = ", ".join(f"{name} sha256 {digest}" for name, digest in digests.items())
    return errors, f"artifact linkage checked: {receipt}"


def supported_target_atomic_polyfill_errors(root: Path = ROOT) -> list[str]:
    """Keep the unsupported-target maintenance exception out of built graphs."""
    errors = []
    for target in ("x86_64-unknown-linux-musl", "aarch64-apple-darwin"):
        output = cargo_tree_output(
            root,
            [
                "--locked", "--workspace", "--all-features",
                "--target", target, "--edges", "normal,build", "--prefix", "none",
                "--format", "{p}",
            ],
            errors,
        )
        if output is not None and re.search(r"(?m)^atomic-polyfill v", output):
            errors.append(
                f"{target}: atomic-polyfill is reachable; its unsupported-target "
                "maintenance exception no longer applies"
            )
    return errors


def workspace_manifest_tables(workspace: Workspace) -> dict[str, dict[str, Any]]:
    return {name: package.manifest for name, package in workspace.packages.items()}


def validate_commit_trace_edge(manifests: dict[str, dict[str, Any]]) -> list[str]:
    """The host -> store exception exists only for the repository crash tracer."""
    tree = manifests.get("plakous-recursive-subagents-store", {})
    dependency = tree.get("dependencies", {}).get("plakous-store", {})
    if (
        dependency.get("optional") is not True
        or dependency.get("features", []) != []
        or tree.get("features", {}).get("commit-trace") != [
            "dep:plakous-store", "plakous-store/commit-trace"
        ]
    ):
        return ["tree store's plakous-store edge must be optional and commit-trace-only"]
    return []


COMMIT_TRACE_SOURCE_PREFIXES = {
    "plakous-store": "src/",
    "plak": "src/store/",
    "plakous-recursive-subagents-store": "src/",
}


def validate_commit_trace_sql(package: str, sources: dict[str, str]) -> list[str]:
    """Pin the traced stores to writes whose commit can reach the WAL callback."""
    prefix = COMMIT_TRACE_SOURCE_PREFIXES.get(package)
    if prefix is None:
        return []
    return [
        f"{package}:{path}: SQL RETURNING is forbidden by the Commit trace contract "
        "in spec 05 section 2.6: reset/finalize-completed writes lose their WAL callback"
        for path, source in sorted(sources.items())
        if path.startswith(prefix)
        and any(
            token.kind == "string" and re.search(r"\bRETURNING\b", token.value, re.IGNORECASE)
            for token in rust_tokens(source)
        )
    ]


# Tracked-file hygiene: nothing owner-specific ships. The home-path pattern is
# written so this file does not match itself.
HYGIENE_HOME_PATH = re.compile(rb"(?<![\w$}.~/-])/(?:Users|home)/([A-Za-z0-9._-]+)")
# Private host, project and maintainer tool names live only in an optional
# untracked file (docs/local is gitignored), one exact name per line; without
# it the private-name check is skipped and the home-path rule still applies.
HYGIENE_PRIVATE_NAMES_FILE = "docs/local/hygiene-private-names.txt"


def load_hygiene_private_names(root: Path = ROOT) -> frozenset[bytes]:
    try:
        text = (root / HYGIENE_PRIVATE_NAMES_FILE).read_bytes()
    except FileNotFoundError:
        return frozenset()
    return frozenset(line.strip() for line in text.splitlines() if line.strip())


# A maximal run of name characters; candidate names are its dot-delimited
# spans, so `name`, `name.local`, `x.name` and `name.sh` all surface `name` or
# `name.sh`, while `my-name` and `names` do not.
HYGIENE_NAME_RUN = re.compile(rb"[A-Za-z0-9_.-]+")
# Generic placeholder users in examples and tests, and the container user the
# eval pmux cell creates. Never a real account name.
HYGIENE_HOME_PLACEHOLDERS = frozenset(
    {"fixture", "me", "op", "pmux", "secret", "test", "tester", "u", "user", "x", "you"}
)


def hygiene_private_names(text: bytes, private_names: frozenset[bytes]) -> list[str]:
    """Private names in `text`, in order: dot-delimited spans of each name run."""
    names: list[str] = []
    for run in HYGIENE_NAME_RUN.findall(text):
        segments = run.split(b".")
        for start in range(len(segments)):
            for end in range(start + 1, len(segments) + 1):
                candidate = b".".join(segments[start:end])
                if candidate in private_names:
                    names.append(candidate.decode())
    return names


def hygiene_findings(
    relative: str, data: bytes, private_names: frozenset[bytes] = frozenset(),
) -> list[str]:
    """Owner-specific strings in one file's bytes; binary files have none."""
    if b"\0" in data:
        return []
    # Scan each distinct run once per file; only a hit needs line numbers.
    has_private_name = bool(private_names) and bool(
        hygiene_private_names(b" ".join(set(HYGIENE_NAME_RUN.findall(data))), private_names)
    )
    findings: list[str] = []
    for number, line in enumerate(data.split(b"\n"), start=1):
        for match in HYGIENE_HOME_PATH.finditer(line):
            if match.group(1).decode() not in HYGIENE_HOME_PLACEHOLDERS:
                findings.append(f"{relative}:{number}: absolute home path {match.group(0).decode()!r}")
        if has_private_name:
            for name in hygiene_private_names(line, private_names):
                findings.append(f"{relative}:{number}: private host or tool name {name!r}")
    return findings


def hygiene_candidate_files(root: Path) -> list[str]:
    """Files a commit would carry: tracked plus untracked, not ignored.

    Outside a git checkout (the musl container's source export holds exactly
    that set) every regular file except build output is scanned.
    """
    try:
        listing = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            cwd=root,
            capture_output=True,
            check=False,
        )
    except OSError:
        listing = None
    if listing is not None and listing.returncode == 0:
        return sorted({name.decode() for name in listing.stdout.split(b"\0") if name})
    files: list[str] = []
    for directory, directories, names in os.walk(root):
        directories[:] = [name for name in directories if name not in {".git", "target"}]
        for name in names:
            files.append((Path(directory) / name).relative_to(root).as_posix())
    return sorted(files)


def validate_tracked_hygiene(
    root: Path = ROOT, private_names: frozenset[bytes] | None = None,
) -> list[str]:
    if private_names is None:
        private_names = load_hygiene_private_names(root)
    errors: list[str] = []
    for relative in hygiene_candidate_files(root):
        path = root / relative
        if path.is_symlink() or not path.is_file():
            continue
        for finding in hygiene_findings(relative, path.read_bytes(), private_names):
            errors.append(f"{finding}: tracked files must not carry owner-specific content")
    return errors


def validate_workspace_excludes(
    root_workspace: dict[str, Any], supported_fixture_paths: set[str],
) -> list[str]:
    """Keep operator state outside Cargo without hiding unreviewed packages."""
    excludes = set(root_workspace.get("exclude", []))
    errors: list[str] = []
    if ".plak" not in excludes:
        errors.append("workspace must exclude .plak operator state and fork worktrees")
    reviewed_vendor_packages = {
        "vendor/codex-code-mode-protocol",
        "vendor/monty-typeshed",
    }
    unknown_excludes = excludes - supported_fixture_paths - reviewed_vendor_packages - {".plak"}
    if unknown_excludes:
        errors.append(
            "workspace excludes unsupported nested packages: "
            f"{sorted(unknown_excludes)!r}"
        )
    return errors


def main() -> int:
    """Run all workspace policy checks."""
    workspace = load_workspace(ROOT)
    packages = workspace.packages
    policy = workspace.policy
    errors: list[str] = []
    linkage_note = "artifact linkage not checked"

    root_workspace = workspace.root_manifest.get("workspace", {})
    if root_workspace.get("resolver") != "3":
        errors.append("workspace resolver must be 3")
    fixtures = supported_compile_fixtures(workspace)
    supported_fixture_paths = {f"tests/fixtures/{fixture_name}" for fixture_name in fixtures}
    errors.extend(validate_workspace_excludes(root_workspace, supported_fixture_paths))

    for retired in RETIRED_PACKAGES:
        if retired in packages:
            errors.append(f"{retired} must not exist")
        for name, package in packages.items():
            if retired in package.dependency_names():
                errors.append(f"{name} must not depend on {retired}")

    errors.extend(validate_tracked_hygiene(ROOT))
    errors.extend(validate_local_production_lints(workspace_manifest_tables(workspace)))
    errors.extend(validate_commit_trace_edge(workspace_manifest_tables(workspace)))
    for name in COMMIT_TRACE_SOURCE_PREFIXES:
        sources, discovery_errors = package_sources(packages[name], ROOT)
        errors.extend(discovery_errors)
        errors.extend(validate_commit_trace_sql(name, sources))

    # Layer policy: metadata shape, layout, edges, classes, pinned dependencies.
    errors.extend(validate_package_metadata(workspace))
    errors.extend(validate_layout(workspace))
    errors.extend(validate_layer_edges(workspace))
    errors.extend(validate_harness_pack_independence(workspace))
    errors.extend(validate_production_dependencies(workspace))
    errors.extend(validate_facade_features(workspace))
    errors.extend(validate_neutral_dependencies(workspace))

    for name, package in sorted(packages.items()):
        metadata = package.metadata
        # This is the pre-release workspace baseline, not the final publication
        # topology. Published-internal packages remain unpublished until the
        # consolidation and registry-cohort stages deliberately change it.
        if metadata.get("publish") != []:
            errors.append(f"{name}: pre-release packages must have publish = false")
        if metadata.get("edition") != "2024":
            errors.append(f"{name}: edition must be 2024")
        if metadata.get("rust_version") != "1.99.0":
            errors.append(f"{name}: rust-version must be 1.99.0")
        if metadata.get("license") != WORKSPACE_LICENSE:
            errors.append(f"{name}: license expression is inconsistent")

    # Every supported library package has exactly one external consumer fixture;
    # `plak` is pinned by cli_contract.rs instead.
    for package in sorted(packages.values(), key=lambda item: item.name):
        if package.package_class != "supported" or package.layer in ABOVE_FACADE_LAYERS:
            continue
        if fixture_name_for(package) not in fixtures:
            errors.append(f"{package.name}: supported package lacks a consumer fixture contract")
    for fixture_name, expected_contract in fixtures.items():
        fixture_root = ROOT / f"tests/fixtures/{fixture_name}"
        fixture_manifest_path = fixture_root / "Cargo.toml"
        with fixture_manifest_path.open("rb") as stream:
            fixture_manifest = tomllib.load(stream)
        fixture_files, fixture_inventory_errors = non_target_file_inventory(fixture_root)
        errors.extend(
            validate_supported_compile_fixture(
                fixture_name,
                fixture_manifest,
                fixture_files,
                expected_contract,
                fixture_inventory_errors,
            )
        )

    # Package shape pins (name-keyed facts, no paths).
    app = packages.get("plak")
    if app is not None:
        targets = app.metadata.get("targets", [])
        binaries = [target for target in targets if "bin" in target["kind"]]
        if len(binaries) != 1 or binaries[0]["name"] != "plak":
            errors.append("plak must expose exactly one binary named plak")
        libraries = [target for target in targets if "lib" in target["kind"]]
        if len(libraries) != 1 or libraries[0]["name"] != "plak":
            errors.append("plak must expose exactly one library named plak")
        app_features = app.metadata.get("features", {})
        if set(app_features) != {
            "default",
            "maintainer",
            "otel",
            "test-hooks",
            "test-support",
            "git-cli",
            "jj-lib",
        }:
            errors.append(
                "plak may expose only default, maintainer, otel, test-hooks, test-support "
                "and workspace backend features"
            )
        elif app_features.get("default") != ["git-cli", "jj-lib"]:
            errors.append("plak default must enable both workspace backends")
        elif app_features.get("maintainer") != []:
            errors.append(
                "plak maintainer feature must be empty: it gates plak's own cfg "
                "sites and plakous-hosted has no maintainer-gated code to forward to"
            )
        elif "dep:opentelemetry" not in app_features.get("otel", []):
            errors.append("plak otel feature must enable the opentelemetry dependency")
        elif sorted(app_features.get("test-hooks", [])) != [
            "dep:plakous-offline-crash-harness",
            "plakous-hosted/test-support",
            "plakous-recursive-subagents-store/commit-trace",
        ]:
            # The offline model selector must reach the binary through exactly
            # the harness and hosted injection edges. The third edge traces
            # the existing tree-store dependency without a harness -> tree ->
            # recursive-subagents -> harness dev-dependency cycle.
            errors.append(
                "plak test-hooks must enable exactly the offline crash harness, "
                "plakous-hosted/test-support and tree-store/commit-trace"
            )

    host = packages.get("plakous-hosted")
    if host is not None:
        targets = host.metadata.get("targets", [])
        if [target for target in targets if "bin" in target["kind"]]:
            errors.append("plakous-hosted must not expose binary targets")
        libraries = [target for target in targets if "lib" in target["kind"]]
        if len(libraries) != 1 or libraries[0]["name"] != "plakous_hosted":
            errors.append("plakous-hosted must expose exactly one library named plakous_hosted")
        host_features = host.metadata.get("features", {})
        if set(host_features) != {"default", "test-support", "git-cli", "jj-lib"}:
            errors.append("plakous-hosted may expose only default, test-support and workspace backend features")
        elif host_features.get("default") != ["git-cli", "jj-lib"]:
            errors.append("plakous-hosted default must enable both workspace backends")
        elif host_features.get("test-support") != [
            "dep:tempfile",
            "plakous-recursive-subagents/test-support",
            "plakous-workspace/test-support",
        ]:
            errors.append(
                "plakous-hosted test-support must enable only tempfile and the "
                "tree and workspace test-support features"
            )

    facade = packages.get("plakous")
    if facade is not None:
        expected_features = {
            "anthropic",
            "auth",
            "composition",
            "composition-python",
            "default",
            "full",
            "mcp",
            "native-subscriptions",
            "openai",
            "openrouter",
            "sessions-sqlite",
            "xai",
        }
        features = facade.metadata.get("features", {})
        actual_features = {name for name in features if not name.startswith("dep:")}
        if actual_features != expected_features:
            errors.append(
                f"plakous: public features {sorted(actual_features)!r}, "
                f"expected {sorted(expected_features)!r}"
            )
        if features.get("default") != []:
            errors.append("plakous: default feature set must remain empty")

    # Every host/bin package: security pins, session-authority pins, the
    # facade-only symbol rule, and any active module rules.
    for package in workspace.in_layers(*ABOVE_FACADE_LAYERS):
        sources, discovery_errors = package_sources(package, ROOT)
        errors.extend(discovery_errors)
        errors.extend(validate_product_session_authority_sources(package.name, sources))
        errors.extend(validate_product_home_discovery(package.name, sources))
        errors.extend(validate_cli_security_sources(package.name, sources))
        errors.extend(
            validate_facade_only_sources(
                package.name, sources, policy.edge_exceptions.get(package.name, ())
            )
        )
        errors.extend(validate_module_rules(package, sources, policy.modules.get(package.name, {})))
    for name in sorted(policy.modules):
        if name not in packages:
            errors.append(f"modules rule names unknown package {name!r}")

    # Neutral packages: no provider dependency (above) and no provider name.
    for package in sorted(packages.values(), key=lambda item: item.name):
        if not package.neutral:
            continue
        sources, discovery_errors = package_sources(package, ROOT)
        errors.extend(discovery_errors)
        errors.extend(validate_neutral_sources(package.name, sources))
    for name, path in NEUTRAL_PROVIDER_NAME_EXEMPTIONS:
        package = packages.get(name)
        if package is None or not package.neutral:
            errors.append(f"provider-name exemption {name}:{path} does not name a neutral package")
    for name in sorted(PRODUCT_ACCOUNT_NAME_WAIVERS):
        package = packages.get(name)
        if package is None or not package.neutral:
            errors.append(f"product-account waiver {name!r} does not name a neutral package")

    errors.extend(rquickjs_linkage_errors(ROOT))
    linkage_errors, linkage_note = artifact_linkage_errors(ROOT)
    errors.extend(linkage_errors)

    errors.extend(supported_target_atomic_polyfill_errors(ROOT))

    def package_tree_linked(package: str, needle: str) -> bool:
        output = cargo_tree_output(
            ROOT, ["-p", package, "-e", "normal"], errors,
        )
        return output is not None and needle in output

    for neutral in TRACING_FREE_PACKAGES:
        for forbidden in ("tracing", "opentelemetry"):
            if package_tree_linked(neutral, forbidden):
                errors.append(f"{neutral} must not link {forbidden}")
    if package_tree_linked("plak", "opentelemetry"):
        default_plak = cargo_tree_output(
            ROOT, ["-p", "plak", "--locked", "-e", "normal"], errors,
        )
        if default_plak is not None and "opentelemetry" in default_plak:
            errors.append("default plak build must not link opentelemetry")

    adjacency: dict[str, set[str]] = {
        name: package.dependency_names() & set(packages) for name, package in packages.items()
    }

    def visit(name: str, active: set[str], complete: set[str]) -> None:
        if name in active:
            errors.append(f"workspace dependency cycle reaches {name}")
            return
        if name in complete:
            return
        active.add(name)
        for dependency in adjacency.get(name, set()):
            visit(dependency, active, complete)
        active.remove(name)
        complete.add(name)

    complete: set[str] = set()
    for package_name in adjacency:
        visit(package_name, set(), complete)

    for package_name, retired_sources in RETIRED_SOURCES.items():
        package = packages.get(package_name)
        if package is None:
            continue
        for relative in retired_sources:
            if (ROOT / package.relative(relative)).exists():
                errors.append(
                    f"{package.relative(relative)}: OpenRouter peer Responses stack is forbidden; "
                    "use plakous-responses-dialect hosts"
                )

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    layers = {layer: len(workspace.in_layers(layer)) for layer in policy.layers}
    populated = ", ".join(f"{layer}={count}" for layer, count in layers.items() if count)
    print(
        f"validated {len(packages)} workspace packages across layers ({populated}), "
        f"dependency boundaries, and the plak/plakous-js-host build split; {linkage_note}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
