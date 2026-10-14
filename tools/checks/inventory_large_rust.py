#!/usr/bin/env python3
"""Reproducible source-planning inventory; no compiler reachability claims.

Run from any directory. Output goes to stdout; save local reports under target/.
Counts physical lines (including comments/blanks), conservatively retaining
conditional/test-support code. Report unresolved cfg sites and source includes
for the boundary reviewer rather than guessing which feature build is shipped.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
from collections import Counter
from pathlib import Path

from check_prod_loc import (
    ROOT, blank_literals, is_excluded_package_source, load_workspace,
    production_line_mask, responsibility_for, scope_of,
)


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True)


def inspect_source(path: Path, external_test: bool, benchmark: bool = False) -> dict:
    lines = path.read_text(encoding="utf-8").splitlines()
    mask = production_line_mask(lines)
    code = blank_literals(lines)
    if external_test or benchmark:
        mask = [False] * len(lines)
    # These are source navigation aids, not a resolved Rust dependency graph.
    declarations = []
    for index, line in enumerate(code):
        match = re.match(r"\s*(pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const)\s+)*(fn|struct|enum|trait|type|mod)\s+(\w+)", line)
        if match and mask[index]:
            declarations.append({"line": index + 1, "kind": match[2], "name": match[3], "visibility": (match[1] or "private").strip()})
    conditions = [
        {"line": i + 1, "source": line.strip()}
        for i, line in enumerate(lines)
        if mask[i] and re.search(r"#\s*\[\s*(?:cfg|cfg_attr)\b", code[i])
    ]
    inclusions = [
        {"line": i + 1, "source": line.strip()}
        for i, line in enumerate(lines)
        if re.search(r"\binclude(?:_str|_bytes)?!", code[i])
    ]
    return {
        "physical_lines": len(lines),
        "production_candidate_lines": sum(mask),
        "inline_test_module_lines": 0 if external_test or benchmark else len(lines) - sum(mask),
        "external_test_lines": len(lines) if external_test else 0,
        "benchmark_lines": len(lines) if benchmark else 0,
        "conditional_sites_in_candidates": conditions,
        "source_or_data_inclusions": inclusions,
        "declarations": declarations,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--threshold", type=int, default=5000)
    args = parser.parse_args()
    workspace = load_workspace(ROOT)
    packages = sorted(workspace.packages.values(), key=lambda p: -len(p.directory))
    paths = sorted(p for p in git("ls-files", "crates", "bin").splitlines() if p.endswith(".rs"))
    churn = Counter(git("log", "-100", "--format=", "--name-only", "--", "crates", "bin").splitlines())
    files = []
    totals = Counter()
    responsibilities: dict[str, Counter] = {}
    for relative in paths:
        path = ROOT / relative
        package = next((p for p in packages if relative.startswith(p.directory + "/")), None)
        if package is None:
            raise ValueError(f"unowned Rust source: {relative}")
        local = relative[len(package.directory) + 1:]
        external = local.startswith("tests/") or is_excluded_package_source(package, local)
        details = inspect_source(path, external, benchmark=local.startswith("benches/"))
        responsibility = responsibility_for(package, local) if local.startswith("src/") and not external else "test_or_build_source"
        categories = {key: details[key] for key in ("physical_lines", "production_candidate_lines", "inline_test_module_lines", "external_test_lines", "benchmark_lines")}
        totals.update(categories)
        responsibilities.setdefault(responsibility, Counter()).update(categories)
        if details["physical_lines"] >= args.threshold:
            files.append({"path": relative, "package": package.name, "scope": scope_of(package), "responsibility": responsibility, "touches_last_100_commits": churn[relative], **details})
    print(json.dumps({
        "source_commit": git("rev-parse", "HEAD").strip(),
        "threshold": args.threshold,
        "scope": "tracked .rs under crates/ and bin/; excludes target/OUT_DIR, vendored upstream and repository-only tests/ packages",
        "counting_contract": "Physical lines, not SLOC. Candidates include conditional instrumentation, all platform branches and test-support. Inline literal cfg(test) modules and conventional test modules excluded. Inspect conditional_sites_in_candidates before calling a count compiled production. Includes are listed, never expanded/double-counted. Generated output outside tracked source is excluded by scope; build.rs remains a candidate source.",
        "all_source_totals": dict(totals),
        "responsibility_totals": {k: dict(v) for k, v in sorted(responsibilities.items())},
        "files": sorted(files, key=lambda f: -f["physical_lines"]),
    }, indent=2))


if __name__ == "__main__":
    main()
