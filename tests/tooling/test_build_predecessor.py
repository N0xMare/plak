"""Predecessor preparation uses a recipe-specific cache with bounded retention."""

import os
from pathlib import Path
import subprocess
import sys
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
            env = dict(os.environ, CARGO_BUILD_TARGET="example-target", RUSTFLAGS="-Copt-level=1")
            with patch.dict(os.environ, env, clear=True):
                _, cache, *_ = predecessor.build_inputs(target, predecessor.revision())
            cache.mkdir(parents=True)
            command = [
                sys.executable,
                str(GATE / "build_predecessor.py"),
                "--check",
                "--target-dir",
                str(target),
            ]

            def check(**flags):
                return subprocess.run(
                    command, env=dict(env, **flags), capture_output=True, text=True
                )

            self.assertEqual(check().returncode, 1)
            for name in predecessor.BINARIES:
                image = cache / name
                image.write_text("fixture image")
                image.chmod(0o700)
            hit = check()
            self.assertEqual(hit.returncode, 0, hit.stderr)
            self.assertEqual(hit.stdout.strip(), str(cache))
            for flag in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
                with self.subTest(flag=flag):
                    self.assertEqual(check(**{flag: "-Copt-level=2"}).returncode, 1)

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
                patch.object(predecessor.subprocess, "run", side_effect=fake_build),
            ):
                published = predecessor.build(
                    root, cache, "toolchain", [], ["build"], current, None
                )
            self.assertEqual(published, cache)
            self.assertTrue(predecessor.cache_ready(cache))
            self.assertEqual(
                {path.name for path in root.iterdir()}, {current, previous, "build.lock"}
            )
