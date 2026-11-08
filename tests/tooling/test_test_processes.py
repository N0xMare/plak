"""Fixture process leaks fail commands without touching another test root."""

import fcntl
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
GUARD = ROOT / "tools/gate/test-processes.py"
SPEC = importlib.util.spec_from_file_location("test_process_guard", GUARD)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class TestProcessGuardTests(unittest.TestCase):
    def test_ignored_hangup_stays_ignored_in_guard_and_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            script = (
                "import signal, time; "
                "assert signal.getsignal(signal.SIGHUP) == signal.SIG_IGN; "
                "time.sleep(0.3)"
            )
            launcher = textwrap.dedent('''
                import os, signal, sys
                signal.signal(signal.SIGHUP, signal.SIG_IGN)
                os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
            ''')
            child = subprocess.Popen(
                [sys.executable, '-c', launcher, str(GUARD), '--', sys.executable, '-c', script],
                env=dict(os.environ, TMPDIR=str(base)),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not list(base.glob('run-*/gitconfig')):
                    self.assertIsNone(child.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                child.send_signal(signal.SIGHUP)
                _, errors = child.communicate(timeout=5)
                self.assertEqual(child.returncode, 0, errors)
            finally:
                if child.poll() is None:
                    child.kill()
                child.communicate(timeout=5)

    def test_child_keeps_run_locked_after_guard_is_killed(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            script = textwrap.dedent(f'''
                import os, pathlib, time
                root = pathlib.Path(os.environ['TMPDIR'])
                os.chdir(root)
                pathlib.Path({str(base / 'ready')!r}).write_text(str(root) + '\\n' + str(os.getpid()))
                deadline = time.monotonic() + 20
                while not pathlib.Path({str(base / 'release')!r}).exists() and time.monotonic() < deadline:
                    assert root.is_dir()
                    time.sleep(0.02)
            ''')
            guard = subprocess.Popen(
                [sys.executable, str(GUARD), '--', sys.executable, '-c', script],
                env=dict(os.environ, TMPDIR=str(base)),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            pid = None
            try:
                deadline = time.monotonic() + 5
                while not (base / 'ready').exists():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                name, recorded_pid = (base / 'ready').read_text().splitlines()
                root, pid = Path(name), int(recorded_pid)
                guard.kill()
                guard.wait(timeout=5)
                # The lock, not just cwd inspection, must survive the guard.
                with (root / 'guard.lock').open('r+') as lock:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                check = subprocess.run(
                    [sys.executable, str(GUARD), '--check'],
                    env=dict(os.environ, TMPDIR=str(base)),
                    capture_output=True,
                    timeout=5,
                )
                self.assertEqual(check.returncode, 0, check.stderr)
                self.assertTrue(root.is_dir())
                os.kill(pid, 0)
            finally:
                if pid is not None:
                    try:
                        os.killpg(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if guard.poll() is None:
                    guard.kill()
                guard.wait(timeout=5)

    def test_free_run_with_non_fixture_cwd_is_not_removed(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / 'run-old'
            root.mkdir()
            (root / 'guard.lock').touch()
            table = MODULE.ProcessTable()
            worker = MODULE.Process(410001, 1, 'worker', None)
            with (
                patch.object(table, 'scan', return_value={worker.pid: worker}),
                patch.object(table, 'inside', return_value=True),
            ):
                MODULE.reap_previous(base, table)
            self.assertTrue(root.is_dir())
            with patch.object(table, 'scan', return_value={}):
                MODULE.reap_previous(base, table)
            self.assertFalse(root.exists())

    def test_publication_cannot_be_observed_before_run_is_locked(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            created, attempted, inspected = threading.Event(), threading.Event(), threading.Event()
            mkdir = Path.mkdir
            errors = []

            def paused_mkdir(path, *args, **kwargs):
                mkdir(path, *args, **kwargs)
                if path.name.startswith('run-'):
                    created.set()
                    self.assertTrue(attempted.wait(5))
                    self.assertFalse(inspected.wait(0.2), 'partly published run was inspected')

            def check():
                try:
                    self.assertTrue(created.wait(5))
                    attempted.set()
                    MODULE.main(['--check'])
                except BaseException as error:
                    errors.append(error)

            def reaper(_base, _table):
                if created.is_set():
                    inspected.set()

            with (
                patch.dict(os.environ, TMPDIR=str(base)),
                patch.object(MODULE, 'reap_previous', side_effect=reaper),
                patch.object(MODULE, 'ProcessTable') as table,
                patch.object(MODULE, 'run', return_value=0),
                patch.object(Path, 'mkdir', paused_mkdir),
            ):
                table.return_value.occupied.return_value = False
                thread = threading.Thread(target=check)
                thread.start()
                try:
                    self.assertEqual(MODULE.main(['--', 'unused']), 0)
                finally:
                    thread.join(timeout=5)
                self.assertFalse(thread.is_alive())
                self.assertFalse(errors, errors)
                self.assertTrue(inspected.is_set())

    def test_macos_scan_uses_locale_independent_creation_times(self):
        table = MODULE.ProcessTable()
        with (
            patch.object(MODULE.sys, 'platform', 'darwin'),
            patch.object(MODULE.subprocess, 'check_output', return_value='') as ps,
        ):
            self.assertEqual(table.scan(), {})
        self.assertEqual(ps.call_args.kwargs['env']['LC_ALL'], 'C')

    def test_cleanup_subprocess_failure_does_not_abandon_interrupted_child(self):
        handlers = {}

        def install(signum, handler):
            handlers[signum] = handler
            return signal.SIG_DFL

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with (
                (root / 'guard.lock').open('a') as lock,
                patch.object(MODULE.signal, 'getsignal', return_value=signal.SIG_DFL),
                patch.object(MODULE.signal, 'signal', side_effect=install),
                patch.object(MODULE.subprocess, 'Popen') as popen,
                patch.object(MODULE.os, 'killpg') as kill,
                patch.object(
                    MODULE, 'reap_leaks', side_effect=[subprocess.SubprocessError('probe'), False]
                ),
            ):
                child = popen.return_value
                child.poll.return_value = None
                child.wait.side_effect = lambda: handlers[signal.SIGTERM](signal.SIGTERM, None)
                self.assertEqual(
                    MODULE.run(['unused'], root, MODULE.ProcessTable(), lock.fileno()), 143
                )
                child.wait.assert_called_once()
                kill.assert_called_once_with(child.pid, signal.SIGTERM)

    def test_short_run_name_keeps_fixture_socket_within_macos_limit(self):
        # Maximum macOS pid and a full-width fixture ordinal and checkout checksum.
        base = '/private/tmp/plak-test-501-4294967295'
        socket = base + '/run-99999-ff/t1869f-ffffffff/home/host/runner/v3/run/runner.sock'
        self.assertLessEqual(len(socket.encode()), 103)
        with patch.object(MODULE.secrets, 'token_hex', return_value='ff') as nonce:
            with (
                tempfile.TemporaryDirectory() as temporary,
                patch.dict(os.environ, TMPDIR=temporary),
            ):
                with (
                    patch.object(MODULE, 'run', return_value=0),
                    patch.object(MODULE, 'ProcessTable') as table,
                ):
                    table.return_value.occupied.return_value = False
                    MODULE.main(['--', 'unused'])
        nonce.assert_called_once_with(1)

    def test_stale_run_leak_is_reaped_without_failing_the_new_command(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            old = base / "run-old"
            old.mkdir()
            (old / "guard.lock").touch()
            (old / "host").write_text("sleep 60 & wait\n")
            child = subprocess.Popen(
                ["plak", "host"],
                executable="/bin/bash",
                cwd=old,
                start_new_session=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                result = subprocess.run(
                    [sys.executable, str(GUARD), "--", sys.executable, "-c", "pass"],
                    env=dict(os.environ, TMPDIR=str(base)),
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("previous run", result.stderr)
                self.assertFalse(old.exists())
                child.wait(timeout=5)
            finally:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                child.wait()

    def test_concurrent_invocations_do_not_reap_each_others_hosts(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            guards = []
            try:
                for index in range(2):
                    script = textwrap.dedent(f"""
                        import os, pathlib, signal, subprocess, time
                        root = pathlib.Path(os.environ['TMPDIR'])
                        (root / 'host').write_text('sleep 60 & wait\\n')
                        child = subprocess.Popen(['plak', 'host'], executable='/bin/bash',
                            cwd=root, start_new_session=True, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
                        pathlib.Path({str(base / f'ready-{index}')!r}).write_text(str(root))
                        try:
                            while not pathlib.Path({str(base / 'release')!r}).exists():
                                time.sleep(0.01)
                            assert child.poll() is None, 'another run killed our host'
                        finally:
                            if child.poll() is None: os.killpg(child.pid, signal.SIGKILL)
                            child.wait()
                    """)
                    guards.append(
                        subprocess.Popen(
                            [sys.executable, str(GUARD), "--", sys.executable, "-c", script],
                            env=dict(os.environ, TMPDIR=str(base)),
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            text=True,
                        )
                    )
                    deadline = time.monotonic() + 10
                    while not (base / f"ready-{index}").exists():
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(0.01)
                self.assertNotEqual((base / 'ready-0').read_text(), (base / 'ready-1').read_text())
                (base / "release").touch()
                for guard in guards:
                    _, error = guard.communicate(timeout=10)
                    self.assertEqual(guard.returncode, 0, error)
                    self.assertNotIn("previous run", error)
            finally:
                (base / "release").touch()
                for guard in guards:
                    if guard.poll() is None:
                        guard.terminate()
                    guard.communicate(timeout=10)

    def test_cwd_is_cached_per_process_identity_and_inspection_errors_are_safe(self):
        table = MODULE.ProcessTable()
        process = MODULE.Process(410001, 1, "before", "host")
        result = subprocess.CompletedProcess([], 0, stdout="n/scoped-fixture/workspace\n")
        with (
            patch.object(MODULE.sys, "platform", "darwin"),
            patch.object(MODULE.subprocess, "run", return_value=result) as probe,
        ):
            self.assertTrue(table.inside(process, Path("/scoped-fixture")))
            self.assertTrue(table.inside(process, Path("/scoped-fixture")))
            self.assertEqual(probe.call_count, 1)
        table = MODULE.ProcessTable()
        with (
            patch.object(MODULE.sys, "platform", "darwin"),
            patch.object(MODULE.subprocess, "run", side_effect=PermissionError("refused")),
        ):
            self.assertFalse(table.inside(process, Path("/scoped-fixture")))
        with (
            patch.object(MODULE.sys, "platform", "darwin"),
            patch.object(MODULE.subprocess, "run", side_effect=FileNotFoundError("lsof")),
        ):
            with self.assertRaisesRegex(RuntimeError, "requires lsof"):
                table.inside(process, Path("/scoped-fixture"))

    def test_daemon_descendants_in_separate_groups_are_killed_first(self):
        daemon = MODULE.Process(410001, 1, "daemon", "runner serve")
        child = MODULE.Process(410002, daemon.pid, "child", None)
        grandchild = MODULE.Process(410003, child.pid, "grandchild", None)
        table = MODULE.ProcessTable()
        rows = {p.pid: p for p in (daemon, child, grandchild)}
        with patch.object(table, "scan", return_value=rows):
            self.assertEqual(table.descendants(daemon.pid), [grandchild, child])
        killed = []
        with (
            patch.object(table, "scan", side_effect=lambda: dict(rows)),
            patch.object(table, "inside", return_value=True),
            patch.object(table, "kill", side_effect=lambda p: (killed.append(p), rows.pop(p.pid))),
            patch.object(MODULE.time, "sleep"),
        ):
            self.assertTrue(MODULE.reap_leaks(Path('/scoped-fixture'), table, settle=False))
        self.assertEqual(killed, [grandchild, child, daemon])

    def test_reused_pid_is_never_signalled(self):
        old = MODULE.Process(410001, 1, 'old', 'host')
        new = MODULE.Process(old.pid, 1, 'new', 'host')
        table = MODULE.ProcessTable()
        with (
            patch.object(table, 'scan', return_value={new.pid: new}),
            patch.object(MODULE.os, 'getpgid', return_value=new.pid),
            patch.object(MODULE.os, 'kill') as kill,
            patch.object(MODULE.os, 'killpg') as killpg,
        ):
            table.kill(old)
        kill.assert_not_called()
        killpg.assert_not_called()

    def test_hosts_are_killed_before_daemons_and_successors_are_relisted(self):
        # Entirely simulated process table: even mutants cannot signal the OS.
        daemon = MODULE.Process(410001, 1, "daemon", "runner serve")
        host = MODULE.Process(410002, 1, "host", "host")
        successor = MODULE.Process(410003, 1, "successor", "runner serve")
        live = {daemon.pid: daemon, host.pid: host}
        killed = []

        class Table:
            def candidates(self, _root):
                return list(live.values())

            def descendants(self, _pid):
                return []

            def kill(self, process):
                if process == daemon:
                    self_test.assertNotIn(host.pid, live, "daemon killed before its launcher")
                live.pop(process.pid)
                killed.append(process)
                if process == host:
                    # The successor appears only on the host's SIGKILL path.
                    live[successor.pid] = successor

        self_test = self
        with patch.object(MODULE.time, "sleep"):
            self.assertTrue(MODULE.reap_leaks(Path("/scoped-fixture"), Table(), settle=False))
        self.assertFalse(live)
        self.assertEqual(killed, [host, daemon, successor])

    def test_previous_runs_are_reaped_without_failing_or_touching_a_locked_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            stale, active = base / "run-old", base / "run-live"
            stale.mkdir()
            active.mkdir()
            (stale / "guard.lock").touch()
            with (active / "guard.lock").open("a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                with patch.object(MODULE, "reap_leaks", return_value=True) as reap:
                    MODULE.reap_previous(base, MODULE.ProcessTable())
                self.assertEqual([call.args[0] for call in reap.call_args_list], [stale])

    def test_signals_allow_a_three_second_cleanup_and_forward_hup_and_quit(self):
        for signum in (signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            with self.subTest(signum=signum), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                script = textwrap.dedent(f"""
                    import os, pathlib, signal, time
                    def stop(signum, frame):
                        time.sleep(3)
                        pathlib.Path({str(root / 'cleaned')!r}).touch()
                        raise SystemExit(0)
                    signal.signal({int(signum)}, stop)
                    pathlib.Path({str(root / 'pid')!r}).write_text(str(os.getpid()))
                    pathlib.Path({str(root / 'ready')!r}).touch()
                    deadline = time.monotonic() + 20
                    while time.monotonic() < deadline: time.sleep(0.1)
                """)
                guarded = subprocess.Popen(
                    [sys.executable, str(GUARD), "--", sys.executable, "-c", script],
                    env=dict(os.environ, TMPDIR=str(root)),
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                try:
                    deadline = time.monotonic() + 10
                    while not (root / "ready").exists():
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(0.01)
                    guarded.send_signal(signum)
                    _, errors = guarded.communicate(timeout=15)
                    self.assertEqual(guarded.returncode, 128 + signum, errors)
                    self.assertTrue((root / "cleaned").exists())
                finally:
                    if (root / 'pid').exists():
                        try:
                            os.killpg(int((root / 'pid').read_text()), signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    if guarded.poll() is None:
                        guarded.kill()
                    guarded.communicate(timeout=5)

    def test_each_command_gets_a_distinct_locked_tmpdir_and_negative_status_is_mapped(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            roots = []
            for _ in range(2):
                result = subprocess.run(
                    [
                        sys.executable,
                        str(GUARD),
                        "--",
                        sys.executable,
                        "-c",
                        "import os; print(os.environ['TMPDIR'])",
                    ],
                    env=dict(os.environ, TMPDIR=str(base)),
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                roots.append(Path(result.stdout.strip()))
            self.assertNotEqual(*roots)
            self.assertTrue(
                all(path.parent == base and path.name.startswith("run-") for path in roots)
            )
            self.assertTrue(all(not path.exists() for path in roots))
            result = subprocess.run(
                [
                    sys.executable,
                    str(GUARD),
                    "--",
                    sys.executable,
                    "-c",
                    "import os, signal; os.kill(os.getpid(), signal.SIGTERM)",
                ],
                env=dict(os.environ, TMPDIR=str(base)),
                capture_output=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 143)

    def test_sigterm_still_checks_and_reaps_a_detached_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "host").write_text("sleep 60 & wait\n")
            (root / "runner").write_text("sleep 60 & wait\n")
            script = textwrap.dedent(f"""
                import os, pathlib, shutil, signal, subprocess, time
                run = pathlib.Path(os.environ['TMPDIR'])
                shutil.copy({str(root / 'host')!r}, run / 'host')
                shutil.copy({str(root / 'runner')!r}, run / 'runner')
                def stopping(signum, frame):
                    subprocess.Popen(
                        ['plak', 'runner', 'serve', '--local'], executable='/bin/bash',
                        cwd=run, start_new_session=True,
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    raise SystemExit(0)
                signal.signal(signal.SIGTERM, stopping)
                child = subprocess.Popen(
                    ['plak', 'host'], executable='/bin/bash', cwd=run,
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
                guarded.communicate(timeout=5)

    def test_deleted_workspace_leak_fails_and_is_reaped(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            script = textwrap.dedent(f"""
                import os, pathlib, subprocess, time
                workspace = pathlib.Path(os.environ['TMPDIR']) / 'offline-tui/workspace'
                workspace.mkdir(parents=True)
                (workspace / 'host').write_text('echo ready > ready; sleep 60 & wait\\n')
                child = subprocess.Popen(
                    ['plak', 'host'], executable='/bin/bash', cwd=workspace,
                    start_new_session=True, stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                pathlib.Path({str(root / 'pid')!r}).write_text(str(child.pid))
                while not (workspace / 'ready').exists():
                    time.sleep(0.01)
                (workspace / 'host').unlink()
                (workspace / 'ready').unlink()
                workspace.rmdir()
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
