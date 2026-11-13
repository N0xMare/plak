#!/usr/bin/env python3
"""Generate a native reopen fixture once, using the explicit predecessor's code."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, help="exact predecessor commit")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument(
        "--target-dir", required=True, type=Path, help="isolated predecessor artifacts"
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    source = subprocess.check_output(
        ["git", "rev-parse", f"{args.source}^{{commit}}"], cwd=root, text=True
    ).strip()
    destination = args.output.resolve()
    if destination.exists():
        parser.error("output must not exist; never overwrite an accepted predecessor fixture")
    target = args.target_dir.resolve()
    if target == Path(os.environ.get("CARGO_TARGET_DIR", root / "target")).resolve():
        parser.error("predecessor must not share the candidate's target directory")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="binding-predecessor-") as temporary:
        scratch = Path(temporary)
        checkout = scratch / "source"
        subprocess.run(
            ["git", "worktree", "add", "--detach", str(checkout), source], cwd=root, check=True
        )
        try:
            test_root = checkout / "crates/hosting/hosted/src/agent"
            shutil.copyfile(
                root / "tools/dev/fixtures/previous_release.rs",
                test_root / "tests/previous_release_generator.rs",
            )
            with (test_root / "tests.rs").open("a") as output:
                output.write("\nmod previous_release_generator;\n")
                output.write("\nmod production_predecessor;\n")
            shutil.copyfile(
                root / "crates/hosting/hosted/src/agent/tests/production_predecessor.rs",
                test_root / "tests/production_predecessor.rs",
            )
            subscriptions = checkout / "crates/plakous/src/subscriptions"
            shutil.copyfile(
                root / "crates/plakous/src/subscriptions/test_support.rs",
                subscriptions / "test_support.rs",
            )
            with subscriptions.with_suffix(".rs").open("a") as output:
                output.write(
                    '\n#[cfg(feature = "test-clock")]\n#[doc(hidden)]\npub mod test_support;\n'
                )
            platform_tests = checkout / "crates/hosting/hosted/src/platform"
            shutil.copyfile(
                root / "crates/hosting/hosted/src/platform/tests/previous_fixture.rs",
                platform_tests / "tests/previous_fixture.rs",
            )
            with (platform_tests / "tests.rs").open("a") as output:
                output.write(
                    '\n#[path = "tests/previous_fixture.rs"]\nmod previous_fixture;\npub(crate) use previous_fixture::{previous_fixture_runtime, fixture_artifact, StableFixtureWorkers};\n'
                )
            environment = dict(
                os.environ,
                CARGO_TARGET_DIR=str(target),
                PLAK_BINDING_FIXTURE_OUTPUT=str(scratch / "fixture"),
                PLAK_BINDING_SOURCE_COMMIT=source,
            )
            subprocess.run(
                [
                    "just",
                    "nextest",
                    "run",
                    "--locked",
                    "-p",
                    "plakous-hosted",
                    "--all-features",
                    "--profile",
                    "offline",
                    "--run-ignored",
                    "only",
                    "-E",
                    "test(generate_previous_release_bindings)",
                ],
                cwd=checkout,
                env=environment,
                check=True,
            )
            produced = scratch / "fixture"
            destination.mkdir()
            database = destination / "previous-runtime.sqlite3"
            with sqlite3.connect(
                (produced / database.name).as_uri() + "?mode=ro", uri=True
            ) as original:
                with sqlite3.connect(database) as copied:
                    original.backup(copied)
            manifest = json.loads((produced / "manifest.json").read_text())
            manifest["sha256"] = hashlib.sha256(database.read_bytes()).hexdigest()
            (destination / "manifest.json").write_text(
                json.dumps(manifest, indent=2, sort_keys=True) + "\n"
            )
        finally:
            subprocess.run(
                ["git", "worktree", "remove", "--force", str(checkout)], cwd=root, check=True
            )


if __name__ == "__main__":
    main()
