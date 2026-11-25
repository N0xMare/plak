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
import subprocess
import sys
from collections import deque
from functools import lru_cache
from pathlib import Path
from typing import Any, NamedTuple

sys.path.insert(0, str(Path(__file__).resolve().parent))

import vcs_listing  # noqa: E402
from plakous_policy import (  # noqa: E402
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
VENDOR_PACKAGE = "plakous-codex-code-mode-protocol"
IMAGE_LEAF = "plakous-image"

PORTABLE_CRATES = (
    "plakous-model",
    "plakous-runner-protocol",
    "plakous-gateway-protocol",
    "plakous-image",
    "plakous-tools",
    "plakous-agent",
)
PORTABLE_DEFAULT_LIBRARIES = ("plakous-store",)

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
    if "cfg" not in source or "test" not in source:
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
    tokens: list[RustToken] = []
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
        return []
    errors = [
        f"{IMAGE_LEAF}: depends on workspace package {dep['name']!r}"
        for dep in image.metadata.get("dependencies", [])
        if dep["name"] in workspace.packages
    ]
    edges = [dep for dep in tools.metadata.get("dependencies", []) if dep["name"] == IMAGE_LEAF]
    for dep in edges:
        if (
            dep.get("kind") is not None
            or dep.get("optional")
            or dep.get("uses_default_features")
            or dep.get("features")
        ):
            errors.append(
                f"plakous-tools: {IMAGE_LEAF} must be default-features = false with no features"
            )
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


_FIXTURE_PREFILTER = re.compile(r"\bdone\b|\bsleep\b")


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


def _check_workspace(root: Path = ROOT) -> int:
    graph = _cargo_resolve_graph(root)
    workspace = load_workspace(root, graph=graph)
    packages = workspace.packages
    policy = workspace.policy
    errors: list[str] = []

    root_ws = workspace.root_manifest.get("workspace", {})
    if root_ws.get("resolver") != "3":
        errors.append("workspace resolver must be 3")

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
        if name in COMMIT_TRACE_SOURCE_PREFIXES:
            errors.extend(validate_commit_trace_sql(name, rust_files))
        if pkg.neutral:
            src_only = {k: v for k, v in rust_files.items() if k.startswith("src/")}
            test_only = declared_test_module_paths(
                {k.removeprefix("src/"): v for k, v in src_only.items()},
                {"lib.rs", "main.rs"},
            )
            prod_files = {
                k: v for k, v in src_only.items() if k.removeprefix("src/") not in test_only
            }
            errors.extend(validate_neutral_sources(name, prod_files))

    errors.extend(graph_isolation_errors(graph))
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
