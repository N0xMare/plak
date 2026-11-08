#!/usr/bin/env python3
"""Publication evidence across a documentation-only delta (spec 06 sec. 6.5).

`just full` records the tree it passed on in `<target>/gate-evidence/full`; a
passing package group records its tree in `<target>/gate-evidence/package`.
`verify` accepts the full evidence for HEAD when HEAD's tree equals it, or when
`git diff --name-only <evidence>..HEAD` touches only:

- documentation no gate reads (ALLOWLIST minus EXCLUDED), whose new content
  still passes the tracked-file hygiene scan; or
- packaged non-code files (PACKAGE_ONLY), which additionally need package
  evidence for exactly HEAD's tree.

Anything else, an unknown evidence tree, or a failing git command refuses.
`audit` is the structure check that keeps the allowlist honest: no gate
input (Rust, Python, shell, manifests, recipes) may name an allowlisted path
unless it is a reviewed reference below.
"""

from __future__ import annotations

import argparse
import fnmatch
import importlib.util
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Documentation no build, test, checker or packaged crate reads.
ALLOWLIST = ("tests/live/STATUS.md", "CHANGELOG.md", "docs/spec/**", "docs/user-guide/**")
# Allowlisted by pattern but read by a gate; a change to one invalidates.
EXCLUDED = {
    "docs/spec/07-config-reference.md": "bin/plak product_config tests compare the config key table and template schema with it",
}
# Packaged into a published crate (`readme.workspace`) and read by no test.
PACKAGE_ONLY = ("README.md",)

# Gate inputs the audit scans for names of allowlisted paths.
SCANNED_SUFFIXES = (".rs", ".py", ".sh", ".toml", ".just", ".yml", ".yaml", ".json")
SCANNED_NAMES = ("justfile",)
# The allowlist definition and its own tests name the paths on purpose.
SELF = ("tools/release/gate_evidence.py", "tests/tooling/test_gate_evidence.py")
# Each token the audit flags, per file, with why it reads no repository file
# the evidence rule treats as inert. Keys are exact matched texts; an entry
# whose text no longer occurs is stale and fails.
REVIEWED_REFERENCES: dict[str, tuple[frozenset[str], str]] = {
    "bin/plak/src/carry/history/tests.rs": (frozenset({'"docs"'}), "an MCP server id fixture"),
    "bin/plak/src/cli/tests.rs": (frozenset({'"docs"'}), "an MCP server id argument fixture"),
    "bin/plak/src/tui/surface_views/tests.rs": (
        frozenset({'"docs"'}),
        "an MCP server id in help text",
    ),
    "bin/plak/src/cli_output/tests.rs": (frozenset({'"STATUS"'}), "a table header"),
    "bin/plak/src/product_config/mcp_edit/tests.rs": (
        frozenset({'"docs"'}),
        "an MCP server id fixture",
    ),
    "bin/plak/src/product_config/tool_sections_tests.rs": (
        frozenset({'"docs"'}),
        "an MCP server id fixture",
    ),
    "bin/plak/src/tui/markdown_tests.rs": (frozenset({"README.md"}), "rendered transcript text"),
    "bin/plak/src/tui/ui/pilot_tests.rs": (frozenset({"README.md"}), "a tool-call path fixture"),
    "bin/plak/src/tui/ui/test_support.rs": (frozenset({"README.md"}), "rendered tool output"),
    "crates/core/agent/src/manifest/tests.rs": (
        frozenset({"README.md"}),
        "a tool-call path fixture",
    ),
    "crates/core/agent/src/profile/tests.rs": (frozenset({'"spec"'}), "an expect message"),
    "crates/core/tools/src/environment/backend/workspace.rs": (
        frozenset({'"docs"', "*.md", "docs/spec", "docs/spec/*.md", "docs/spec/{a,b}/*.md"}),
        "glob-prefix unit inputs, not repository paths",
    ),
    "crates/core/tools/src/exposure/lowering.rs": (frozenset({"*.md"}), "a file-type glob table"),
    "crates/core/tools/tests/integration/environment_backend.rs": (
        frozenset({'"docs"', "*.md", "docs/spec", "docs/spec/*.md", "docs/spec/01.md"}),
        "a temporary workspace that creates its own docs/spec tree",
    ),
    "crates/harness/compose/tests/integration/coding_menu.rs": (
        frozenset({"docs/spec/*.md"}),
        "a tool-call argument fixture",
    ),
    "crates/harness/compose/tests/integration/pack_registrations.rs": (
        frozenset({"README.md"}),
        "a tool-call argument fixture",
    ),
    "bin/plak/src/refusal_text/tests.rs": (
        frozenset({'"docs"'}),
        "an MCP server id fixture; these refusal scenarios read no documentation",
    ),
    "crates/hosting/hosted/src/runtime/tool_facts/tests.rs": (
        frozenset({"README.md"}),
        "a conflict path fixture",
    ),
    "crates/orchestration/recursive-subagents/src/tools/tests/registrations.rs": (
        frozenset({'"docs"'}),
        "an MCP server id fixture",
    ),
    "eval/harbor_adapter/pmux_provenance.py": (
        frozenset({"README.md"}),
        "docstring naming eval/README.md",
    ),
    "eval/tests/test_eval_harbor_adapter.py": (
        frozenset({"README.md"}),
        "reads eval/README.md; not a gate",
    ),
    "tools/dev/api_model_census.py": (frozenset({'"docs"', "*.md"}), "census fixture directories"),
    "tools/prompts/native_compare_prompts.py": (
        frozenset({"README.md"}),
        "writes a scratch README",
    ),
    "tests/fixtures/facade-consumer/src/inventory.rs": (frozenset({'"spec"'}), "an expect message"),
    "tests/live/features.py": (frozenset({"README.md"}), "a scenario repository file; docstring"),
    "tests/live/jobs.py": (frozenset({"README.md"}), "a scenario repository file"),
    "tests/live/long_session.py": (frozenset({"README.md"}), "a scenario repository file"),
    "tests/live/lookup_retry.py": (frozenset({"README.md"}), "a scenario repository file"),
    "tests/live/soak.py": (frozenset({'"docs"'}), "a soak task name"),
    "tests/tooling/test_api_model_census.py": (frozenset({"*.md"}), "census fixture pages"),
    "tests/tooling/test_build_plak.py": (
        frozenset({'"docs"'}),
        "a temporary parent for the copied reference home, not spec or the user guide",
    ),
    "tests/tooling/test_changed_packages.py": (
        frozenset({"docs/spec/05-testing-and-evidence.md"}),
        "a path the mapper classifies, never opened",
    ),
    "tests/tooling/test_policy_validators.py": (
        frozenset({"README.md"}),
        "a temporary package fixture",
    ),
    "tests/tooling/test_publish.py": (
        frozenset(
            {
                "CHANGELOG.md",
                "README.md",
                "STATUS.md",
                "docs/spec/01-plakous-library.md",
                "docs/spec/01.md",
                "docs/spec/05.md",
                "docs/spec/07-config-reference.md",
                "docs/user-guide/cli.md",
            }
        ),
        "commits to a throwaway fixture repository that exercise this rule",
    ),
    "tests/tooling/test_publish_shape.py": (frozenset({"README.md"}), "a temporary source tree"),
    "tests/tooling/test_release.py": (frozenset({"README.md"}), "a release-archive fixture"),
    "tools/gate/check_workspace.py": (
        frozenset({"README.md"}),
        "an expected manifest readme value",
    ),
    "vendor/monty-typeshed/src/lib.rs": (
        frozenset({"README.md"}),
        "the vendored crate's own README",
    ),
}
# Gate readers of an excluded path: the exact path each one reads.
EXCLUDED_READERS = {
    "bin/plak/src/product_config/tests.rs": "docs/spec/07-config-reference.md",
    "bin/plak/src/host/console/definitions/settings.rs": "docs/spec/07-config-reference.md",
}
# Allowlisted paths, their path components and Markdown globs. Bare component
# literals catch path joins such as `.join("docs").join("user-guide")` and
# `ROOT / "docs" / "spec"`.
REFERENCE = re.compile(
    r"docs/spec(?:/[\w.*{},-]+)*|docs/user-guide(?:/[\w.*-]+)*|CHANGELOG(?:\.md)?|STATUS\.md|README(?:\.md)?"
    r"|\*\.md\b"
)
COMPONENT = re.compile(r"[\"'](?:[^\"'\s]*/)?(?:docs|spec|user-guide|STATUS)/?[\"']")
# A root README reader in code: an include resolved from the source file, a
# path joined to the crate directory, or the repository root joined with
# README.md. Manifest `readme` keys are what packaging reads, which the
# package group covers.
README_INCLUDE = re.compile(r"include_(?:str|bytes)!\(\s*\"([^\"]*README\.md)\"")
README_MANIFEST_JOIN = re.compile(r"CARGO_MANIFEST_DIR[^\n]{0,160}?\"([^\"]*README\.md)\"")
README_ROOT_JOIN = re.compile(r"\bROOT\s*/\s*[\"']README\.md")
MANIFEST_README_KEY = re.compile(r"^\s*readme\s*=")
OBJECT_ID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
RUST_COMMENT = re.compile(r"^\s*(//|/\*|\*)")
HASH_COMMENT = re.compile(r"^\s*#")


def matches(path: str, pattern: str) -> bool:
    if pattern.endswith("/**"):
        return path.startswith(pattern[:-2])
    return fnmatch.fnmatchcase(path, pattern)


def classify(path: str) -> str:
    """`docs`, `package` or `other` for one changed path."""
    if path in EXCLUDED:
        return "other"
    if any(matches(path, pattern) for pattern in ALLOWLIST):
        return "docs"
    if path in PACKAGE_ONLY:
        return "package"
    return "other"


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=root, check=True, capture_output=True, text=True
    ).stdout


def read_evidence(path: Path) -> str:
    try:
        return path.read_text().strip()
    except FileNotFoundError:
        return ""


def hygiene_findings(root: Path, paths: list[str]) -> list[str]:
    """The tracked-file hygiene scan of check_workspace.py over `paths`."""
    spec = importlib.util.spec_from_file_location(
        "check_workspace", root / "tools/gate/check_workspace.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load tools/gate/check_workspace.py")
    workspace = importlib.util.module_from_spec(spec)
    # Publication runs on a clean tree; a bytecode cache must not dirty it.
    sys.dont_write_bytecode = True
    spec.loader.exec_module(workspace)
    private_names = workspace.load_hygiene_private_names(root)
    findings: list[str] = []
    for relative in paths:
        path = root / relative
        if path.is_file() and not path.is_symlink():
            findings.extend(workspace.hygiene_findings(relative, path.read_bytes(), private_names))
    return findings


def tree_delta(root: Path, evidence: str, head: str) -> list[tuple[str, str, str]]:
    """(old mode, new mode, path) per changed entry, from plumbing.

    `diff-tree` with `--ignore-submodules=none` reads no porcelain diff
    configuration, so user settings cannot hide a gitlink or other change.
    """
    raw = git(
        root, "diff-tree", "-r", "--no-renames", "-z", "--ignore-submodules=none", evidence, head
    )
    fields = raw.split("\0")
    entries: list[tuple[str, str, str]] = []
    index = 0
    while index + 1 < len(fields) and fields[index]:
        modes = fields[index].lstrip(":").split()
        entries.append((modes[0], modes[1], fields[index + 1]))
        index += 2
    return sorted(entries, key=lambda entry: entry[2])


def verify(root: Path, target_dir: Path) -> tuple[bool, list[str]]:
    """Whether publication evidence covers HEAD, and the audit lines to print."""
    head = git(root, "rev-parse", "HEAD^{tree}").strip()
    evidence = read_evidence(target_dir / "gate-evidence" / "full")
    if not evidence:
        return False, [
            f"no passing `just full` evidence for tree {head} in {target_dir / 'gate-evidence' / 'full'}"
        ]
    if evidence == head:
        return True, [f"evidence: `just full` passed on tree {head} (HEAD)"]
    unknown = (
        f"no passing `just full` evidence for tree {head}: evidence names unknown tree {evidence!r}"
    )
    if not OBJECT_ID.fullmatch(evidence):
        return False, [unknown]
    probe = subprocess.run(
        ["git", "cat-file", "-t", "--end-of-options", evidence],
        cwd=root,
        capture_output=True,
        text=True,
    )
    if probe.returncode != 0 or probe.stdout.strip() != "tree":
        return False, [unknown]
    entries = tree_delta(root, evidence, head)
    lines = [f"evidence: `just full` passed on tree {evidence}; HEAD tree {head}"]
    if not entries:
        return False, lines + [
            f"no passing `just full` evidence for tree {head}: the trees differ but no delta "
            "was read; rerun `just full`"
        ]
    delta = [path for _, _, path in entries]
    classes = {path: classify(path) for path in delta}
    lines.extend(f"  delta {classes[path]}: {path}" for path in delta)
    special = [path for old, new, path in entries if {old, new} & {"120000", "160000"}]
    if special:
        return False, lines + [
            f"no passing `just full` evidence for tree {head}: the delta changes a symlink or submodule "
            f"({', '.join(special)}); rerun `just full`"
        ]
    other = [path for path in delta if classes[path] == "other"]
    if other:
        return False, lines + [
            f"no passing `just full` evidence for tree {head}: the delta from {evidence} changes gate inputs "
            f"({', '.join(other)}); rerun `just full`"
        ]
    findings = hygiene_findings(root, delta)
    if findings:
        return False, lines + findings + [
            f"no passing `just full` evidence for tree {head}: the delta fails hygiene"
        ]
    if any(classes[path] == "package" for path in delta):
        package = read_evidence(target_dir / "gate-evidence" / "package")
        if package != head:
            return False, lines + [
                f"no package evidence for tree {head}: the delta changes packaged files; "
                "rerun `just steps package` on this tree"
            ]
        lines.append(f"evidence: package group passed on tree {head}")
    lines.append("evidence: the delta is documentation only; the full evidence stands")
    return True, lines


def audit(root: Path, files: list[str]) -> list[str]:
    """Gate inputs that name an allowlisted path, a path component of one, or
    a Markdown glob without review.

    A best-effort lint: it cannot see every way code builds a path. The rule
    itself stays fail-safe because only listed documentation is inert and
    every other delta invalidates the evidence.
    """
    errors: list[str] = []
    for path in EXCLUDED:
        if not any(matches(path, pattern) for pattern in ALLOWLIST):
            errors.append(f"EXCLUDED path {path} is not under the allowlist; drop the exclusion")
    for reader, target in EXCLUDED_READERS.items():
        if target not in EXCLUDED:
            errors.append(f"{reader} reads {target}, which must be EXCLUDED from the allowlist")
    seen: set[tuple[str, str]] = set()
    for relative in files:
        name = Path(relative).name
        if relative in SELF or not (relative.endswith(SCANNED_SUFFIXES) or name in SCANNED_NAMES):
            continue
        if classify(relative) != "other":
            continue
        path = root / relative
        if not path.is_file() or path.is_symlink():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        comment = RUST_COMMENT if relative.endswith(".rs") else HASH_COMMENT
        reviewed, _reason = REVIEWED_REFERENCES.get(relative, (frozenset(), ""))
        for number, line in enumerate(text.splitlines(), start=1):
            if comment.match(line) or (
                relative.endswith(".toml") and MANIFEST_README_KEY.match(line)
            ):
                continue
            plain = [
                match for match in REFERENCE.finditer(line) if "://" not in line[: match.start()]
            ]
            components = [
                match
                for match in COMPONENT.finditer(line)
                if "://" not in match.group(0)
                and not any(match.start() <= other.start() < match.end() for other in plain)
            ]
            for match in plain + components:
                named = match.group(0)
                seen.add((relative, named))
                if EXCLUDED_READERS.get(relative) == named or named in reviewed:
                    continue
                errors.append(
                    f"{relative}:{number}: names {named!r}, an allowlisted path or a component of one; a gate "
                    "input may not read documentation the evidence rule treats as inert (exclude the file from "
                    "the allowlist, or add the exact text to REVIEWED_REFERENCES with why it reads nothing)"
                )
        # Multi-line include_str!(concat!(...)) forms read across lines.
        joined = " ".join(text.split())
        if reads_root_readme(relative, joined, files):
            errors.append(
                f"{relative}: reads the root README.md; PACKAGE_ONLY assumes no gate but packaging does"
            )
    listed = set(files)
    for relative, (texts, _reason) in sorted(REVIEWED_REFERENCES.items()):
        if relative not in listed:
            continue
        for named in sorted(texts):
            if (relative, named) not in seen:
                errors.append(
                    f"REVIEWED_REFERENCES entry {relative}: {named!r} no longer occurs; drop it"
                )
    return errors


def reads_root_readme(relative: str, line: str, files: list[str]) -> bool:
    if README_ROOT_JOIN.search(line):
        return True
    include = README_INCLUDE.search(line)
    if include is not None:
        return os.path.normpath(Path(relative).parent / include.group(1)) == "README.md"
    joined = README_MANIFEST_JOIN.search(line)
    if joined is not None:
        crate = Path(relative).parent
        while crate != Path(".") and (crate / "Cargo.toml").as_posix() not in files:
            crate = crate.parent
        return os.path.normpath(crate / joined.group(1).lstrip("/")) == "README.md"
    return False


def listed_files(root: Path) -> list[str]:
    sys.path.insert(0, str(root / "tools/gate"))
    import vcs_listing  # noqa: PLC0415

    return vcs_listing.listed_files(root)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    check = sub.add_parser("verify", help="whether publication evidence covers HEAD")
    check.add_argument("--target-dir", type=Path, required=True)
    sub.add_parser("audit", help="no gate input reads an allowlisted path")
    args = parser.parse_args()
    if args.command == "verify":
        try:
            ok, lines = verify(ROOT, args.target_dir)
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            ok, lines = False, [f"cannot verify publication evidence: {error}"]
        for line in lines:
            print(f"publish: {line}", file=sys.stdout if ok else sys.stderr)
        return 0 if ok else 1
    errors = audit(ROOT, listed_files(ROOT))
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    return int(bool(errors))


if __name__ == "__main__":
    sys.exit(main())
