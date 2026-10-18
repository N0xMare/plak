"""The published `plak` manifest drops exactly `test-hooks` and its harness edge."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "publish_shape", ROOT / "tools/release/publish_shape.py"
)
assert SPEC is not None and SPEC.loader is not None
publish_shape = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publish_shape)


class PublishedPlakManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repository = (ROOT / publish_shape.PLAK_MANIFEST).read_text(encoding="utf-8")

    def test_only_the_feature_and_its_harness_edge_are_removed(self) -> None:
        before = tomllib.loads(self.repository)
        after = tomllib.loads(publish_shape.published_plak_manifest(self.repository))
        self.assertNotIn("test-hooks", after["features"])
        self.assertNotIn(publish_shape.HARNESS, after["dependencies"])
        # The version-less dev-dependency stays; `cargo package` strips it.
        self.assertNotIn("version", after["dev-dependencies"][publish_shape.HARNESS])
        before["features"].pop("test-hooks")
        before["dependencies"].pop(publish_shape.HARNESS)
        self.assertEqual(after, before)

    def test_a_moved_block_fails_closed(self) -> None:
        edited = self.repository.replace('"plakous-hosted/test-support",\n]', '"plakous-hosted/test-support",\n    "x",\n]')
        edited = edited.replace("optional = true }", "optional = true, features = [] }")
        with self.assertRaises(publish_shape.ShapeError):
            publish_shape.published_plak_manifest(edited)

    def test_stage_writes_the_published_manifest_into_a_tree_copy(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            source = Path(scratch) / "src"
            (source / "bin/plak").mkdir(parents=True)
            (source / "bin/plak/Cargo.toml").write_text(self.repository, encoding="utf-8")
            (source / "README.md").write_text("x\n", encoding="utf-8")
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            subprocess.run(["git", "-C", str(source), "add", "-A"], check=True)
            destination = Path(scratch) / "staged"
            publish_shape.stage(source, destination)
            self.assertEqual((destination / "README.md").read_text(), "x\n")
            staged = (destination / "bin/plak/Cargo.toml").read_text(encoding="utf-8")
            self.assertEqual(staged, publish_shape.published_plak_manifest(self.repository))
            with self.assertRaises(publish_shape.ShapeError):
                publish_shape.stage(source, destination)


if __name__ == "__main__":
    sys.exit(unittest.main())
