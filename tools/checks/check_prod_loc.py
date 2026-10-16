#!/usr/bin/env python3
"""Report Rust production lines by architectural scope and responsibility.

Test-only files and modules are excluded. Counts are informational: this report
measures source size, not correctness or design quality.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from plakous_policy import (  # noqa: E402
    ABOVE_FACADE_LAYERS,
    BELOW_FACADE_LAYERS,
    L0_LAYERS,
    Package,
    Workspace,
    load_workspace,
)

ROOT = Path(__file__).resolve().parents[2]

SCOPE_LAYERS: dict[str, tuple[str, ...]] = {
    "l0": L0_LAYERS,
    "below_facade": BELOW_FACADE_LAYERS,
    "host": ("host",),
    "product": ("bin",),
    "test": ("test",),
}
SCOPES = tuple(SCOPE_LAYERS)
# Scopes that count as shipped production code (everything but test).
PRODUCTION_SCOPES = ("l0", "below_facade", "host", "product")

RESPONSIBILITIES = (
    "runtime",
    "event/presentation",
    "checkpoint/rehydration",
    "session/facade",
    "store",
    "family kernels",
    "provider glue",
    "auth",
    "prompts",
    "compose",
    "tool packs",
    "product",
)

# Ordered, source-bound ownership rules keyed by (package, package-relative
# path). A path ending in `/` is a directory prefix; `""` is the whole
# package. Keep these deliberately path based: the report is a reproducible
# baseline, not a guess based on identifiers or comments that can drift as
# implementations are edited. The unit test rejects every unclassified
# production source and reconciles every line to the scope totals.
RESPONSIBILITY_RULES: tuple[tuple[str, tuple[tuple[str, str], ...]], ...] = (
    ("store", (("plakous-store", "src/"),)),
    ("auth", (("plakous-auth", "src/"),)),
    ("auth", (
        ("plakous-openai", "src/native_auth.rs"),
        ("plakous-anthropic", "src/native_auth.rs"),
        ("plakous-xai", "src/native_auth.rs"),
    )),
    ("event/presentation", (
        ("plakous-agent", "src/event/"),
        ("plakous-agent", "src/telemetry.rs"),
        ("plakous-model", "src/event.rs"),
        ("plakous-model", "src/diagnostic.rs"),
    )),
    ("checkpoint/rehydration", (
        ("plakous-agent", "src/checkpoint_bundle.rs"),
        ("plakous-agent", "src/checkpoint_bundle/"),
        ("plakous-agent", "src/recovery_scope.rs"),
        ("plakous-agent", "src/manifest.rs"),
        ("plakous-agent", "src/manifest/"),
        ("plakous-agent", "src/rehydration.rs"),
    )),
    ("session/facade", (
        ("plakous-agent", "src/facade_support.rs"),
        ("plakous-agent", "src/session.rs"),
        ("plakous", "src/"),
    )),
    ("family kernels", (
        ("plakous-anthropic", "src/"),
        ("plakous-chat-completions", "src/"),
        ("plakous-exchange", "src/"),
        ("plakous-messages", "src/"),
        ("plakous-responses", "src/"),
        ("plakous-responses-dialect", "src/"),
    )),
    ("provider glue", (
        ("plakous-openai", "src/"),
        ("plakous-openrouter", "src/"),
        ("plakous-xai", "src/"),
    )),
    # The harness layer: model-visible instruction text and its assembler.
    # Not "runtime" -- nothing here dispatches; it is the text a turn carries.
    ("prompts", (("plakous-prompts", "src/"),)),
    # Composition presentations: the outer cell tools and their nested catalogs.
    ("compose", (("plakous-compose", "src/"),)),
    # The coding packs, the MCP client pack and the hosted-retrieval wrappers:
    # executors, menus, and their own model-visible text.
    ("tool packs", (
        ("plakous-mcp", "src/"),
        ("plakous-tool-files", "src/"),
        ("plakous-tool-plan", "src/"),
        ("plakous-tool-process", "src/"),
        ("plakous-tool-search", "src/"),
        ("plakous-tool-web", "src/"),
    )),
    ("runtime", (
        ("plakous-agent", "src/"),
        ("plakous-attempt", "src/"),
        ("plakous-code-mode", "src/"),
        ("plakous-js-eval", "src/"),
        ("plakous-js-runtime", "src/"),
        ("plakous-py-eval", "src/"),
        ("plakous-py-worker", "src/"),
        ("plakous-http", "src/"),
        ("plakous-model", "src/"),
        ("plakous-tools", "src/"),
    )),
)
# Host and binary sources share the product responsibility in this report.
LAYER_RESPONSIBILITIES: dict[str, str] = {layer: "product" for layer in ABOVE_FACADE_LAYERS}

MOD_TESTS = re.compile(r"(?:^|[^\w:])mod\s+tests\s*\{")
INLINE_MODULE = re.compile(r"(?:^|[^\w:])mod\s+(?:r#)?[A-Za-z_][A-Za-z_0-9]*\s*\{")
CFG_TEST = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
ATTRIBUTE_LOOKBACK = 24

# External modules declared only under cfg(test) cannot be inferred safely by
# a small regex: Rust permits cfg expressions, #[path], inline parents, raw
# identifiers, and multiple declarations. Keep this list explicit and
# reviewable, keyed by (package, package-relative path). Missing an entry
# conservatively over-counts; a mistaken generic inference could silently
# under-count production. Each entry is pinned by the repository test below.
TEST_ONLY_EXTERNAL_SOURCES: frozenset[tuple[str, str]] = frozenset(
    {
        ("plak", "src/product_crash_tests/actual_host.rs"),
        ("plak", "src/product_crash_tests/fixture.rs"),
        ("plak", "src/product_crash_tests/mod.rs"),
        ("plak", "src/product_crash_tests/product_release.rs"),
        ("plak", "src/product_crash_tests/protocol.rs"),
        ("plak", "src/test_support.rs"),
        ("plakous", "src/test_model.rs"),
    }
)


# Whole-file `#[cfg(test)]` bodies, recognised by file name rather than by
# enumeration because both conventions are repository-wide and mechanical:
#   * `tests.rs` is the file behind a `#[cfg(test)] mod tests;` declaration;
#   * `rendered_*.rs` holds assembled goldens whose entire body is `#[cfg(test)]`.
# `test_repository_cfg_test_file_conventions_hold` pins that every file matching
# these names really is test-only, so the rule cannot silently hide production.
TEST_ONLY_FILE_NAMES = ("tests.rs",)
TEST_ONLY_FILE_PREFIXES = ("rendered_",)


def is_test_only_file_name(name: str) -> bool:
    return name in TEST_ONLY_FILE_NAMES or any(
        name.startswith(prefix) for prefix in TEST_ONLY_FILE_PREFIXES
    )


def blank_literals(lines: list[str]) -> list[str]:
    """Return `lines` with comments, strings and char literals replaced by spaces.

    Brace depth is only meaningful over real code: `println!("{}", x)` and a
    `// closing }` comment would otherwise unbalance the count. The blanked
    text keeps every line's length and index, so callers can still report
    positions against the original file.
    """
    blanked: list[str] = []
    # Cross-line lexer state: block-comment nesting depth, and the open string
    # (None, or the terminator of a normal/raw string).
    comment_depth = 0
    string_end: str | None = None
    for line in lines:
        out: list[str] = []
        i = 0
        n = len(line)
        while i < n:
            ch = line[i]
            if comment_depth:
                if line.startswith("/*", i):
                    comment_depth += 1
                    out.append("  ")
                    i += 2
                    continue
                if line.startswith("*/", i):
                    comment_depth -= 1
                    out.append("  ")
                    i += 2
                    continue
                out.append(" ")
                i += 1
                continue
            if string_end is not None:
                if string_end == '"' and ch == "\\":
                    out.append("  ")
                    i += 2
                    continue
                if line.startswith(string_end, i):
                    out.append(" " * len(string_end))
                    i += len(string_end)
                    string_end = None
                    continue
                out.append(" ")
                i += 1
                continue
            if line.startswith("//", i):
                out.append(" " * (n - i))
                break
            if line.startswith("/*", i):
                comment_depth = 1
                out.append("  ")
                i += 2
                continue
            raw = re.match(r'(?:b|c)?r(#*)"', line[i:])
            if raw:
                string_end = '"' + raw.group(1)
                out.append(" " * raw.end())
                i += raw.end()
                continue
            if ch == '"':
                string_end = '"'
                out.append(" ")
                i += 1
                continue
            if ch == "'":
                # A char literal (`'x'`, `'\n'`, `'{'`) versus a lifetime (`'a`).
                char = re.match(r"'(?:\\.|[^\\'])'", line[i:])
                if char:
                    out.append(" " * char.end())
                    i += char.end()
                    continue
                out.append(ch)
                i += 1
                continue
            out.append(ch)
            i += 1
        blanked.append("".join(out))
    return blanked


def _attribute_run_start(blanked: list[str], index: int) -> int:
    """Return the first line of the attribute run immediately above `index`."""
    start = index
    while start > 0:
        found = None
        floor = max(0, start - ATTRIBUTE_LOOKBACK)
        for j in range(start - 1, floor - 1, -1):
            chunk = "\n".join(blanked[j:start]).strip()
            if not chunk.startswith("#["):
                continue
            if chunk.endswith("]") and chunk.count("[") == chunk.count("]"):
                found = j
                break
        if found is None:
            return start
        start = found
    return start


def production_line_mask(lines: list[str]) -> list[bool]:
    """Conservative physical-line classification, independent of target features.

    Exclude balanced inline `mod tests` bodies and inline modules explicitly
    gated by cfg(test), including their attributes. Other cfg expressions and
    test-support features remain counted: they can contain shipped code.
    External declarations are counted here; whole-file exclusions are separate.
    This is a lexical metric, not Rust reachability or a compiler configuration.
    """
    blanked = blank_literals(lines)
    counted = [True] * len(lines)
    depth = 0
    test_depth: int | None = None
    for index, code in enumerate(blanked):
        start = index
        test_module = False
        if test_depth is None and INLINE_MODULE.search(code):
            start = _attribute_run_start(blanked, index)
            attributes = "\n".join(blanked[start:index + 1])
            test_module = bool(MOD_TESTS.search(code) or CFG_TEST.search(attributes))
        if test_depth is None and test_module:
            test_depth = depth
            for j in range(start, index):
                counted[j] = False
        if test_depth is not None:
            counted[index] = False
        depth += code.count("{") - code.count("}")
        if test_depth is not None and depth <= test_depth:
            test_depth = None
    return counted


def prod_lines_in_file(path: Path) -> int:
    """Count physical production-candidate lines; see production_line_mask."""
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return 0
    return sum(production_line_mask(lines))


def is_excluded_src_file(path: Path) -> bool:
    """Return whether a `src/**/*.rs` file is a whole-file test body by name or directory."""
    name = path.name
    if name.endswith("_tests.rs") or name == "client_tests.rs":
        return True
    if is_test_only_file_name(name):
        return True
    # Exclude a test-only module directory *inside* a crate's src tree, not an
    # unpublished workspace package merely because its crate root is under the
    # repository-level tests/ directory.
    src_index = max(
        (index for index, part in enumerate(path.parts) if part == "src"),
        default=-1,
    )
    return src_index >= 0 and "tests" in path.parts[src_index + 1 :]


def is_excluded_package_source(package: Package, relative: str) -> bool:
    """Package-aware exclusion over a package-relative path (`src/...`)."""
    if (package.name, relative) in TEST_ONLY_EXTERNAL_SOURCES:
        return True
    return is_excluded_src_file(Path(relative))


def package_sources(package: Package, root: Path = ROOT) -> list[tuple[str, int]]:
    """Return (package-relative path, production LOC) for every counted source."""
    src = root / package.directory / "src"
    if not src.is_dir():
        return []
    counted: list[tuple[str, int]] = []
    for path in sorted(src.rglob("*.rs")):
        relative = path.relative_to(root / package.directory).as_posix()
        if is_excluded_package_source(package, relative):
            continue
        counted.append((relative, prod_lines_in_file(path)))
    return counted


def crate_prod_loc(crate_root: Path) -> tuple[int, int]:
    """Return (prod_loc, file_count) for crate_root/src without package context."""
    src = crate_root / "src"
    if not src.is_dir():
        return 0, 0
    total = 0
    files = 0
    for path in src.rglob("*.rs"):
        if is_excluded_src_file(path):
            continue
        total += prod_lines_in_file(path)
        files += 1
    return total, files


def scope_of(package: Package) -> str | None:
    for scope, layers in SCOPE_LAYERS.items():
        if package.layer in layers:
            return scope
    return None


@dataclass(frozen=True)
class SourceLoc:
    path: str
    package: str
    scope: str
    responsibility: str
    loc: int


def responsibility_for(package: Package, relative: str) -> str:
    """Return the single ordered responsibility for a package-relative source path."""
    if package.layer in LAYER_RESPONSIBILITIES:
        return LAYER_RESPONSIBILITIES[package.layer]
    matches = []
    for responsibility, sources in RESPONSIBILITY_RULES:
        for owner, prefix in sources:
            if owner != package.name:
                continue
            if prefix == "" or (prefix.endswith("/") and relative.startswith(prefix)) or relative == prefix:
                matches.append(responsibility)
                break
    if not matches:
        raise ValueError(f"unclassified production source: {package.name}:{relative}")
    # Overlap is intentional only for narrower rules preceding a broad owner
    # (provider native auth, event/checkpoint/session files, model event files).
    return matches[0]


def source_inventory(workspace: Workspace) -> list[SourceLoc]:
    inventory: list[SourceLoc] = []
    for package in sorted(workspace.packages.values(), key=lambda item: item.name):
        scope = scope_of(package)
        if scope is None or scope not in PRODUCTION_SCOPES:
            continue
        for relative, loc in package_sources(package, workspace.root):
            inventory.append(
                SourceLoc(
                    package.relative(relative),
                    package.name,
                    scope,
                    responsibility_for(package, relative),
                    loc,
                )
            )
    return inventory


def responsibility_report(workspace: Workspace) -> dict[str, object]:
    sources = source_inventory(workspace)
    totals = {name: 0 for name in RESPONSIBILITIES}
    by_scope = {scope: {name: 0 for name in RESPONSIBILITIES} for scope in PRODUCTION_SCOPES}
    for source in sources:
        totals[source.responsibility] += source.loc
        by_scope[source.scope][source.responsibility] += source.loc
    scope_totals = {scope: sum(values.values()) for scope, values in by_scope.items()}
    return {
        "classifier_revision": "plakous-responsibility-loc-v3",
        "classification_basis": (
            "layer-derived scopes; explicit test-only exclusion plus ordered "
            "package-keyed exact-file-or-directory-prefix over production Rust sources"
        ),
        "rules": [
            {"responsibility": responsibility, "sources": [f"{owner}:{prefix}" for owner, prefix in paths]}
            for responsibility, paths in RESPONSIBILITY_RULES
        ],
        "layer_rules": dict(LAYER_RESPONSIBILITIES),
        "totals": totals,
        "by_scope": by_scope,
        "scope_totals": scope_totals,
        "sources": [source.__dict__ for source in sources],
    }


def scope_totals(workspace: Workspace) -> tuple[dict[str, int], dict[str, dict[str, int]]]:
    """Return per-scope LOC totals and per-scope per-package LOC."""
    totals = {scope: 0 for scope in SCOPES}
    by_package: dict[str, dict[str, int]] = {scope: {} for scope in SCOPES}
    unscoped: list[str] = []
    for package in sorted(workspace.packages.values(), key=lambda item: item.name):
        scope = scope_of(package)
        if scope is None:
            unscoped.append(package.name)
            continue
        loc = sum(count for _path, count in package_sources(package, workspace.root))
        by_package[scope][package.directory] = loc
        totals[scope] += loc
    if unscoped:
        raise ValueError(f"packages without a density scope: {unscoped!r}")
    return totals, by_package


def report(workspace: Workspace) -> dict[str, object]:
    totals, by_package = scope_totals(workspace)
    l0 = totals["l0"]
    library = l0 + totals["below_facade"]
    responsibilities = responsibility_report(workspace)
    expected = {scope: totals[scope] for scope in PRODUCTION_SCOPES}
    if responsibilities["scope_totals"] != expected:
        raise RuntimeError("responsibility LOC does not reconcile to density scopes")
    return {
        "scopes": totals,
        "by_package": by_package,
        "l0": l0,
        "library": library,
        "production_total": sum(totals[scope] for scope in PRODUCTION_SCOPES),
        "responsibility_report": responsibilities,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit machine-readable summary on stdout",
    )
    args = parser.parse_args()

    workspace = load_workspace(ROOT)
    data = report(workspace)
    totals: dict[str, int] = data["scopes"]  # type: ignore[assignment]
    by_package: dict[str, dict[str, int]] = data["by_package"]  # type: ignore[assignment]

    if args.json:
        print(json.dumps(data, indent=2, sort_keys=True))
    else:
        print("Production LOC report (informational)")
        print(f"  l0 (core+facade):  {data['l0']:>8}")
        print(f"  below-facade:      {totals['below_facade']:>8}")
        print(f"  library (l0+below):{data['library']:>8}")
        print(f"  host:              {totals['host']:>8}")
        print(f"  product:           {totals['product']:>8}")
        print(f"  test (info):       {totals['test']:>8}  (excluded from production)")
        print(f"  production total:  {data['production_total']:>8}")
        for scope in SCOPES:
            print(f"  {scope} by package:")
            for rel, n in sorted(by_package[scope].items(), key=lambda x: -x[1]):
                print(f"    {n:>8}  {rel}")
        responsibilities = data["responsibility_report"]
        print("  production LOC by responsibility (all production scopes):")
        for name, n in responsibilities["totals"].items():  # type: ignore[index]
            print(f"    {n:>8}  {name}")
        scopes = responsibilities["scope_totals"]  # type: ignore[index]
        print(
            "  responsibility reconciliation: "
            + ", ".join(f"{scope}={scopes[scope]}" for scope in PRODUCTION_SCOPES)  # type: ignore[index]
            + f", total={sum(scopes.values())}"  # type: ignore[union-attr]
        )

    return 0


if __name__ == "__main__":
    sys.exit(main())
