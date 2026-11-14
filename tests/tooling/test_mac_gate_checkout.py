"""The Mac gate owns a detached checkout and retains its target across revisions."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class MacGateCheckoutTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.git("init", "-q")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        (self.root / ".gitignore").write_text(".plak/\ntarget/\n.dev-tools/\n")
        self.script = self.root / "gate.sh"
        shutil.copyfile(ROOT / "tools/gate/mac-checkout.sh", self.script)
        self.git("add", ".")
        self.git("commit", "-qm", "first")
        self.first = self.git("rev-parse", "HEAD").strip()
        (self.root / "value").write_text("second")
        self.git("add", ".")
        self.git("commit", "-qm", "second")
        self.second = self.git("rev-parse", "HEAD").strip()
        self.checkout = self.root / ".plak/gate-macos"
        (self.root / ".dev-tools/bin").mkdir(parents=True)
        (self.root / ".dev-tools/jj").mkdir()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True)

    def run_gate(self, revision, *command, **environment):
        env = {**os.environ, **environment}
        env.pop("PLAK_MAC_GATE_CHECKOUT", None)
        env.update(environment)
        return subprocess.run(
            ["bash", str(self.script), revision, *command],
            cwd=self.root,
            env=env,
            text=True,
            capture_output=True,
            timeout=15,
        )

    def test_exact_revision_and_warm_target_do_not_switch_the_launch_checkout(self):
        command = [
            "bash",
            "-c",
            'mkdir -p "$CARGO_TARGET_DIR"; echo retained > "$CARGO_TARGET_DIR/warm"; git rev-parse HEAD',
        ]
        first = self.run_gate(self.first, *command)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertIn(self.first, first.stdout)
        self.assertFalse((self.checkout / '.dev-tools').is_symlink())
        self.assertEqual(
            subprocess.check_output(
                ['git', '-C', str(self.checkout), 'status', '--porcelain'], text=True
            ),
            '',
        )
        self.assertEqual(self.git("rev-parse", "HEAD").strip(), self.second)
        second = self.run_gate(
            self.second, "bash", "-c", 'test -f "$CARGO_TARGET_DIR/warm"; git rev-parse HEAD'
        )
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn(self.second, second.stdout)
        self.assertEqual(self.git("rev-parse", "HEAD").strip(), self.second)

    def test_dirty_checkout_and_held_lock_refuse_without_discarding_work(self):
        result = self.run_gate(self.first, "true")
        self.assertEqual(result.returncode, 0, result.stderr)
        dirty = self.checkout / "untracked"
        dirty.write_text("keep")
        result = self.run_gate(self.second, "true")
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertEqual(dirty.read_text(), "keep")
        dirty.unlink()
        lock = Path(str(self.checkout) + ".lock")
        lock.mkdir()
        result = self.run_gate(self.second, "true")
        self.assertEqual(result.returncode, 3)
        self.assertTrue(lock.exists())

    def test_primary_checkout_is_never_repurposed_and_command_failure_unlocks(self):
        result = self.run_gate(self.first, "true", PLAK_MAC_GATE_CHECKOUT=str(self.root))
        self.assertEqual(result.returncode, 3)
        self.assertEqual(self.git("rev-parse", "HEAD").strip(), self.second)
        result = self.run_gate(self.first, "bash", "-c", "exit 17")
        self.assertEqual(result.returncode, 17)
        self.assertFalse(Path(str(self.checkout) + ".lock").exists())


if __name__ == "__main__":
    unittest.main()
