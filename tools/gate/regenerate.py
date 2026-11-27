"""Regenerate mergeable projections without changing their inputs or checks."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from collections.abc import Callable
from pathlib import Path

import nextest_layout

ROOT = Path(__file__).resolve().parents[2]
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


def bindings() -> None:
    subprocess.run(
        [*HOSTED_TEST, "canonical_route_bindings_and_plans_match_the_reviewed_matrix"],
        cwd=ROOT,
        check=True,
        env=dict(os.environ, PLAK_UPDATE_BINDING_CONTRACTS="1"),
    )


def nextest() -> None:
    config = nextest_layout.CONFIG
    write_if_changed(
        config, nextest_layout.sync_membership(config.read_text(), nextest_layout.install_catalog())
    )


def reference() -> None:
    subprocess.run(
        ["cargo", "test", "--locked", "--offline", "-p", "plakd", "reference"],
        cwd=ROOT,
        check=True,
        env=dict(os.environ, PLAK_REGENERATE_REFERENCE_CONFIGS="1"),
    )


def wire() -> None:
    output = subprocess.check_output(
        [
            "cargo",
            "run",
            "--locked",
            "--offline",
            "-p",
            "plakd",
            "--bin",
            "plakd",
            "--",
            "wire",
            "schema",
        ],
        cwd=ROOT,
        text=True,
    )
    fixture = ROOT / "bin/plakd/tests/fixtures/protocol-13-wire-schema.json"
    updated, count = re.subn(r'(?m)^  "binary_id": "[^"]+",$', '  "binary_id": null,', output)
    if count != 1:
        raise ValueError("expected one serving binary identity")

    def pin(schema: str) -> str:
        body = json.loads(schema)
        del body["binary_id"]
        payload = json.dumps(body, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
        return hashlib.sha256(payload.encode()).hexdigest()

    old_pin, new_pin = pin(fixture.read_text()), pin(updated)
    for relative in (
        "bin/plakd/src/wire_schema/tests.rs",
        "docs/protocol/13/README.md",
        "docs/spec/02-plak-product.md",
    ):
        path = ROOT / relative
        source = path.read_text()
        if source.count(old_pin) != 1:
            raise ValueError(f"expected one prior wire schema pin in {relative}")
        write_if_changed(path, source.replace(old_pin, new_pin))
    write_if_changed(fixture, updated)
    print(f"protocol 13 schema: {new_pin}", flush=True)


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
        "bindings": bindings,
        "nextest": nextest,
        "prompts": prompts,
        "reference": reference,
        "wire": wire,
    }
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("parts", nargs="*", metavar="PART", help=", ".join(generators))
    parts = parser.parse_args().parts
    unknown = set(parts) - generators.keys()
    if unknown:
        parser.error(f"unknown parts: {', '.join(sorted(unknown))}")
    for name, generate in generators.items():
        if not parts or name in parts:
            print(f"regenerating {name}", flush=True)
            try:
                generate()
            except subprocess.CalledProcessError as error:
                raise SystemExit(f"regenerating {name} failed: {error}") from None


if __name__ == "__main__":
    main()
