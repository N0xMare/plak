"""Every test entry point takes its scratch directory from one selector, which
keeps it off the operator's HOME and on a physical, caller-owned path."""
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class TestTmpdirSelectorTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.checkout = self.base / "checkout"
        (self.checkout / "scripts").mkdir(parents=True)
        shutil.copyfile(ROOT / "scripts/test-tmpdir.sh", self.checkout / "scripts/test-tmpdir.sh")
        self.shared = self.base / "shared"
        self.shared.mkdir()
        self.fallback = self.checkout / "target/plak-test-tmp"
        self.env = dict(os.environ)
        self.env.pop("PLAK_TEST_TMPDIR", None)

    def select(self, *args):
        result = subprocess.run(["bash", str(self.checkout / "scripts/test-tmpdir.sh"), *args],
                                cwd=self.base, env=self.env, capture_output=True, text=True,
                                timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        return Path(result.stdout.strip()), result.stderr

    def per_checkout_name(self):
        digest = subprocess.run(["cksum"], input=str(self.checkout), capture_output=True,
                                text=True, check=True).stdout.split()[0]
        return f"plak-test-{os.getuid()}-{digest}"

    def test_linux_and_darwin_use_a_private_per_checkout_shared_directory(self):
        for system in ("Linux", "Darwin"):
            with self.subTest(system=system):
                selected, stderr = self.select("--system", system, "--shared-root", str(self.shared))
                self.assertEqual(selected, self.shared / self.per_checkout_name())
                self.assertEqual(stat.S_IMODE(selected.stat().st_mode), 0o700)
                self.assertFalse(selected.is_symlink())
                self.assertEqual(stderr, "")
                shutil.rmtree(selected)

    def test_fresh_empties_the_shared_directory_and_plain_runs_keep_it(self):
        selected, _ = self.select("--system", "Darwin", "--shared-root", str(self.shared))
        (selected / "left-over").write_text("x")
        self.assertEqual(self.select("--system", "Darwin", "--shared-root", str(self.shared))[0], selected)
        self.assertTrue((selected / "left-over").exists())
        self.assertEqual(self.select("--fresh", "--system", "Darwin",
                                     "--shared-root", str(self.shared))[0], selected)
        self.assertEqual(list(selected.iterdir()), [])

    def test_default_shared_roots_are_dev_shm_and_private_tmp(self):
        for system, shared_root in (("Linux", Path("/dev/shm")), ("Darwin", Path("/private/tmp"))):
            with self.subTest(system=system):
                usable = (shared_root.is_dir() and not shared_root.is_symlink()
                          and shared_root.resolve() == shared_root and os.access(shared_root, os.W_OK))
                if not usable:
                    continue
                selected, _ = self.select("--system", system)
                self.assertEqual(selected, shared_root / self.per_checkout_name())
                shutil.rmtree(selected)

    def test_symlinked_shared_paths_fall_back_to_the_checkout(self):
        linked_root = self.base / "linked-root"
        linked_root.symlink_to(self.shared)
        ancestor_link = self.base / "ancestor-link"
        ancestor_link.symlink_to(self.base)
        planted = self.base / "planted"
        planted.mkdir()
        (planted / self.per_checkout_name()).symlink_to(self.shared)
        for shared_root in (linked_root, ancestor_link / "shared", planted):
            with self.subTest(shared_root=shared_root):
                selected, stderr = self.select("--system", "Darwin", "--shared-root", str(shared_root))
                self.assertEqual(selected, self.fallback)
                self.assertIn("unusable; using target/plak-test-tmp", stderr)
        self.assertEqual(list(self.shared.iterdir()), [])

    def test_unknown_system_uses_the_checkout_quietly(self):
        selected, stderr = self.select("--system", "FreeBSD")
        self.assertEqual(selected, self.fallback)
        self.assertTrue(selected.is_dir())
        self.assertEqual(stderr, "")

    def test_explicit_directory_wins_and_is_resolved(self):
        explicit = self.base / "explicit"
        link = self.base / "explicit-link"
        explicit.mkdir()
        link.symlink_to(explicit)
        self.env["PLAK_TEST_TMPDIR"] = str(link / "nested")
        selected, _ = self.select("--fresh", "--system", "Linux", "--shared-root", str(self.shared))
        self.assertEqual(selected, explicit / "nested")
        self.assertTrue(selected.is_dir())
        self.assertEqual(list(self.shared.iterdir()), [])

    def test_every_test_entry_point_uses_the_selector(self):
        for path in ("scripts/dev-check.sh", "scripts/perf-smoke.sh", "scripts/crash-sweep.py"):
            with self.subTest(path=path):
                self.assertIn("test-tmpdir.sh", (ROOT / path).read_text())
        justfile = (ROOT / "Justfile").read_text()
        self.assertEqual(justfile.count('TMPDIR="$(bash scripts/test-tmpdir.sh)"'), 2)
        self.assertNotIn('TMPDIR="$PWD/target/plak-test-tmp"', justfile)


if __name__ == "__main__":
    unittest.main()
