"""`just fast` maps working-tree paths to the packages it lints and tests."""

import importlib.util
import sys
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "changed_packages", ROOT / "tools/checks/changed_packages.py"
)
assert SPEC and SPEC.loader
CHANGED = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHANGED
SPEC.loader.exec_module(CHANGED)

DIRS = {
    "bin/plak": "plak",
    "crates/core/model": "plakous-model",
    "crates/runtimes/py-eval": "plakous-py-eval",
    "crates/runtimes/py-worker": "plakous-py-worker",
}


class TouchedPackagesTests(unittest.TestCase):
    def test_ignored_proptest_regressions_are_package_inputs(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(
                os.environ, {"PATH": "/usr/bin:/bin"}):
            root = Path(temporary)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True)
            git("init", "-q")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            (root / ".gitignore").write_text("proptest-regressions/\nignored.rs\n")
            git("add", ".")
            git("commit", "-qm", "base")
            regression = "crates/core/model/proptest-regressions/canonical.txt"
            (root / regression).parent.mkdir(parents=True)
            (root / regression).write_text("seed")
            (root / "ignored.rs").write_text("ignored")
            paths = CHANGED.changed_paths(root)
            self.assertEqual(paths, [regression])
            self.assertEqual(CHANGED.touched_packages(paths, DIRS), ["plakous-model"])

    def touched(self, *paths):
        return CHANGED.touched_packages(list(paths), DIRS)

    def test_a_file_belongs_to_its_innermost_package(self):
        self.assertEqual(
            self.touched("crates/core/model/src/lib.rs", "bin/plak/tests/goldens/a.frame",
                         "crates/core/model/Cargo.toml"),
            ["plak", "plakous-model"],
        )

    def test_sibling_directory_prefixes_do_not_match(self):
        self.assertEqual(self.touched("crates/runtimes/py-eval/src/lib.rs"), ["plakous-py-eval"])
        self.assertEqual(self.touched("crates/core/model-extra/src/lib.rs"), [])

    def test_files_outside_every_package_touch_nothing(self):
        self.assertEqual(self.touched("docs/spec/05-testing-and-evidence.md",
                                      "tools/checks/check_features.py", "Justfile"), [])

    def test_workspace_inputs_select_the_whole_workspace(self):
        for path in ("Cargo.toml", "Cargo.lock", ".config/nextest.toml", ".cargo/config.toml",
                     "rust-toolchain.toml"):
            with self.subTest(path=path):
                self.assertEqual(self.touched("bin/plak/src/lib.rs", path), ["--workspace"])

    def test_the_real_workspace_maps_every_manifest_to_its_package(self):
        dirs = CHANGED.package_dirs()
        self.assertEqual(dirs["bin/plak"], "plak")
        for directory, name in dirs.items():
            self.assertEqual(self.touched_in(dirs, f"{directory}/Cargo.toml"), [name])

    @staticmethod
    def touched_in(dirs, path):
        return CHANGED.touched_packages([path], dirs)


if __name__ == "__main__":
    unittest.main()
