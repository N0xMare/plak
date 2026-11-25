#!/usr/bin/env python3
"""Validate the Plak application's Plakous workspace boundaries.

Layer facts are data, not tables in this file: every member manifest carries
`[package.metadata.plakous] layer/class[/neutral]` and the root `Cargo.toml`
carries `[workspace.metadata.plakous]` (layer matrix, edge exceptions, layout
templates, legal classes, host/bin production dependencies, facade features,
module rules). `tools/gate/plakous_policy.py` loads both from `cargo metadata`.
From that the checker derives the manifest inventory and paths, the internal
edge map, the package classification, the neutral-core set, the security scan
set (every host/bin package), and the facade-only symbol rule.

What stays hardcoded here, on purpose: the security pins (environment reads,
process commands and imports, macro allowlists, forbidden identifiers), the
approved `plakous::` symbols per host/bin package, the neutral-core provider
name exemptions, the retired-source pins, the consumer fixture inventories, the
rquickjs / tracing linkage pins, the tracked-file hygiene patterns, and the
portable-crate set with its native-only denylist.
`PORTABLE_CRATES` is the only list of crates that must stay portable; it grows
one crate at a time. Every path-bearing pin is keyed by package
name plus a package-relative path, so a package move never edits them.
"""

from __future__ import annotations

import fnmatch
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tomllib
from collections.abc import Callable, Iterable, Iterator
from contextlib import contextmanager, redirect_stderr, redirect_stdout
from functools import lru_cache
from pathlib import Path

from typing import Any, NamedTuple

sys.path.insert(0, str(Path(__file__).resolve().parent))

import cargo_key  # noqa: E402
import vcs_listing  # noqa: E402
from workspace_receipts import Receipts  # noqa: E402

from plakous_policy import (  # noqa: E402
    ABOVE_FACADE_LAYERS,
    PACKAGE_CLASSES,
    RAW_STRING_START,
    RUST_CHARACTER,
    Package,
    Workspace,
    blank_rust_non_code,
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
# The reviewed vendor snapshot keeps its upstream edition, MSRV and lints.
VENDOR_PACKAGE = "plakous-codex-code-mode-protocol"

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
            "src/inventory.rs",
            "src/lifecycle.rs",
            "src/lifecycle_configuration.rs",
            "src/local_composition.rs",
            "src/main.rs",
            "src/prompts.rs",
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
            "auth",
            "composition-python",
            "codex",
            "grok",
            "claude",
            "openai",
            "openrouter",
            "sessions-sqlite",
            "web-fetch",
            "xai",
        ),
        "extra-dependencies": {
            "futures-util": "0.3.31",
            "rusqlite": {
                "version": "=0.41.0",
                "default-features": False,
                "features": ["bundled"],
            },
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
                "rust-version": "1.100.0",
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
                "version": workspace.root_manifest["workspace"]["package"]["version"],
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

# Capture the whole predicate, including nested parentheses; classify it below.
_CFG_TEST_PREDICATE = r"cfg\s*\((?P<predicate>[^\]]*)\)"
# Attribute bodies may span lines (e.g. multi-line `#[allow(..., reason = "...")]`).
_CFG_ATTR = r"#\s*\[[^\]]*\]\s*"
CFG_TEST_BLOCK_ITEM = re.compile(
    rf"(?P<attributes>(?:{_CFG_ATTR})*#\s*\[\s*{_CFG_TEST_PREDICATE}\s*\]\s*"
    rf"(?:{_CFG_ATTR})*)"
    r"(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:(?:const|async|unsafe|extern(?:\s*\"[^\"]*\")?)\s+)*"
    r"(?:mod\s+(?:r#)?[A-Za-z_][A-Za-z0-9_]*|"
    r"fn\s+(?:r#)?[A-Za-z_][A-Za-z0-9_]*[^;{]*|"
    r"impl\b[^;{]*|trait\b[^;{]*|enum\b[^;{]*|struct\b[^;{]*)\s*\{",
    re.MULTILINE,
)
CFG_TEST_SEMICOLON_ITEM = re.compile(
    rf"(?P<attributes>(?:{_CFG_ATTR})*#\s*\[\s*{_CFG_TEST_PREDICATE}\s*\]\s*"
    rf"(?:{_CFG_ATTR})*)"
    r"(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:const(?!\s+(?:(?:async|unsafe|extern(?:\s*\"[^\"]*\")?)\s+)*fn\b)|static|type|use|mod)\b[^;]*;",
    re.MULTILINE,
)

MODULE_ITEM = re.compile(
    rf"(?P<attributes>(?:{_CFG_ATTR})*)"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<body>[;{])",
    re.MULTILINE,
)
CFG_ATTRIBUTE = re.compile(rf"#\s*\[\s*{_CFG_TEST_PREDICATE}\s*\]")

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
# Review files that exercise host authority, not individual calls or their arguments.
COMPILE_TIME_ENVIRONMENT_SITES: set[SourceKey] = {
    ("plakd", "src/build_identity.rs"),
    ("plakd", "src/commands/exchange_capture.rs"),
}
WORKING_DIRECTORY_SITES: set[SourceKey] = {
    ("plakd", "src/build_identity.rs"),
    ("plakd", "src/shell/home.rs"),
    ("plakous-workspace", "src/git_redirect_fence.rs"),
}
ENVIRONMENT_READER_SITES: set[SourceKey] = {
    ("plakd", "src/build_identity/interactive.rs"),
    ("plakd", "src/commands/doctor.rs"),
    ("plakd", "src/commands/exchange_capture.rs"),
    ("plakd", "src/commands/run/face.rs"),
    ("plakd", "src/commands/support.rs"),
    ("plakd", "src/diagnostics.rs"),
    ("plakd", "src/environment.rs"),
    ("plakd", "src/home/paths.rs"),
    ("plakd", "src/home/secrets.rs"),
    ("plakd", "src/oauth_ui.rs"),
    ("plakd", "src/presentation.rs"),
    ("plakd", "src/sidecars/js_host.rs"),
    ("plakd", "src/sidecars/python_bundle.rs"),
    ("plakd", "src/test_hooks.rs"),
    ("plakd", "src/test_support.rs"),
    ("plakous-hosted", "src/runtime/oneshot/test_support_run_deadline.rs"),
    ("plakous-hosted", "src/test_support_runner.rs"),
    ("plakous-workspace", "src/git_redirect_fence.rs"),
    ("plakous-workspace", "src/occupancy_vcs/git.rs"),
    ("plakous-workspace", "src/occupancy_vcs/mint.rs"),
    ("plakous-workspace", "src/vcs_fixtures.rs"),
}
LAUNCH_DIRECTORY_SITES: dict[str, SourceKey] = {
    "launch_directory": ("plakd", "src/lib.rs"),
    "Services::from_env": ("plakd", "src/lib.rs"),
    "Home::from_env": ("plakd", "src/shell/services.rs"),
}
LAUNCH_DIRECTORY_NAMES = frozenset({"Services", "Home", "launch_directory"})
PROCESS_SITES: set[SourceKey] = {
    ("plakd", "src/carry/installed.rs"),
    ("plakd", "src/carry/upgrade/process.rs"),
    ("plakd", "src/commands/host_install.rs"),
    ("plakd", "src/commands/login_host.rs"),
    ("plakd", "src/commands/runner.rs"),
    ("plakd", "src/commands/runner/dial.rs"),
    ("plakd", "src/commands/runner/dial/children.rs"),
    ("plakd", "src/commands/runner/local/start.rs"),
    ("plakd", "src/commands/support.rs"),
    ("plakd", "src/host/reexec.rs"),
    ("plakd", "src/host/starter.rs"),
    ("plakd", "src/oauth_ui.rs"),
    ("plakd", "src/operator_process.rs"),
    ("plakd", "src/product_doctor/jj_cli.rs"),
    ("plakd", "src/product_doctor/runner_ssh.rs"),
    ("plakd", "src/terminal_application.rs"),
    ("plakous-workspace", "src/occupancy_vcs/git.rs"),
    ("plakous-workspace", "src/occupancy_vcs/mint.rs"),
    ("plakous-workspace", "src/vcs_fixtures.rs"),
}
NON_LAUNCH_PROCESS_SITES: set[SourceKey] = {
    ("plakd", "src/lib.rs"),
    ("plakd", "src/main.rs"),
    ("plakd", "src/snapshot_install/archive.rs"),
}
EXPECTED_PRODUCT_GENERATED_MACROS: dict[SourceKey, tuple[str, ...]] = {
    # The closed content/received vocabulary shares variant definitions only.
    ("plakous-host-protocol", "src/observation/vocabulary.rs"): ("macro_rules",),
    # Export-v8's closed neutral tag mappings, never runtime code generation.
    ("plakd", "src/carry/history.rs"): ("macro_rules",),
    # Spec 02 §3.2: the help pages are one-level static text, so they are static
    # files under `bin/plakd/help/` included at compile time — three global
    # fragments (head, maintainer verbs, tail) and one page per verb. The
    # renderer supplies only what a page cannot state for itself.
    ("plakd", "src/cli/render.rs"): ("include_str",) * 30,
    # Each route owns its four bundled role files; the common multi-provider
    # profile remains in profiles.rs. The total stays sixteen documents.
    ("plakd", "src/profiles.rs"): ("include_str",) * 4,
    ("plakd", "src/capabilities/codex.rs"): ("include_str",) * 4,
    ("plakd", "src/capabilities/claude.rs"): ("include_str",) * 4,
    ("plakd", "src/capabilities/grok.rs"): ("include_str",) * 4,
    # Git and jj redirect shims are static text rendered into the copy.
    ("plakous-workspace", "src/git_redirect_fence.rs"): ("include_str",) * 3,
    # Pinned executor metadata is shared by inspection and contract qualification.
    ("plakous-hosted", "src/agent/binding_inspection.rs"): ("include_str",),
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
    ("plakd", "src/terminal_registry/control.rs"): {"select"},
    ("plakd", "src/host/console/terminals/stream.rs"): {"select"},
    ("plakd", "src/cli.rs"): {"value_parser"},
    ("plakous-hosted", "src/agent/roots.rs"): {"warn"},
    ("plakd", "src/commands/runner/local.rs"): {"warn"},
    ("plakd", "src/commands/runner/dial/children.rs"): {"warn"},
    ("plakd", "src/host.rs"): {"select"},
    ("plakd", "src/host/child_shutdown.rs"): {"warn"},
    ("plakd", "src/host/send.rs"): {"pin", "select"},
    ("plakd", "src/host/sessions/options.rs"): {"pin", "select"},
    ("plakd", "src/host/console/files/resolve.rs"): {"select"},
    ("plakd", "src/commands/runner/services.rs"): {"info", "select", "warn"},
    ("plakd", "src/commands/runner/registry.rs"): {"info", "warn"},
    ("plakd", "src/commands/runner/services/host.rs"): {"warn"},
    # Typed, output-free runner diagnostics and the recorded stop's fixed payload.
    ("plakous-hosted", "src/runner_adoption.rs"): {"info", "json", "warn"},
    # The tree stage a VCS effect runs as, read by a runner-backed port.
    ("plakous-workspace", "src/occupancy_vcs/stage.rs"): {"task_local"},
    ("plakous-host-protocol", "src/observation.rs"): {"define_observations"},
    ("plakous-host-protocol", "src/observation/vocabulary.rs"): {"define_observations"},
    # Disposable transcript projection invariants, with release-mode resync. A
    # repair or a refused snapshot logs its cause, scope and key shape, never rows.
    ("plakous-hosted", "src/runtime/projection/transcript.rs"): {"debug_assert", "warn"},
    # Content-free retirement housekeeping and disposition persistence failures.
    ("plakous-recursive-subagents", "src/tools/driver/retention.rs"): {"debug", "warn"},
    ("plakous-hosted", "src/sessions/transcript.rs"): {"debug_assert"},
    # A reload request dropped or settled at a session switch logs its id.
    ("plakd", "src/reload_request.rs"): {"info", "warn"},
    # Fold integrity refusals: fixed diagnostics and the affected agent id only.
    # An effect run as neither a stage nor an unstaged scope is a defect.
    ("plakous-recursive-subagents", "src/remote_vcs.rs"): {"debug", "debug_assert"},
    # The ledger a cross-runner carry records its stages through.
    ("plakous-recursive-subagents", "src/tree/cross_runner.rs"): {"task_local"},
    ("plakous-recursive-subagents", "src/tree/candidates.rs"): {"error"},
    ("plakous-recursive-subagents", "src/tree/workspaces.rs"): {"error"},
    ("plakous-recursive-subagents", "src/tree/reopen_law.rs"): {"error"},
    # Private capture diagnostics use closed metadata, never request bodies.
    ("plakd", "src/commands/exchange_capture.rs"): {"warn"},
    # A published export whose directory sync failed is kept and logged.
    ("plakd", "src/carry/document.rs"): {"warn"},
    ("plakd", "src/carry/history.rs"): {"tags", "stringify"},
    ("plakous-hosted", "src/mcp_binding.rs"): {"warn"},
    # The instruction-read wire of the hidden runner seam (spec 01 §8.5).
    ("plakous-hosted", "src/runner_reader.rs"): {"json"},
    # `test-support` only: scripted offline model plans for child fixtures.
    ("plakd", "src/test_hooks.rs"): {"json"},
    ("plakd", "src/test_hooks/host_lifecycle.rs"): {"json"},
    ("plakd", "src/build_identity.rs"): {"cfg"},
    # The `test-support`-only persistence benchmark checks that its final
    # reload retained every transcript line it timed.
    ("plakd", "src/store/benchmark.rs"): {"assert_eq"},
    # A `#[cfg(test)]`-only module of the host crate: it cannot be an
    # integration test because the projection it exercises is
    # `pub(crate)`. Nothing here ships in the CLI binary.
    ("plakd", "src/topology_resources_projection_tests.rs"): {"assert_eq"},
    ("plakd", "src/commands/session_delete_tests.rs"): {"assert_eq", "assert_ne"},
    # Read-only child history projects its bounded JSONL response at the CLI boundary.
    ("plakd", "src/commands/session/read_only.rs"): {"json"},
    ("plakd", "src/lib.rs"): {"eprint", "info", "join"},
    # The help pages; see EXPECTED_PRODUCT_GENERATED_MACROS.
    ("plakd", "src/cli/render.rs"): {"include_str"},
    ("plakd", "src/profiles.rs"): {"include_str"},
    ("plakd", "src/capabilities/codex.rs"): {"include_str"},
    ("plakd", "src/capabilities/claude.rs"): {"include_str"},
    ("plakd", "src/capabilities/grok.rs"): {"include_str"},
    ("plakous-workspace", "src/git_redirect_fence.rs"): {"include_str"},
    ("plakous-hosted", "src/agent/binding_inspection.rs"): {"include_str"},
    ("plakd", "src/commands/application.rs"): {"select"},
    # The web endpoint: logins, connections and refusals (no secret or frame),
    # and the accept loop and one connection's socket, session and pings.
    ("plakd", "src/web.rs"): {"info", "pin", "select"},
    ("plakd", "src/web/auth.rs"): {"info", "warn"},
    ("plakd", "src/web/listen.rs"): {"select"},
    ("plakd", "src/web/socket.rs"): {"info", "select"},
    ("plakd", "src/host/socket.rs"): {"select"},
    ("plakd", "src/host/reexec.rs"): {"cmsg_space"},
    # The wire schema document's envelope.
    ("plakd", "src/wire_schema.rs"): {"json"},
    ("plakd", "src/commands/secret_input.rs"): {"select"},
    # The SIGTERM/SIGHUP watcher thread waits on two signal streams at once.
    ("plakd", "src/signals.rs"): {"select"},
    # Gateway serve waits on SIGTERM, Ctrl-C and the admin drain together.
    ("plakd", "src/gateway/service.rs"): {"pin", "select"},
    # The bridge's panic redaction is scoped to the current worker call.
    ("plakd", "src/gateway/bridge.rs"): {"thread_local"},
    # Install selects the user-service definition for this operating system.
    ("plakd", "src/gateway/install.rs"): {"cfg"},
    ("plakous-hosted", "src/runtime/turn_drive/mod.rs"): {"pin", "select", "debug_assert"},
    ("plakous-hosted", "src/runtime/network_watch.rs"): {"select"},
    ("plakous-hosted", "src/host_service/wire/host.rs"): {"info", "select"},
    ("plakous-hosted", "src/host_service/wire/host/step_drain/cutover.rs"): {"pin", "select"},
    ("plakous-hosted", "src/runtime/host_loop/idle.rs"): {"select"},
    ("plakous-hosted", "src/runtime/host_loop/step_hold.rs"): {"select"},
    ("plakous-hosted", "src/driver/transient.rs"): {"pin", "select", "debug_assert"},
    ("plakous-hosted", "src/runtime/turn_drive/admission.rs"): {"thread_local"},
    ("plakous-hosted", "src/interrupt.rs"): {"pin", "select"},
    # A live child's actor serves its mailbox beside its turn watch.
    ("plakous-hosted", "src/agent/live.rs"): {"pin", "select"},
    # One turn's drive selects its result, events and role wakes, for every role.
    ("plakous-hosted", "src/node.rs"): {"pin", "select"},
    # A child turn's drive takes its event source or its result, whichever first.
    ("plakous-hosted", "src/runtime/child_projection.rs"): {"pin", "select"},
    # The product half of the session record keeps the one-entry store cache.
    ("plakd", "src/store/sessions.rs"): {"thread_local"},
    # The highlighter cache is thread-local because syntect parse/highlight
    # state is mutable and host observation may occur on more than one thread.
    # cfg(test) counter of composer wrap calls. Production paint does not
    # include it; the macro is still in this source file.
    ("plakous-host-protocol", "src/jsonl.rs"): {"json"},
    ("plakous-host-protocol", "src/topology.rs"): {"json"},
    ("plakous-hosted", "src/session_log.rs"): {"thread_local"},
    ("plakous-hosted", "src/skills.rs"): {"json"},
    # Explicit unreachable operation in the feature-gated hosted test fixture.
    ("plakous-hosted", "src/runtime/test_kit.rs"): {"panic"},
    ("plakous-hosted", "src/sessions.rs"): {"thread_local"},
    ("plakd", "src/store/mod.rs"): {"params"},
    ("plakd", "src/store/views.rs"): {"params", "debug_assert"},
    ("plakd", "src/store/tree.rs"): {"params", "warn"},
    ("plakous-recursive-subagents-store", "src/admission.rs"): {"params"},
    ("plakous-recursive-subagents-store", "src/rows.rs"): {"params"},
    ("plakous-recursive-subagents-store", "src/rows/codec.rs"): {"params"},
    ("plakous-recursive-subagents", "src/epoch.rs"): {"json"},
    ("plakous-recursive-subagents", "src/hub.rs"): {"error"},
    ("plakous-recursive-subagents", "src/hub/registry.rs"): {"info", "pin", "select", "warn"},
    ("plakous-recursive-subagents", "src/hub/shutdown.rs"): {"warn"},
    # A custody release the store refused logs only the tree id and the error;
    # a last share dropped without its release panics in debug builds only.
    ("plakous-recursive-subagents", "src/tree/custody/custodian.rs"): {"panic", "warn"},
    # Bounded VCS refusal detail, once per workspace/detail change; never file content.
    ("plakous-recursive-subagents", "src/tree/dormancy.rs"): {"info", "warn"},
    # Completion failure logs carry only captured agent/attempt ids and the typed error kind.
    ("plakous-recursive-subagents", "src/tools/cancel.rs"): {"error", "json"},
    # Retirement-notice overflow logs carry only the number of missed observations.
    ("plakous-hosted", "src/runtime/tree.rs"): {"warn"},
    ("plakous-hosted", "src/topology_navigator.rs"): {"error"},
    ("plakous-hosted", "src/runtime/topology_state.rs"): {"error"},
    ("plakous-recursive-subagents", "src/tools/jobs.rs"): {"json"},
    # A claim or effect no owner could release logs only ids and the error code.
    ("plakous-recursive-subagents", "src/tools/driver.rs"): {"error"},
    # A custody release the store refused when the driver ended logs only the error.
    ("plakous-recursive-subagents", "src/tools/driver/custody.rs"): {"warn"},
    ("plakous-recursive-subagents", "src/tools/mail.rs"): {"error", "json"},
    ("plakous-recursive-subagents", "src/tools/mod.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tools/registrations.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tools/spawn.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tools/wait.rs"): {"json", "select"},
    ("plakous-recursive-subagents", "src/tree/mail.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tree/mod.rs"): {"json"},
    ("plakous-recursive-subagents", "src/tree/projection.rs"): {"json"},
    # Static WAL-checkpoint contention warning; no SQL, paths, or session data.
    ("plakd", "src/telemetry_store.rs"): {"params", "warn"},
    # Typed provider status/code and bounded, display-redacted failure excerpt.
    ("plakd", "src/telemetry_store/model_failure.rs"): {"json", "params", "warn"},
    # Typed SQLite binds only; neither projection admits or dispatches work.
    ("plakd", "src/telemetry_store/recorded_usage.rs"): {"params"},
    ("plakd", "src/host/console/recorded.rs"): {"params"},
    # Private login cancellation races only; no process signal handler.
    ("plakd", "src/host/console/account_writes/login.rs"): {"select"},
    ("plakd", "src/host/console/account_writes/login/gateway.rs"): {"select"},
    # Bounded categorical operator audit fields, never frames or secrets.
    ("plakd", "src/host/console/audit.rs"): {"json"},
    ("plakd-wire-types", "src/console/secret.rs"): {"json_schema"},
    ("plakd", "src/telemetry_sink.rs"): {"warn"},
    # Static panic notice; no payload, path or session data.
    ("plakd", "src/shell/chrome_refresh.rs"): {"warn"},
    # A refused tree publish (the store's compare-and-swap found another
    # writer) is reported through the tracing boundary, like the
    # credential-refresh warning above; typed fields only.
    ("plakous-hosted", "src/agent/children/bind.rs"): {"error"},
    ("plakous-hosted", "src/agent/children/turns.rs"): {"error"},
    ("plakous-hosted", "src/agent/children/waiter.rs"): {"error", "warn"},
    # A restored child turn cancelled without its abandonment report: typed
    # agent and turn ids, the cause and the error kind only.
    ("plakous-hosted", "src/agent/children/reopen.rs"): {"warn"},
    # Background recovery reports only typed child ids and closed recovery
    # dispositions after the pass has settled; it never logs prompt content.
    ("plakous-hosted", "src/agent/recovery.rs"): {"info"},
    # Each model-request retry logs its closed notice fields and a bounded
    # detail, which may carry the provider's credential-filtered error
    # message; never request or response bodies.
    ("plakous-hosted", "src/runtime/events.rs"): {"warn"},
    # A wake the session store refused logs its cause class, never content.
    ("plakous-hosted", "src/runtime/host_loop/wake.rs"): {"warn"},
    # Wake decisions and starvation warnings carry ids, closed reasons and counts only.
    ("plakous-hosted", "src/runtime/host_loop/wake/diagnostics.rs"): {"info", "warn"},
    # A host's stop logs the session id and whether it was recorded; the
    # wire request that asked for it also logs the connection's face and
    # login (spec 04 section 2.10). Never content.
    ("plakous-hosted", "src/host_service/wire.rs"): {"info"},
    ("plakous-hosted", "src/runtime/host_loop/stop.rs"): {"info"},
    # Context-meter diagnostics are bounded; allowance reads join independent deadlines.
    ("plakous-hosted", "src/runtime/usage.rs"): {"join", "warn"},
    # Pending HUD diagnostics carry only source, counts and delivery status.
    ("plakous-hosted", "src/runtime/host_loop/pending.rs"): {"debug"},
    ("plakd", "src/sidecars/python_bundle.rs"): {"cfg"},
    # The sidecar's stdio loop multiplexes the client channel and the evaluator.
    # Presentation-only invariants and the per-frame tracing span.
    # Background panic: thread name and source location only, never payload.
}
# Rust keywords that may legally precede `!` without naming a macro (`if !x`).
_NOT_MACRO_KEYWORDS = {"else", "if", "in", "match", "return", "while"}

# The facade symbols a host/bin production source may import, per package.
# Today every scanned package shares one approved list; a per-package narrowing
# is a one-row edit here.
_SHARED_PLAKOUS_SYMBOLS = {
    "api_key": {
        "ApiKey",
        "ApiKeyAuthorityToken",
        "ApiKeyError",
        "ApiKeyGenerationGrant",
        "ApiKeyRequest",
        "ApiKeySource",
        "CredentialGenerationFingerprint",
        "CredentialGenerationFingerprintError",
        "CredentialProviderTag",
        "StaticApiKeySource",
    },
    "openai": {
        "OpenAiApiKeyRequest",
        "static_openai_api_key_source",
        "OPENAI_PLATFORM_MODEL_MANIFEST_REVIEW_DUE_AT_UNIX_SECONDS",
        "CredentialAuthorityId",
        "CredentialReference",
        "EndpointOrigin",
        "ApiKey",
        "OpenAiKeyGeneration",
        "OpenAiApiModel",
        "OpenAiApiModelStatus",
        "OpenAiAuthError",
        "OpenAiListedModel",
        "OpenAiPlatformModel",
        "OpenAiPlatformModelBuilder",
        "OpenAiPlatformModelFacts",
        "OpenAiKeySource",
        "PlatformProjectPolicy",
        "PlatformStoragePolicy",
        "StaticOpenAiKeySource",
        "accept_openai_listed_models",
        "list_openai_api_models",
        "openai_api_bundled_listing",
        "openai_api_key_fingerprint",
        "openai_api_models",
    },
    "anthropic": {
        "AnthropicApiKeyRequest",
        "anthropic_api_key_fingerprint",
        "ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_UNIX_SECONDS",
        "ApiKey",
        "AnthropicKeySource",
        "AnthropicLanguageModel",
        "AnthropicError",
        "AnthropicErrorKind",
        "AnthropicErrorPhase",
        "claude_max_output_tokens",
        "CLAUDE_ENROLLMENTS",
        "AnthropicApiModel",
        "AnthropicApiModelStatus",
        "AnthropicListedModel",
        "accept_anthropic_listed_models",
        "anthropic_api_bundled_listing",
        "anthropic_api_models",
        "list_anthropic_api_models",
    },
    "xai": {
        "XaiPlatformApiKeyRequest",
        "xai_api_key_generation",
        "XAI_PLATFORM_REVIEW_DUE_AT_UNIX_SECONDS",
        "XaiApiModel",
        "XaiApiModelError",
        "XaiApiModelStatus",
        "XaiListedModel",
        "ApiKey",
        "XaiPlatformApiKeyAuthority",
        "XaiPlatformKeySource",
        "XaiPlatformBuildError",
        "XaiPlatformKeyGeneration",
        "XaiPlatformModel",
        "accept_xai_listed_models",
        "grok_max_output_tokens",
        "list_xai_api_models",
        "xai_api_bundled_listing",
        "xai_api_key_fingerprint",
        "xai_api_models",
        "xai_platform_selection",
        "xai_platform_selection_for",
        "XAI_PLATFORM_GROK_46_MODEL",
        "XAI_PLATFORM_GROK_46_CONTEXT_TOKENS",
        "XAI_PLATFORM_MAX_OUTPUT_TOKENS",
    },
    "exchange_observation": {
        "ConnectionUse",
        "ExchangeObserver",
        "ExchangeObservation",
        "ExchangeTerminal",
    },
    "openrouter": {
        "OPENROUTER_CHAT_REVIEW_DUE_AT",
        "OPENROUTER_CHAT_PROTOCOL_ID",
        "OPENROUTER_MESSAGES_PROTOCOL_ID",
        "OPENROUTER_RESPONSES_PROTOCOL_ID",
        "ApiKey",
        "OpenRouterKeyGeneration",
        "OpenRouterKeySource",
        "OpenRouterAuthError",
        "OpenRouterCredentialRequest",
        "OpenRouterChatClient",
        "OpenRouterChatDiscoveryError",  # Host refuses a model the catalog does not list.
        "OpenRouterChatEndpointCandidate",
        "OpenRouterChatModelEvidence",
        "OpenRouterReasoningFacts",
        "OpenRouterToolChoiceModes",
        "OpenRouterCredentialBinding",
        "OpenRouterKeyUsage",
        "OpenRouterKeyUsageError",  # Host renders typed 401/403 refusals with safe source labels.
        "OpenRouterRetentionPolicy",
        "StaticOpenRouterKeySource",
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
        # Names the frozen binding input an exact reopen found changed.
        "BindingChange",
        "AgentEvent",
        "AgentEventKind",
        "AgentRequestPolicy",
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
        "ModelReresolutionRefusal",
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
        # One shared per-account bound on concurrent model requests.
        "RequestSlots",
        # Read-only in-memory native credential wait scope, not admission authority.
        "CredentialWait",
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
        # The host's idle fence asks Plakous's one scheduling verdict
        # (spec 01 section 10.3) instead of keeping its own predicates.
        "CompactionMeasure",
        "CompactionVerdict",
        "MeasuredFit",
        "DurabilityAttemptKind",
        "DurabilityAttemptOutcome",
        "TelemetryContext",
        "TelemetryEmitter",
        "TelemetryObservation",
        "TelemetryObservationKind",
        "TelemetrySink",
        "TelemetrySinkError",
        "TelemetryTerminalStatus",
        # Closed wait labels persisted by the telemetry projection.
        "WaitOutcome",
        "WaitTargetKind",
        "ChildProcessKind",
        "ChildProcessExit",
        "CredentialProviderLabel",
        "CredentialAccountLabel",
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
    # Task sets spawn through the executor-neutral runtime (spec 01 section 2.5).
    "runtime": {
        "Runtime",
        "SpawnedTask",
        "TokioRuntime",
    },
    "model": {
        "Clock",
        "ToolResultKind",
        "TurnOrigin",
        "SystemClock",
        "SystemIdGenerator",
        "Actor",
        "DriverInstanceId",
        "LineId",
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
        # Settling an interrupted carry holds its source branch (spec 02 §2.6).
        "BranchHold",
        "carried_from_provenance",
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
        "RUNTIME_FORMAT_CUT",
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
        "OpenVerification",
        "OpenedSession",
        "PortableSessionHistory",
        "PruneHistoryOutcome",
        "RebindSessionOutcome",
        # A rebind refused before it was attempted (spec 01 §5.7).
        "RebindRefused",
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
        "SessionInputWindow",
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
        "CatalogPublicationSkipped",
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
        "USAGE_OBSERVATION_FRESHNESS",
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
        # A waiting tool's typed fact: counts and a closed wake for measurement.
        "WaitFact",
        "WaitWake",
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
        "LocalJobHost",
        "JobStatus",
        "JobView",
        "JobStopReason",
        "JoinedProcessSessions",
        # A registry holds each owner's service through its port, and a host
        # asks workspace-path questions through the workspace probe, so either
        # can be answered where the workspace lives (spec 01 section 8.5).
        "JoinedSessionsPort",
        "LocalWorkspaceProbe",
        "ProbeError",
        "WorkspaceProbe",
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
        "PythonHarnessSdkV1",
        "PythonHarnessSdkDescriptorV1",
        "CellDescriptor",
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
        "standard_coding_packs_with_files",
        "FilesToolPack",
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
    # assembler and its vocabulary, and bounded AGENTS.md and Agent Skills
    # discovery. The tool
    # descriptions the product still owns are not here -- they travel with the
    # topology pack and live in `crates/orchestration/recursive-subagents/src/text/`.
    "prompts": {
        "bases",
        "identity",
        "instructions",
        "pins",
        "plan",
        "ptc",
        "skills",
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
        "discover_skill_roots",
        "discover_skills",
        "InstructionSourceReader",
        "LocalInstructionSourceReader",
        "openrouter_direct_base_instructions",
        "Skill",
        "SkillDiscovery",
        "SkillRoot",
        "SkillScope",
        "SkillsPresentation",
        "target_instructions",
    },
}
# Doc-hidden credential-gateway vocabulary (spec 01 §7.5). plakd is the only
# product caller; other host packages do not import it.
_GATEWAY_SYMBOLS = {
    "AccountFacts",
    "AccountGlob",
    "AccountStatusRow",
    "AdminRefusal",
    "AdminRequest",
    "AdminResponse",
    "AfterRejection",
    "Any",
    "BoundedText",
    "CallError",
    "CallerExists",
    "CallerId",
    "CallerKey",
    "CallerRow",
    "CallerUnknown",
    "Capability",
    "CredentialFacts",
    "CredentialStatus",
    "CustodyError",
    "CustodySlot",
    "DeviceCode",
    "DisableTarget",
    "ERROR_HEADER",
    "Endpoint",
    "EnvelopeInvalid",
    "FactsReply",
    "FactsRequest",
    "GatewayAccount",
    "GatewayRefusal",
    "GatewayService",
    "IfDue",
    "Injection",
    "KEY_HEADER",
    "KeyFacts",
    "KeyHeaderForm",
    "KeyInjection",
    "KeySource",
    "LimitExceeded",
    "Local",
    "LocalAdminRequired",
    "LocalCustody",
    "LocalKey",
    "LocalKeyCustodian",
    "LocalKeyStore",
    "LoginMethod",
    "LoginPhase",
    "LogoutMode",
    "PROTOCOL_VERSION",
    "PasteInvalid",
    "Peer",
    "RefreshMode",
    "RefreshReply",
    "RefreshRequest",
    "RefusalBody",
    "RelayEnvelope",
    "RelayOutcome",
    "RelayRecord",
    "RelayServer",
    "SUPPORTED_VERSIONS",
    "ServiceFuture",
    "ScopeSet",
    "SecretText",
    "SourceNet",
    "TextError",
    "UpstreamPin",
    "UuidText",
    "VERSIONS_PATH",
    "VersionsReply",
    "WireIdentity",
    "admin",
    "call",
    "custody",
    "decode_request",
    "envelope",
    "facts",
    "new",
    "pins",
    "protocol",
    "refusal",
    "scope",
    "self",
    "server",
}

ALLOWED_PLAKOUS_SYMBOLS_BY_PACKAGE: dict[str, dict[str, set[str]]] = {
    "plakd": {
        **_SHARED_PLAKOUS_SYMBOLS,
        "gateway": _GATEWAY_SYMBOLS
        | {
            "GatewayCustody",
            "GatewayEndpoint",
            "SensitiveBytes",
        },
        # Hidden, unsupported: who holds route credentials (spec 01 sec. 7.5).
        "custody": {"Custody"},
        "subscriptions": _SHARED_PLAKOUS_SYMBOLS["subscriptions"]
        | {
            "custody",
            "SubscriptionCustodian",
        },
        "openrouter": _SHARED_PLAKOUS_SYMBOLS["openrouter"]
        | {
            "decode_openrouter_key_usage",
            "static_openrouter_api_key_source",
        },
    },
    # Wire DTOs only; the facade is not a dependency.
    "plakd-wire-types": {},
    "plakous-host-protocol": _SHARED_PLAKOUS_SYMBOLS,
    "plakous-hosted": {
        **_SHARED_PLAKOUS_SYMBOLS,
        # Hidden, unsupported preview-minimization seam; not a product API.
        "tools": _SHARED_PLAKOUS_SYMBOLS["tools"]
        | {
            "CodexMinimalLocalCodeModeDescriptorV1",
            "preview_redaction",
            # Hidden, unsupported seam: the mark a job tail cut inside its
            # only line starts with, so a face shows no fragment.
            "JOB_TAIL_FRAGMENT",
        },
        # Hidden, unsupported: who holds route credentials (spec 01 sec. 7.5).
        "custody": {"Custody"},
    },
    "plakous-workspace": _SHARED_PLAKOUS_SYMBOLS,
    "plakous-recursive-subagents": {
        **_SHARED_PLAKOUS_SYMBOLS,
        "tools": _SHARED_PLAKOUS_SYMBOLS["tools"],
    },
    "plakous-recursive-subagents-store": _SHARED_PLAKOUS_SYMBOLS,
}
ALLOWED_PLAKOUS_SYMBOLS_BY_SOURCE: dict[SourceKey, dict[str, set[str]]] = {
    ("plakd", "src/host/store_revisions.rs"): {
        "sessions": {
            "bootstrap_stopped_runtime",
            "rehearsal",
            "rehearse_runtime_bootstrap",
            "runtime_bootstrap_pending",
        },
    },
    # Child traces share the transcript redactor without inheriting preview bounds.
    ("plakd", "src/carry/history/trace_redaction.rs"): {"tools": {"preview_redaction"}},
    # Deploy distinguishes API refusals without exposing provider bodies,
    # and its Codex cutoff must refuse an explicit loss of output events.
    ("plakd", "src/deploy/canary.rs"): {"gateway": {"ApiCanaryFailure"}},
    ("plakd", "src/deploy/canary/local.rs"): {"agent": {"EventDelivery"}},
    # The synchronous account bridge carries the same stack as its executor.
    ("plakd", "src/gateway/bridge.rs"): {"agent": {"RUNTIME_EXECUTOR_STACK_BYTES"}},
    # Route admission reads non-secret key facts through gateway custody.
    ("plakd", "src/gateway/route_custody.rs"): {"gateway": {"custody::CredentialCustodian"}},
    # Frozen construction data and pure metadata checks, not runtime authority.
    ("plakd", "src/commands/session/check_binding.rs"): {"sessions": {"rehearsal"}},
    ("plakd", "src/carry/reload/child_reopen.rs"): {"sessions": {"rehearsal"}},
    ("plakous-hosted", "src/agent/binding_inspection.rs"): {
        "sessions": {"FrozenSessionPresentation"},
        "tools": {"RegisteredToolIdentity", "ToolBindingError"},
    },
    ("plakous-hosted", "src/agent/feed_projection.rs"): {"sessions": {"FeedTerminalOrigin"}},
    ("plakous-hosted", "src/agent/recovery.rs"): {"sessions": {"FeedTerminalOrigin"}},
    ("plakous-hosted", "src/runtime/projection.rs"): {"sessions": {"FeedTerminalOrigin"}},
    # Operator inspection uses only unkeyed, contained execution reads.
    ("plakd", "src/host/console/files.rs"): {
        "tools": {
            "ExecutionBackendErrorKind",
            "OperationContext",
            "RepositoryMatchCase",
            "RepositoryQuery",
            "RepositoryQueryLimits",
            "RepositoryQueryResult",
            "RepositoryQueryStopReason",
            "WorkspaceReadSliceRequest",
        },
    },
    ("plakd", "src/host/console/files/changes.rs"): {
        "tools": {"OperationContext", "changes"},
    },
    ("plakd", "src/host/console/files/cursors.rs"): {
        "tools": {"WorkspaceReadCursor"},
    },
    ("plakd", "src/host/console/files/resolve.rs"): {
        "model": {"RunId"},
        "runner": {"RunnerRegistry"},
        "tools": {
            "ExecutionEnvironment",
            "ExecutionEnvironmentConfig",
            "ExecutionEnvironmentScope",
            "runner::runner_environment",
            "runner::RunnerBackend",
            "runner::RunnerCall",
            "runner::RunnerCallFuture",
            "runner::RunnerFailure",
            "runner::RunnerLink",
        },
    },
    ("plakd", "src/carry/upgrade/remote.rs"): {
        "runner": {"RunnerServices", "PlacementFailure"},
        "tools": {"runner::RunnerBackend"},
    },
    ("plakd", "src/carry/upgrade.rs"): {"runner": {"protocol", "VersionTriple"}},
    ("plakd", "src/host/upgrade.rs"): {"runner": {"protocol", "VersionTriple"}},
    ("plakd", "src/host/upgrade/runner_carry.rs"): {
        "runner": {"protocol", "LineStatus", "VersionTriple"}
    },
    ("plakd", "src/host/sessions.rs"): {"runner": {"RunnerRow"}},
    ("plakd", "src/host/child_shutdown.rs"): {
        "agent": {"host_worker_shutdown"},
        "tools": {"host_child_shutdown"},
    },
    ("plakous-hosted", "src/jobs.rs"): {"tools": {"LineOwner"}},
    ("plakd", "src/platform_product/anthropic.rs"): {
        "anthropic": {"ANTHROPIC_PLATFORM_ORIGIN"},
    },
    # The tree's VCS port over the runner link, and the runner extension
    # that answers it (spec 01 §8.5).
    ("plakous-recursive-subagents", "src/remote_vcs.rs"): {
        "tools": {
            "runner::RunnerCall",
            "runner::RunnerExtension",
            "runner::RunnerFailure",
            "runner::RunnerLink",
            "runner::RunnerVerb",
            "runner::ServeFailure",
            "runner::ServeFuture",
        },
    },
    ("plakous-hosted", "src/runtime/child_live.rs"): {
        "agent": {"TurnViewState"},
    },
    ("plakous-hosted", "src/runtime/step_hold.rs"): {
        "agent": {"WeakAgent"},
    },
    # The unsupported host-only boundary hold, not a raw runtime endpoint.
    ("plakous-hosted", "src/host_service/wire/host/step_drain.rs"): {
        "agent": {
            "host_control::BoundaryHold",
            "host_control::BoundaryPhase",
            "host_control::BoundaryProgress",
            "host_control::hold_step_boundary",
        },
    },
    # The turn drive's event source is the admitted turn's own event stream.
    ("plakous-hosted", "src/node.rs"): {"agent": {"TurnEvents"}},
    ("plakous-hosted", "src/runtime/child_projection.rs"): {
        "agent": {
            "CommandAck",
            "TurnEvents",
            "EventDelivery",
            "SubscriptionEnd",
            "AgentViewState",
            "TurnViewState",
        },
    },
    # The hidden runner seam's instruction reads (spec 01 §8.5): the reader
    # contract's answer types, and the seam's call, link and extension.
    ("plakous-hosted", "src/runner_reader.rs"): {
        'prompts': {
            'DirectoryAnswer',
            'DirectoryRefusal',
            'InstructionChainEntry',
            'SourceEntryKind',
            'SourceFile',
            'SourceReadError',
            'SourceResolution',
        },
        'runner': {'RunnerServices'},
        'tools': {
            'runner::RunnerCall',
            'runner::RunnerExtension',
            'runner::RunnerFailure',
            'runner::RunnerLink',
            'runner::RunnerVerb',
            'runner::ServeFailure',
            'runner::ServeFuture',
            'runner::call_blocking',
            'runner::environment_failure',
            'runner::workspace_refusal',
        },
    },
    ("plakous-hosted", "src/process_environment.rs"): {
        "runner": {"operator", "protocol", "OperatorRefusal"}
    },
    ("plakd", "src/commands/runner/operator.rs"): {
        "runner": {
            "operator",
            "protocol",
            "OperatorWorkspace",
            "OperatorAccountFallback",
            "OperatorRefusal",
        }
    },
    ("plakd", "src/host/console/terminals.rs"): {
        "runner": {
            "protocol",
            "operator",
            "TerminalDimensions",
            "TerminalCursor",
            "TerminalEvent",
            "TerminalKey",
            "TerminalStatus",
            "TerminalProcessIdentity",
            "OperatorId",
            "OperatorRefusal",
        }
    },
    ("plakd", "src/terminal_registry.rs"): {
        "tools": {"runner", "RunnerFailure"},
        "runner": {
            "protocol",
            "operator",
            "RunnerClient",
            "RunnerConnectionRevision",
            "OperatorAnswer",
            "OperatorId",
            "OperatorNamespace",
            "OperatorOperation",
            "OperatorRefusal",
            "TerminalKey",
            "TerminalRow",
            "TerminalStatus",
        },
    },
    ("plakd", "src/terminal_registry/control.rs"): {
        "runner": {
            "protocol",
            "operator",
            "OperatorTerminalStream",
            "TerminalCursor",
            "TerminalDimensions",
            "TerminalEvent",
        }
    },
    ("plakd", "src/terminal_registry/placement.rs"): {
        "runner": {"protocol", "operator", "OperatorNamespace", "TerminalKey"}
    },
    ("plakd", "src/commands/runner/operator/admission.rs"): {
        "runner": {"ClientWindows", "RunnerClient"}
    },
    # The export document names its runner by the library's runner identity.
    ("plakd", "src/carry/document.rs"): {"tools": {"RunnerId"}},
    # `plakd runner`: the daemon over the hidden runner ends, its environment
    # and identity, and the host's inspecting link (spec 02 §3.1).
    ("plakd", "src/commands/runner.rs"): {
        'model': {'RunId'},
        'runner': {
            'Bind',
            'ClientWindows',
            'FileOwnerFloors',
            'LineStatus',
            'PlacementFailure',
            'Refusal',
            'RunnerClient',
            'RunnerIdentity',
            'RunnerLimits',
            'RunnerOptions',
            'RunnerPin',
            'RunnerService',
            'WorkspaceCatalog',
            'daemon',
            'machine_id',
            'protocol',
            'protocol::retention',
        },
        'tools': {
            'ExecutionEnvironment',
            'ExecutionEnvironmentConfig',
            'ExecutionEnvironmentError',
            'ExecutionEnvironmentScope',
            'RunnerId',
            'runner',
            'runner::RunnerExtension',
            'runner::workspace_refusal',
        },
    },
    # The host's strict ssh dial and journaled bind to a runner.
    ("plakd", "src/commands/runner/dial.rs"): {
        'runner': {
            'Bind',
            'CallJournal',
            'ClientWindows',
            'ConnectError',
            'FileCallJournal',
            'Hello',
            'PlacementFailure',
            'ProbePhase',
            'RunnerClient',
            'VersionTriple',
            'protocol',
        }
    },
    # The host starts and journal-binds its home-local daemon before opening sessions.
    ("plakd", "src/commands/runner/local.rs"): {
        "runner": {
            "ClientWindows",
            "ConnectError",
            "FileCallJournal",
            "FileOwnerFloors",
            "RunnerClient",
            "protocol",
            "daemon",
        },
    },
    ("plakd", "src/shell/services.rs"): {
        'runner': {
            'MachineOverlapGuard',
            'PlacementFailure',
            'ProbePhase',
            'RunnerClient',
            'RunnerRegistry',
            'RunnerServices',
        },
        'tools': {'runner', 'runner::LocalDaemonBackend'},
    },
    ("plakous-hosted", "src/agent/roots.rs"): {
        "sessions": {
            "LeaseOnlyHead",
            "FrozenSessionPresentation",
            "rehearsal",
            "sqlite_reopen_inspection",
        },
        'runner': {'RunnerClient', 'RunnerRegistry', 'RunnerServices', 'RunnerStatus'},
        'tools': {'runner::LocalDaemonBackend'},
    },
    ("plakous-hosted", "src/agent/roots/local_runner.rs"): {
        "tools": {"runner::RunnerFailure"},
        "runner": {
            "ClientWindows",
            "ConnectError",
            "RunnerClient",
            "protocol",
            "Bind",
            "PROTOCOL_VERSION",
            "VersionTriple",
        },
    },
    # `test-support` only: the offline script's child runner, bound and journaled
    # as a host's dial is, then selected for the process (spec 01 §8.5).
    ("plakd", "src/test_hooks.rs"): {
        'model': {'FixedClock'},
        'runner': {'RunnerServices'},
    },
    ("plakd", "src/test_hooks/child_runner.rs"): {
        'runner': {
            'CallJournal',
            'FileCallJournal',
            'RunnerClient',
            'RunnerServices',
        },
        'tools': {'runner::RunnerBackend'},
    },
    ('plakous-hosted', 'src/runtime/dormant_stop.rs'): {'sessions': {'LeaseOnlyHead'}},
    # Re-adoption of a runner's lines through a lease-only head (spec 01 §8.4).
    ("plakous-hosted", "src/runner_adoption.rs"): {
        "sessions": {"LeaseOnlyHead"},
        "runner": {"RunnerClient", "protocol"},
        "tools": {
            "LineOwner",
            "runner::RunnerFailure",
            "runner::decode_jobs_snapshot",
            "runner::RunnerCall",
            "runner::RunnerLink",
            "runner::RunnerVerb",
        },
    },
    # Plak owns the export document, not a second runtime. This one codec
    # reconstructs validated public content data, never mutable authority.
    ("plakd", "src/carry/history.rs"): {
        "agent": {"MAX_USER_IMAGE_BYTES"},
        "model": {
            "AudioBlock",
            "CanonicalHistory",
            "CitationBlock",
            "CitationRange",
            "CitationSource",
            "CitationTarget",
            "CitationUnit",
            "CompactionBlock",
            "CompactionReplayBinding",
            "DataClassification",
            "DocumentBlock",
            "ImageBlock",
            "ImageDetailHint",
            "McpServerId",
            "MediaSource",
            "ModelFamilyId",
            "NamespaceId",
            "OpaqueBlock",
            "OpaqueFallback",
            "ReasoningBlock",
            "RefusalBlock",
            "ReplayEnvelope",
            "ReplayKindId",
            "ReplayScope",
            "SummarizedTurns",
            "TextPhase",
            "ToolCallBlock",
            "ToolCallState",
            "ToolCorrelation",
            "ToolExecutionOwner",
            "ToolResultBlock",
            "ToolResultUpdateBlock",
        },
    },
    # Prompt images: the library's admission bounds, asserted equal to the
    # attachment bounds of `plakous-image`.
    ("plakous-hosted", "src/attachments.rs"): {
        "agent": {
            "MAX_USER_IMAGES",
            "MAX_USER_IMAGE_BYTES",
            "MAX_USER_MESSAGE_BYTES",
        },
    },
    # Workspace image reads install only the decoder. The agent derives the
    # route, model and image-input fact from the bound provider configuration.
    ("plakous-hosted", "src/visual_read.rs"): {
        "tools": {"ImageNormalizationError", "ImageNormalizer", "PreparedImage"},
    },
    ("plakous-hosted", "src/sessions/queue.rs"): {
        "agent": {"MAX_USER_IMAGES", "MAX_USER_IMAGE_BYTES", "UserInput", "UserPart"},
        "tools": {"sniff_image"},
    },
    ("plakous-hosted", "src/sessions/transcript.rs"): {"tools": {"sniff_image"}},
    ("plakous-hosted", "src/runtime/admission.rs"): {"agent": {"MAX_USER_IMAGES"}},
    # Pending-input context queries measure the message with its images.
    ("plakous-hosted", "src/runtime/compact.rs"): {"agent": {"UserInput"}},
    ("plakous-hosted", "src/runtime/compact/scheduled.rs"): {"agent": {"UserInput"}},
    ("plakous-hosted", "src/runtime/host_loop/wake.rs"): {"agent": {"UserInput"}},
    ("plakous-hosted", "src/agent/build/session.rs"): {
        # A scheduled compaction qualifies on the pending message, images included.
        "agent": {"UserInput"},
        "sessions": {"internal"},
    },
    # A Direct extension's registry owner names its MCP server.
    ("plakous-hosted", "src/agent/build/tools.rs"): {"tools": {"ToolOwnerClass"}},
    ("plakous-hosted", "src/agent/build/api.rs"): {
        'runner': {'RunnerServices'},
        'tools': {'RegisteredToolIdentity'},
    },
    # MCP binding: the facade's MCP client, a host environment for the pool's
    # service processes or the selected runner's for a server placed there,
    # and the named-secret value type.
    ("plakous-hosted", "src/mcp_binding.rs"): {
        'mcp': {
            'HeaderName',
            'HeaderValue',
            'McpError',
            'McpErrorKind',
            'McpHttpEndpoint',
            'McpHttpSecret',
            'McpHttpSecretResolver',
            'McpLease',
            'McpNamedSecretMissing',
            'McpOAuthSecretResolver',
            'McpOAuthSettings',
            'McpOAuthStatus',
            'McpPool',
            'McpPrompt',
            'McpPromptExpansion',
            'McpSecretResolver',
            'McpServerSpec',
            'McpToolPack',
            'McpToolRecord',
            'McpToolSelection',
            'SensitiveBytes',
            'ServiceSecretValue',
            'exposed_tool_name',
        },
        'model': {'RunId'},
        'runner': {'RunnerServices'},
        'tools': {
            'EnvironmentVariableName',
            'ExecutionEnvironment',
            'ExecutionEnvironmentConfig',
            'ExecutionEnvironmentScope',
            'runner::RunnerBackend',
            'runner::runner_environment',
        },
    },
    # `plakd mcp list|show` report an OAuth server's stored login.
    ("plakd", "src/commands/mcp.rs"): {"mcp": {"McpOAuthStatus"}},
    ("plakous-hosted", "src/mcp_binding/secret_value.rs"): {
        "mcp": {"SensitiveBytes", "ServiceSecretValue"},
    },
    ("plakd", "src/commands/mcp_login.rs"): {
        "mcp": {
            "McpError",
            "McpLocalRetirement",
            "McpLoginOptions",
            "McpLogoutOutcome",
            "McpOAuthStatus",
            "McpPool",
            "McpRemoteLogin",
            "McpServerSpec",
        }
    },
    # The MCP records the root's frozen tools keep in its session record.
    ("plakous-hosted", "src/frozen_tools.rs"): {"mcp": {"McpToolRecord"}},
    # The untrusted-data paragraph an MCP tool earns.
    ("plakous-hosted", "src/tools.rs"): {
        "sessions": {"FrozenSessionPresentation"},
        "tools": {"ProcessPackConfig"},
        "prompts": {"UNTRUSTED_TOOL_DATA_AGREEMENTS", "UNTRUSTED_TOOL_DATA_CONTRIBUTION_ID"},
        "runner": {"RunnerServices"},
    },
    # Binding reads each route's release-owned hosted retrieval table.
    ("plakous-hosted", "src/tool_plan.rs"): {
        "model": {
            "HostedRetrievalSupport",
            "HostedRetrievalTable",
            "HostedRetrievalTool",
            "HostedRetrievalUnsupported",
        },
        # The MCP records a root's frozen selection carries to its bind.
        "mcp": {"McpToolRecord"},
        # The deferred ports the session's executor connects to, the bounds
        # a planned fetch_url runs under, and the retrieval options a route
        # must serve.
        "tools": {
            "DEFAULT_WEB_SEARCH_MAX_USES",
            "FetchUrlLimits",
            "ImageGenFuture",
            "ImageGenPort",
            "ImageGenRequest",
            "OperationContext",
            "RetrievalError",
            "RetrievalErrorKind",
            "RetrievalFuture",
            "RetrievalKind",
            "RetrievalOptions",
            "RetrievalRequest",
        },
    },
    # The Anthropic, OpenAI and xAI API executors connect their side-request
    # and image ports.
    ("plakous-hosted", "src/platform/anthropic.rs"): {
        "anthropic": {"AnthropicRetrievalPort"},
    },
    ("plakous-hosted", "src/platform/openai.rs"): {
        "openai": {"OpenAiImagePort", "OpenAiRetrievalPort", "lowest_responses_effort"},
    },
    ("plakous-hosted", "src/platform/xai.rs"): {
        "xai": {"XaiImagePort", "XaiRetrievalPort", "lowest_responses_effort"},
    },
    # The OpenRouter executor connects its side-request port.
    ("plakous-hosted", "src/openrouter.rs"): {
        "openrouter": {"OpenRouterRetrieval", "OpenRouterRetrievalPort"},
        "runner": {"RunnerServices"},
    },
    # The frozen [tools.fetch_url], [tools.web_search] and [tools.x_search]
    # tables read into the tools' bounds and options.
    ("plakous-hosted", "src/frozen_tables.rs"): {
        "tools": {"FetchUrlLimits", "RetrievalOptions"},
    },
    # The compile-time check that the host release timeout covers the store deadline.
    ("plakous-hosted", "src/lib.rs"): {"agent": {"DEFAULT_STORE_OPERATION_TIMEOUT"}},
    ("plakd", "src/commands/reload_rehearsal.rs"): {
        "sessions": {"rehearsal", "inspect_sqlite", "OfflineReopenOutcome"},
    },
    # A host names who holds a session another process has open (spec 02
    # §4.6): the runtime store's holder record, read-only.
    ("plakd", "src/store/holders.rs"): {
        "sessions": {"holder::BranchHolder", "holder::sqlite_branch_holder"},
    },
    ("plakous-hosted", "src/capabilities.rs"): {
        "model": {"HostedRetrievalTable"},
    },
    ("plakous-hosted", "src/capabilities/anthropic.rs"): {
        "model": {"HostedRetrievalTable"},
        "anthropic": {"ANTHROPIC_API_HOSTED_RETRIEVAL"},
    },
    ("plakous-hosted", "src/capabilities/claude.rs"): {
        "model": {"HostedRetrievalTable"},
    },
    ("plakous-hosted", "src/capabilities/codex.rs"): {
        "model": {"HostedRetrievalTable"},
    },
    ("plakous-hosted", "src/capabilities/grok.rs"): {
        "model": {"HostedRetrievalTable"},
        "xai": {"SUPERGROK_ALLOW_TOOL_RESULT_IMAGES"},
    },
    ("plakous-hosted", "src/capabilities/openai.rs"): {
        "model": {"HostedRetrievalTable"},
        "openai": {"OPENAI_API_HOSTED_RETRIEVAL"},
    },
    ("plakous-hosted", "src/capabilities/openrouter.rs"): {
        "model": {"HostedRetrievalTable"},
        "openrouter": {"OPENROUTER_HOSTED_RETRIEVAL"},
    },
    ("plakous-hosted", "src/capabilities/xai.rs"): {
        "model": {"HostedRetrievalTable"},
        "xai": {"XAI_API_HOSTED_RETRIEVAL"},
    },
    ('plakd', 'src/host.rs'): {'runner': {'RunnerStatus'}},
    ('plakd', 'src/host/sessions/options.rs'): {'runner': {'RunnerRow', 'WorkspaceCatalog'}},
    ('plakd', 'src/host/sessions/runner_views.rs'): {
        'runner': {
            'PlacementFailure',
            'ProbePhase',
            'RepositoryHead',
            'RunnerRow',
            'RunnerStatus',
            'WorkspaceRefusal',
        },
    },
    ('plakd', 'src/test_hooks/disabled.rs'): {
        'runner': {
            'RunnerServices',
            'RunnerLeaseClock',
            'ClientWindows',
            'RunnerClient',
            'PlacementFailure',
        },
        'tools': {'runner::RunnerLink'},
    },
    ('plakd', 'src/test_hooks/runner.rs'): {
        'runner': {
            'RunnerLeaseClock',
            'ClientWindows',
            'RunnerClient',
            'PlacementFailure',
            'ProbePhase',
        },
    },
    ('plakd', 'src/test_hooks/runner/link.rs'): {
        'runner': {'RunnerClient'},
        'tools': {
            'LineOwner',
            'runner::RunnerCall',
            'runner::RunnerCallFuture',
            'runner::RunnerFailure',
            'runner::RunnerLink',
            'runner::RunnerOpenFuture',
        },
    },
    ('plakd', 'src/commands/runner/registry.rs'): {
        'runner': {
            'AdmissionSnapshot',
            'ConfiguredRunner',
            'MachineOverlapGuard',
            'PlacementFailure',
            'ProbePhase',
            'RunnerPin',
            'WorkspaceCatalog',
            'WorkspaceOptions',
            'machine_id',
            'protocol',
        }
    },
    ('plakd', 'src/commands/runner/home.rs'): {'runner': {'daemon', 'protocol'}},
    ('plakd', 'src/commands/runner/services.rs'): {
        'sessions': {
            'SessionLocator',
            'LeaseOnlyHead',
            'SessionOperationId',
            'AgentSessionsErrorKind',
        },
        'runner': {
            'PlacementFailure',
            'ProbePhase',
            'RunnerClient',
            'RunnerRegistry',
            'RunnerServices',
            'RunnerStatus',
            'WorkspaceCatalog',
            'protocol',
        },
        'tools': {'runner::RunnerBackend'},
    },
    ('plakd', 'src/commands/runner/environment.rs'): {
        'runner': {'RunnerClient'},
        'tools': {'runner::LocalDaemonBackend'},
    },
    ('plakd', 'src/commands/runner/services/host.rs'): {
        'runner': {'PlacementFailure', 'ProbePhase', 'WorkspaceCatalog'}
    },
    ('plakous-hosted', 'src/skills.rs'): {'runner': {'RunnerServices'}},
    # A node's explicit plan and prompt name the backend its tools run on.
    ('plakous-hosted', 'src/agent/build/policy.rs'): {
        'runner': {'RunnerServices'},
        'tools': {'ExecutionBackendDescriptor'},
    },
    ('plakous-hosted', 'src/agent/build/profile.rs'): {'runner': {'RunnerServices'}},
    ('plakous-hosted', 'src/agent/build/topology.rs'): {'runner': {'RunnerServices'}},
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
# recorded in a persisted projection, an enrolled-candidate table, or the
# original target of an operator remedy. None decides provider behavior.
PRODUCT_ACCOUNT_NAME_EXEMPTIONS: dict[SourceKey, set[str]] = {
    # Persisted notice remedies preserve the exact original account identity.
    ("plakous-host-protocol", "src/notice_remedy.rs"): {
        "anthropic",
        "chatgpt",
        "claude",
        "codex",
        "grok",
        "openrouter",
        "openai",
        "supergrok",
        "xai",
        "xai-oauth",
    },
    # `CodingPresentation::{Codex, Grok, Claude}` is the model vocabulary --
    # which tool dialect a model is shown. It is irreducibly named after the
    # vendor whose dialect it reproduces.
    ("plakous-tools", "src/coding_presentation.rs"): {"codex", "grok", "claude"},
    # `ToolProjectionLoweringV2::{CodexExecCommand, GrokRunTerminalCommand,
    # ClaudeBash, ...}` is durable: a persisted projection descriptor records
    # the lowering identity, so the variant names are wire format. Lowering and
    # raising match those identities to produce each vocabulary's shapes. The
    # `grok` and `claude` module names are those same identities.
    ("plakous-tools", "src/exposure/projection.rs"): {"codex", "grok", "claude"},
    ("plakous-tools", "src/exposure/lowering.rs"): {"claude"},
    ("plakous-tools", "src/exposure/raising.rs"): {"claude", "grok"},
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
RUST_IDENTIFIER = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*")


class RustToken(NamedTuple):
    """One significant Rust token; comments and test-only modules are absent."""

    kind: str
    value: str


_blank_rust_non_code = lru_cache(maxsize=None)(blank_rust_non_code)


def cfg_implies_test(predicate: str) -> bool:
    """Conservatively prove that a masked cfg predicate requires `test`."""
    predicate = predicate.strip()
    if predicate == "test":
        return True
    match = re.fullmatch(r"(all|any)\s*\((.*)\)", predicate, re.DOTALL)
    if match is None:
        return False
    children: list[str] = []
    depth = 0
    start = 0
    body = match.group(2)
    for index, character in enumerate(body):
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
            if depth < 0:
                return False
        elif character == "," and depth == 0:
            children.append(body[start:index])
            start = index + 1
    if depth:
        return False
    if body[start:].strip():
        children.append(body[start:])
    implied = [cfg_implies_test(child) for child in children]
    return any(implied) if match.group(1) == "all" else bool(implied) and all(implied)


def _block_end(masked: str, opening: int) -> int:
    left = masked[opening]
    right = {"{": "}", "(": ")", "[": "]"}[left]
    depth = 0
    for index in range(opening, len(masked)):
        if masked[index] == left:
            depth += 1
        elif masked[index] == right:
            depth -= 1
            if depth == 0:
                return index + 1
    return len(masked)


@lru_cache(maxsize=None)
def production_rust_source(source: str) -> str:
    """Blank test-only items without truncating later production items."""
    masked = _blank_rust_non_code(source)
    spans: list[tuple[int, int]] = []
    cursor = 0
    while match := CFG_TEST_BLOCK_ITEM.search(masked, cursor):
        # A production gate may precede a test gate on the same item.
        cursor = match.start() + 1
        if not any(
            cfg_implies_test(gate.group("predicate"))
            for gate in CFG_ATTRIBUTE.finditer(match.group("attributes"))
        ):
            continue
        closing = _block_end(masked, match.end() - 1)
        spans.append((match.start(), closing))
        cursor = closing
    cursor = 0
    while match := CFG_TEST_SEMICOLON_ITEM.search(masked, cursor):
        cursor = match.start() + 1
        if not any(
            cfg_implies_test(gate.group("predicate"))
            for gate in CFG_ATTRIBUTE.finditer(match.group("attributes"))
        ):
            continue
        cursor = match.end()
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
    # own mutable list. Caches are process-local and exact-content keyed.
    return list(_cached_rust_tokens(source))


def all_rust_tokens(source: str) -> list[RustToken]:
    """Lex Rust including test-only items; comments are still absent."""
    return list(_cached_all_rust_tokens(source))


@lru_cache(maxsize=None)
def _cached_rust_tokens(source: str) -> tuple[RustToken, ...]:
    return _cached_all_rust_tokens(production_rust_source(source))


@lru_cache(maxsize=None)
def _cached_all_rust_tokens(source: str) -> tuple[RustToken, ...]:
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

        raw = RAW_STRING_START.match(source, index) if source[index] in "br" else None
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
            tokens.append(RustToken("string", _decode_rust_string(source[content_start:end])))
            index = min(end + 1, len(source))
            continue

        character = RUST_CHARACTER.match(source, index) if source[index] in "b'" else None
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

    `use crate::{presentation::X, shell::Y}` is two paths, not one flattened path: the
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


def _inline_module_paths(
    tokens: list[RustToken], spans: list[tuple[int, int]]
) -> set[tuple[int, tuple[str, ...]]]:
    """Every `crate::a::b` and `super::a::b` path written outside a `use`
    tree, as far as its identifiers go (a generic argument, a call or a
    field ends it), with the index of its first token."""
    paths: set[tuple[int, tuple[str, ...]]] = set()
    for index, token in enumerate(tokens):
        if token.kind != "identifier" or token.value not in {"crate", "super"}:
            continue
        if any(start <= index < end for start, end in spans):
            continue
        if index > 0 and tokens[index - 1].value == "::":
            continue
        names = [token.value]
        cursor = index + 1
        while (
            cursor + 1 < len(tokens)
            and tokens[cursor].value == "::"
            and tokens[cursor + 1].kind == "identifier"
        ):
            names.append(tokens[cursor + 1].value)
            cursor += 2
        if len(names) > 1:
            paths.add((index, tuple(names)))
    return paths


def _inline_modules(tokens: list[RustToken]) -> list[tuple[str, ...]]:
    """For each token, the inline `mod name { ... }` blocks it sits in,
    outermost first."""
    stack: list[tuple[str, int]] = []
    depth = 0
    pending: str | None = None
    names: list[tuple[str, ...]] = []
    for index, token in enumerate(tokens):
        if token.value == "{":
            depth += 1
            if pending is not None:
                stack.append((pending, depth))
                pending = None
        elif token.value == "}":
            if stack and stack[-1][1] == depth:
                stack.pop()
            depth = max(0, depth - 1)
        elif (
            token == RustToken("identifier", "mod")
            and index + 2 < len(tokens)
            and tokens[index + 1].kind == "identifier"
            and tokens[index + 2].value == "{"
        ):
            pending = tokens[index + 1].value
        names.append(tuple(name for name, _ in stack))
    return names


def _file_module_path(path: str) -> list[str]:
    """The module a source file is, as `crate` path segments:
    `src/shell/settings.rs` is `crate::shell::settings`, a `mod.rs` its
    directory's module."""
    parts = path.removeprefix("src/").removesuffix(".rs").split("/")
    if parts[-1] in {"mod", "lib", "main"}:
        parts.pop()
    return ["crate", *parts]


def _resolve_module_path(names: tuple[str, ...] | list[str], module: list[str]) -> str | None:
    """`names` as a `crate::` path, its leading `super`s taken against the
    file's `module`; `None` when the chain climbs above the crate."""
    if names[0] == "crate":
        return "::".join(names)
    base = list(module)
    rest = list(names)
    while rest and rest[0] == "super":
        if len(base) == 1:
            return None
        base.pop()
        rest.pop(0)
    return "::".join(base + rest)


def _identifier_values(tokens: list[RustToken]) -> list[str]:
    return [token.value for token in tokens if token.kind == "identifier"]


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
    """Find facade symbols outside the package's approved `plakous::` surface.

    An approved `a::b` grants the one item `b` of the facade module's
    submodule `a`.
    """
    unapproved: set[str] = set()
    for statement in _use_statements(tokens):
        for names in _use_tree_paths(statement):
            if not names or names[0] != "plakous":
                continue
            if len(names) < 2 or names[1] not in allowed_symbols:
                unapproved.add("::".join(names[:2]))
                continue
            module = names[1]
            allowed = allowed_symbols[module]
            index = 2
            while index < len(names):
                name = names[index]
                if index + 1 < len(names) and f"{name}::{names[index + 1]}" in allowed:
                    index += 2
                    continue
                if name not in allowed:
                    reported = _unapproved_name(module, allowed, names[index : index + 2])
                    unapproved.add(reported)
                    index += reported.count("::")
                    continue
                index += 1

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
        if not (
            index + 4 < len(tokens)
            and tokens[index + 3].value == "::"
            and tokens[index + 4].kind == "identifier"
        ):
            continue
        item = tokens[index + 4].value
        if item in allowed_symbols[module]:
            continue
        if (
            index + 6 < len(tokens)
            and tokens[index + 5].value == "::"
            and tokens[index + 6].value == "{"
        ):
            # A use tree: the use-statement walk above checks each path.
            continue
        path = [item]
        if (
            index + 6 < len(tokens)
            and tokens[index + 5].value == "::"
            and tokens[index + 6].kind == "identifier"
        ):
            path.append(tokens[index + 6].value)
            if "::".join(path) in allowed_symbols[module]:
                continue
        unapproved.add(_unapproved_name(module, allowed_symbols[module], path))
    return unapproved


def _unapproved_name(module: str, allowed: set[str], path: list[str]) -> str:
    """Name an unapproved facade item, down to the item of a submodule whose
    items are approved one by one."""
    if len(path) > 1 and any(symbol.startswith(f"{path[0]}::") for symbol in allowed):
        return "::".join([module, *path[:2]])
    return f"{module}::{path[0]}"


def _environment_path_names(tokens: list[RustToken]) -> set[str]:
    return {
        tokens[index].value
        for index in range(2, len(tokens))
        if tokens[index].kind == "identifier"
        and tokens[index].value in {"var", "var_os", "vars", "vars_os"}
        and tokens[index - 1].value == "::"
        and tokens[index - 2] == RustToken("identifier", "env")
    }


def validate_claude_oauth_state_comparison(source: str) -> list[str]:
    """Pin the production callback rejection to subtle's constant-time comparison."""
    tokens = rust_tokens(source)
    try:
        function = next(
            index
            for index, token in enumerate(tokens[:-1])
            if token.value == "fn"
            and tokens[index + 1].value == "parse_claude_oauth_authorization_input"
        )
        opening = next(
            index for index in range(function, len(tokens)) if tokens[index].value == "{"
        )
        body = tokens[opening : _token_group_end(tokens, opening)]
    except StopIteration:
        body = []
    rejection = rust_tokens("""
        if let Some(state) = state
            && !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes()))
        { return None; }
    """)
    if any(body[index : index + len(rejection)] == rejection for index in range(len(body))):
        return []
    return [
        "plakous-anthropic:src/oauth.rs: callback state must reject through the pinned constant-time ct_eq comparison"
    ]


def validate_product_session_authority_sources(package: str, sources: dict[str, str]) -> list[str]:
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


def validate_product_update_hook_sources(sources: dict[str, str]) -> list[str]:
    """Allow only the submission invalidator and its exclusive trace alternative."""
    update_hooks: list[str] = []
    trace_hooks: list[str] = []
    update = RustToken("identifier", "update_hook")
    trace = rust_tokens("commit_trace::install")
    for path, source in sorted(sources.items()):
        tokens = rust_tokens(source)
        update_hooks.extend(path for token in tokens if token == update)
        trace_hooks.extend(
            path for index in range(len(tokens)) if tokens[index : index + len(trace)] == trace
        )
    errors: list[str] = []
    if update_hooks != ["src/store/views/submissions.rs"]:
        errors.append(
            f"plakd: one update hook belongs to the submission index, found {update_hooks!r}"
        )
    if trace_hooks != ["src/store/mod.rs"]:
        errors.append(
            f"plakd: only the product store opener may install the trace, found {trace_hooks!r}"
        )
    return errors


# Where the product home may be discovered from the process environment.
#
# `PlakPaths::from_env()` reads `$PLAK_HOME`/XDG: it is a composition-root act,
# and a library that performs it has decided for its caller which home the
# session belongs to. Only `bin/plakd`, the composition root, may discover
# the product home; library callers must supply it explicitly.
PRODUCT_HOME_DISCOVERY: str = "from_env"
PRODUCT_HOME_DISCOVERY_ALLOWED_PREFIXES: tuple[tuple[str, str], ...] = (("plakd", ""),)


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
    """Enforce one host/bin package's reviewed process and environment sites.

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
        errors.append(f"plakd CLI contains forbidden delegated-login symbol: {symbol}")
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
    for value in sorted(FORBIDDEN_CLI_STRINGS & strings):
        errors.append(f"plakd CLI contains forbidden delegated-login option: {value}")
    for value in strings:
        lowered = value.lower().replace("\\", "/")
        if re.search(r"(?:^|/)\.(?:codex)(?:/|$)|(?:^|/)opencode(?:/|$)", lowered):
            errors.append("plakd CLI contains forbidden delegated-login storage/owner locator")
            break
    if any(re.search(r"(?:^|/)proc/+self/+environ(?:$|/)", value.lower()) for value in strings):
        errors.append("plakd CLI contains forbidden /proc/self/environ access")

    for path, tokens in tokens_by_file.items():
        site = (package, path)
        process_import = command_import = False
        for statement in _use_statements(tokens):
            names = _identifier_values(statement)
            if "env" in names and (
                names != ["std", "env"] or any(token.value in {"*", "as"} for token in statement)
            ):
                errors.append("plakd CLI must import std::env as a module, not ambient readers")
            if "Command" in names and "plakous" in names:
                errors.append("plakd CLI imports a forbidden Command process capability")
            for names in _use_tree_paths(statement):
                if names[:2] == ["std", "process"]:
                    process_import = True
                    command_import |= len(names) == 2 or names[2] in {"Command", "self", "*"}
        if site not in PROCESS_SITES and (
            command_import
            or (process_import and site not in NON_LAUNCH_PROCESS_SITES)
            or any(
                [t.value for t in tokens[i : i + 7]]
                == ["std", "::", "process", "::", "Command", "::", "new"]
                for i, token in enumerate(tokens)
                if token == RustToken("identifier", "std")
            )
        ):
            errors.append(f"{package}:{path}: std::process is outside the reviewed process sites")

        source_symbols = ALLOWED_PLAKOUS_SYMBOLS_BY_SOURCE.get(site, {})
        file_symbols = {
            module: allowed_symbols.get(module, set()) | source_symbols.get(module, set())
            for module in allowed_symbols.keys() | source_symbols.keys()
        }
        unapproved_plakous = _unapproved_plakous_symbols(tokens, file_symbols)
        if unapproved_plakous:
            errors.append(
                f"{package}:{path}: plakd CLI uses unapproved plakous facade symbols: "
                f"{sorted(unapproved_plakous)!r}"
            )
        foreign = _foreign_function_names(tokens) & FORBIDDEN_CLI_FOREIGN_FUNCTIONS
        if foreign:
            errors.append(
                f"{package}:{path}: plakd CLI declares forbidden foreign environment/process functions: "
                f"{sorted(foreign)!r}"
            )

        environment_paths = _environment_path_names(tokens)
        if environment_paths & {"vars", "vars_os"}:
            errors.append(f"{package}:{path}: plakd CLI contains ambient environment iteration")
        if environment_paths & {"var", "var_os"} and site not in ENVIRONMENT_READER_SITES:
            errors.append(f"{package}:{path}: environment reader outside the reviewed sites")
        if site not in COMPILE_TIME_ENVIRONMENT_SITES and any(
            token.kind == "identifier"
            and token.value in {"env", "option_env"}
            and tokens[index + 1].value == "!"
            for index, token in enumerate(tokens[:-1])
        ):
            errors.append(f"{package}:{path}: compile-time environment outside the reviewed sites")

        # Any mention but a method call (`Command::current_dir` sets a child's
        # directory): a path call, an import, an alias or a re-export.
        working_directory = {
            tokens[index].value
            for index in range(len(tokens))
            if tokens[index].kind == "identifier"
            and tokens[index].value in {"current_dir", "set_current_dir"}
            and (index == 0 or tokens[index - 1].value != ".")
        }
        if "set_current_dir" in working_directory:
            errors.append(f"{package}:{path}: set_current_dir is forbidden")
        if "current_dir" in working_directory and site not in WORKING_DIRECTORY_SITES:
            errors.append(
                f"{package}:{path}: working-directory reads outside the CLI edge; "
                "below it a workspace is explicit"
            )
        # Every call or path to `launch_directory` (not a field of that
        # name), and every `Home::from_env` / `Services::from_env`.
        launch_readers = {
            "launch_directory"
            if tokens[index].value == "launch_directory"
            else f"{tokens[index - 2].value}::from_env"
            for index in range(len(tokens))
            if tokens[index].kind == "identifier"
            and (
                (
                    tokens[index].value == "launch_directory"
                    and (index == 0 or tokens[index - 1].value not in {"fn", "."})
                    and (
                        (index > 0 and tokens[index - 1].value == "::")
                        or (index + 1 < len(tokens) and tokens[index + 1].value == "(")
                    )
                )
                or (
                    tokens[index].value == "from_env"
                    and index >= 2
                    and tokens[index - 1].value == "::"
                    and tokens[index - 2].value in {"Home", "Services"}
                )
            )
        }
        renamed = sorted(
            {
                tokens[index].value
                for index in range(len(tokens) - 1)
                if tokens[index].kind == "identifier"
                and tokens[index].value in LAUNCH_DIRECTORY_NAMES
                and tokens[index + 1] == RustToken("identifier", "as")
            }
        )
        if renamed:
            errors.append(
                f"{package}:{path}: {renamed!r} imported under another name; the "
                "launch-directory readers keep their names so the composition-root pin sees them"
            )
        unexpected = sorted(name for name in launch_readers if LAUNCH_DIRECTORY_SITES[name] != site)
        if unexpected:
            errors.append(
                f"{package}:{path}: launch-directory readers {unexpected!r} outside their "
                "reviewed composition-root files"
            )

        allowed_macros = ALLOWED_CLI_MACROS | ALLOWED_CLI_MACROS_BY_SOURCE.get(site, set())
        if site in COMPILE_TIME_ENVIRONMENT_SITES:
            allowed_macros |= {"env", "option_env"}
        macro_names = {
            token.value
            for index, token in enumerate(tokens[:-2])
            if token.kind == "identifier"
            and token.value not in _NOT_MACRO_KEYWORDS
            and tokens[index + 1].value == "!"
            and tokens[index + 2].value in {"(", "[", "{"}
        }
        for macro in sorted(macro_names - allowed_macros):
            errors.append(
                f"{package}:{path}: unapproved macro invocation cannot cross CLI boundary: {macro}!"
            )

    return errors


class _SourceScan:
    """Filesystem snapshots belong to one checker invocation, never a later run."""

    def __init__(self, receipts: Receipts | None) -> None:
        self.receipts = receipts
        self.bytes: dict[Path, bytes] = {}
        self.text: dict[Path, str] = {}
        self.discoveries: dict[
            tuple[Path, tuple[tuple[Path, bool], ...]],
            tuple[tuple[Path, ...], tuple[str, ...]],
        ] = {}


_source_scan: _SourceScan | None = None


@contextmanager
def source_scan(receipts: Receipts | None = None) -> Iterator[None]:
    global _source_scan
    previous = _source_scan
    _source_scan = _SourceScan(receipts)
    try:
        yield
    finally:
        _source_scan = previous
        for cached in (
            _blank_rust_non_code,
            production_rust_source,
            _cached_rust_tokens,
            _cached_all_rust_tokens,
            _cached_test_module_targets,
            _cached_test_module_files,
        ):
            cached.cache_clear()


def source_check(
    rule: str, sources: dict[str, str], check: Callable[[], list[str]], *policy: object
) -> list[str]:
    receipts = _source_scan.receipts if _source_scan is not None else None
    if receipts is None:
        return check()
    return receipts.check(
        check, rule, [(path, receipts.source(data)) for path, data in sources.items()], *policy
    )


def each_source(
    validator: Callable[..., list[str]],
    package: str,
    sources: dict[str, str],
    *policy: Any,
) -> list[str]:
    # Only use this for validators whose file results are independent.
    errors: list[str] = []
    for path, data in sources.items():
        one = {path: data}
        errors.extend(
            source_check(
                validator.__name__, one, lambda: validator(package, one, *policy), package, *policy
            )
        )
    return errors


def _read_rust_bytes(path: Path) -> bytes:
    path = Path(os.path.abspath(path))
    if _source_scan is None:
        return path.read_bytes()
    if path not in _source_scan.bytes:
        _source_scan.bytes[path] = path.read_bytes()
    return _source_scan.bytes[path]


def read_rust_source(path: Path) -> str:
    path = Path(os.path.abspath(path))
    if _source_scan is None:
        return path.read_text(encoding="utf-8")
    if path not in _source_scan.text:
        with io.TextIOWrapper(io.BytesIO(_read_rust_bytes(path)), encoding="utf-8") as stream:
            _source_scan.text[path] = stream.read()
    return _source_scan.text[path]


def discover_rust_files(
    repository: Path,
    targets: list[tuple[Path, bool]],
) -> tuple[list[Path], list[str]]:
    key = (Path(os.path.abspath(repository)), tuple(targets))
    if _source_scan is None:
        return _discover_rust_files(repository, targets)
    if key not in _source_scan.discoveries:
        files, errors = _discover_rust_files(repository, targets)
        _source_scan.discoveries[key] = (tuple(files), tuple(errors))
    files, errors = _source_scan.discoveries[key]
    return list(files), list(errors)


def _discover_rust_files(
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
                    errors.append(f"production Rust source is a symbolic link: {label}")
                elif path.is_file():
                    files.add(path)
                else:
                    errors.append(f"production Rust source is not a regular file: {label}")

    pending = list(files)
    inspected: set[Path] = set()

    def resolve_explicit_target(source: Path, path_value: str, kind: str) -> None:
        if not path_value or "\0" in path_value or "\\" in path_value:
            errors.append(f"production Rust source has invalid {kind} target: {relative(source)}")
            return
        target = Path(os.path.abspath(source.parent / path_value))
        label = relative(target) or str(target)
        if relative(target) is None:
            errors.append(f"production Rust {kind} target escapes repository: {label}")
        elif has_symlink(target):
            errors.append(f"production Rust {kind} target traverses a symbolic link: {label}")
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
            source_text = read_rust_source(source)
        except OSError as error:
            errors.append(f"cannot inspect production Rust source {relative(source)}: {error}")
            continue
        has_explicit_source_target = (
            re.search(r"#\s*\[\s*path\s*=", source_text) is not None
            or "cfg_attr" in source_text
            or re.search(r"\binclude(?:_str)?\s*!", source_text) is not None
        )
        if source not in module_discovery_sources and not has_explicit_source_target:
            continue
        # An inline module's path attribute names a directory, not a source file.
        discovery_source = list(production_rust_source(source_text))
        masked = _blank_rust_non_code("".join(discovery_source))
        for module in MODULE_ITEM.finditer(masked):
            if module.group("body") != "{":
                continue
            for attribute in re.finditer(r"#\s*\[\s*path\s*=[^\]]*\]", module.group("attributes")):
                offset = module.start("attributes")
                for index in range(offset + attribute.start(), offset + attribute.end()):
                    discovery_source[index] = " "
        tokens = rust_tokens("".join(discovery_source))
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
                    errors.append(f"production Rust module target escapes repository: {label}")
                elif has_symlink(target):
                    errors.append(
                        f"production Rust module target traverses a symbolic link: {label}"
                    )
                elif not target.is_file():
                    errors.append(f"production Rust module target is not a regular file: {label}")
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
                    if cursor + 2 >= len(tokens) or tokens[cursor + 2].kind != "string":
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
                tokens[index].kind == "identifier"
                and tokens[index].value in {"include", "include_str"}
                and tokens[index + 1].value == "!"
                and tokens[index + 2].value in {"(", "[", "{"}
            ):
                continue
            closing = {"(": ")", "[": "]", "{": "}"}[tokens[index + 2].value]
            cursor = index + 3
            if tokens[index].value == "include_str" and (
                cursor >= len(tokens)
                or tokens[cursor].kind != "string"
                or Path(tokens[cursor].value).suffix != ".rs"
            ):
                continue
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


class ModuleTarget(NamedTuple):
    path: Path
    directory: Path | None
    test_only: bool
    redirected: bool


def test_module_targets(
    path: Path,
    source: str,
    crate_root: bool,
) -> list[ModuleTarget]:
    """Resolve module edges, retaining test gates and Rust's child directory.

    Path attributes inside inline modules have different Rust resolution rules;
    leave those counted rather than guessing a target and hiding production.
    """
    return list(_cached_test_module_targets(path, source, crate_root))


@lru_cache(maxsize=None)
def _cached_test_module_targets(
    path: Path,
    source: str,
    crate_root: bool,
) -> tuple[ModuleTarget, ...]:
    if "mod" not in source:
        return ()
    targets: list[ModuleTarget] = []
    masked = _blank_rust_non_code(source)
    base = path.parent
    if not crate_root and path.name != "mod.rs":
        base /= path.stem
    macros = [
        (match.start(), _block_end(masked, match.end() - 1))
        for match in re.finditer(r"\bmacro_rules\s*!\s*(?:r#)?\w+\s*[{([]", masked)
    ]
    inline: list[tuple[int, int, str, bool, bool]] = []
    for match in MODULE_ITEM.finditer(masked):
        if any(start <= match.start() < end for start, end in macros):
            continue
        ancestors = [span for span in inline if span[0] <= match.start() < span[1]]
        attributes = match.group("attributes")
        gated = any(span[3] for span in ancestors) or any(
            cfg_implies_test(gate.group("predicate")) for gate in CFG_ATTRIBUTE.finditer(attributes)
        )
        has_path = re.search(r"\bpath\s*=", attributes) is not None
        name = match.group("name")
        if match.group("body") == "{":
            inline.append((match.end(), _block_end(masked, match.end() - 1), name, gated, has_path))
            continue
        if any(span[4] for span in ancestors):
            continue
        redirect = re.search(r"#\s*\[\s*path\s*=[^\]]*\]", attributes)
        if redirect is not None:
            if ancestors:
                continue
            offset = match.start("attributes")
            tokens = rust_tokens(source[offset + redirect.start() : offset + redirect.end()])
            if len(tokens) != 6 or tokens[4].kind != "string":
                continue
            target = Path(os.path.normpath(path.parent / tokens[4].value))
            targets.append(
                ModuleTarget(
                    target, target.parent if target.name == "mod.rs" else None, gated, True
                )
            )
        elif has_path:
            # A conditional path override is not a default-path declaration.
            continue
        else:
            directory = base.joinpath(*(span[2] for span in ancestors), name)
            targets.extend(
                (
                    ModuleTarget(directory.with_suffix(".rs"), directory, gated, False),
                    ModuleTarget(directory / "mod.rs", directory, gated, False),
                )
            )
    return tuple(targets)


def explicit_rust_source_targets(path: Path, source: str) -> set[Path]:
    """Literal source references that must keep a production file counted."""
    targets: set[Path] = set()
    if "path" not in source and "include" not in source:
        return targets
    tokens = rust_tokens(source)
    for index in range(len(tokens) - 2):
        if (
            tokens[index] == RustToken("identifier", "path")
            and tokens[index + 1].value == "="
            and tokens[index + 2].kind == "string"
        ):
            targets.add(Path(os.path.normpath(path.parent / tokens[index + 2].value)))
        if (
            index + 3 < len(tokens)
            and tokens[index].value in {"include", "include_str"}
            and tokens[index + 1].value == "!"
            and tokens[index + 2].value in {"(", "[", "{"}
            and tokens[index + 3].kind == "string"
        ):
            target = Path(os.path.normpath(path.parent / tokens[index + 3].value))
            if target.suffix == ".rs":
                targets.add(target)
    return targets


def _test_module_files(files: dict[Path, str], crate_roots: set[Path]) -> set[Path]:
    return set(_cached_test_module_files(tuple(files.items()), frozenset(crate_roots)))


@lru_cache(maxsize=None)
def _cached_test_module_files(
    entries: tuple[tuple[Path, str], ...],
    crate_roots: frozenset[Path],
) -> frozenset[Path]:
    """Follow test module edges, then restore every production-reachable file."""
    files = dict(entries)
    excluded: set[Path] = set()
    path_loaded = {
        target.path
        for path, source in files.items()
        for target in test_module_targets(path, source, path in crate_roots)
        if target.redirected
    }
    edges = {
        path: test_module_targets(path, source, path in crate_roots or path in path_loaded)
        for path, source in files.items()
    }
    descendants: dict[Path, set[Path]] = {}
    for path in files:
        for parent in (path, *path.parents):
            descendants.setdefault(parent, set()).add(path)
    references = {
        path: explicit_rust_source_targets(path, source) for path, source in files.items()
    }
    while True:
        previous = excluded.copy()
        for path, targets in edges.items():
            for target in targets:
                if target.path not in files:
                    continue
                if not target.test_only and path not in excluded:
                    continue
                excluded.add(target.path)
                if target.directory is not None:
                    excluded.update(descendants.get(target.directory, ()))
        if previous == excluded:
            break

    # Shared files are production even if another root reaches them only in tests.
    excluded.difference_update(crate_roots)
    while True:
        restored: set[Path] = set()
        for path in files.keys() - excluded:
            restored.update(references[path])
            restored.update(target.path for target in edges[path] if not target.test_only)
        restored.intersection_update(excluded)
        if not restored:
            return frozenset(excluded)
        excluded.difference_update(restored)


def cfg_test_module_files(
    sources: Iterable[Path],
    crate_roots: set[Path],
) -> set[Path]:
    """Exclude module files reachable only from test code."""
    files: dict[Path, str] = {}
    for source in sources:
        try:
            files[source] = read_rust_source(source)
        except OSError:
            continue
    return _test_module_files(files, crate_roots)


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
    crate_roots = {Path(os.path.abspath(repository / target)) for target, _ in targets}
    test_only = cfg_test_module_files(sources, crate_roots)
    sources = [
        source
        for source in sources
        if source not in test_only and "product_crash_tests" not in source.parts
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
        if name == VENDOR_PACKAGE:
            unsafe_code = manifest.get("lints", {}).get("rust", {}).get("unsafe_code")
            if unsafe_code != "forbid":
                errors.append(f'{name}: unsafe_code must be explicitly "forbid"')
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


IMAGE_LEAF = "plakous-image"


def validate_image_leaf(workspace: Workspace) -> list[str]:
    """Keep `plakous-image` a workspace leaf that L0 links without features.

    `plakous-tools` reaches it under a recorded core edge exception. The
    exception stays harmless only while the leaf names no workspace package
    and the tool contracts take it without default features or any feature,
    so the decoder and anything a later edit adds stay out of L0's closure.
    """
    image = workspace.packages.get(IMAGE_LEAF)
    tools = workspace.packages.get("plakous-tools")
    if image is None or tools is None:
        return [f"{IMAGE_LEAF} and plakous-tools must both be workspace members"]
    errors = [
        f"{IMAGE_LEAF}: depends on workspace package {dependency['name']!r}; "
        "the image leaf names no workspace package"
        for dependency in image.metadata.get("dependencies", [])
        if dependency["name"] in workspace.packages
    ]
    edges = [
        dependency
        for dependency in tools.metadata.get("dependencies", [])
        if dependency["name"] == IMAGE_LEAF
    ]
    if len(edges) != 1:
        errors.append(f"plakous-tools: expected exactly one {IMAGE_LEAF} dependency")
    for dependency in edges:
        if (
            dependency.get("kind") is not None
            or dependency.get("target") is not None
            or dependency.get("optional")
            or dependency.get("uses_default_features")
            or dependency.get("features")
        ):
            errors.append(
                f"plakous-tools: {IMAGE_LEAF} must be a plain normal dependency with "
                "default-features = false and no features"
            )
    features = tools.manifest.get("features", {})
    for feature, members in sorted(features.items() if isinstance(features, dict) else []):
        for member in members if isinstance(members, list) else []:
            target = str(member).split("/", 1)[0].removeprefix("dep:").rstrip("?")
            if target == IMAGE_LEAF:
                errors.append(
                    f"plakous-tools: feature {feature!r} enables {member!r}; the tool "
                    f"contracts take {IMAGE_LEAF} without features"
                )
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
            if (
                index == 0
                or statement[index - 1].value in {"{", ","}
                and (index < 2 or statement[index - 2].value != "::")
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
    package: Package,
    sources: dict[str, str],
    rules: dict[str, dict[str, Any]],
    reductions: dict[str, int] | None = None,
) -> list[str]:
    """Apply `[workspace.metadata.plakous.modules.<package>]` per file.

    A rule for module `m` covers every production source under `src/m/`. Such a
    file may `use` std/core/alloc, its own external dependencies, the paths
    listed in `may-use`, and nothing matching `forbid-crates` unless it is
    listed in `allow-crates`; any other `crate::` path is an error, whether
    a `use` declares it or the code names it inline, and so is a `super::`
    chain that resolves, by the file's module and the inline `mod` blocks
    around it, to one. With a
    `may-use-ceiling`, `may-use` lists at most that many paths. Source
    restrictions are inactive until the module directory exists.
    """
    errors: list[str] = []
    for module, rule in sorted(rules.items()):
        prefix = f"src/{module}/"
        allowed_crates = rule.get("allow-crates", [])

        def forbidden(crate: str) -> bool:
            if any(_crate_pattern_matches(pattern, crate) for pattern in allowed_crates):
                return False
            return any(_crate_pattern_matches(pattern, crate) for pattern in rule["forbid-crates"])

        # The module itself is always reachable: `may-use` names it.
        def reachable(full: str | None) -> bool:
            return full is not None and any(
                full == allowed or full.startswith(allowed + "::") for allowed in rule["may-use"]
            )

        ceiling = rule.get("may-use-ceiling")
        count = len(rule["may-use"])
        if ceiling is not None:
            if count > ceiling:
                errors.append(
                    f"{package.name}: module {module!r} may-use lists {count} paths, "
                    f"over its may-use-ceiling {ceiling}"
                )
            elif reductions is not None and count < ceiling:
                reductions[module] = count

        for path, contents in sorted(sources.items()):
            if not path.startswith(prefix):
                continue
            tokens = rust_tokens(contents)
            spans = _use_statement_spans(tokens)
            file_module = _file_module_path(path)
            inline_modules = _inline_modules(tokens)

            def module_at(index: int) -> list[str]:
                return [*file_module, *inline_modules[index]]

            for start, end in spans:
                statement = tokens[start:end]
                refused_roots: set[str] = set()
                for names in _use_tree_paths(statement):
                    if not names:
                        continue
                    root = names[0]
                    if root in {"std", "core", "alloc", "self"}:
                        continue
                    if root in {"crate", "super"}:
                        full = _resolve_module_path(names, module_at(start))
                        if not reachable(full):
                            errors.append(
                                f"{package.name}:{path}: module {module!r} may not use {full!r}"
                            )
                        continue
                    if forbidden(root) and root not in refused_roots:
                        refused_roots.add(root)
                        errors.append(
                            f"{package.name}:{path}: module {module!r} may not use crate {root!r}"
                        )
            # A path written inline names what a `use` would: the same
            # `may-use` list governs both.
            inline = {
                _resolve_module_path(names, module_at(index))
                for index, names in _inline_module_paths(tokens, spans)
            }
            for full in sorted(inline, key=str):
                if not reachable(full):
                    errors.append(f"{package.name}:{path}: module {module!r} may not name {full!r}")
            for root in sorted(_path_root_identifiers(tokens, spans)):
                crate = root.replace("_", "-")
                if forbidden(crate):
                    errors.append(
                        f"{package.name}:{path}: module {module!r} names forbidden crate {crate!r}"
                    )
    return errors


# Test seams are composition-root registrations, not production control flow.
TEST_SEAM_FEATURES = frozenset(
    {
        "test-support",
        "test-hooks",
        "fault-injection",
        "scripted-code-mode-enrolment",
    }
)


def _token_group_end(tokens: list[RustToken], opening: int) -> int:
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[tokens[opening].value]]
    cursor = opening + 1
    while cursor < len(tokens) and stack:
        value = tokens[cursor].value
        if value in pairs:
            stack.append(pairs[value])
        elif value == stack[-1]:
            stack.pop()
        cursor += 1
    return cursor


def _registry_entry_positions(tokens: list[RustToken]) -> set[int]:
    """Direct elements of const/static slice initializers, not function bodies."""
    positions: set[int] = set()
    for index, token in enumerate(tokens):
        if token.value not in {"static", "const"}:
            continue
        cursor = index + 1
        while cursor < len(tokens) and tokens[cursor].value not in {"=", ";", "{"}:
            cursor += 1
        if cursor == len(tokens) or tokens[cursor].value != "=":
            continue
        cursor += 1
        if cursor < len(tokens) and tokens[cursor].value == "&":
            cursor += 1
        if cursor == len(tokens) or tokens[cursor].value != "[":
            continue
        end = _token_group_end(tokens, cursor)
        cursor += 1
        while cursor < end - 1:
            positions.add(cursor)
            if tokens[cursor].value in {"(", "[", "{"}:
                cursor = _token_group_end(tokens, cursor)
            else:
                cursor += 1
    return positions


def configuration_debt(
    package: str, sources: dict[str, str], rule: dict[str, Any]
) -> dict[str, list[str]]:
    """Inventory placement violations, including cfg(any(test, feature)).

    A disjunction with test still ships under the feature, so it must not be
    stripped like cfg(test) or cfg(all(test, ...)). Count one predicate, not
    every feature name inside it. Imported environment readers count at import
    (including aliases/function items), not at each subsequent call.
    """
    debt: dict[str, list[str]] = {"feature": [], "test-seam": [], "env": []}
    feature_sites = rule.get("feature-sites", ["src/lib.rs", "src/registry.rs"])
    env_sites = rule.get("env-sites", [])
    for path, source in sorted(sources.items()):
        parts = Path(path).parts
        if any(part in {"tests", "benches", "examples"} for part in parts) or (
            Path(path).name == "tests.rs"
            or Path(path).name.endswith("_tests.rs")
            or any(part.startswith("test_support") for part in parts)
        ):
            continue
        tokens = rust_tokens(source)
        registry_positions = _registry_entry_positions(tokens) if path in feature_sites else set()
        cursor = 0
        while cursor < len(tokens):
            token = tokens[cursor]
            if token.value not in {"cfg", "cfg_attr"}:
                cursor += 1
                continue
            opening = cursor + 1
            if opening < len(tokens) and tokens[opening].value == "!":
                opening += 1
            if opening >= len(tokens) or tokens[opening].value != "(":
                cursor += 1
                continue
            end = _token_group_end(tokens, opening)
            features = {
                tokens[i + 2].value
                for i in range(opening + 1, end - 2)
                if tokens[i].value == "feature"
                and tokens[i + 1].value == "="
                and tokens[i + 2].kind == "string"
            }
            if not features:
                cursor = end
                continue
            following = end
            if following < len(tokens) and tokens[following].value == "]":
                following += 1
            while following + 1 < len(tokens) and tokens[following].value == "#":
                if tokens[following + 1].value != "[":
                    break
                following = _token_group_end(tokens, following + 1)
            if following < len(tokens) and tokens[following].value == "pub":
                following += 1
                if following < len(tokens) and tokens[following].value == "(":
                    following = _token_group_end(tokens, following)
            is_module = following < len(tokens) and tokens[following].value == "mod"
            is_registration = (
                cursor >= 2
                and cursor - 2 in registry_positions
                and tokens[cursor - 2].value == "#"
                and following + 2 < len(tokens)
                and tokens[following].value == "&"
                and tokens[following + 1].kind == "identifier"
                and tokens[following + 2].value in {"::", ",", "]"}
            )
            label = f"{path}: {token.value}({', '.join(sorted(features))})"
            if not is_module and not is_registration:
                debt["feature"].append(label)
            if features & TEST_SEAM_FEATURES:
                if package == "plakd":
                    allowed = path in {"src/lib.rs", "src/test_hooks.rs"}
                else:
                    name = (
                        tokens[following + 1].value
                        if is_module and following + 1 < len(tokens)
                        else ""
                    )
                    allowed = is_module and (
                        name.startswith("test") or name.endswith("test_support")
                    )
                if not allowed:
                    debt["test-seam"].append(label)
            cursor = end
        if path in env_sites:
            continue
        spans = _use_statement_spans(tokens)
        for start, end in spans:
            for names in _use_tree_paths(tokens[start:end]):
                if names[:2] == ["std", "env"] and (
                    len(names) == 2 or names[2] in {"var", "var_os", "vars", "vars_os", "*"}
                ):
                    debt["env"].append(f"{path}: import {'::'.join(names)}")
        for index in range(2, len(tokens)):
            if any(start <= index < end for start, end in spans):
                continue
            if tokens[index].value in {"var", "var_os", "vars", "vars_os"} and (
                tokens[index - 2].value == "env" and tokens[index - 1].value == "::"
            ):
                debt["env"].append(f"{path}: env::{tokens[index].value}")
    return debt


def validate_configuration_rules(
    package: str,
    sources: dict[str, str],
    rule: dict[str, Any],
    reductions: dict[str, int] | None = None,
) -> list[str]:
    errors: list[str] = []
    if not isinstance(rule, dict):
        return [f"cfg.{package}: expected a table"]
    allowed_keys = {
        "feature-sites",
        "env-sites",
        "feature-ceiling",
        "test-seam-ceiling",
        "env-ceiling",
        "reason",
    }
    for key in sorted(set(rule) - allowed_keys):
        errors.append(f"cfg.{package}: unknown key {key!r}")
    if "reason" in rule and not isinstance(rule["reason"], str):
        errors.append(f"cfg.{package}.reason: expected a review reason string")
    for key in ("feature-sites", "env-sites"):
        if key in rule and (
            not isinstance(rule[key], list)
            or any(
                not isinstance(path, str) or not path.startswith("src/") or ".." in path.split("/")
                for path in rule[key]
            )
        ):
            errors.append(f"cfg.{package}.{key}: expected package-relative src paths")
    for kind in ("feature", "test-seam", "env"):
        ceiling = rule.get(f"{kind}-ceiling", 0)
        if type(ceiling) is not int or ceiling < 0:
            errors.append(f"cfg.{package}.{kind}-ceiling: expected nonnegative integer")
    if errors:
        return errors
    debt: dict[str, list[str]] = {"feature": [], "test-seam": [], "env": []}
    for path, data in sorted(sources.items()):
        one = {path: data}

        def sites() -> list[str]:
            found = configuration_debt(package, one, rule)
            for kind, labels in found.items():
                debt[kind].extend(labels)
            # Only the empty result is reusable. Recount real debt, preserving
            # its diagnostic detail and the package-wide ceiling.
            return [label for labels in found.values() for label in labels]

        source_check("configuration-sites", one, sites, package, rule)
    if (
        any(rule.get(f"{kind}-ceiling", 0) for kind in debt)
        and not str(rule.get("reason", "")).strip()
    ):
        errors.append(f"cfg.{package}: nonzero ceilings require a reviewed reason")
    elif (rule.get("env-sites") or rule.get("feature-sites")) and not str(
        rule.get("reason", "")
    ).strip():
        errors.append(f"cfg.{package}: explicit boundary sites require a reviewed reason")
    for kind, sites in debt.items():
        key = f"{kind}-ceiling"
        ceiling = rule.get(key, 0)
        if len(sites) > ceiling:
            errors.append(
                f"{package}: {kind} placement debt {len(sites)} exceeds ceiling {ceiling}: "
                + "; ".join(sites)
            )
        elif reductions is not None and len(sites) < ceiling:
            reductions[key] = len(sites)
    return errors


def declared_test_module_paths(
    files: dict[str, str],
    crate_roots: set[str],
) -> set[str]:
    """Package-relative test-only module files, shared with production scans."""
    return {
        path.as_posix()
        for path in _test_module_files(
            {Path(path): source for path, source in files.items()},
            {Path(path) for path in crate_roots},
        )
    }


def package_sources(package: Package, repository: Path = ROOT) -> tuple[dict[str, str], list[str]]:
    """Return the package's production sources keyed by package-relative path."""
    sources, errors = production_rust_sources(package.metadata, repository)
    base = (repository / package.directory).resolve()
    by_path = {
        Path(os.path.relpath(source.resolve(), base)).as_posix(): read_rust_source(source)
        for source in sources
    }
    return by_path, errors


# The only portable-crate list. Grow it one crate at a time, in the same change
# that makes that crate's normal closure portable and its library check clean
# for wasm32-unknown-unknown.
PORTABLE_CRATES = (
    "plakous-model",
    "plakous-runner-protocol",
    "plakous-gateway-protocol",
    "plakous-image",
    "plakous-tools",
    "plakous-agent",
)
# Libraries whose default features compile for the portable target although an
# optional feature makes their full closure native, so they stay off
# PORTABLE_CRATES and its fence. Standalone checks their default library
# for wasm32 beside the portable set; `--wasm-libraries` prints both.
PORTABLE_DEFAULT_LIBRARIES = ("plakous-store",)
# The fence resolves this target, not the host: cfg(target_arch = "wasm32")
# dependencies are in the closure and cfg(not(...)) ones are not.
PORTABLE_TARGET = "wasm32-unknown-unknown"
# Tokio itself is allowed. These features are native runtimes and OS effects.
BANNED_TOKIO_FEATURES = frozenset({"fs", "net", "process", "rt-multi-thread", "signal"})
# Reviewed against Cargo.lock. `notify` is banned before it is a dependency.
# `libc` is banned: the target has no C library, and nothing in the wasm
# resolution needs it (getrandom and cpufeatures pull it only on native hosts).
BANNED_NATIVE_CRATES = frozenset(
    {
        "errno",
        "filedescriptor",
        "hyper",
        "hyper-util",
        "libc",
        "linux-raw-sys",
        "mio",
        "nix",
        "notify",
        "portable-pty",
        "redox_syscall",
        "reqwest",
        "rusqlite",
        "rustix",
        "serial2",
        "socket2",
    }
)
_TREE_LINE = re.compile(
    r"^(?P<depth>\d+)(?P<body>.+?) feats=\[(?P<features>[^\]]*)\](?P<duplicate> \(\*\))?$"
)
_PACKAGE_BODY = re.compile(
    r"^(?P<name>\S+) v(?P<version>\S+)(?P<proc_macro> \(proc-macro\))?(?: \(.+\))?$"
)


class _TreeNode(NamedTuple):
    depth: int
    name: str
    version: str
    features: tuple[str, ...]
    duplicate: bool
    proc_macro: bool = False


def portable_tree_args(package: str, *, all_features: bool) -> list[str]:
    """Cargo tree arguments for one portable crate's normal closure."""
    args = [
        "-p",
        package,
        "-e",
        "normal",
        "--target",
        PORTABLE_TARGET,
        "--locked",
        "--prefix",
        "depth",
        "--format",
        "{p} feats=[{f}]",
    ]
    if all_features:
        args.append("--all-features")
    return args


def parse_feature_tree(output: str) -> tuple[list[_TreeNode], list[str]]:
    """Parse `cargo tree --prefix depth --format '{p} feats=[{f}]'`."""
    nodes: list[_TreeNode] = []
    errors: list[str] = []
    for line_number, line in enumerate(output.splitlines(), 1):
        if not line.strip():
            continue
        match = _TREE_LINE.match(line)
        body = _PACKAGE_BODY.match(match.group("body")) if match else None
        if match is None or body is None:
            errors.append(f"unparsed cargo tree line {line_number}: {line}")
            continue
        features = tuple(part for part in match.group("features").split(",") if part)
        nodes.append(
            _TreeNode(
                depth=int(match.group("depth")),
                name=body.group("name"),
                version=body.group("version"),
                features=features,
                duplicate=match.group("duplicate") is not None,
                proc_macro=body.group("proc_macro") is not None,
            )
        )
    return nodes, errors


def native_offense(node: _TreeNode) -> str | None:
    """Return the offending dependency text, or None when the node is portable."""
    if node.name in BANNED_NATIVE_CRATES:
        return f"{node.name} v{node.version}"
    if node.name == "tokio":
        banned = sorted(set(node.features) & BANNED_TOKIO_FEATURES)
        if banned:
            label = "feature" if len(banned) == 1 else "features"
            return f"tokio v{node.version} {label} {', '.join(banned)}"
    return None


def closure_findings(package: str, resolution: str, nodes: list[_TreeNode]) -> list[str]:
    """Name each banned dependency once, with the path that pulls it in.

    A proc-macro and everything below it are built for the host, not the
    portable target, so that subtree is outside the fence.
    """
    if not nodes or nodes[0].depth != 0 or nodes[0].name != package:
        return [f"{package} [{resolution}]: cargo tree did not start at the crate"]
    stack: list[tuple[int, str]] = []
    seen: set[str] = set()
    findings: list[str] = []
    host_depth: int | None = None
    for node in nodes:
        if host_depth is not None and node.depth > host_depth:
            continue
        host_depth = None
        while stack and stack[-1][0] >= node.depth:
            stack.pop()
        if node.proc_macro:
            host_depth = node.depth
            continue
        path = [*(item[1] for item in stack), node.name]
        stack.append((node.depth, node.name))
        offense = native_offense(node)
        if offense is None or offense in seen:
            continue
        seen.add(offense)
        findings.append(f"{package} [{resolution}]: {offense} via {' -> '.join(path)}")
    return findings


def portable_closure_errors(workspace: Workspace) -> list[str]:
    """Fail when a portable crate's normal closure is native-only.

    Default features and `--all-features` are both checked, each resolved for
    `PORTABLE_TARGET` (Cargo needs no installed target for that).
    Dev-dependencies and build-dependencies are not part of the normal closure.
    """
    lock = workspace.root / "Cargo.lock"
    try:
        lock.read_bytes()
    except OSError as error:
        return [f"Cargo.lock: {error}"]
    errors: list[str] = []
    for package in PORTABLE_CRATES:
        if package not in workspace.packages:
            errors.append(f"{package}: portable crate is not a workspace member")
            continue
        for resolution, all_features in (("default", False), ("all-features", True)):
            output = cargo_tree_output(
                workspace.root, portable_tree_args(package, all_features=all_features), errors
            )
            if output is None:
                continue
            nodes, parse_errors = parse_feature_tree(output)
            if parse_errors:
                errors.extend(f"{package} [{resolution}]: {item}" for item in parse_errors)
                continue
            errors.extend(closure_findings(package, resolution, nodes))
    return errors


def cargo_tree_output(root: Path, spec: list[str], errors: list[str]) -> str | None:
    """Fail closed with Cargo's diagnostic rather than treating failure as absence."""
    command = ["cargo", "tree", "--offline", *spec]
    result = subprocess.run(
        command,
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        errors.append(
            f"{shlex.join(command)} failed (exit {result.returncode}): "
            f"{result.stderr.strip() or 'no stderr'}"
        )
        return None
    return result.stdout


def rquickjs_reaches_plak_jointly(root: Path, errors: list[str]) -> bool | None:
    """Ask Cargo whether `plakd` links rquickjs when built beside the sidecar."""
    output = cargo_tree_output(
        root,
        [
            "-e",
            "features",
            "-i",
            "rquickjs",
            "-p",
            "plakd",
            "-p",
            "plakous-js-runtime",
            "-p",
            "plakous-py-worker",
        ],
        errors,
    )
    if output is None:
        return None
    return re.search(r"(?m)^[\s│├└─]*plakd v\d", output) is not None


def rquickjs_linkage_errors(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    for spec, must_link, message in (
        (
            ["-p", "plakous", "--all-features"],
            False,
            "plakous must not link rquickjs under any feature; Code Mode uses the sidecar",
        ),
        (["-p", "plakd"], False, "plakd must not link rquickjs; Code Mode uses plakous-js-host"),
        (["-p", "plakous-js-runtime"], True, "plakous-js-runtime must link rquickjs"),
    ):
        # An inverse query errors when rquickjs is absent from a selected graph.
        output = cargo_tree_output(root, [*spec, "--prefix", "none", "--format", "{p}"], errors)
        if output is not None and bool(re.search(r"(?m)^rquickjs v", output)) != must_link:
            errors.append(message)
    if rquickjs_reaches_plak_jointly(root, errors):
        errors.append("building both workers alongside plakd must not link rquickjs into plakd")
    return errors


# Symbols that appear only when the QuickJS engine itself is linked. The Rust
# markers are v0-mangled crate names (`<length><crate>`) and carry both
# supported platforms; the C markers are the engine's own entry points, and
# Mach-O prefixes those with an underscore while ELF does not, so both
# spellings are listed. Host-side adapters are not the interpreter, so a
# substring match on an engine name proves nothing.
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
PRODUCT_EXECUTABLES = ("plakd", "plakous-js-host", "plakous-py-host")

# Length-prefixed crate identifiers in legacy and v0 Rust symbols.
PYTHON_RUNTIME_SYMBOLS = re.compile(
    r"(?<![0-9])(?:11micro_monty|10mm_compile|7mm_core|9mm_engine|8mm_parse|9mm_stdlib)"
    r"(?=[0-9A-Z_]|$)"
)


def symbol_tool() -> list[str] | None:
    """Return the symbol lister to use, preferring the LLVM one when present."""
    for candidate in (["llvm-nm"], ["nm"]):
        if shutil.which(candidate[0]) is not None:
            return candidate
    return None


def cargo_target_dir(root: Path) -> Path:
    """The Cargo target directory the gate is using.

    `CARGO_TARGET_DIR` when set, otherwise `root / "target"`. An empty value
    is unset. A relative value is under `root`.
    """
    target = Path(os.environ.get("CARGO_TARGET_DIR") or "target")
    if not target.is_absolute():
        target = root / target
    return target


def built_product_artifacts(root: Path) -> dict[str, Path]:
    """Return the release product executables, which is the product build.

    Only `<target>/release` counts, where `<target>` is `CARGO_TARGET_DIR`
    or `root / "target"`. `target/debug` holds gate builds of a different
    feature selection (`--all-features`, including `test-support`).
    The product parent is `cargo build --release --locked -p plakd`, whose facade
    features are the explicit set `bin/plakd` declares.
    """
    release = cargo_target_dir(root) / "release"
    return {
        name: candidate for name in PRODUCT_EXECUTABLES if (candidate := release / name).is_file()
    }


def engine_symbol_markers(symbol_output: str) -> set[str]:
    """Return the QuickJS engine markers present in one symbol listing."""
    return {marker for marker in RQUICKJS_ENGINE_SYMBOL_MARKERS if marker in symbol_output}


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
        python_runtime = PYTHON_RUNTIME_SYMBOLS.search(output) is not None
        if name != "plakous-py-host" and python_runtime:
            errors.append(f"{name} links micro-monty; interpreter must remain in the worker")
        if name == "plakous-py-host" and not python_runtime:
            errors.append("plakous-py-host does not link micro-monty; cannot verify isolation")
        if name == "plakd" and found:
            errors.append(
                f"plakd links the QuickJS engine ({sorted(found)!r}); the product "
                "evaluates JavaScript only in the plakous-js-host worker"
            )
        if name == "plakous-js-host" and not found:
            errors.append(
                "plakous-js-host does not link the QuickJS engine; the worker is "
                "the only place the product evaluates JavaScript"
            )
        if name == "plakous-py-host" and found:
            errors.append(f"plakous-py-host links the QuickJS engine ({sorted(found)!r})")
    if len(set(digests.values())) != len(digests):
        errors.append(f"the product executables are not separate artifacts: {digests!r}")
    return errors


def newest_tracked_source_mtime(root: Path) -> float | None:
    """Return the newest mtime among tracked Rust sources and Cargo metadata.

    `Cargo.lock` is matched by the same listing, so a dependency change dates
    the tree just as a source edit does. Returns `None` only when there is no
    extant source input; repository listing failures propagate.
    """
    listed = [
        path
        for path in vcs_listing.listed_files(root)
        if path.endswith((".rs", "Cargo.toml")) or path == "Cargo.lock"
    ]
    newest: float | None = None
    for relative in listed:
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
    return sorted(name for name, path in artifacts.items() if path.stat().st_mtime < newest)


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
    missing = [
        name
        for name in PRODUCT_EXECUTABLES
        if name not in artifacts or not artifacts[name].is_file()
    ]
    if missing:
        return [f"missing product artifacts: {', '.join(missing)}"], "artifact linkage refused"
    tool = symbol_tool()
    if tool is None:
        return ["artifact linkage requires nm or llvm-nm on PATH"], "artifact linkage refused"

    def symbol_listing(path: Path) -> str | None:
        result = subprocess.run([*tool, str(path)], text=True, capture_output=True, check=False)
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
                "--locked",
                "--workspace",
                "--all-features",
                "--target",
                target,
                "--edges",
                "normal,build",
                "--prefix",
                "none",
                "--format",
                "{p}",
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
        or sorted(tree.get("features", {}).get("test-support", []))
        != [
            "dep:plakous-store",
            "plakous-recursive-subagents/test-support",
            "plakous-store/test-support",
        ]
    ):
        return ["tree store's plakous-store edge must be optional and test-support-only"]
    return []


COMMIT_TRACE_SOURCE_PREFIXES = {
    "plakous-store": "src/",
    "plakd": "src/store/",
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
# Generic placeholder users and the synthetic qualification/eval container
# accounts. Never an operator's account name.
HYGIENE_HOME_PLACEHOLDERS = frozenset(
    {
        "fixture",
        "me",
        "op",
        "plak-qual",
        "pmux",
        "secret",
        "test",
        "tester",
        "u",
        "user",
        "x",
        "you",
    }
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
    relative: str,
    data: bytes,
    private_names: frozenset[bytes] = frozenset(),
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
                findings.append(
                    f"{relative}:{number}: absolute home path {match.group(0).decode()!r}"
                )
        if has_private_name:
            for name in hygiene_private_names(line, private_names):
                findings.append(f"{relative}:{number}: private host or tool name {name!r}")
    return findings


def hygiene_candidate_files(root: Path) -> list[str]:
    """Files a commit would carry: tracked plus untracked, not ignored.

    Outside a git checkout (the musl container's source export holds exactly
    that set) every regular file except build output is scanned.
    """
    return vcs_listing.listed_files(root)


def validate_tracked_hygiene(
    root: Path = ROOT,
    private_names: frozenset[bytes] | None = None,
    *,
    listed: list[str] | None = None,
) -> list[str]:
    if private_names is None:
        private_names = load_hygiene_private_names(root)
    errors: list[str] = []
    for relative in hygiene_candidate_files(root) if listed is None else listed:
        path = root / relative
        if path.is_symlink() or not path.is_file():
            continue
        data = _read_rust_bytes(path) if path.suffix == ".rs" else path.read_bytes()

        def check() -> list[str]:
            return hygiene_findings(relative, data, private_names)

        receipts = _source_scan.receipts if _source_scan is not None else None
        findings = (
            receipts.check(
                check,
                "tracked-hygiene",
                relative,
                receipts.source(data),
                [receipts.source(name) for name in sorted(private_names)],
            )
            if receipts is not None
            else check()
        )
        for finding in findings:
            errors.append(f"{finding}: tracked files must not carry owner-specific content")
    return errors


INCLUDED_FILE = re.compile(
    r'include_(?:str|bytes)!\s*\(\s*(?:concat!\s*\(\s*env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*)?"([^"]+)"'
)


# A shell loop written into a Rust string: a fixture process a test spawns.
# `do` and `done` count only in command position, so prose such as "failed
# while waiting; do not retry until the job is done" is not a loop.
LOOP_KEYWORD = re.compile(r"(?<![\w$-])(?:while|until|for)(?![\w-])")
LOOP_HEAD = re.compile(
    r"(?P<kind>while|until)\b(?P<head>.*?)[;\n]\s*do(?![\w-])"
    r"|for\s+(?P<arith>\(\(.*?\)\))\s*[;\n]?\s*do(?![\w-])"
    r"|for\s+\w+(?:\s+in\b[^;\n]*)?\s*[;\n]\s*do(?![\w-])",
    re.S,
)
LOOP_WORD = re.compile(r"(?:(?<=[;\n])|(?<=[;&\n]\s))\s*(do|done)(?![\w-])")
# One comparison of a counter with a literal: `$((n += 1)) -le N`, `"$n" -lt N`.
COUNTER_TEST = re.compile(
    r'"?\$(?:\(\((?P<expr>[^)]*)\)\)|\{?(?P<var>\w+)\}?)"?\s+-(?P<op>le|lt|ge|gt|eq)\s+"?\d+"?'
)


def _counter_step(name: str) -> re.Pattern[str]:
    """`name += 1`, `name=$((name + 1))` or `name++`."""
    step = rf"\b{name}\s*\+=\s*1\b|\b{name}\s*=\s*\$\(\(\s*\$?{name}\s*\+\s*1\s*\)\)"
    return re.compile(rf"{step}|\b{name}\+\+")


EXIT_GUARD = re.compile(r"\s*\]\s*(?P<join>\|\||&&)\s*(?:exit|break|return)\b")
READ_HEAD = re.compile(r"^\s*(?:IFS=\S*\s+)?read\b")
# A pipe source that never reaches end of file.
ENDLESS_SOURCE = re.compile(r"\btail\s+(?:-\w*[fF]|--follow)|\byes\b")
FIXTURE_SLEEP = re.compile(
    r"(?<![\w-])sleep\s+"
    r"(?:(?P<infinity>infinity)|(?P<value>\d+(?:\.\d+)?)(?P<unit>[smhd]?))(?![\w.])"
)
SLEEP_UNITS = {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}
MAX_FIXTURE_SLEEP_SECONDS = 600
# Shell text that is displayed or highlighted, never run.
UNRUN_SHELL_SOURCES = set()


def _string_runs(tokens: list[RustToken]) -> list[str]:
    """String literals, with comma-separated runs (argv arrays, `concat!`) joined."""
    runs: list[str] = []
    current: list[str] = []
    expect_string = True
    for token in tokens:
        if token.kind == "string" and expect_string:
            current.append(token.value)
            expect_string = False
        elif token.value == "," and current and not expect_string:
            expect_string = True
        else:
            if current:
                runs.append(" ".join(current))
            current = [token.value] if token.kind == "string" else []
            expect_string = token.kind != "string"
    if current:
        runs.append(" ".join(current))
    return runs


def _loop_body_end(text: str, start: int) -> int | None:
    """Index just past the `done` closing a loop body that begins at `start`."""
    depth = 1
    for word in LOOP_WORD.finditer(text, start):
        depth += 1 if word.group(1) == "do" else -1
        if depth == 0:
            return word.end()
    return None


def _counted(loop_kind: str, head: str, body: str) -> bool:
    """A counter stepped in the loop is compared with a literal bound that ends it."""
    text = f"{head}\n{body}"
    for part, in_head in ((head, True), (body, False)):
        for test in COUNTER_TEST.finditer(part):
            operand = test.group("var") or re.match(r"\s*\$?(\w*)", test.group("expr")).group(1)
            if not operand or not _counter_step(operand).search(text):
                continue
            stops_when_small = test.group("op") in {"le", "lt"}
            if in_head:
                if stops_when_small == (loop_kind != "until"):
                    return True
                continue
            guard = EXIT_GUARD.match(part, test.end())
            if guard and stops_when_small == (guard.group("join") == "||"):
                return True
    return False


def _unbounded_loops(text: str) -> list[str]:
    loops: list[str] = []
    for keyword in LOOP_KEYWORD.finditer(text):
        head = LOOP_HEAD.match(text, keyword.start())
        if head is None:
            continue
        end = _loop_body_end(text, head.end())
        if end is None:
            continue
        loop = text[keyword.start() : end]
        body = text[head.end() : end]
        if head.group("kind"):
            condition = head.group("head")
            if READ_HEAD.match(condition):
                pipeline = re.split(r"[;\n&(]", text[: keyword.start()])[-1]
                if "|" not in pipeline or not ENDLESS_SOURCE.search(pipeline):
                    continue
            elif _counted(head.group("kind"), condition, body):
                continue
        elif head.group("arith") is not None:
            if re.search(r";\s*\w+\s*[<>]=?\s*\d+\s*;", head.group("arith")):
                continue
        else:
            # `for name in words` runs once per word of a finished expansion.
            continue
        loops.append(loop)
    return loops


def validate_fixture_loops(package_name: str, sources: dict[str, str]) -> list[str]:
    """Every shell loop a Rust string spawns ends within a bound.

    A fixture process outlives a test that dies: its temporary directory goes,
    and a sentinel it polls for never appears. Each `while`/`until` loop
    therefore steps a counter that it compares with a literal bound, in its
    condition (`[ $((n += 1)) -le N ]`) or in an exit guard in its body
    (`[ "$waits" -lt N ] || exit`), or reads its input until end of file from
    a source that has one; a `for ((...))` loop compares with a literal; and a
    single sleep lasts at most 600 s (spec 05 §2). Test modules count, and a
    loop split across comma-separated string literals is joined first.
    """
    errors: list[str] = []
    for path, source in sorted(sources.items()):
        if (package_name, path) in UNRUN_SHELL_SOURCES:
            continue
        found: list[str] = []
        for text in _string_runs(all_rust_tokens(source)):
            for loop in _unbounded_loops(text):
                snippet = " ".join(loop.split())[:80]
                found.append(
                    f"{package_name}:{path}: unbounded shell loop in a fixture: {snippet!r}; "
                    "count its passes against a literal bound ($((n += 1)) -le N), "
                    "guard its body ([ \"$n\" -lt N ] || exit) or read to end of file"
                )
            for sleep in FIXTURE_SLEEP.finditer(text):
                seconds = (
                    float("inf")
                    if sleep.group("infinity")
                    else float(sleep.group("value")) * SLEEP_UNITS[sleep.group("unit")]
                )
                if seconds > MAX_FIXTURE_SLEEP_SECONDS:
                    found.append(
                        f"{package_name}:{path}: fixture {sleep.group(0)!r} exceeds "
                        f"{MAX_FIXTURE_SLEEP_SECONDS} s"
                    )
        errors.extend(dict.fromkeys(found))
    return errors


def validate_package_local_includes(
    package_name: str,
    sources: dict[str, str],
) -> list[str]:
    """Every `include_str!`/`include_bytes!` stays inside its own package.

    `sources` maps package-relative Rust paths to their text.
    """
    errors: list[str] = []
    for relative, text in sorted(sources.items()):
        for match in INCLUDED_FILE.finditer(text):
            literal = match.group(1)
            if "CARGO_MANIFEST_DIR" in match.group(0):
                target = os.path.normpath(literal.lstrip("/"))
            else:
                target = os.path.normpath(os.path.join(os.path.dirname(relative), literal))
            if target == ".." or target.startswith("../") or os.path.isabs(target):
                line = text.count("\n", 0, match.start()) + 1
                errors.append(
                    f"{package_name}: {relative}:{line} includes {literal!r}, "
                    "outside the package root"
                )
    return errors


def validate_rust_file_policies(package: str, sources: dict[str, str]) -> list[str]:
    ordered = dict(sorted(sources.items()))
    return [
        *each_source(validate_package_local_includes, package, ordered),
        *each_source(validate_fixture_loops, package, ordered),
    ]


def package_rust_files(
    root: Path, package: Package, nested: set[str], *, listed: list[str] | None = None
) -> dict[str, str]:
    """Tracked Rust files of one package, excluding nested package roots.

    Outside a git checkout (the musl container's source export holds exactly
    the tracked and nonignored untracked files) every `*.rs` file under the
    package directory except build output counts as tracked.
    """
    if listed is None:
        listed = vcs_listing.listed_files(root)
    files: dict[str, str] = {}
    for path in listed:
        if not path.startswith(f"{package.directory}/") or not path.endswith(".rs"):
            continue
        if any(path.startswith(f"{other}/") for other in nested):
            continue
        files[os.path.relpath(path, package.directory)] = read_rust_source(root / path)
    return files


def tightened_manifest(text: str, reductions: dict[tuple[str, ...], dict[str, int]]) -> str:
    """Change only the enrolled integers; refuse an unrecognized TOML layout."""
    expected = tomllib.loads(text)
    tables = re.split(r"(?m)^(\[[^\r\n]+\][ \t]*(?:#[^\r\n]*)?)$", text)
    for path, ceilings in reductions.items():
        if not ceilings:
            continue
        header = f"[{'.'.join(path)}]"
        index = next(
            (i for i in range(1, len(tables), 2) if tables[i].split("#", 1)[0].strip() == header),
            None,
        )
        if index is None:
            raise ValueError(f"cannot locate ceiling table {header}")
        table = expected
        for part in path:
            table = table[part]
        for key, count in ceilings.items():
            pattern = (
                rf"(?m)^([ \t]*{re.escape(key)}[ \t]*=[ \t]*)[0-9][0-9_]*([ \t]*(?:#[^\r\n]*)?)$"
            )
            tables[index + 1], matches = re.subn(
                pattern, lambda match: f"{match[1]}{count}{match[2]}", tables[index + 1]
            )
            if matches != 1:
                raise ValueError(f"cannot locate {header}.{key}")
            table[key] = count
    updated = "".join(tables)
    if tomllib.loads(updated) != expected:
        raise ValueError("tightening changed unexpected TOML values")
    return updated


def tighten_ceilings(workspace: Workspace) -> int:
    """Recount with the static validators; write nothing if any ratchet fails."""
    root = workspace.root
    policy = workspace.policy
    cfg = workspace.root_manifest["workspace"]["metadata"]["plakous"].get("cfg", {})
    errors = []
    errors.extend(
        f"modules rule names unknown package {name!r}"
        for name in sorted(policy.modules.keys() - workspace.packages.keys())
    )
    errors.extend(
        f"cfg rule names unknown package {name!r}"
        for name in sorted(cfg.keys() - workspace.packages.keys())
    )
    root_edits: dict[tuple[str, ...], dict[str, int]] = {}
    for name, package in sorted(workspace.packages.items()):
        sources, discovery_errors = package_sources(package, root)
        errors.extend(discovery_errors)
        placement: dict[str, int] = {}
        errors.extend(validate_configuration_rules(name, sources, cfg.get(name, {}), placement))
        root_edits["workspace", "metadata", "plakous", "cfg", name] = placement
        if package.layer not in ABOVE_FACADE_LAYERS:
            continue
        module_counts: dict[str, int] = {}
        errors.extend(
            validate_module_rules(package, sources, policy.modules.get(name, {}), module_counts)
        )
        for module, count in module_counts.items():
            root_edits["workspace", "metadata", "plakous", "modules", name, module] = {
                "may-use-ceiling": count
            }
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    if any(root_edits.values()):
        manifest = root / "Cargo.toml"
        try:
            updated = tightened_manifest(manifest.read_text(encoding="utf-8"), root_edits)
        except ValueError as error:
            print(f"Cargo.toml: {error}", file=sys.stderr)
            return 1
        manifest.write_text(updated, encoding="utf-8")
        print("tightened Cargo.toml")
    else:
        print("ceilings already tight")
    return 0


def validate_workspace_excludes(
    root_workspace: dict[str, Any],
    supported_fixture_paths: set[str],
) -> list[str]:
    """Keep operator state outside Cargo without hiding unreviewed packages."""
    excludes = set(root_workspace.get("exclude", []))
    errors: list[str] = []
    if ".plak" not in excludes:
        errors.append("workspace must exclude .plak operator state and fork worktrees")
    unknown_excludes = excludes - supported_fixture_paths - {".plak"}
    if unknown_excludes:
        errors.append(
            f"workspace excludes unsupported nested packages: {sorted(unknown_excludes)!r}"
        )
    return errors


def _check_workspace() -> int:
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

    listed = hygiene_candidate_files(ROOT)
    errors.extend(validate_tracked_hygiene(ROOT, listed=listed))
    errors.extend(validate_local_production_lints(workspace_manifest_tables(workspace)))
    errors.extend(validate_commit_trace_edge(workspace_manifest_tables(workspace)))
    for name in COMMIT_TRACE_SOURCE_PREFIXES:
        sources, discovery_errors = package_sources(packages[name], ROOT)
        errors.extend(discovery_errors)
        errors.extend(each_source(validate_commit_trace_sql, name, dict(sorted(sources.items()))))

    sources, discovery_errors = package_sources(packages["plakous-anthropic"], ROOT)
    errors.extend(discovery_errors)
    errors.extend(validate_claude_oauth_state_comparison(sources.get("src/oauth.rs", "")))

    directories = {package.directory for package in packages.values()}
    for name, package in sorted(packages.items()):
        nested = {
            other
            for other in directories
            if other != package.directory and other.startswith(f"{package.directory}/")
        }
        rust_files = package_rust_files(ROOT, package, nested, listed=listed)
        errors.extend(validate_rust_file_policies(name, rust_files))

    # Layer policy: metadata shape, layout, edges, classes, pinned dependencies.
    errors.extend(validate_package_metadata(workspace))
    errors.extend(validate_layout(workspace))
    errors.extend(validate_layer_edges(workspace))
    errors.extend(validate_harness_pack_independence(workspace))
    errors.extend(validate_production_dependencies(workspace))
    errors.extend(validate_facade_features(workspace))
    errors.extend(validate_neutral_dependencies(workspace))
    errors.extend(validate_image_leaf(workspace))

    for name, package in sorted(packages.items()):
        metadata = package.metadata
        if package.package_class == "repository-only" and metadata.get("publish") != []:
            errors.append(f"{name}: repository-only packages must have publish = false")
        if name == VENDOR_PACKAGE:
            continue
        if metadata.get("edition") != "2024":
            errors.append(f"{name}: edition must be 2024")
        if metadata.get("rust_version") != "1.100.0":
            errors.append(f"{name}: rust-version must be 1.100.0")
        if metadata.get("license") != WORKSPACE_LICENSE:
            errors.append(f"{name}: license expression is inconsistent")
    # Every supported library package has exactly one external consumer fixture;
    # `plakd` is pinned by cli_contract.rs instead.
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
    app = packages.get("plakd")
    if app is not None:
        targets = app.metadata.get("targets", [])
        binaries = [target for target in targets if "bin" in target["kind"]]
        if len(binaries) != 1 or binaries[0]["name"] != "plakd":
            errors.append("plakd must expose exactly one binary named plakd")
        libraries = [target for target in targets if "lib" in target["kind"]]
        if len(libraries) != 1 or libraries[0]["name"] != "plakd":
            errors.append("plakd must expose exactly one library named plakd")
        for dependency in ("ratatui", "crossterm", "pulldown-cmark", "syntect", "arboard"):
            if dependency in app.manifest.get("dependencies", {}):
                errors.append(f"plakd: public terminal dependency {dependency} is not allowed")

    host = packages.get("plakous-hosted")
    if host is not None:
        targets = host.metadata.get("targets", [])
        if [target for target in targets if "bin" in target["kind"]]:
            errors.append("plakous-hosted must not expose binary targets")
        libraries = [target for target in targets if "lib" in target["kind"]]
        if len(libraries) != 1 or libraries[0]["name"] != "plakous_hosted":
            errors.append("plakous-hosted must expose exactly one library named plakous_hosted")
    facade = packages.get("plakous")
    if facade is not None:
        expected_features = {
            "anthropic",
            "auth",
            "composition",
            "composition-python",
            "default",
            "full",
            "gateway",
            "mcp",
            "codex",
            "grok",
            "claude",
            "openai",
            "openrouter",
            "sessions-sqlite",
            "store",
            "test-support",
            "web-fetch",
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
        errors.extend(
            each_source(validate_product_session_authority_sources, package.name, sources)
        )
        if package.name == "plakd":
            errors.extend(
                source_check(
                    "product-update-hooks",
                    sources,
                    lambda: validate_product_update_hook_sources(sources),
                )
            )
        errors.extend(each_source(validate_product_home_discovery, package.name, sources))
        errors.extend(
            source_check(
                "cli-security",
                sources,
                lambda: validate_cli_security_sources(package.name, sources),
                package.name,
            )
        )
        errors.extend(
            each_source(
                validate_facade_only_sources,
                package.name,
                dict(sorted(sources.items())),
                sorted(policy.edge_exceptions.get(package.name, ())),
            )
        )
        module_rules = policy.modules.get(package.name, {})
        errors.extend(
            source_check(
                "module-rules",
                sources,
                lambda: validate_module_rules(package, sources, module_rules),
                package.name,
                module_rules,
            )
        )
    for name in sorted(policy.modules):
        if name not in packages:
            errors.append(f"modules rule names unknown package {name!r}")

    configuration_rules = root_workspace.get("metadata", {}).get("plakous", {}).get("cfg", {})
    for name in sorted(set(configuration_rules) - set(packages)):
        errors.append(f"cfg rule names unknown package {name!r}")
    for package in sorted(packages.values(), key=lambda item: item.name):
        sources, discovery_errors = package_sources(package, ROOT)
        errors.extend(discovery_errors)
        errors.extend(
            validate_configuration_rules(
                package.name, sources, configuration_rules.get(package.name, {})
            )
        )

    # Neutral packages: no provider dependency (above) and no provider name.
    for package in sorted(packages.values(), key=lambda item: item.name):
        if not package.neutral:
            continue
        sources, discovery_errors = package_sources(package, ROOT)
        errors.extend(discovery_errors)
        errors.extend(
            each_source(validate_neutral_sources, package.name, dict(sorted(sources.items())))
        )
    for name, path in NEUTRAL_PROVIDER_NAME_EXEMPTIONS:
        package = packages.get(name)
        if package is None or not package.neutral:
            errors.append(f"provider-name exemption {name}:{path} does not name a neutral package")
    for name in sorted(PRODUCT_ACCOUNT_NAME_WAIVERS):
        package = packages.get(name)
        if package is None or not package.neutral:
            errors.append(f"product-account waiver {name!r} does not name a neutral package")

    errors.extend(rquickjs_linkage_errors(ROOT))
    errors.extend(portable_closure_errors(workspace))
    linkage_errors, linkage_note = artifact_linkage_errors(ROOT)
    errors.extend(linkage_errors)

    errors.extend(supported_target_atomic_polyfill_errors(ROOT))

    for neutral in TRACING_FREE_PACKAGES:
        output = cargo_tree_output(
            ROOT,
            ["-p", neutral, "-e", "normal"],
            errors,
        )
        for forbidden in ("tracing", "opentelemetry"):
            if output is not None and forbidden in output:
                errors.append(f"{neutral} must not link {forbidden}")
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

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    layers = {layer: len(workspace.in_layers(layer)) for layer in policy.layers}
    populated = ", ".join(f"{layer}={count}" for layer, count in layers.items() if count)
    line = (
        f"validated {len(packages)} workspace packages across layers ({populated}), "
        f"dependency boundaries, and the plakd/plakous-js-host build split; {linkage_note}"
    )
    global _success_line
    _success_line = line
    print(line)
    return 0


# Successful runs only. A miss, a failure, or any unreadable input recomputes.
_success_line: str | None = None
_CACHE_NAME = "workspace-check-cache.json"
_CacheDoubt = cargo_key.CacheDoubt
# Local modules whose bytes decide the check. A change misses, even if the tree does not.
_CHECKER_SOURCES = (
    Path(__file__).resolve(),
    Path(__file__).resolve().parent / "plakous_policy.py",
    Path(__file__).resolve().parent / "vcs_listing.py",
    Path(__file__).resolve().parent / "cargo_key.py",
    Path(__file__).resolve().parent / "workspace_receipts.py",
)


class _ReadLog:
    """Paths the checker actually touched, with the identity observed at that moment.

    A path observed twice with different identities is `unstable`, and a walk
    that could not read a directory is a doubt; neither can be recorded.
    """

    def __init__(self) -> None:
        self.files: dict[str, dict[str, object]] = {}
        self.dirs: dict[str, list[list[object]]] = {}
        self.probes: dict[str, dict[str, object]] = {}
        self.listings: list[list[str]] = []
        self.doubt = False


_read_log: _ReadLog | None = None
_originals: dict[tuple[object, str], Any] = {}
# The only Path methods a receipt may name. Replay calls nothing else.
_PROBES = ("exists", "is_file", "is_dir", "is_symlink")
# Values `path_identity` and `directory_listing` never return, so they never replay.
_UNREADABLE_FILE: dict[str, object] = {"state": "unreadable"}
_UNSTABLE_FILE: dict[str, object] = {"state": "unstable"}
_UNREADABLE_DIR: list[list[object]] = [["unreadable"]]
_UNSTABLE_DIR: list[list[object]] = [["unstable"]]


def workspace_cache_path(root: Path = ROOT) -> Path:
    """The result cache beside the gate target, same place as the feature-closure cache."""
    return cargo_target_dir(root) / _CACHE_NAME


def _abs(path: Path | str) -> str:
    return os.path.abspath(path)


def _record(table: dict[str, Any], key: str, value: Any, unstable: Any) -> None:
    if key in table and table[key] != value:
        value = unstable
    table[key] = value


def _note_file(path: Path) -> None:
    """Record a file before it is read, so a read that raises still records absence."""
    if _read_log is None:
        return
    key = _abs(path)
    try:
        identity = cargo_key.path_identity(Path(key))
    except cargo_key.CacheDoubt:
        identity = _UNREADABLE_FILE
    _record(_read_log.files, key, identity, _UNSTABLE_FILE)


def _note_dir(path: str, seen: list[str] | None = None) -> None:
    """Record a directory listing; `seen` is what the walk itself listed there."""
    if _read_log is None:
        return
    key = _abs(path)
    try:
        listing = cargo_key.directory_listing(Path(key))
    except cargo_key.CacheDoubt:
        listing = _UNREADABLE_DIR
    if seen is not None and [entry[0] for entry in listing] != seen:
        listing = _UNSTABLE_DIR
    _record(_read_log.dirs, key, listing, _UNSTABLE_DIR)


def _note_probe(kind: str, path: Path, result: bool) -> None:
    if _read_log is None:
        return
    key = _abs(path)
    try:
        identity = cargo_key.path_identity(Path(key))
    except cargo_key.CacheDoubt:
        identity = _UNREADABLE_FILE
    unstable = {"result": result, "identity": _UNSTABLE_FILE}
    _record(_read_log.probes, f"{kind}\0{key}", {"result": result, "identity": identity}, unstable)


def _patched(owner: object, name: str, wrapper: Any) -> None:
    _originals[(owner, name)] = getattr(owner, name)
    setattr(owner, name, wrapper)


def _install_read_log(log: _ReadLog) -> None:
    global _read_log
    _read_log = log

    def walk(top: Any, topdown: bool = True, onerror: Any = None, followlinks: bool = False) -> Any:
        def unreadable(error: OSError) -> None:
            log.doubt = True
            if onerror is not None:
                onerror(error)

        original = _originals[(os, "walk")]
        _note_dir(top)
        for dirpath, dirnames, filenames in original(top, topdown, unreadable, followlinks):
            _note_dir(dirpath, sorted([*dirnames, *filenames]))
            yield dirpath, dirnames, filenames

    # Each wrapper records the path itself and reads through the unwrapped open,
    # so no read depends on another wrapper to be recorded.
    def read_bytes(self: Path) -> bytes:
        _note_file(self)
        with _originals[(Path, "open")](self, "rb") as stream:
            return stream.read()

    def read_text(
        self: Path,
        encoding: str | None = None,
        errors: str | None = None,
        newline: str | None = None,
    ) -> str:
        _note_file(self)
        encoding = io.text_encoding(encoding)
        with _originals[(Path, "open")](
            self, "r", encoding=encoding, errors=errors, newline=newline
        ) as stream:
            return stream.read()

    def open_path(self: Path, *args: Any, **kwargs: Any) -> Any:
        _note_file(self)
        return _originals[(Path, "open")](self, *args, **kwargs)

    def probe(kind: str) -> Any:
        original = getattr(Path, kind)

        def wrapped(self: Path, *args: Any, **kwargs: Any) -> bool:
            result = original(self, *args, **kwargs)
            _note_probe(kind, self, bool(result))
            return result

        return wrapped

    def listed(root: Path) -> list[str]:
        result = _originals[(vcs_listing, "listed_files")](root)
        log.listings.append(list(result))
        return result

    _patched(os, "walk", walk)
    _patched(Path, "read_bytes", read_bytes)
    _patched(Path, "read_text", read_text)
    _patched(Path, "open", open_path)
    _patched(vcs_listing, "listed_files", listed)
    for kind in _PROBES:
        _patched(Path, kind, probe(kind))


def _remove_read_log() -> None:
    global _read_log
    for (owner, name), original in _originals.items():
        setattr(owner, name, original)
    _originals.clear()
    _read_log = None


@contextmanager
def recording_reads() -> Iterator[_ReadLog]:
    """Record file reads, directory walks and existence probes for one check."""
    log = _ReadLog()
    try:
        _install_read_log(log)
        yield log
    finally:
        _remove_read_log()


def _observation(log: _ReadLog) -> dict[str, object]:
    return {
        "files": dict(log.files),
        "dirs": dict(log.dirs),
        "probes": dict(log.probes),
        "listings": [list(listing) for listing in log.listings],
    }


def replay_observation(stored: dict[str, object], root: Path) -> dict[str, object]:
    """Re-fingerprint exactly the paths a previous run recorded.

    Replay only lstats, reads, lists and probes with the four allowed kinds; a
    receipt naming any other operation is a doubt.
    """
    files = stored.get("files")
    dirs = stored.get("dirs")
    probes = stored.get("probes")
    listings = stored.get("listings")
    if (
        not isinstance(files, dict)
        or not isinstance(dirs, dict)
        or not isinstance(probes, dict)
        or not isinstance(listings, list)
    ):
        raise _CacheDoubt("observation is not a record of files, directories, probes and listings")
    replayed_probes: dict[str, dict[str, object]] = {}
    for key in probes:
        if not isinstance(key, str) or "\0" not in key:
            raise _CacheDoubt("probe key is not kind and path")
        kind, path = key.split("\0", 1)
        if kind not in _PROBES:
            raise _CacheDoubt(f"unknown probe {kind!r}")
        replayed_probes[key] = {
            "result": bool(getattr(Path(path), kind)()),
            "identity": cargo_key.path_identity(Path(path)),
        }
    replayed_files = {path: cargo_key.path_identity(Path(path)) for path in files}
    replayed_dirs = {path: cargo_key.directory_listing(Path(path)) for path in dirs}
    replayed_listings = [vcs_listing.listed_files(root) for _entry in listings]
    return {
        "files": replayed_files,
        "dirs": replayed_dirs,
        "probes": replayed_probes,
        "listings": replayed_listings,
    }


def global_cache_digest(root: Path = ROOT) -> str:
    """Hash the inputs that are not discovered by watching the checker read."""
    digest = hashlib.sha256()

    def add(label: str, payload: bytes) -> None:
        digest.update(label.encode())
        digest.update(b"\0")
        digest.update(hashlib.sha256(payload).digest())

    try:
        cargo = subprocess.check_output(["cargo", "--version"], cwd=root, text=True)
        rustc = subprocess.check_output(["rustc", "-vV"], cwd=root, text=True)
    except (OSError, subprocess.CalledProcessError) as error:
        raise _CacheDoubt(str(error)) from error
    add("cargo", cargo.encode())
    add("rustc", rustc.encode())
    add("root", _abs(root).encode())
    for name, value in cargo_key.tracked_env():
        add(f"env:{name}", value.encode())
    for path in _CHECKER_SOURCES:
        add(f"checker:{path.name}", path.read_bytes())
    for tool in ("llvm-nm", "nm"):
        found = shutil.which(tool)
        if found is None:
            add(f"tool:{tool}", b"absent")
            continue
        add(
            f"tool:{tool}",
            json.dumps(cargo_key.path_identity(Path(found)), sort_keys=True).encode(),
        )
    for path in cargo_key.config_candidates(root):
        add(f"config:{path}", json.dumps(cargo_key.path_identity(path), sort_keys=True).encode())
    # Untracked and optional, it decides the hygiene check: present or absent is keyed here too.
    hygiene = root / HYGIENE_PRIVATE_NAMES_FILE
    add("hygiene", json.dumps(cargo_key.path_identity(hygiene), sort_keys=True).encode())
    return digest.hexdigest()


def _canonical(payload: dict[str, object]) -> bytes:
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def _receipt_checksum(body: dict[str, object]) -> str:
    return hashlib.sha256(_canonical(body)).hexdigest()


def read_cached_success(root: Path, digest: str) -> str | None:
    """Return the recorded line when the global key and the recorded reads still match."""
    try:
        payload = json.loads(workspace_cache_path(root).read_text(encoding="utf-8"))
        if not isinstance(payload, dict):
            return None
        line = payload.get("line")
        cached = payload.get("digest")
        observation = payload.get("observation")
        checksum = payload.get("checksum")
        body = {"digest": cached, "line": line, "observation": observation}
        if (
            not isinstance(line, str)
            or not line.startswith("validated ")
            or cached != digest
            or not isinstance(observation, dict)
            or checksum != _receipt_checksum(body)
            or replay_observation(observation, root) != observation
        ):
            return None
        return line
    except (OSError, json.JSONDecodeError, TypeError, UnicodeError, cargo_key.CacheDoubt):
        return None


def write_cached_success(
    root: Path, digest: str, observation: dict[str, object], line: str
) -> None:
    """Record one successful run. A write failure leaves the checker result unchanged."""
    if not line.startswith("validated "):
        return
    path = workspace_cache_path(root)
    body: dict[str, object] = {"digest": digest, "line": line, "observation": observation}
    payload = dict(body)
    payload["checksum"] = _receipt_checksum(body)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".json.tmp")
        temporary.write_text(json.dumps(payload, sort_keys=True), encoding="utf-8")
        os.replace(temporary, path)
    except OSError:
        return


def check_root(root: Path, *, full: bool = False) -> int:
    """Run the checks at `root`, skipping them when the recorded reads are unchanged."""
    global _success_line
    _success_line = None
    if full:
        with source_scan():
            return _check_workspace()
    try:
        digest = global_cache_digest(root)
    except _CacheDoubt:
        digest = None
    if digest is not None and (cached := read_cached_success(root, digest)) is not None:
        print(cached)
        return 0
    receipts = (
        Receipts(workspace_cache_path(root).with_name("workspace-policy-receipts.json"), digest)
        if digest is not None
        else None
    )
    out, err = io.StringIO(), io.StringIO()
    with recording_reads() as log, redirect_stdout(out), redirect_stderr(err):
        with source_scan(receipts):
            code = _check_workspace()
        observed = _observation(log)
    stable = False
    if digest is not None and not log.doubt:
        try:
            stable = (
                global_cache_digest(root) == digest
                and replay_observation(observed, root) == observed
            )
        except _CacheDoubt:
            pass
    if receipts is not None and receipts.reused and not stable:
        return check_root(root, full=True)
    sys.stdout.write(out.getvalue())
    sys.stderr.write(err.getvalue())
    if code == 0 and stable and receipts is not None and _success_line is not None:
        receipts.write()
        write_cached_success(root, receipts.identity, observed, _success_line)
    return code


def main() -> int:
    """Run the workspace checks, skipping them when the recorded reads are unchanged."""
    if sys.argv[1:] == ["--standalone-packages"]:
        for name, package in sorted(load_workspace(ROOT).packages.items()):
            if package.package_class != "repository-only":
                print(name)
        return 0
    if sys.argv[1:] == ["--wasm-libraries"]:
        for name in (*PORTABLE_CRATES, *PORTABLE_DEFAULT_LIBRARIES):
            print(name)
        return 0
    if sys.argv[1:] == ["--tighten-ceilings"]:
        with source_scan():
            return tighten_ceilings(load_workspace(ROOT))
    if sys.argv[1:] == ["--full-scan"]:
        return check_root(ROOT, full=True)
    if sys.argv[1:]:
        print(
            "usage: check_workspace.py [--standalone-packages | --wasm-libraries | --tighten-ceilings | --full-scan]",
            file=sys.stderr,
        )
        return 2
    return check_root(ROOT)


if __name__ == "__main__":
    raise SystemExit(main())
