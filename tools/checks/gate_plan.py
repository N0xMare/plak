#!/usr/bin/env python3
"""Conservative change-scoped gate plan; unknown inputs widen to check.

The ordered GROUPS list is also the runner's full-tier inventory. Package
filters use nextest's rdeps, while compilation retains the shared workspace
all-features artifact set used by fast/check.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path, PurePosixPath
import os
import re
import subprocess
from functools import lru_cache
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from changed_packages import REPO_ROOT, WORKSPACE_INPUTS, changed_paths, package_dirs


GROUPS = (
    "static", "builds", "rustdoc", "tests-parallel", "tests-serial",
    "tests-durability", "contracts", "tooling", "perf", "loc",
)
# Non-nested tier selections share the full inventory. Platform omits the Python
# structure checks inside static and includes the durability lane. Check includes
# structure checks, default/maintainer builds, contracts and tooling, but not
# the durability lane.
PLATFORM_GROUPS = tuple(group for group in GROUPS if group in {
    "static", "builds", "tests-parallel", "tests-serial", "tests-durability",
})
CHECK_GROUPS = tuple(group for group in GROUPS if group in {
    "static", "builds", "tests-parallel", "tests-serial", "contracts", "tooling",
})

# Executed by check, or used to choose its commands and test inventory. Editing
# the planner must widen even if the edited package rules would select less.
GATE_INPUTS = {
    "scripts/dev-check.sh", "scripts/dev-tools.sh", "Justfile",
    ".config/nextest.toml",
    "tools/checks/gate_plan.py", "tools/checks/changed_packages.py",
    "tools/checks/budget_filterset.py", "tools/checks/check_workspace.py",
    "tools/checks/check_features.py", "tools/checks/plakous_policy.py",
    "tools/checks/check_public_compile.py",
}

# Executable discovery is not represented in Cargo's dependency graph.
ARTIFACT_CONSUMERS = {
    "plakous-py-worker": {"plakous-py-eval", "plakous-hosted", "plak"},
    "plakous-js-runtime": {"plakous-js-eval", "plak"},
}

# All matching rules union, rather than first-match winning. Public core crates
# are included conservatively: implementation changes can alter nameability.
RULE_TABLE = {
    "cargo": ("builds", "tests-parallel", "tests-serial", "contracts"),
    "rust": ("builds", "tests-parallel", "tests-serial", "contracts"),
    "package-input": ("tests-parallel", "tests-serial"),
    # Owning package only. Not part of the blanket rust/cargo rules: check and
    # unknown-path widening must not pick this group up.
    "durability": ("tests-durability",),
    "api": ("contracts", "rustdoc"),
    "tooling": ("tooling",),
    "gate": CHECK_GROUPS,
    "readers": ("tests-parallel", "tests-serial"),
    "prompts": ("builds", "tests-parallel", "tests-serial"),
    "unknown": CHECK_GROUPS,
}
# Packages that own a tests-durability member. `just changed` selects that
# group, scoped with the plan's rdeps filter, only when a changed path is in
# one of these packages.
DURABILITY_OWNERS = frozenset({
    "plak",
    "plakous",
    "plakous-store",
    "plakous-offline-crash-harness",
})

API_PREFIXES = (
    "crates/plakous/", "crates/core/agent/", "crates/core/model/",
    "crates/core/tools/", "tests/fixtures/",
)


@dataclass
class Plan:
    reasons: dict[str, list[str]] = field(default_factory=dict)
    packages: set[str] = field(default_factory=set)
    workspace: bool = False
    readers: bool = False
    checker_reasons: dict[str, list[str]] = field(
        default_factory=lambda: {"workspace": [], "features": []})

    def add(self, groups: tuple[str, ...], reason: str) -> None:
        for group in groups:
            if reason not in self.reasons.setdefault(group, []):
                self.reasons[group].append(reason)

    @property
    def groups(self) -> list[str]:
        return [group for group in GROUPS if group in self.reasons]

    @property
    def scope(self) -> str:
        if self.workspace:
            return ""
        terms = [f"rdeps(={name})" for name in sorted(self.packages)]
        return "(" + " | ".join(terms) + ")" if terms else ""

    def as_dict(self) -> dict:
        return dict(groups=self.groups, packages=sorted(self.packages),
                    workspace=self.workspace, readers=self.readers,
                    scope=self.scope, reasons=self.reasons, checkers=self.checker_reasons)


def owner_of(path: str, dirs: dict[str, str]) -> str | None:
    return next((dirs[parent.as_posix()] for parent in PurePosixPath(path).parents
                 if parent.as_posix() in dirs), None)


def is_reader_source(path: str, dirs: dict[str, str]) -> bool:
    owner = owner_of(path, dirs)
    if not owner:
        return False
    directory = next(directory for directory, name in dirs.items() if name == owner)
    return not {"benches", "examples"} & set(PurePosixPath(path).relative_to(directory).parts[:-1])


@lru_cache(maxsize=4)
def artifact_dependencies(root: Path = REPO_ROOT) -> dict[str, set[str]]:
    """Conservative workspace closure, including optional/target/dev edges."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        cwd=root, text=True))
    dependencies = {p["name"]: {d["name"] for d in p["dependencies"]}
                    for p in metadata["packages"]}
    closures = {}
    for artifact in ARTIFACT_CONSUMERS:
        closure = {artifact}
        pending = [artifact]
        while pending:
            for name in dependencies.get(pending.pop(), ()):
                if name in dependencies and name not in closure:
                    closure.add(name)
                    pending.append(name)
        closures[artifact] = closure
    return closures


# Preserve literals while removing comments, so prose paths are not readers.
RUST_TOKENS = re.compile(
    r'''r(?P<hashes>\#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|'''
    r'''b?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'|'''
    r'//[^\n]*|/\*.*?\*/', re.S)

# Exact, reviewed non-readers with dynamic roots. Every other ambiguous root
# still widens; tooling tests reject stale entries. One locator literal repeats.
NON_READER_TEMPLATES = {
    ("crates/core/tools/src/environment/backend/job_output/tests.rs",
     "{}/jobs/{SESSION}/{BRANCH}-p{number}-"):
        "Private job spool path under a test's temporary state home.",
    ("crates/orchestration/recursive-subagents-store/tests/view_deletion.rs",
     "{}/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"):
        "Runtime session/branch/turn locator, not a filesystem path.",
    ("crates/orchestration/recursive-subagents-store/tests/view_deletion.rs",
     "{SESSION}/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"):
        "Runtime session/branch/turn locator, not a filesystem path (two occurrences).",
    ("crates/providers/openrouter/src/discovery.rs", "{ENDPOINTS_PREFIX}/{}/{}/endpoints"):
        "HTTP endpoint path under the provider's /api/v1/models prefix.",
    ("crates/providers/xai/src/native_auth/tests.rs", "{}/credential-capture"):
        "Redirect URL under a loopback mock server origin.",
    ("crates/providers/xai/src/responses_dialect.rs", "{SUPERGROK_RESPONSES_ORIGIN}/v1"):
        "Provider URL under the literal HTTPS SuperGrok origin.",
}


def reader_source_files(root: Path = REPO_ROOT) -> list[str]:
    """Tracked Rust sources, or the Rust sources of a git-free source export.

    The export is already free of ignored files, but gates regenerate build
    and tool output inside it; never mistake that output for reader sources.
    """
    if (root / ".git").exists():
        return sorted(filter(None, subprocess.check_output(
            ["git", "ls-files", "-z", "*.rs"], cwd=root).decode().split("\0")))
    files = []
    for directory, children, names in os.walk(root):
        children[:] = sorted(set(children) - {"target", ".git", ".dev-tools", "__pycache__"})
        files.extend((Path(directory) / name).relative_to(root).as_posix()
                     for name in names if name.endswith(".rs"))
    return sorted(files)


@lru_cache(maxsize=4)
def reader_inputs(directories: tuple[tuple[str, str], ...], root: Path = REPO_ROOT
                  ) -> tuple[dict[str, set[str]], list[str]]:
    """Discover file/directory readers, including cross-package embedded inputs.

    Literal paths are tried relative to source, package and checkout. Templates
    use their literal directory prefix; ambiguous dynamic roots and unsupported
    embedded-input expressions widen. OUT_DIR inputs are
    generated by Cargo: their tracked build inputs use the Rust/package rules.
    """
    dirs = dict(directories)
    references: dict[str, set[str]] = {}
    unresolved = []
    for file in reader_source_files(root):
        owner = owner_of(file, dirs)
        if not is_reader_source(file, dirs):
            continue
        source = root / file
        if not source.exists():
            continue
        text = source.read_text()
        tokens = list(RUST_TOKENS.finditer(text))
        literals = [(token.start(), token.end(), token.group()) for token in tokens
                    if token.group().startswith(('"', 'r'))]
        masked = list(text)
        for token in tokens:
            masked[token.start():token.end()] = " " * (token.end() - token.start())
        code = "".join(masked)
        package = next(root / directory for directory, name in directories if name == owner)

        def record(value: str, embedded: bool = False, template: bool = False) -> bool:
            if "\n" in value or "\0" in value:
                return False
            found = False
            for base in (source.parent, package, root):
                candidate = Path(os.path.normpath(base / value))
                if not candidate.is_relative_to(root):
                    continue
                relative = candidate.relative_to(root).as_posix()
                try:
                    exists = candidate.exists()
                except OSError:
                    exists = False
                if exists or (not embedded and relative.startswith(("docs/", "configs/"))):
                    if relative == ".":
                        if embedded or template:
                            unresolved.append(f"{file}: repository-root input {value}")
                        found = True
                        continue
                    if embedded or (dirs.get(relative) or owner_of(relative, dirs)) != owner:
                        references.setdefault(relative.rstrip("/"), set()).add(owner)
                    found = True
            return found

        for _, _, literal in literals:
            value = literal[literal.index('"') + 1:literal.rindex('"')]
            # Globs do not resolve to filesystem targets. The directory literals
            # used to construct them are discovered separately.
            if ("/" in value or value == ".") and "\\" not in value and "*" not in value:
                if "{" in value or "}" in value:
                    if (file, value) in NON_READER_TEMPLATES:
                        continue
                    portions = re.split(r'\{[^{}]*\}', value)
                    # Check literal portions, not format specifiers: prose, URLs
                    # and serialized data are not repository path templates.
                    if any(re.search(r'[^\w./~\-]', part) for part in portions):
                        continue
                    prefix = portions[0].rsplit("/", 1)[0] if "/" in portions[0] else ""
                    if prefix and prefix.strip("./"):
                        record(prefix, template=True)
                    elif any("/" in part and part.strip("/") for part in portions):
                        unresolved.append(f"{file}: interpolated input {value[:80]}")
                else:
                    record(value)

        for macro in re.finditer(r'include_(?:str|bytes)!\s*\(|#\s*\[\s*path\s*=', code):
            start = macro.end()
            if macro.group().startswith("#"):
                end = code.find("]", start)
            else:
                depth, end = 1, start
                while end < len(code) and depth:
                    depth += (code[end] == "(") - (code[end] == ")")
                    end += 1
            expression = text[start:end - (not macro.group().startswith("#"))].strip().rstrip(",").strip()
            values = [literal[literal.index('"') + 1:literal.rindex('"')]
                      for begin, finish, literal in literals if start <= begin and finish <= end]
            simple = RUST_TOKENS.fullmatch(expression)
            expression_code = code[start:end - (not macro.group().startswith("#"))].strip()
            concat = re.fullmatch(r'concat!\s*\((?:\s|,|env!\s*\(\s*\))*\)', expression_code)
            if not simple and not concat:
                unresolved.append(f"{file}: unsupported embedded input {expression[:80]}")
                continue
            if concat and "OUT_DIR" in values:
                continue
            if "CARGO_MANIFEST_DIR" in values:
                values = [str(package) if value == "CARGO_MANIFEST_DIR" else value for value in values]
            target = "".join(values)
            if not values or not record(target, embedded=True):
                unresolved.append(f"{file}: {expression.strip()[:80]}")
    return references, unresolved


def default_base(root: Path = REPO_ROOT) -> str:
    try:
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
        base = subprocess.check_output(["git", "merge-base", "HEAD", "refs/heads/main"],
                                       cwd=root, text=True, stderr=subprocess.PIPE).strip()
        branch = subprocess.run(["git", "symbolic-ref", "--quiet", "--short", "HEAD"],
                                cwd=root, text=True, capture_output=True).stdout.strip()
        if branch == "main":
            return subprocess.check_output(["git", "rev-parse", "HEAD~"], cwd=root,
                                           text=True, stderr=subprocess.PIPE).strip()
    except subprocess.CalledProcessError as error:
        raise ValueError("cannot determine a checked default base from main; pass an explicit BASE") from error
    if base == head:
        raise ValueError("merge-base is HEAD, but HEAD is not main; pass an explicit BASE")
    return base


def make_plan(paths: list[str], dirs: dict[str, str]) -> Plan:
    plan = Plan()
    plan.add(("static",), "always: fmt, workspace clippy; input-scoped structure/features")
    references, unresolved = reader_inputs(tuple(sorted(dirs.items()))) if paths else ({}, [])
    for path in sorted(set(paths)):
        owner = owner_of(path, dirs)
        matched = False
        gate_input = path in GATE_INPUTS
        if gate_input:
            plan.workspace = True
            plan.add(RULE_TABLE["gate"], f"gate: {path} -> whole check tier + tooling")
            matched = True
        if (path in WORKSPACE_INPUTS or path == "deny.toml"
                or path.startswith((".cargo/", "rust-toolchain"))
                or (PurePosixPath(path).name == "Cargo.toml"
                    and (owner or path.startswith("crates/")))):
            plan.workspace = True
            plan.add(RULE_TABLE["cargo"], f"cargo: {path}")
            matched = True
        if owner:
            plan.packages.add(owner)
            rule = ("rust" if path.endswith(".rs") else "prompts"
                    if path.startswith("crates/harness/prompts/") else "package-input")
            plan.add(RULE_TABLE[rule], f"{rule}: {path} -> {owner} + rdeps")
            if owner in DURABILITY_OWNERS:
                plan.add(RULE_TABLE["durability"],
                         f"durability: {path} -> {owner} + rdeps")
            matched = True
        if path.startswith(API_PREFIXES) or path == "tools/checks/check_public_compile.py":
            plan.add(RULE_TABLE["api"], f"api: {path}")
            # Additive: a fixture or other non-Rust input must still be classified.
            if path.startswith("tests/fixtures/") or path == "tools/checks/check_public_compile.py":
                matched = True
        if path.startswith(("scripts/", "tools/", "tests/tooling/")) or path == "Justfile":
            plan.add(RULE_TABLE["tooling"], f"tooling: {path} (includes dev-check self-tests)")
            matched = True
        readers = {package for target, packages in references.items()
                   if path == target or path.startswith(target + "/") for package in packages}
        if readers:
            plan.readers = True
            plan.packages.update(readers)
            plan.add(RULE_TABLE["readers"], f"readers: {path} -> " + ", ".join(sorted(readers)) + " + rdeps")
            matched = True
        if path.startswith(("docs/", "configs/")):
            matched = True
        if unresolved:
            plan.workspace = True
            examples = "; ".join(unresolved[:3])
            plan.add(CHECK_GROUPS, f"unresolved readers ({len(unresolved)}): {examples}"
                     + ("; ... -> whole check tier" if len(unresolved) > 3 else " -> whole check tier"))
        if not matched:
            plan.workspace = True
            plan.add(RULE_TABLE["unknown"], f"unknown: {path} -> whole check tier (safe default)")
        manifest = PurePosixPath(path).name in {"Cargo.toml", "Cargo.lock"}
        shared_checker = path == "tools/checks/plakous_policy.py"
        if manifest or path.endswith(".rs"):
            plan.add(("contracts",), f"contracts: {path} (whole facade closure)")
        if (manifest or path.endswith(".rs") or path.startswith("tests/fixtures/") or shared_checker
                or gate_input or not matched or unresolved):
            plan.checker_reasons["workspace"].append(path)
        if (manifest or path.endswith(".rs") or shared_checker or path.startswith((".cargo/", "rust-toolchain"))
                or path == "deny.toml" or gate_input or not matched or unresolved):
            plan.checker_reasons["features"].append(path)
    planned = set(plan.packages)
    if planned:
        for artifact, closure in artifact_dependencies().items():
            inputs = planned & closure
            if inputs:
                consumers = ARTIFACT_CONSUMERS[artifact]
                plan.packages.update(consumers)
                plan.add(RULE_TABLE["readers"], f"artifact: {artifact} dependencies "
                         + ", ".join(sorted(inputs)) + " -> " + ", ".join(sorted(consumers)) + " + rdeps")
    return plan


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", nargs="?")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--groups", action="store_true")
    mode.add_argument("--tier", choices=("platform", "check", "full"),
                      help="ordered groups for one unconditional tier")
    mode.add_argument("--shell", action="store_true", help="groups, scope and checker flags, one line each")
    mode.add_argument("--json", action="store_true")
    mode.add_argument("--checkers", action="store_true", help="input-scoped checker flags for fast")
    args = parser.parse_args()
    if args.groups:
        print("\n".join(GROUPS))
        return 0
    if args.tier:
        groups = {"platform": PLATFORM_GROUPS, "check": CHECK_GROUPS, "full": GROUPS}[args.tier]
        print(",".join(groups))
        return 0
    try:
        base = args.base or ("HEAD" if args.checkers else default_base())
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    paths = changed_paths(base=base)
    plan = make_plan(paths, package_dirs())
    if not paths and not args.checkers:
        print("WARNING: empty diff; changed verifies no changes. BASE must already have passed just check on this toolchain.", file=sys.stderr)
    if args.checkers:
        for checker in ("workspace", "features"):
            print("true" if plan.checker_reasons[checker] else "false")
        return 0
    if args.json:
        print(json.dumps(dict(base=base, **plan.as_dict()), indent=2))
        return 0
    output = sys.stderr if args.shell else sys.stdout
    print(f"changed plan against {base}:", file=output)
    for group in plan.groups:
        print(f"  {group}: " + "; ".join(plan.reasons[group]), file=output)
    print("  test scope: " + (plan.scope or "whole workspace"), file=output)
    print("  build scope: workspace all-features (shared artifact set)", file=output)
    for checker, inputs in plan.checker_reasons.items():
        reason = ", ".join(inputs) if inputs else "no matching checker inputs"
        print(f"  {checker} checker: {'run' if inputs else 'skip'} ({reason})", file=output)
    if args.shell:
        print(",".join(plan.groups))
        print(plan.scope)
        for checker in ("workspace", "features"):
            print("true" if plan.checker_reasons[checker] else "false")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
