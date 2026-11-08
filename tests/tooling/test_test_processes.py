"""Fixture process leaks fail commands without touching another test root."""

import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
GUARD = ROOT / "tools/gate/test-processes.py"


class TestProcessGuardTests(unittest.TestCase):
    def test_sigterm_still_checks_and_reaps_a_detached_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "host").write_text("sleep 60 & wait\n")
            (root / "runner").write_text("sleep 60 & wait\n")
            script = textwrap.dedent(f"""
                import pathlib, signal, subprocess, time
                def stopping(signum, frame):
                    subprocess.Popen(
                        ['plak', 'runner', 'serve', '--local'], executable='/bin/bash',
                        cwd={str(root)!r}, start_new_session=True,
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    raise SystemExit(0)
                signal.signal(signal.SIGTERM, stopping)
                child = subprocess.Popen(
                    ['plak', 'host'], executable='/bin/bash', cwd={str(root)!r},
                    start_new_session=True, stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                pathlib.Path({str(root / 'ready')!r}).touch()
                time.sleep(60)
            """)
            guarded = subprocess.Popen(
                [sys.executable, str(GUARD), "--", sys.executable, "-c", script],
                env=dict(os.environ, TMPDIR=str(root)),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not (root / "ready").exists():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                guarded.terminate()
                _, errors = guarded.communicate(timeout=10)
                self.assertEqual(guarded.returncode, 143, errors)
                self.assertIn("test process leak:", errors)
                self.assertIn("(host)", errors)
                self.assertIn("(runner serve)", errors)
            finally:
                if guarded.poll() is None:
                    guarded.kill()
                guarded.communicate()

    def test_deleted_workspace_leak_fails_and_is_reaped(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            workspace = root / "offline-tui/workspace"
            workspace.mkdir(parents=True)
            # Keep the script present until bash has read it. The child records
            # readiness itself before the parent removes its cwd.
            (workspace / "host").write_text(f"echo ready > {root / 'ready'}\nsleep 60 & wait\n")
            script = textwrap.dedent(f"""
                import pathlib, subprocess, time
                child = subprocess.Popen(
                    ['plak', 'host'], executable='/bin/bash', cwd={str(workspace)!r},
                    start_new_session=True, stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                pathlib.Path({str(root / 'pid')!r}).write_text(str(child.pid))
                while not pathlib.Path({str(root / 'ready')!r}).exists():
                    time.sleep(0.01)
                pathlib.Path({str(workspace / 'host')!r}).unlink()
                pathlib.Path({str(workspace)!r}).rmdir()
            """)
            result = subprocess.run(
                [sys.executable, str(GUARD), "--", sys.executable, "-c", script],
                env=dict(os.environ, TMPDIR=str(root)),
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("test process leak:", result.stderr)
            pid = int((root / "pid").read_text())
            # A zombie has no cwd or sockets and cannot be a leak candidate.
            probe = subprocess.run(
                ["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True
            )
            self.assertTrue(
                not probe.stdout.strip() or probe.stdout.lstrip().startswith("Z"), probe.stdout
            )

    def test_another_root_is_not_signalled_and_exit_status_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "host").write_text("sleep 60 & wait\n")
            child = subprocess.Popen(
                ["plak", "host"],
                executable="/bin/bash",
                cwd=root,
                start_new_session=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                other = root / "other"
                other.mkdir()
                result = subprocess.run(
                    [sys.executable, str(GUARD), "--", sys.executable, "-c", "raise SystemExit(7)"],
                    env=dict(os.environ, TMPDIR=str(other)),
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 7, result.stderr)
                self.assertIsNone(child.poll())
            finally:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()


if __name__ == "__main__":
    unittest.main()
