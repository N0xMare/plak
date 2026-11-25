"""Predecessor preparation uses a recipe-specific cache with bounded retention."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

GATE = Path(__file__).resolve().parents[2] / "tools/gate"
sys.path.insert(0, str(GATE))
import build_predecessor as predecessor  # noqa: E402


class PredecessorCacheTests(unittest.TestCase):
    def test_check_only_reports_complete_images_for_the_requested_recipe(self):
        with tempfile.TemporaryDirectory() as work:
            target = Path(work) / "target with spaces"
            revision = predecessor.revision()
            env = dict(os.environ, CARGO_BUILD_TARGET="example-target", RUSTFLAGS="-Copt-level=1")
            with patch.dict(os.environ, env, clear=True):
                _, cache, *_ = predecessor.build_inputs(target, revision)
            cache.mkdir(parents=True)
            command = [
                sys.executable,
                str(GATE / "build_predecessor.py"),
                "--check",
                "--target-dir",
                str(target),
            ]

            def check(*args, **flags):
                return subprocess.run(
                    command + list(args), env=dict(env, **flags), capture_output=True, text=True
                )

            self.assertEqual(check().returncode, 1)
            for name in predecessor.BINARIES:
                image = cache / name
                image.write_text("fixture image")
                image.chmod(0o700)
            self.assertEqual(check().returncode, 1)
            record = cache / predecessor.MANIFEST
            record.write_text(json.dumps(predecessor.manifest(cache, revision, cache.name)))
            hit = check()
            self.assertEqual(hit.returncode, 0, hit.stderr)
            self.assertEqual(hit.stdout.strip(), str(cache))
            for flag in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
                with self.subTest(flag=flag):
                    self.assertEqual(check(**{flag: "-Copt-level=2"}).returncode, 1)

            wrong_archive = Path(work) / "wrong.tar"
            wrong_revision = "a" * 40 if revision != "a" * 40 else "b" * 40
            with tarfile.open(wrong_archive, "w", pax_headers={"comment": wrong_revision}):
                pass
            refused = check("--source-archive", str(wrong_archive))
            self.assertEqual(refused.returncode, 1)
            self.assertIn("predecessor archive does not match", refused.stderr)

            (cache / predecessor.BINARIES[-1]).write_text("different image")
            self.assertEqual(check().returncode, 1)

    def test_published_image_must_report_the_pinned_build(self):
        revision = "a" * 40
        with tempfile.TemporaryDirectory() as work:
            binary = Path(work) / "plakd"
            for actual in ("b" * 40, revision):
                facts = json.dumps(
                    {"revision": actual, "flavor": "maintainer debug", "test_hooks": True}
                )
                binary.write_text(f"#!/bin/sh\nprintf '%s\\n' '{facts}'\n")
                binary.chmod(0o700)
                if actual == revision:
                    predecessor.verify_image(binary, revision)
                else:
                    with self.assertRaisesRegex(ValueError, "pinned source"):
                        predecessor.verify_image(binary, revision)

    def test_publish_discards_interrupted_scratch_and_keeps_two_revisions(self):
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            current, previous, older = (letter * 40 for letter in "abc")
            for rev, tick in ((previous, 2), (older, 1)):
                (root / rev).mkdir()
                os.utime(root / rev, (tick, tick))
            scratch = root / "build-interrupted"
            scratch.mkdir()
            cache = root / current / "toolchain" / "target" / "recipe"

            def fake_build(command, **kwargs):
                if command[2] == "build":
                    artifacts = Path(kwargs["env"]["CARGO_TARGET_DIR"]) / "debug"
                    artifacts.mkdir(parents=True)
                    for name in predecessor.BINARIES:
                        image = artifacts / name
                        image.touch(mode=0o700)
                return subprocess.CompletedProcess(command, 0)

            with (
                patch.object(predecessor, "archive"),
                patch.object(predecessor, "verify_image") as verify,
                patch.object(predecessor.subprocess, "run", side_effect=fake_build),
            ):
                published = predecessor.build(
                    root, cache, "toolchain", [], ["build"], current, None
                )
            self.assertEqual(published, cache)
            verify.assert_called_once()
            self.assertEqual(verify.call_args.args[1], current)
            self.assertTrue(predecessor.cache_ready(cache, current))
            self.assertFalse(predecessor.cache_ready(cache, previous))
            self.assertEqual(
                {path.name for path in root.iterdir()},
                {current, previous, "build.lock", "cargo-target"},
            )
