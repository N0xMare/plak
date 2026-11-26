#!/usr/bin/env python3
"""Validate workspace layering, security, linkage, SQL trace, loop and hygiene policy.

Replaces the legacy 6,000-line Python AST/cache linter with a lean, uncached
policy checker that runs in under one second while preserving every real
architectural, security, linkage and hygiene invariant.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tomllib
from collections import deque
from collections.abc import Iterable
from concurrent.futures import ThreadPoolExecutor
from functools import lru_cache
from pathlib import Path
from typing import Any, NamedTuple

sys.path.insert(0, str(Path(__file__).resolve().parent))

import vcs_listing  # noqa: E402
from plakous_policy import (  # noqa: E402
    ABOVE_FACADE_LAYERS,
    PACKAGE_CLASSES,
    Package,
    Workspace,
    blank_rust_non_code,
    edge_permitted,
    expand_layout_template,
    layout_matches,
    load_workspace,
)

ROOT = Path(__file__).resolve().parents[2]
WORKSPACE_LICENSE = "MIT"
WORKSPACE_REPOSITORY = "https://github.com/N0xMare/plak"
VENDOR_PACKAGE = "plakous-codex-code-mode-protocol"
IMAGE_LEAF = "plakous-image"
PORTABLE_TARGET = "wasm32-unknown-unknown"
EXPECTED_FACADE_FEATURES = frozenset(
    {
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
)
SUPPORTED_COMPILE_FIXTURE_FILES: dict[str, frozenset[str]] = {
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
SUPPORTED_COMPILE_FIXTURES: dict[str, dict[str, Any]] = {
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
SUPPORTED_COMPILE_FIXTURE_PATHS = frozenset(
    f"tests/fixtures/{fixture_name}" for fixture_name in SUPPORTED_COMPILE_FIXTURES
)

PORTABLE_CRATES = (
    "plakous-model",
    "plakous-runner-protocol",
    "plakous-gateway-protocol",
    "plakous-image",
    "plakous-tools",
    "plakous-agent",
)
PORTABLE_DEFAULT_LIBRARIES = ("plakous-store",)
BANNED_TOKIO_FEATURES = frozenset({"fs", "net", "process", "rt-multi-thread", "signal"})
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

SourceKey = tuple[str, str]
FORBIDDEN_CLI_SYMBOLS = frozenset(
    {"CodexAuthBroker", "CodexLoginStore", "LoginCommand", "OpenCodeXaiLoginStore"}
)
FORBIDDEN_CLI_STRINGS = frozenset({"--api-key", "--base-url", "--endpoint", "--origin", "--token"})
FORBIDDEN_CLI_FOREIGN_FUNCTIONS = frozenset(
    {
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
)
FORBIDDEN_PRODUCT_SESSION_AUTHORITY_IDENTIFIERS = frozenset(
    {
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
)
FORBIDDEN_PLAKD_TERMINAL_DEPS = ("ratatui", "crossterm", "pulldown-cmark", "syntect", "arboard")
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
    ("plakd", "src/commands/host_install.rs"),
    ("plakd", "src/commands/login_host.rs"),
    ("plakd", "src/commands/runner.rs"),
    ("plakd", "src/commands/runner/dial.rs"),
    ("plakd", "src/commands/runner/dial/children.rs"),
    ("plakd", "src/commands/runner/local/start.rs"),
    ("plakd", "src/commands/support.rs"),
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
INCLUDED_FILE = re.compile(
    r'include_(?:str|bytes)!\s*\(\s*(?:concat!\s*\(\s*env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*)?"([^"]+)"'
)

PRODUCT_EXECUTABLES = ("plakd", "plakous-js-host", "plakous-py-host")
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
PYTHON_RUNTIME_SYMBOLS = re.compile(
    r"(?<![0-9])(?:11micro_monty|10mm_compile|7mm_core|9mm_engine|8mm_parse|9mm_stdlib)"
    r"(?=[0-9A-Z_]|$)"
)
TRACING_FREE_PACKAGES = ("plakous-agent", "plakous-model")
FORBIDDEN_NEUTRAL_DEPENDENCY_PARTS = ("anthropic", "gemini", "openai", "openrouter", "xai")
PROVIDER_NAMES = frozenset({"anthropic", "gemini", "openai", "openrouter", "xai"})
PRODUCT_ACCOUNT_NAMES = frozenset({"claude", "codex", "grok", "supergrok"})
PRODUCT_ACCOUNT_NAME_EXEMPTIONS: dict[tuple[str, str], set[str]] = {
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
    ("plakous-tools", "src/coding_presentation.rs"): {"codex", "grok", "claude"},
    ("plakous-tools", "src/exposure/projection.rs"): {"codex", "grok", "claude"},
    ("plakous-tools", "src/exposure/lowering.rs"): {"claude"},
    ("plakous-tools", "src/exposure/raising.rs"): {"claude", "grok"},
    ("plakous-tools", "src/exposure/raising/claude.rs"): {"claude"},
    ("plakous-tools", "src/capabilities.rs"): {"codex", "grok", "claude"},
    ("plakous-tools", "src/composition_markers.rs"): {"codex"},
}

COMMIT_TRACE_SOURCE_PREFIXES = {
    "plakous-store": "src/",
    "plakd": "src/store/",
    "plakous-recursive-subagents-store": "src/",
}

HYGIENE_HOME_PATH = re.compile(rb"(?<![\w$}.~/-])/(?:Users|home)/([A-Za-z0-9._-]+)")
HYGIENE_PRIVATE_NAMES_FILE = "docs/local/hygiene-private-names.txt"
HYGIENE_NAME_RUN = re.compile(rb"[A-Za-z0-9_.-]+")
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


class RustToken(NamedTuple):
    """One significant Rust token; comments are absent."""

    kind: str
    value: str


RUST_IDENTIFIER = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*")
CFG_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\(\s*(?P<predicate>[^]]+)\s*\)\s*\]", re.DOTALL)
CFG_TEST_BLOCK_ITEM = re.compile(
    r"(?P<attributes>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?"
    r"(?:unsafe\s+|async\s+|const\s+)?"
    r"(?:mod|fn|struct|enum|trait|impl|union)\b[^{;]*\{",
    re.DOTALL,
)
CFG_TEST_SEMICOLON_ITEM = re.compile(
    r"(?P<attributes>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?"
    r"(?:mod|use|type|const|static|extern\s+crate)\b[^;{]*;",
    re.DOTALL,
)
_FAST_TOKEN = re.compile(
    r"""
    (?P<ws>\s+)
    | (?P<line_comment>//[^\n]*)
    | (?P<block_comment>/\*)
    | (?P<raw>[bc]?r(?P<hashes>\#*)\")
    | (?P<str>(?:[bc])?\"(?:\\.|[^\"\\])*\")
    | (?P<char>b?'(?:\\.|[^'\\])')
    | (?P<ident>(?:r\#)?[A-Za-z_][A-Za-z0-9_]*)
    | (?P<colon2>::)
    | (?P<punct>.)
    """,
    re.VERBOSE | re.DOTALL,
)
_SIMPLE_TOKEN = re.compile(
    r"""
    \s+
    | //[^\n]*
    | b?'(?:\\.|[^'\\])'
    | (?P<str>(?:[bc])?\"(?:\\.|[^\"\\])*\")
    | (?P<ident>(?:r\#)?[A-Za-z_][A-Za-z0-9_]*)
    | (?P<colon2>::)
    | (?P<punct>[^\sA-Za-z_"/]|/(?!/))
    """,
    re.VERBOSE | re.DOTALL,
)
_RAW_STRING_HINT = re.compile(r'[bc]?r#*"')


def cfg_implies_test(predicate: str) -> bool:
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
    for index, ch in enumerate(body):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth < 0:
                return False
        elif ch == "," and depth == 0:
            children.append(body[start:index])
            start = index + 1
    if depth:
        return False
    if body[start:].strip():
        children.append(body[start:])
    implied = [cfg_implies_test(child) for child in children]
    return any(implied) if match.group(1) == "all" else bool(implied) and all(implied)


def _block_end(masked: str, opening: int) -> int:
    right = {"{": "}", "(": ")", "[": "]"}[masked[opening]]
    left = masked[opening]
    depth = 0
    for index in range(opening, len(masked)):
        ch = masked[index]
        if ch == left:
            depth += 1
        elif ch == right:
            depth -= 1
            if depth == 0:
                return index + 1
    return len(masked)


def production_rust_source(source: str) -> str:
    """Blank `#[cfg(test)]` items without truncating later production items."""
    if "cfg" not in source or "test" not in source or not CFG_ATTRIBUTE.search(source):
        return source
    masked = blank_rust_non_code(source)
    spans: list[tuple[int, int]] = []
    cursor = 0
    while match := CFG_TEST_BLOCK_ITEM.search(masked, cursor):
        cursor = match.start() + 1
        if any(
            cfg_implies_test(g.group("predicate"))
            for g in CFG_ATTRIBUTE.finditer(match.group("attributes"))
        ):
            closing = _block_end(masked, match.end() - 1)
            spans.append((match.start(), closing))
            cursor = closing
    cursor = 0
    while match := CFG_TEST_SEMICOLON_ITEM.search(masked, cursor):
        cursor = match.start() + 1
        if any(
            cfg_implies_test(g.group("predicate"))
            for g in CFG_ATTRIBUTE.finditer(match.group("attributes"))
        ):
            cursor = match.end()
            if not any(s <= match.start() < e for s, e in spans):
                spans.append((match.start(), match.end()))
    if not spans:
        return source
    production = list(source)
    for start, end in spans:
        for index in range(start, end):
            if production[index] not in "\r\n":
                production[index] = " "
    return "".join(production)


def _decode_rust_string(value: str) -> str:
    if "\\" not in value:
        return value
    escapes = {"0": "\0", "n": "\n", "r": "\r", "t": "\t", "\\": "\\", '"': '"'}
    out: list[str] = []
    index = 0
    while index < len(value):
        if value[index] != "\\":
            out.append(value[index])
            index += 1
            continue
        index += 1
        if index >= len(value):
            out.append("\\")
            break
        esc = value[index]
        index += 1
        if esc in escapes:
            out.append(escapes[esc])
        elif esc == "x" and re.fullmatch(r"[0-9A-Fa-f]{2}", value[index : index + 2]):
            out.append(chr(int(value[index : index + 2], 16)))
            index += 2
        elif esc == "u" and index < len(value) and value[index] == "{":
            close = value.find("}", index + 1)
            digits = value[index + 1 : close].replace("_", "") if close != -1 else ""
            if close != -1 and re.fullmatch(r"[0-9A-Fa-f]{1,6}", digits):
                out.append(chr(int(digits, 16)))
                index = close + 1
            else:
                out.extend(("\\u", value[index:]))
                break
        elif esc in "\r\n":
            if esc == "\r" and index < len(value) and value[index] == "\n":
                index += 1
            while index < len(value) and value[index] in " \t\r\n":
                index += 1
        else:
            out.extend(("\\", esc))
    return "".join(out)


def all_rust_tokens(source: str) -> list[RustToken]:
    """Fast regex-backed Rust tokenizer (comments absent, test items included)."""
    if "/*" not in source and _RAW_STRING_HINT.search(source) is None:
        tokens: list[RustToken] = []
        append = tokens.append
        for match in _SIMPLE_TOKEN.finditer(source):
            kind = match.lastgroup
            if kind is None:
                continue
            if kind == "ident":
                val = match.group(0)
                append(RustToken("identifier", val[2:] if val.startswith("r#") else val))
            elif kind == "str":
                raw = match.group(0)
                quote = raw.index('"')
                append(RustToken("string", _decode_rust_string(raw[quote + 1 : -1])))
            elif kind == "colon2":
                append(RustToken("punct", "::"))
            else:
                append(RustToken("punct", match.group(0)))
        return tokens

    tokens = []
    pos = 0
    length = len(source)
    while pos < length:
        match = _FAST_TOKEN.match(source, pos)
        assert match is not None
        kind = match.lastgroup
        pos = match.end()
        if kind in ("ws", "line_comment", "char"):
            continue
        if kind == "block_comment":
            depth = 1
            while pos < length and depth:
                if source.startswith("/*", pos):
                    depth += 1
                    pos += 2
                elif source.startswith("*/", pos):
                    depth -= 1
                    pos += 2
                else:
                    pos += 1
            continue
        if kind == "raw":
            terminator = '"' + match.group("hashes")
            closing = source.find(terminator, pos)
            if closing == -1:
                tokens.append(RustToken("string", source[pos:]))
                break
            tokens.append(RustToken("string", source[pos:closing]))
            pos = closing + len(terminator)
            continue
        if kind == "str":
            raw = match.group(0)
            quote = raw.index('"')
            tokens.append(RustToken("string", _decode_rust_string(raw[quote + 1 : -1])))
            continue
        if kind == "ident":
            tokens.append(RustToken("identifier", match.group(0).removeprefix("r#")))
            continue
        if kind == "colon2":
            tokens.append(RustToken("punct", "::"))
            continue
        tokens.append(RustToken("punct", match.group(0)))
    return tokens


def rust_tokens(source: str) -> list[RustToken]:
    return all_rust_tokens(production_rust_source(source))


def _inline_modules(tokens: list[RustToken]) -> list[tuple[str, ...]]:
    """For each token, the inline `mod name { ... }` blocks it sits in."""
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


def declared_test_module_paths(files: dict[str, str], crate_roots: set[str]) -> set[str]:
    """Package-relative external module files reachable only under `#[cfg(test)]`."""
    mod_decl = re.compile(
        r"(?P<attrs>(?:#\s*\[[^\]]*\]\s*)*)"
        r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*;",
        re.DOTALL,
    )
    inline_mod = re.compile(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{")
    path_attr = re.compile(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]')

    def resolve_child(
        parent: str,
        inline: tuple[str, ...],
        name: str,
        explicit: str | None,
        via_path: bool,
    ) -> str | None:
        parent_path = Path(parent)
        base_dir = (
            parent_path.parent
            if via_path or parent_path.name in {"lib.rs", "main.rs", "mod.rs"}
            else parent_path.with_suffix("")
        )
        for seg in inline:
            base_dir = base_dir / seg
        candidates = (
            [(base_dir / explicit).as_posix(), (parent_path.parent / explicit).as_posix()]
            if explicit is not None
            else [(base_dir / f"{name}.rs").as_posix(), (base_dir / name / "mod.rs").as_posix()]
        )
        for cand in candidates:
            norm = os.path.normpath(cand).replace("\\", "/")
            if norm in files:
                return norm
        return None

    def inline_at(masked: str, pos: int) -> tuple[str, ...]:
        stack: list[tuple[str, int]] = []
        depth = 0
        for match in re.finditer(r"(\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{)|([{}])", masked[:pos]):
            if match.group(2):
                depth += 1
                stack.append((match.group(2), depth))
            elif match.group(3) == "{":
                depth += 1
            else:
                if stack and stack[-1][1] == depth:
                    stack.pop()
                depth = max(0, depth - 1)
        return tuple(name for name, _ in stack)

    prod_reached: set[str] = set()
    test_reached: set[str] = set()
    queue: deque[tuple[str, bool, bool]] = deque(
        (root, False, False) for root in sorted(crate_roots) if root in files
    )
    seen: set[tuple[str, bool, bool]] = set()
    while queue:
        current, in_test, via_path = queue.popleft()
        if (current, in_test, via_path) in seen:
            continue
        seen.add((current, in_test, via_path))
        (test_reached if in_test else prod_reached).add(current)
        source = files[current]
        if "mod " not in source and "mod\n" not in source and "mod\t" not in source:
            continue
        masked = blank_rust_non_code(source)
        has_inline = bool(inline_mod.search(masked))
        for match in mod_decl.finditer(masked):
            attrs = match.group("attrs")
            is_test = in_test or any(
                cfg_implies_test(g.group("predicate")) for g in CFG_ATTRIBUTE.finditer(attrs)
            )
            raw_attrs = source[match.start("attrs") : match.end("attrs")]
            explicit_match = path_attr.search(raw_attrs)
            explicit_val = explicit_match.group(1) if explicit_match else None
            inline = inline_at(masked, match.start()) if has_inline else ()
            child = resolve_child(
                current,
                inline,
                match.group("name"),
                explicit_val,
                via_path,
            )
            if child is not None:
                queue.append((child, is_test, explicit_val is not None))
    return test_reached - prod_reached


# ---------------------------------------------------------------------------
# Workspace & layering validators
# ---------------------------------------------------------------------------


def validate_local_production_lints(manifests: dict[str, dict[str, Any]]) -> list[str]:
    errors: list[str] = []
    for name, manifest in sorted(manifests.items()):
        if name == VENDOR_PACKAGE:
            if manifest.get("lints", {}).get("rust", {}).get("unsafe_code") != "forbid":
                errors.append(f'{name}: unsafe_code must be explicitly "forbid"')
            continue
        if manifest.get("lints") != {"workspace": True}:
            errors.append(f"{name}: workspace lint inheritance is required")
    return errors


def validate_package_metadata(workspace: Workspace) -> list[str]:
    errors: list[str] = []
    policy = workspace.policy
    for package in sorted(workspace.packages.values(), key=lambda p: p.name):
        if package.layer not in policy.layers:
            errors.append(f"{package.name}: unknown layer {package.layer!r}")
        elif package.package_class not in PACKAGE_CLASSES:
            errors.append(f"{package.name}: unknown class {package.package_class!r}")
        elif package.package_class not in policy.classes.get(package.layer, ()):
            errors.append(
                f"{package.name}: class {package.package_class!r} not legal for layer {package.layer!r}"
            )
    return errors


def validate_layout(workspace: Workspace) -> list[str]:
    errors: list[str] = []
    for package in sorted(workspace.packages.values(), key=lambda p: p.name):
        if package.layer in workspace.policy.layout and not layout_matches(
            workspace.policy, package
        ):
            expected = [
                expand_layout_template(t, package) for t in workspace.policy.layout[package.layer]
            ]
            errors.append(
                f"{package.name}: directory {package.directory!r} does not match {expected!r}"
            )
    return errors


def internal_production_edges(workspace: Workspace) -> dict[str, set[str]]:
    return {
        name: pkg.production_dependency_names() & set(workspace.packages)
        for name, pkg in workspace.packages.items()
    }


def validate_layer_edges(workspace: Workspace) -> list[str]:
    errors: list[str] = []
    policy = workspace.policy
    edges = internal_production_edges(workspace)
    for src_name in sorted(edges):
        src = workspace.packages[src_name]
        for dst_name in sorted(edges[src_name]):
            dst = workspace.packages[dst_name]
            if not edge_permitted(policy, src, dst):
                errors.append(
                    f"{src_name} ({src.layer}) -> {dst_name} ({dst.layer}): edge outside layer matrix"
                )
    for src_name, targets in sorted(policy.edge_exceptions.items()):
        if src_name not in workspace.packages:
            errors.append(f"edge-exception names unknown package {src_name!r}")
            continue
        for dst_name in targets:
            if dst_name not in workspace.packages:
                errors.append(f"edge-exception {src_name} -> {dst_name} names unknown package")
            elif dst_name not in edges[src_name]:
                errors.append(f"edge-exception {src_name} -> {dst_name} is stale: no such edge")
    return errors


def validate_harness_pack_independence(workspace: Workspace) -> list[str]:
    errors: list[str] = []
    edges = internal_production_edges(workspace)
    for src_name in workspace.policy.harness_pack_independent:
        src = workspace.packages.get(src_name)
        if src is None or src.layer != "harness":
            errors.append(f"harness-pack-independent names invalid package {src_name!r}")
            continue
        for dst_name in sorted(edges.get(src_name, set())):
            if workspace.packages[dst_name].layer == "harness":
                errors.append(
                    f"{src_name} -> {dst_name}: instruction text may not depend on a harness pack"
                )
    return errors


def validate_neutral_dependencies(workspace: Workspace) -> list[str]:
    errors: list[str] = []
    for pkg in sorted(workspace.packages.values(), key=lambda p: p.name):
        if not pkg.neutral:
            continue
        for name in sorted(pkg.dependency_names()):
            low = name.lower()
            if any(part in low for part in FORBIDDEN_NEUTRAL_DEPENDENCY_PARTS):
                errors.append(f"{pkg.name}: provider dependency forbidden: {low}")
    return errors


def validate_image_leaf(workspace: Workspace) -> list[str]:
    image = workspace.packages.get(IMAGE_LEAF)
    tools = workspace.packages.get("plakous-tools")
    if image is None or tools is None:
        return [f"{IMAGE_LEAF} and plakous-tools must both be workspace members"]
    errors = [
        f"{IMAGE_LEAF}: depends on workspace package {dep['name']!r}; "
        "the image leaf names no workspace package"
        for dep in image.metadata.get("dependencies", [])
        if dep["name"] in workspace.packages
    ]
    edges = [dep for dep in tools.metadata.get("dependencies", []) if dep["name"] == IMAGE_LEAF]
    if len(edges) != 1:
        errors.append(f"plakous-tools: expected exactly one {IMAGE_LEAF} dependency")
    for dep in edges:
        if (
            dep.get("kind") is not None
            or dep.get("target") is not None
            or dep.get("optional")
            or dep.get("uses_default_features")
            or dep.get("features")
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


def fixture_name_for(package: Package) -> str:
    return "facade-consumer" if package.layer == "facade" else f"{package.short}-consumer"


def supported_compile_fixtures(workspace: Workspace) -> dict[str, dict[str, Any]]:
    contracts: dict[str, dict[str, Any]] = {}
    for fixture_name, contract in SUPPORTED_COMPILE_FIXTURES.items():
        fixture_root = f"tests/fixtures/{fixture_name}"
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


def non_target_file_inventory(root: Path) -> tuple[set[str], tuple[str, ...]]:
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


def validate_supported_compile_fixture(
    fixture_name: str,
    manifest: dict[str, Any],
    files: set[str],
    expected_contract: dict[str, Any],
    inventory_errors: tuple[str, ...] = (),
) -> list[str]:
    errors = [f"{fixture_name}: {error}" for error in inventory_errors]
    if manifest != expected_contract["manifest"]:
        errors.append(
            f"{fixture_name}: normalized manifest differs from the supported fixture contract"
        )
    if files != expected_contract["files"]:
        errors.append(
            f"{fixture_name}: non-target files {sorted(files)!r}, "
            f"expected {sorted(expected_contract['files'])!r}"
        )
    return errors


def validate_workspace_excludes(
    root_workspace: dict[str, Any],
    supported_fixture_paths: Iterable[str] = SUPPORTED_COMPILE_FIXTURE_PATHS,
) -> list[str]:
    """Keep operator state outside Cargo without hiding unreviewed packages."""
    excludes = set(root_workspace.get("exclude", []))
    allowed = set(supported_fixture_paths) | {".plak"}
    errors: list[str] = []
    if ".plak" not in excludes:
        errors.append("workspace must exclude .plak operator state and fork worktrees")
    unknown_excludes = excludes - allowed
    if unknown_excludes:
        errors.append(
            f"workspace excludes unsupported nested packages: {sorted(unknown_excludes)!r}"
        )
    return errors


def validate_package_local_includes(package_name: str, sources: dict[str, str]) -> list[str]:
    """Every `include_str!`/`include_bytes!` stays inside its own package."""
    errors: list[str] = []
    for relative, text in sorted(sources.items()):
        if "include_str!" not in text and "include_bytes!" not in text:
            continue
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


# ---------------------------------------------------------------------------
# Security, facade-boundary, and host/bin authority validators
# ---------------------------------------------------------------------------


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


def _identifier_values(tokens: list[RustToken]) -> list[str]:
    return [token.value for token in tokens if token.kind == "identifier"]


def _concat_values(tokens: list[RustToken]) -> set[str]:
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
            elif value in {")", "]", "}"}:
                opening = {")": "(", "]": "[", "}": "{"}[value]
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


def _foreign_function_names(tokens: list[RustToken]) -> set[str]:
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


def _environment_path_names(tokens: list[RustToken]) -> set[str]:
    return {
        tokens[index].value
        for index in range(2, len(tokens))
        if tokens[index].kind == "identifier"
        and tokens[index].value in {"var", "var_os", "vars", "vars_os"}
        and tokens[index - 1].value == "::"
        and tokens[index - 2] == RustToken("identifier", "env")
    }


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
            if (
                index == 0
                or statement[index - 1].value in {"{", ","}
                and (index < 2 or statement[index - 2].value != "::")
            ):
                crates.add(token.value.replace("_", "-"))
    return crates


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


def validate_product_session_authority_sources(
    package: str,
    sources: dict[str, str],
    tokens_by_file: dict[str, list[RustToken]] | None = None,
) -> list[str]:
    """Forbid known Plak-owned canonical context, compaction, and fork seams."""
    errors: list[str] = []
    for path, source in sorted(sources.items()):
        tokens = tokens_by_file[path] if tokens_by_file is not None else rust_tokens(source)
        forbidden = sorted(
            set(_identifier_values(tokens)) & FORBIDDEN_PRODUCT_SESSION_AUTHORITY_IDENTIFIERS
        )
        if forbidden:
            errors.append(
                f"{package}:{path}: Plak contains competing session authority identifiers: {forbidden!r}"
            )
    return errors


def validate_product_update_hook_sources(
    sources: dict[str, str],
    tokens_by_file: dict[str, list[RustToken]] | None = None,
) -> list[str]:
    """Allow only the submission invalidator and its exclusive trace alternative."""
    update_hooks: list[str] = []
    trace_hooks: list[str] = []
    update = RustToken("identifier", "update_hook")
    trace = rust_tokens("commit_trace::install")
    for path, source in sorted(sources.items()):
        if "update_hook" not in source and "commit_trace" not in source:
            continue
        tokens = tokens_by_file[path] if tokens_by_file is not None else rust_tokens(source)
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


def validate_product_home_discovery(package: str, sources: dict[str, str]) -> list[str]:
    """No host library may discover the product home for its caller."""
    if package == "plakd":
        return []
    errors: list[str] = []
    for path, source in sorted(sources.items()):
        if "PlakPaths::from_env" in source:
            errors.append(
                f"{package}:{path}: PlakPaths::from_env is composition-root only; "
                "the product home is a fact a caller states, not one a library discovers"
            )
    return errors


def validate_facade_only_sources(
    package_name: str,
    sources: dict[str, str],
    permitted: Iterable[str] = (),
    tokens_by_file: dict[str, list[RustToken]] | None = None,
) -> list[str]:
    """Host/bin production sources never spell `plakous_<x>::`; the facade is the surface."""
    errors: list[str] = []
    permitted_crates = set(permitted)
    for path, contents in sorted(sources.items()):
        if "plakous_" not in contents:
            continue
        tokens = tokens_by_file[path] if tokens_by_file is not None else rust_tokens(contents)
        crates = _component_crate_paths(tokens) - permitted_crates
        if crates:
            errors.append(
                f"{package_name}:{path}: names Plakous component crates {sorted(crates)!r}; "
                "host/bin sources reach L0 only through `plakous`"
            )
    return errors


def validate_cli_security_sources(
    package: str,
    sources: dict[str, str],
    tokens_by_file: dict[str, list[RustToken]] | None = None,
) -> list[str]:
    """Enforce one host/bin package's reviewed process, environment, and authority boundaries."""
    errors: list[str] = []
    if tokens_by_file is None:
        tokens_by_file = {path: rust_tokens(source) for path, source in sources.items()}
    all_tokens = [token for tokens in tokens_by_file.values() for token in tokens]
    identifiers = set(_identifier_values(all_tokens))
    strings = (
        set().union(*(_significant_strings(tokens) for tokens in tokens_by_file.values()))
        if tokens_by_file
        else set()
    )

    for symbol in sorted(FORBIDDEN_CLI_SYMBOLS & identifiers):
        errors.append(f"plakd CLI contains forbidden delegated-login symbol: {symbol}")
    for value in sorted(FORBIDDEN_CLI_STRINGS & strings):
        errors.append(f"plakd CLI contains forbidden delegated-login option: {value}")
    for value in strings:
        lowered = value.lower().replace("\\", "/")
        if re.search(r"(?:^|/)\.(?:codex)(?:/|$)|(?:^|/)opencode(?:/|$)", lowered):
            errors.append("plakd CLI contains forbidden delegated-login storage/owner locator")
            break
    if any(re.search(r"(?:^|/)proc/+self/+environ(?:$|/)", value.lower()) for value in strings):
        errors.append("plakd CLI contains forbidden /proc/self/environ access")

    for path, tokens in sorted(tokens_by_file.items()):
        site = (package, path)
        process_import = command_import = False
        for statement in _use_statements(tokens):
            names = _identifier_values(statement)
            if "env" in names and (
                names != ["std", "env"] or any(token.value in {"*", "as"} for token in statement)
            ):
                errors.append(
                    f"{package}:{path}: plakd CLI must import std::env as a module, not ambient readers"
                )
            if "Command" in names and "plakous" in names:
                errors.append(
                    f"{package}:{path}: plakd CLI imports a forbidden Command process capability"
                )
            for tree_names in _use_tree_paths(statement):
                if tree_names[:2] == ["std", "process"]:
                    process_import = True
                    command_import |= len(tree_names) == 2 or tree_names[2] in {
                        "Command",
                        "self",
                        "*",
                    }
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

    return errors


# ---------------------------------------------------------------------------
# Portable wasm32 closure fence
# ---------------------------------------------------------------------------


class _TreeNode(NamedTuple):
    depth: int
    name: str
    version: str
    features: tuple[str, ...]
    duplicate: bool
    proc_macro: bool = False


def portable_tree_args(package: str, *, all_features: bool) -> list[str]:
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
    if node.name in BANNED_NATIVE_CRATES:
        return f"{node.name} v{node.version}"
    if node.name == "tokio":
        banned = sorted(set(node.features) & BANNED_TOKIO_FEATURES)
        if banned:
            label = "feature" if len(banned) == 1 else "features"
            return f"tokio v{node.version} {label} {', '.join(banned)}"
    return None


def closure_findings(package: str, resolution: str, nodes: list[_TreeNode]) -> list[str]:
    """Name each banned dependency once, with the path that pulls it in."""
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
    """Fail when a portable crate's normal closure pulls in native-only deps for wasm32."""
    lock = workspace.root / "Cargo.lock"
    try:
        lock.read_bytes()
    except OSError as error:
        return [f"Cargo.lock: {error}"]

    errors: list[str] = []
    jobs: list[tuple[str, str, list[str]]] = []
    for package in PORTABLE_CRATES:
        if package not in workspace.packages:
            errors.append(f"{package}: portable crate is not a workspace member")
            continue
        for resolution, all_features in (("default", False), ("all-features", True)):
            jobs.append(
                (package, resolution, portable_tree_args(package, all_features=all_features))
            )

    def run_one(job: tuple[str, str, list[str]]) -> list[str]:
        package, resolution, args = job
        proc = subprocess.run(
            ["cargo", "tree", *args],
            cwd=workspace.root,
            text=True,
            capture_output=True,
            check=False,
        )
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip().splitlines()[:1]
            suffix = f": {detail[0]}" if detail else ""
            return [f"{package} [{resolution}]: cargo tree failed{suffix}"]
        nodes, parse_errors = parse_feature_tree(proc.stdout)
        if parse_errors:
            return [f"{package} [{resolution}]: {item}" for item in parse_errors]
        return closure_findings(package, resolution, nodes)

    with ThreadPoolExecutor(max_workers=4) as pool:
        for result in pool.map(run_one, jobs):
            errors.extend(result)
    return errors


def supported_target_atomic_polyfill_errors(root: Path = ROOT) -> list[str]:
    """Keep the unsupported-target atomic-polyfill exception out of supported graphs."""

    def check_target(target: str) -> list[str]:
        proc = subprocess.run(
            [
                "cargo",
                "tree",
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
            cwd=root,
            text=True,
            capture_output=True,
            check=False,
        )
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip().splitlines()[:1]
            suffix = f": {detail[0]}" if detail else ""
            return [f"{target}: cargo tree failed{suffix}"]
        if re.search(r"(?m)^atomic-polyfill v", proc.stdout):
            return [
                f"{target}: atomic-polyfill is reachable; its unsupported-target "
                "maintenance exception no longer applies"
            ]
        return []

    errors: list[str] = []
    with ThreadPoolExecutor(max_workers=2) as pool:
        for result in pool.map(check_target, ("x86_64-unknown-linux-musl", "aarch64-apple-darwin")):
            errors.extend(result)
    return errors


_ANY_PROVIDER_HINT = re.compile(
    r"anthropic|gemini|openai|openrouter|xai|claude|codex|grok|supergrok", re.IGNORECASE
)


def validate_neutral_sources(package_name: str, sources: dict[str, str]) -> list[str]:
    """Forbid provider and product-account names in neutral packages' production code."""
    errors: list[str] = []
    checked = PROVIDER_NAMES | PRODUCT_ACCOUNT_NAMES
    for path, contents in sorted(sources.items()):
        if not _ANY_PROVIDER_HINT.search(contents):
            continue
        allowed = PRODUCT_ACCOUNT_NAME_EXEMPTIONS.get((package_name, path), set())
        tokens = rust_tokens(contents)
        found = {
            tok.value.lower()
            for tok in tokens
            if tok.kind == "identifier" and tok.value.lower() in checked
        }
        for tok in tokens:
            if tok.kind != "string":
                continue
            low = tok.value.lower()
            for provider in checked:
                if re.search(rf"(?<![a-z0-9]){re.escape(provider)}(?![a-z0-9])", low):
                    found.add(provider)
        for provider in sorted(found - allowed):
            errors.append(f"{package_name}:{path}: provider name {provider!r} in neutral package")
    return errors


# ---------------------------------------------------------------------------
# Engine / tracing / test-support graph isolation & built artifact linkage
# ---------------------------------------------------------------------------


def _cargo_resolve_graph(root: Path) -> dict[str, Any]:
    raw = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--offline"],
        cwd=root,
        text=True,
    )
    return json.loads(raw)


def _normal_reachable(graph: dict[str, Any], root_pkg_name: str) -> set[str]:
    id_to_name = {pkg["id"]: pkg["name"] for pkg in graph["packages"]}
    name_to_id = {pkg["name"]: pkg["id"] for pkg in graph["packages"] if pkg.get("source") is None}
    nodes = {node["id"]: node for node in graph["resolve"]["nodes"]}
    start = name_to_id.get(root_pkg_name)
    if start is None:
        return set()
    visited: set[str] = set()
    queue: deque[str] = deque([start])
    while queue:
        cur = queue.popleft()
        if cur in visited:
            continue
        visited.add(cur)
        for dep in nodes.get(cur, {}).get("deps", []):
            kinds = dep.get("dep_kinds") or [{"kind": None}]
            if any(k.get("kind") is None for k in kinds):
                queue.append(dep["pkg"])
    return {id_to_name[pid] for pid in visited if pid in id_to_name}


def graph_isolation_errors(graph: dict[str, Any]) -> list[str]:
    """Verify engine isolation, tracing-free neutral core, and no workspace cycles."""
    errors: list[str] = []
    for pkg_name, msg in (
        ("plakous", "plakous must not link rquickjs; Code Mode uses the sidecar"),
        ("plakd", "plakd must not link rquickjs; Code Mode uses plakous-js-host"),
    ):
        reached = _normal_reachable(graph, pkg_name)
        if "rquickjs" in reached or "rquickjs-sys" in reached:
            errors.append(msg)
        if "micro-monty" in reached or "mm-engine" in reached:
            errors.append(f"{pkg_name} must not link micro-monty; use plakous-py-host")
    if "rquickjs" not in _normal_reachable(graph, "plakous-js-runtime"):
        errors.append("plakous-js-runtime must link rquickjs")
    if "micro-monty" not in _normal_reachable(graph, "plakous-py-worker"):
        errors.append("plakous-py-worker must link micro-monty")
    for neutral in TRACING_FREE_PACKAGES:
        reached = _normal_reachable(graph, neutral)
        for forbidden in ("tracing", "opentelemetry"):
            if forbidden in reached:
                errors.append(f"{neutral} must not link {forbidden}")
    return errors


def symbol_tool() -> list[str] | None:
    for candidate in (["llvm-nm"], ["nm"]):
        if shutil.which(candidate[0]) is not None:
            return candidate
    return None


def cargo_target_dir(root: Path) -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR") or "target")
    return target if target.is_absolute() else root / target


def built_product_artifacts(root: Path) -> dict[str, Path]:
    release = cargo_target_dir(root) / "release"
    return {
        name: candidate for name in PRODUCT_EXECUTABLES if (candidate := release / name).is_file()
    }


def engine_symbol_markers(symbol_output: str) -> set[str]:
    return {marker for marker in RQUICKJS_ENGINE_SYMBOL_MARKERS if marker in symbol_output}


def artifact_linkage_findings(
    symbols: dict[str, str | None], digests: dict[str, str], tool_name: str = "nm"
) -> list[str]:
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
                f"plakd links the QuickJS engine ({sorted(found)!r}); "
                "JavaScript evaluation must stay in plakous-js-host"
            )
        if name == "plakous-js-host" and not found:
            errors.append("plakous-js-host does not link the QuickJS engine")
        if name == "plakous-py-host" and found:
            errors.append(f"plakous-py-host links the QuickJS engine ({sorted(found)!r})")
    if len(set(digests.values())) != len(digests):
        errors.append(f"the product executables are not separate artifacts: {digests!r}")
    return errors


def newest_tracked_source_mtime(root: Path) -> float | None:
    newest: float | None = None
    for relative in vcs_listing.listed_files(root):
        if not (relative.endswith((".rs", "Cargo.toml")) or relative == "Cargo.lock"):
            continue
        try:
            stamp = (root / relative).stat().st_mtime
        except OSError:
            continue
        if newest is None or stamp > newest:
            newest = stamp
    return newest


def stale_release_artifacts(root: Path, artifacts: dict[str, Path]) -> list[str]:
    newest = newest_tracked_source_mtime(root)
    if newest is None:
        return []
    return sorted(name for name, path in artifacts.items() if path.stat().st_mtime < newest)


def verify_artifact_linkage(artifacts: dict[str, Path]) -> tuple[list[str], str]:
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
    digests = {
        name: hashlib.sha256(path.read_bytes()).hexdigest()
        for name, path in sorted(artifacts.items())
    }
    symbols: dict[str, str | None] = {}
    for name, path in sorted(artifacts.items()):
        result = subprocess.run([*tool, str(path)], text=True, capture_output=True, check=False)
        symbols[name] = None if result.returncode != 0 else result.stdout
    errors = artifact_linkage_findings(symbols, digests, tool[0])
    receipt = ", ".join(f"{name} sha256 {digest}" for name, digest in digests.items())
    return errors, f"artifact linkage checked: {receipt}"


def artifact_linkage_errors(root: Path = ROOT) -> tuple[list[str], str]:
    artifacts = built_product_artifacts(root)
    missing = [name for name in PRODUCT_EXECUTABLES if name not in artifacts]
    if missing:
        return [], f"artifact linkage not checked: release {', '.join(missing)} is not built"
    stale = stale_release_artifacts(root, artifacts)
    if stale:
        return (
            [],
            f"artifact linkage not checked: stale release build; {', '.join(stale)} is older than source",
        )
    return verify_artifact_linkage(artifacts)


# ---------------------------------------------------------------------------
# SQL RETURNING trace ban, tracked hygiene, and fixture shell loop bounds
# ---------------------------------------------------------------------------


def validate_commit_trace_edge(manifests: dict[str, dict[str, Any]]) -> list[str]:
    tree = manifests.get("plakous-recursive-subagents-store", {})
    dep = tree.get("dependencies", {}).get("plakous-store", {})
    if (
        dep.get("optional") is not True
        or dep.get("features", []) != []
        or sorted(tree.get("features", {}).get("test-support", []))
        != [
            "dep:plakous-store",
            "plakous-recursive-subagents/test-support",
            "plakous-store/test-support",
        ]
    ):
        return ["tree store's plakous-store edge must be optional and test-support-only"]
    return []


def validate_commit_trace_sql(package: str, sources: dict[str, str]) -> list[str]:
    prefix = COMMIT_TRACE_SOURCE_PREFIXES.get(package)
    if prefix is None:
        return []
    errors: list[str] = []
    for path, source in sorted(sources.items()):
        if not path.startswith(prefix) or "returning" not in source.lower():
            continue
        if any(
            tok.kind == "string" and re.search(r"\bRETURNING\b", tok.value, re.IGNORECASE)
            for tok in rust_tokens(source)
        ):
            errors.append(
                f"{package}:{path}: SQL RETURNING is forbidden by the Commit trace contract"
            )
    return errors


def load_hygiene_private_names(root: Path = ROOT) -> frozenset[bytes]:
    try:
        text = (root / HYGIENE_PRIVATE_NAMES_FILE).read_bytes()
    except FileNotFoundError:
        return frozenset()
    return frozenset(line.strip() for line in text.splitlines() if line.strip())


def hygiene_private_names(text: bytes, private_names: frozenset[bytes]) -> list[str]:
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
    relative: str, data: bytes, private_names: frozenset[bytes] = frozenset()
) -> list[str]:
    if b"\0" in data:
        return []
    has_home = b"/Users/" in data or b"/home/" in data
    has_private = bool(private_names) and bool(
        hygiene_private_names(b" ".join(set(HYGIENE_NAME_RUN.findall(data))), private_names)
    )
    if not has_home and not has_private:
        return []
    findings: list[str] = []
    for number, line in enumerate(data.split(b"\n"), start=1):
        if has_home:
            for match in HYGIENE_HOME_PATH.finditer(line):
                if match.group(1).decode() not in HYGIENE_HOME_PLACEHOLDERS:
                    findings.append(
                        f"{relative}:{number}: absolute home path {match.group(0).decode()!r}"
                    )
        if has_private:
            for name in hygiene_private_names(line, private_names):
                findings.append(f"{relative}:{number}: private host or tool name {name!r}")
    return findings


def validate_tracked_hygiene(
    root: Path = ROOT,
    private_names: frozenset[bytes] | None = None,
    *,
    listed: list[str] | None = None,
) -> list[str]:
    if private_names is None:
        private_names = load_hygiene_private_names(root)
    errors: list[str] = []
    for relative in vcs_listing.listed_files(root) if listed is None else listed:
        path = root / relative
        if path.is_symlink() or not path.is_file():
            continue
        for finding in hygiene_findings(relative, path.read_bytes(), private_names):
            errors.append(f"{finding}: tracked files must not carry owner-specific content")
    return errors


LOOP_KEYWORD = re.compile(r"(?<![\w$-])(?:while|until|for)(?![\w-])")
LOOP_HEAD = re.compile(
    r"(?P<kind>while|until)\b(?P<head>.*?)[;\n]\s*do(?![\w-])"
    r"|for\s+(?P<arith>\(\(.*?\)\))\s*[;\n]?\s*do(?![\w-])"
    r"|for\s+\w+(?:\s+in\b[^;\n]*)?\s*[;\n]\s*do(?![\w-])",
    re.S,
)
LOOP_WORD = re.compile(r"(?:(?<=[;\n])|(?<=[;&\n]\s))\s*(do|done)(?![\w-])")
COUNTER_TEST = re.compile(
    r'"?\$(?:\(\((?P<expr>[^)]*)\)\)|\{?(?P<var>\w+)\}?)"?\s+-(?P<op>le|lt|ge|gt|eq)\s+"?\d+"?'
)
EXIT_GUARD = re.compile(r"\s*\]\s*(?P<join>\|\||&&)\s*(?:exit|break|return)\b")
READ_HEAD = re.compile(r"^\s*(?:IFS=\S*\s+)?read\b")
ENDLESS_SOURCE = re.compile(r"\btail\s+(?:-\w*[fF]|--follow)|\byes\b")
FIXTURE_SLEEP = re.compile(
    r"(?<![\w-])sleep\s+(?:(?P<infinity>infinity)|(?P<value>\d+(?:\.\d+)?)(?P<unit>[smhd]?))(?![\w.])"
)
SLEEP_UNITS = {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}
MAX_FIXTURE_SLEEP_SECONDS = 600


@lru_cache(maxsize=64)
def _counter_step(name: str) -> re.Pattern[str]:
    step = rf"\b{name}\s*\+=\s*1\b|\b{name}\s*=\s*\$\(\(\s*\$?{name}\s*\+\s*1\s*\)\)"
    return re.compile(rf"{step}|\b{name}\+\+")


def _string_runs(tokens: list[RustToken]) -> list[str]:
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
    depth = 1
    for word in LOOP_WORD.finditer(text, start):
        depth += 1 if word.group(1) == "do" else -1
        if depth == 0:
            return word.end()
    return None


def _counted(loop_kind: str, head: str, body: str) -> bool:
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
            continue
        loops.append(loop)
    return loops


_FIXTURE_PREFILTER = re.compile(r"(?:\b(?:while|until|for)\b.*\bdone\b)|\bsleep\b", re.S)


def validate_fixture_loops(package_name: str, sources: dict[str, str]) -> list[str]:
    """Every shell loop a Rust fixture spawns ends within a bound."""
    errors: list[str] = []
    for path, source in sorted(sources.items()):
        if not _FIXTURE_PREFILTER.search(source):
            continue
        found: list[str] = []
        for text in _string_runs(all_rust_tokens(source)):
            for loop in _unbounded_loops(text):
                snippet = " ".join(loop.split())[:80]
                found.append(
                    f"{package_name}:{path}: unbounded shell loop in a fixture: {snippet!r}"
                )
            for sleep in FIXTURE_SLEEP.finditer(text):
                seconds = (
                    float("inf")
                    if sleep.group("infinity")
                    else float(sleep.group("value")) * SLEEP_UNITS[sleep.group("unit")]
                )
                if seconds > MAX_FIXTURE_SLEEP_SECONDS:
                    found.append(
                        f"{package_name}:{path}: fixture {sleep.group(0)!r} exceeds {MAX_FIXTURE_SLEEP_SECONDS} s"
                    )
        errors.extend(dict.fromkeys(found))
    return errors


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def _package_rust_files(root: Path, package: Package, listed: list[str]) -> dict[str, str]:
    prefix = f"{package.directory}/"
    result: dict[str, str] = {}
    for rel in listed:
        if rel.startswith(prefix) and rel.endswith(".rs"):
            path = root / rel
            if path.is_file() and not path.is_symlink():
                result[rel.removeprefix(prefix)] = path.read_text(encoding="utf-8")
    return result


def _production_src_files(rust_files: dict[str, str]) -> dict[str, str]:
    src_only = {k: v for k, v in rust_files.items() if k.startswith("src/")}
    test_only = declared_test_module_paths(
        {k.removeprefix("src/"): v for k, v in src_only.items()},
        {"lib.rs", "main.rs"},
    )
    return {k: v for k, v in src_only.items() if k.removeprefix("src/") not in test_only}


def _check_workspace(root: Path = ROOT) -> int:
    graph = _cargo_resolve_graph(root)
    workspace = load_workspace(root, graph=graph)
    packages = workspace.packages
    policy = workspace.policy
    errors: list[str] = []

    root_ws = workspace.root_manifest.get("workspace", {})
    if root_ws.get("resolver") != "3":
        errors.append("workspace resolver must be 3")
    fixtures = supported_compile_fixtures(workspace)
    errors.extend(
        validate_workspace_excludes(
            root_ws, {f"tests/fixtures/{fixture_name}" for fixture_name in fixtures}
        )
    )

    listed = vcs_listing.listed_files(root)
    errors.extend(validate_tracked_hygiene(root, listed=listed))

    manifests = {name: pkg.manifest for name, pkg in packages.items()}
    errors.extend(validate_local_production_lints(manifests))
    errors.extend(validate_commit_trace_edge(manifests))
    errors.extend(validate_package_metadata(workspace))
    errors.extend(validate_layout(workspace))
    errors.extend(validate_layer_edges(workspace))
    errors.extend(validate_harness_pack_independence(workspace))
    errors.extend(validate_neutral_dependencies(workspace))
    errors.extend(validate_image_leaf(workspace))

    for pkg in sorted(packages.values(), key=lambda item: item.name):
        if pkg.package_class != "supported" or pkg.layer in ABOVE_FACADE_LAYERS:
            continue
        if fixture_name_for(pkg) not in fixtures:
            errors.append(f"{pkg.name}: supported package lacks a consumer fixture contract")
    for fixture_name, expected_contract in fixtures.items():
        fixture_root = root / f"tests/fixtures/{fixture_name}"
        try:
            with (fixture_root / "Cargo.toml").open("rb") as stream:
                fixture_manifest = tomllib.load(stream)
        except OSError as error:
            errors.append(f"{fixture_name}: {error}")
            continue
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

    app = packages.get("plakd")
    if app is not None:
        targets = app.metadata.get("targets", [])
        binaries = [target for target in targets if "bin" in target["kind"]]
        if len(binaries) != 1 or binaries[0]["name"] != "plakd":
            errors.append("plakd must expose exactly one binary named plakd")
        libraries = [target for target in targets if "lib" in target["kind"]]
        if len(libraries) != 1 or libraries[0]["name"] != "plakd":
            errors.append("plakd must expose exactly one library named plakd")
        for dependency in FORBIDDEN_PLAKD_TERMINAL_DEPS:
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
        features = facade.metadata.get("features", {})
        actual_features = {name for name in features if not name.startswith("dep:")}
        if actual_features != EXPECTED_FACADE_FEATURES:
            errors.append(
                f"plakous: public features {sorted(actual_features)!r}, "
                f"expected {sorted(EXPECTED_FACADE_FEATURES)!r}"
            )
        if features.get("default") != []:
            errors.append("plakous: default feature set must remain empty")

    for name, pkg in sorted(packages.items()):
        meta = pkg.metadata
        if pkg.package_class == "repository-only" and meta.get("publish") != []:
            errors.append(f"{name}: repository-only packages must have publish = false")
        if name != VENDOR_PACKAGE:
            if meta.get("edition") != "2024":
                errors.append(f"{name}: edition must be 2024")
            if meta.get("rust_version") != "1.100.0":
                errors.append(f"{name}: rust-version must be 1.100.0")
            if meta.get("license") != WORKSPACE_LICENSE:
                errors.append(f"{name}: license expression is inconsistent")

        rust_files = _package_rust_files(root, pkg, listed)
        errors.extend(validate_fixture_loops(name, rust_files))
        errors.extend(validate_package_local_includes(name, rust_files))
        if name in COMMIT_TRACE_SOURCE_PREFIXES:
            errors.extend(validate_commit_trace_sql(name, rust_files))
        if name == "plakous-anthropic":
            errors.extend(
                validate_claude_oauth_state_comparison(rust_files.get("src/oauth.rs", ""))
            )
        if pkg.neutral:
            errors.extend(validate_neutral_sources(name, _production_src_files(rust_files)))
        if pkg.layer in ABOVE_FACADE_LAYERS:
            prod_files = _production_src_files(rust_files)
            tokens_by_file = {path: rust_tokens(src) for path, src in prod_files.items()}
            errors.extend(
                validate_product_session_authority_sources(name, prod_files, tokens_by_file)
            )
            if name == "plakd":
                errors.extend(validate_product_update_hook_sources(prod_files, tokens_by_file))
            errors.extend(validate_product_home_discovery(name, prod_files))
            errors.extend(validate_cli_security_sources(name, prod_files, tokens_by_file))
            errors.extend(
                validate_facade_only_sources(
                    name,
                    prod_files,
                    sorted(policy.edge_exceptions.get(name, ())),
                    tokens_by_file,
                )
            )

    errors.extend(graph_isolation_errors(graph))
    errors.extend(portable_closure_errors(workspace))
    errors.extend(supported_target_atomic_polyfill_errors(root))
    linkage_errors, linkage_note = artifact_linkage_errors(root)
    errors.extend(linkage_errors)

    # Acyclicity across workspace packages.
    adjacency = {name: pkg.dependency_names() & set(packages) for name, pkg in packages.items()}
    complete: set[str] = set()

    def visit(node: str, active: set[str]) -> None:
        if node in active:
            errors.append(f"workspace dependency cycle reaches {node}")
            return
        if node in complete:
            return
        active.add(node)
        for dep in adjacency.get(node, set()):
            visit(dep, active)
        active.remove(node)
        complete.add(node)

    for pkg_name in sorted(adjacency):
        visit(pkg_name, set())

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    layers = {layer: len(workspace.in_layers(layer)) for layer in policy.layers}
    populated = ", ".join(f"{layer}={count}" for layer, count in layers.items() if count)
    print(
        f"validated {len(packages)} workspace packages across layers ({populated}), "
        f"dependency boundaries, and the plakd/plakous-js-host build split; {linkage_note}"
    )
    return 0


def main() -> int:
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
        print("no placement ceilings to tighten")
        return 0
    if sys.argv[1:] in ([], ["--full-scan"]):
        return _check_workspace(ROOT)
    print(
        "usage: check_workspace.py [--standalone-packages | --wasm-libraries | --tighten-ceilings | --full-scan]",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
