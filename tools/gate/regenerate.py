"""Regenerate mergeable projections without changing their inputs or checks."""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

import check_features
import nextest_layout

ROOT = check_features.ROOT
HOSTED = ROOT / "crates/hosting/hosted"
HOSTED_TEST = ["cargo", "test", "--locked", "--offline", "-p", "plakous-hosted", "--lib"]
PROMPT_TABLES = {
    "PROMPT_RENDER_PINS": "rendered_prompts.rs",
    "LIFECYCLE_RENDER_PINS": "rendered_lifecycle.rs",
    "COMPACTION_WIRE_PINS": "rendered_wire.rs",
}
PIN_ROW = r'        \("([a-zA-Z0-9_.-]+)", "([0-9a-f]{64})"\),'


def read_prompt_rows(output: str, table: str) -> list[tuple[str, str]]:
    matches = list(re.finditer(rf"(?m)^{table}:\n((?:{PIN_ROW}\n)+)END {table}$", output))
    if len(matches) != 1:
        raise ValueError(f"expected one emitted {table} table, found {len(matches)}")
    rows = re.findall(PIN_ROW, matches[0][1])
    if len(rows) != len({name for name, _ in rows}):
        raise ValueError(f"{table} contains duplicate pins")
    return rows


def replace_block(source: str, begin: str, end: str, render: Callable[[str], str]) -> str:
    pattern = rf"(?ms)^({re.escape(begin)}\n)(.*?)(^{re.escape(end)}$)"
    updated, count = re.subn(pattern, lambda match: match[1] + render(match[2]) + match[3], source)
    if count != 1:
        raise ValueError(f"expected one generated block starting with {begin!r}, found {count}")
    return updated


def write_if_changed(path: Path, updated: str) -> None:
    if updated != path.read_text():
        path.write_text(updated)


def manifest_digest_rows(rows: str, digests: dict[str, str]) -> str:
    pending = dict(digests)
    pattern = r'(?m)^((?:    # [^\n]*\n)*)    "([a-z0-9-]+)": "[0-9a-f]{64}",\n'

    def update(match: re.Match[str]) -> str:
        if match[2] not in pending:
            return ""
        return f'{match[1]}    "{match[2]}": "{pending.pop(match[2])}",\n'

    updated = re.sub(pattern, update, rows)
    return updated + "".join(
        f'    "{name}": "{digest}",\n' for name, digest in sorted(pending.items())
    )


def manifests() -> None:
    workspace = check_features.load_workspace(ROOT)
    digests = {
        name: check_features.security_surface_digest(workspace.packages[name].manifest)
        for name in check_features.digest_packages(workspace)
    }
    path = ROOT / "tools/gate/check_features.py"
    source = path.read_text()
    updated = replace_block(
        source,
        "EXPECTED_SECURITY_SURFACE_DIGESTS = {",
        "}",
        lambda rows: manifest_digest_rows(rows, digests),
    )
    write_if_changed(path, updated)


def bindings() -> None:
    subprocess.run(
        [*HOSTED_TEST, "canonical_route_bindings_and_plans_match_the_reviewed_matrix"],
        cwd=ROOT,
        check=True,
        env=dict(os.environ, PLAK_UPDATE_BINDING_CONTRACTS="1"),
    )


def current_bindings() -> None:
    subprocess.run([sys.executable, "tools/dev/fixtures/current_bindings.py"], cwd=ROOT, check=True)


def nextest() -> None:
    config = nextest_layout.CONFIG
    write_if_changed(
        config, nextest_layout.sync_membership(config.read_text(), nextest_layout.install_catalog())
    )


def prompt_pin_rows(previous: str, rows: list[tuple[str, str]]) -> str:
    pins = dict(rows)
    order = dict.fromkeys(re.findall(r'\(\s*"([a-zA-Z0-9_.-]+)",', previous))
    order |= dict.fromkeys(pins)
    return "".join(
        f'        (\n            "{name}",\n            "{pins[name]}",\n        ),\n'
        for name in order
        if name in pins
    )


def prompts() -> None:
    output = subprocess.check_output(
        [*HOSTED_TEST, "rendered_", "--", "--nocapture", "--test-threads=1"],
        cwd=ROOT,
        env=dict(os.environ, PLAK_PRINT_PROMPT_PINS="1"),
        text=True,
    )
    updates = {}
    for table, filename in PROMPT_TABLES.items():
        rows = read_prompt_rows(output, table)
        path = HOSTED / "tests/common" / filename
        updates[path] = replace_block(
            path.read_text(),
            f"    const {table}: &[(&str, &str)] = &[",
            "    ];",
            lambda previous: prompt_pin_rows(previous, rows),
        )
    for path, updated in updates.items():
        write_if_changed(path, updated)


def main() -> None:
    generators = {
        "manifests": manifests,
        "bindings": bindings,
        "current-bindings": current_bindings,
        "nextest": nextest,
        "prompts": prompts,
    }
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("parts", nargs="*", metavar="PART", help=", ".join(generators))
    parts = parser.parse_args().parts
    unknown = set(parts) - generators.keys()
    if unknown:
        parser.error(f"unknown parts: {', '.join(sorted(unknown))}")
    if (not parts or "current-bindings" in parts) and sys.platform != "linux":
        parser.error("current-bindings requires Linux; select other parts explicitly")
    for name, generate in generators.items():
        if not parts or name in parts:
            print(f"regenerating {name}", flush=True)
            generate()


if __name__ == "__main__":
    main()
