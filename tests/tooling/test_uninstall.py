"""Offline uninstall in an isolated HOME; no downloads or Rust builds."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class UninstallTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name).resolve()
        self.private_tmp = self.root / "private/tmp"
        self.home = self.private_tmp / "home"
        self.home.mkdir(parents=True)
        self.environment = {key: value for key, value in os.environ.items() if key != "CARGO_HOME"}
        self.lib = self.home / ".local/lib/plak"
        self.bin = self.home / ".local/bin"
        self.bin.mkdir(parents=True)
        for name in ("release", "maintainer", "sidecars"):
            (self.lib / name / "fixture").mkdir(parents=True)
            (self.lib / name / "fixture/artifact").write_text("installed")
        for name in ("plak", "plakous-js-host", "plakous-py-host", "monty"):
            (self.bin / name).symlink_to(self.lib / "release/fixture" / name)
        (self.home / ".plak").mkdir()
        (self.home / ".plak/keep").write_text("product state")
        (self.home / ".local/state/plak/subscriptions").mkdir(parents=True)
        (self.home / ".local/state/plak/subscriptions/keep").write_text("fixture only")

    def uninstall(self, *args):
        return subprocess.run(["sh", str(ROOT / "install.sh"), "uninstall", *args],
                              env={**self.environment, "HOME": str(self.home)},
                              capture_output=True, text=True, timeout=10)

    def test_removes_installed_artifacts_but_preserves_product_home(self):
        result = self.uninstall()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.bin.iterdir()), [])
        self.assertEqual(list(self.lib.iterdir()), [])
        self.assertEqual((self.home / ".plak/keep").read_text(), "product state")
        self.assertTrue((self.home / ".local/state/plak/subscriptions/keep").exists())
        self.assertEqual(self.uninstall().returncode, 0)

    def test_unrelated_links_and_files_are_retained(self):
        (self.bin / "plak").unlink()
        (self.bin / "plak").write_text("not installed by Plak")
        (self.bin / "monty").unlink()
        (self.bin / "monty").symlink_to("/unrelated/monty")
        self.assertEqual(self.uninstall().returncode, 0)
        self.assertTrue((self.bin / "plak").is_file())
        self.assertEqual(os.readlink(self.bin / "monty"), "/unrelated/monty")

    def test_symlinked_root_is_refused_before_any_removal(self):
        shutil.rmtree(self.lib / "sidecars")
        (self.lib / "sidecars").symlink_to(self.home / ".plak", target_is_directory=True)
        result = self.uninstall()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe or symlinked", result.stderr)
        self.assertTrue((self.bin / "plak").is_symlink())
        self.assertTrue((self.lib / "release").exists())
        self.assertTrue((self.home / ".plak/keep").exists())

    def test_unknown_option_cannot_remove_product_home(self):
        result = self.uninstall("--purge-home")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.home / ".plak/keep").exists())

    def noncanonical_links(self):
        alias = self.root / "tmp"
        alias.symlink_to(self.private_tmp, target_is_directory=True)
        logical_home = alias / "home"
        for link in self.bin.iterdir():
            link.unlink()
            link.symlink_to(logical_home / ".local/lib/plak/release/fixture" / link.name)
        return logical_home

    def test_noncanonical_targets_with_physical_home_are_removed(self):
        self.noncanonical_links()
        result = self.uninstall()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.bin.iterdir()), [])

    def test_symlinked_tmp_home_matches_default_macos_layout(self):
        self.home = self.noncanonical_links()
        result = self.uninstall()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.bin.iterdir()), [])
        self.assertEqual((self.home / ".plak/keep").read_text(), "product state")

    def cargo_links(self, home):
        directory = home / "bin"
        directory.mkdir(parents=True)
        for name in ("plak", "plakous-js-host", "plakous-py-host"):
            (directory / name).symlink_to(self.lib / "maintainer/fixture" / name)
        return directory

    def test_relinked_default_cargo_entries_are_removed(self):
        directory = self.cargo_links(self.home / ".cargo")
        result = self.uninstall()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(directory.iterdir()), [])

    def test_custom_cargo_home_removes_only_plak_links(self):
        cargo_home = self.home / "custom-cargo"
        directory = self.cargo_links(cargo_home)
        (directory / "plak").unlink()
        (directory / "plak").symlink_to("/unrelated/plak")
        self.environment["CARGO_HOME"] = str(cargo_home)
        result = self.uninstall()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(directory.iterdir()), [directory / "plak"])
        self.assertEqual(os.readlink(directory / "plak"), "/unrelated/plak")

    def test_dry_run_lists_links_and_roots_without_removing_anything(self):
        cargo = self.cargo_links(self.home / ".cargo")
        result = self.uninstall("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        for path in [*self.bin.iterdir(), *cargo.iterdir(), *self.lib.iterdir()]:
            self.assertIn(str(path), result.stderr)
            self.assertTrue(path.exists() or path.is_symlink())
        self.assertEqual(len(list(self.bin.iterdir())), 4)
        self.assertEqual(len(list(cargo.iterdir())), 3)
        self.assertEqual(len(list(self.lib.iterdir())), 3)
        self.assertEqual((self.home / ".plak/keep").read_text(), "product state")
