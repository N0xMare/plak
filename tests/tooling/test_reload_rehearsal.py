import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import selectors
import signal
from contextlib import contextmanager
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("reload_rehearsal", ROOT / "tools/build/reload_rehearsal.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ReloadRehearsalTests(unittest.TestCase):
    def test_child_inspection_timeout_reports_child_and_step_not_staging(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)

            def timed_out(*_args, on_stdout):
                on_stdout("c11: step=projection\n")
                raise subprocess.TimeoutExpired("private /home/secret SELECT token", 120)

            with patch.object(MODULE, "capture_home"), \
                 patch.object(MODULE, "run_command", side_effect=timed_out), \
                 contextlib.redirect_stdout(io.StringIO()) as report:
                code = MODULE.rehearse_copy(base, base / "backup", "key", base / "plak", "session", True, 300, 0.01)
            self.assertEqual(code, 1)
            self.assertEqual(report.getvalue(), "c11: step=projection\nrehearse refused: inspection_timeout\n")

    def test_same_key_busy_wait_is_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)

            def busy(*_args, on_stdout):
                on_stdout("c2: busy: active_turn\n")
                return subprocess.CompletedProcess([], 1, "c2: busy: active_turn\n", "plak: offline reload rehearsal refused\n")

            with patch.object(MODULE, "capture_home"), \
                 patch.object(MODULE, "run_command", side_effect=busy) as run, \
                 patch.object(MODULE.time, "monotonic", side_effect=[0, 0, 0, 30, 30]), \
                 patch.object(MODULE.time, "sleep"), contextlib.redirect_stdout(io.StringIO()) as report:
                code = MODULE.rehearse_copy(base, base / "backup", "key", base / "plak", "session", True, 30, 0.01)
            self.assertEqual(code, 1)
            self.assertEqual(run.call_count, 1)
            self.assertIn("wait_idle_timeout", report.getvalue())

    def test_same_key_live_child_recaptures_before_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            live, backup = base / "live", base / "backup"
            results = [
                subprocess.CompletedProcess([], 1, "c2: busy: active_turn\n", "plak: offline reload rehearsal refused\n"),
                subprocess.CompletedProcess([], 0, "c2: offline=OpensQuiescent\n", ""),
            ]
            captures = []

            def capture(source, target, key):
                self.assertEqual((source, key), (live, "key"))
                self.assertFalse(target.exists(), "a retry reused stale state")
                target.mkdir()
                captures.append(target)

            def complete(*_args, on_stdout):
                result = results.pop(0)
                on_stdout(result.stdout)
                return result

            with patch.object(MODULE, "capture_home", capture), \
                 patch.object(MODULE, "run_command", side_effect=complete) as run, \
                 patch.object(MODULE.time, "sleep"), contextlib.redirect_stdout(io.StringIO()) as report:
                code = MODULE.rehearse_copy(live, backup, "key", base / "plak", "session", True, 30, 0.01)
            self.assertEqual(code, 0)
            self.assertEqual(len(captures), 2)
            self.assertEqual(run.call_count, 2)
            self.assertIn("c2: busy: active_turn", report.getvalue())

    def test_same_key_broken_child_and_no_wait_do_not_retry(self):
        for stdout, wait in [("c2: refuses: projection_integrity\n", True),
                             ("c2: busy: active_turn\n", False),
                             ("c2: busy: active_turn\nc3: config=refused\n", True)]:
            with self.subTest(stdout=stdout, wait=wait), tempfile.TemporaryDirectory() as directory:
                base = Path(directory)
                result = subprocess.CompletedProcess([], 1, stdout, "plak: offline reload rehearsal refused\n")
                def complete(*_args, on_stdout):
                    on_stdout(result.stdout)
                    return result
                with patch.object(MODULE, "capture_home"), \
                     patch.object(MODULE, "run_command", side_effect=complete) as run, \
                     patch.object(MODULE.time, "sleep") as sleep, contextlib.redirect_stdout(io.StringIO()) as report:
                    code = MODULE.rehearse_copy(base, base / "backup", "key", base / "plak", "session", wait, 30, 0.01)
                self.assertEqual(code, 1)
                self.assertEqual(run.call_count, 1)
                sleep.assert_not_called()
                self.assertIn(stdout.strip(), report.getvalue())

    def test_backup_sees_live_wal_and_omits_secrets_and_holders(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source, target = base / "live", base / "copy"
            state = source / "state/r22.e20.p28.t3"
            state.mkdir(parents=True)
            connection = sqlite3.connect(state / "runtime.sqlite3")
            connection.execute("PRAGMA journal_mode=WAL")
            connection.execute("CREATE TABLE fixture(value TEXT)")
            connection.execute("INSERT INTO fixture VALUES('synthetic')")
            connection.commit()
            (state / "runtime.sqlite3-holders").mkdir()
            (state / "runtime.sqlite3-holders/held.lock").write_text("holder")
            (source / "secrets").mkdir()
            (source / "secrets/token").write_text("synthetic-secret")
            (source / "config.toml").write_text("# fixture")
            for name in ("agents", "profiles", "references"):
                (source / name).mkdir()
                (source / name / "fixture").write_text("fixture")
            before = (state / "runtime.sqlite3").read_bytes()
            MODULE.capture_home(source, target, "r22.e20.p28.t3")
            copy = sqlite3.connect(target / "state/r22.e20.p28.t3/runtime.sqlite3")
            self.assertEqual(copy.execute("SELECT value FROM fixture").fetchone()[0], "synthetic")
            copy.close()
            self.assertEqual((state / "runtime.sqlite3").read_bytes(), before)
            self.assertFalse((target / "secrets").exists())
            self.assertFalse((target / "state/r22.e20.p28.t3/runtime.sqlite3-holders").exists())
            self.assertEqual((target / "config.toml").read_text(), "# fixture")
            connection.close()

    def test_link_in_copy_allowlist_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            (base / "source/agents").mkdir(parents=True)
            (base / "private").write_text("synthetic-secret")
            (base / "source/agents/linked").symlink_to(base / "private")
            with self.assertRaises(ValueError):
                MODULE.capture_home(base / "source", base / "copy", "key")

    def test_run_environment_does_not_inherit_credentials_or_test_hooks(self):
        env = MODULE.run_environment(Path("/fixture"), {
            "PATH": "/bin", "HOME": "/home/fixture", "OPENAI_API_KEY": "synthetic",
            "PLAK_TEST_SCRIPT": "fixture", "PLAK_HOME": "/live", "ANTHROPIC_API_KEY": "synthetic",
        })
        self.assertEqual(env["PLAK_HOME"], "/fixture")
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertNotIn("ANTHROPIC_API_KEY", env)
        self.assertNotIn("PLAK_TEST_SCRIPT", env)

    @contextmanager
    def wrapper_fixture(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            repo = base / "repo"
            (repo / "scripts").mkdir(parents=True)
            (repo / "tools/build").mkdir(parents=True)
            import shutil
            shutil.copy(ROOT / "scripts/reload-rehearsal.sh", repo / "scripts")
            shutil.copy(ROOT / "tools/build/reload_rehearsal.py", repo / "tools/build")
            (repo / "tools/build/check-artifact-linkage.py").write_text(
                "import sys\nfrom pathlib import Path\n"
                "Path('linkage-checked').write_text(sys.argv[1])\n")
            tools = base / "bin"
            tools.mkdir()
            (tools / "python3").symlink_to(sys.executable)
            (tools / "cargo").write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$HOME/cargo-args"\nexit 0\n')
            (tools / "cargo").chmod(0o700)
            (tools / "just").write_text(
                '#!/bin/sh\nprintf "%s\\n%s\\n" "$PWD" "$*" > "$HOME/install-call"\n')
            (tools / "just").chmod(0o700)
            output = repo / "target/maintainer"
            output.mkdir(parents=True)
            for name in ("plakous-js-host", "monty"):
                (output / name).write_text("#!/bin/sh\nexit 0\n")
                (output / name).chmod(0o700)
            (output / "plak").write_text("#!/usr/bin/env python3\nimport json,os,sys\n"
                "from pathlib import Path\n"
                "if '--reload-probe' in sys.argv: print(json.dumps({'state_key':'key'}))\n"
                "elif sys.argv[1:] == ['version']: print('plak fixture (build fixture-id)')\n"
                "else:\n"
                " Path(os.environ['HOME'],'observed').write_text(os.environ['PLAK_HOME'])\n"
                " print('fixture refusal'); sys.exit(1)\n")
            (output / "plak").chmod(0o700)
            home = base / "home"
            (home / "state/key").mkdir(parents=True)
            env = os.environ.copy()
            env.update(PATH=str(tools) + os.pathsep + env["PATH"], HOME=str(base), PLAK_HOME=str(home))
            env.pop("CARGO_BUILD_TARGET", None)
            env.pop("CARGO_TARGET_DIR", None)
            yield base, repo, home, env, output

    def run_wrapper(self, repo, env, extra=()):
        return subprocess.run(["bash", str(repo / "scripts/reload-rehearsal.sh"),
                               "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", *extra],
                              env=env, text=True, capture_output=True)

    def test_wrapper_stages_without_install_and_cleans_up_on_refusal(self):
        with self.wrapper_fixture() as (base, repo, _home, env, _output):
            result = self.run_wrapper(repo, env)
            self.assertEqual(result.returncode, 1, result.stderr)
            arguments = (base / "cargo-args").read_text().splitlines()
            self.assertEqual(arguments, [
                "build --locked --offline -p plak --features plak/maintainer --profile maintainer",
                "build --locked --offline -p plakous-js-runtime -p plakous-py-worker --profile maintainer",
            ])
            self.assertEqual((repo / "linkage-checked").read_text(), str(repo / "target/maintainer"))
            self.assertIn("fixture-id", result.stdout)
            self.assertFalse(Path((base / "observed").read_text()).exists())
            self.assertFalse((base / ".local").exists())

    def test_refusal_classes_hide_paths_sql_and_config(self):
        locked = sqlite3.OperationalError(
            "database is locked: /var/secret/plak/runtime.sqlite3 SELECT token FROM config"
        )
        locked.sqlite_errorcode = sqlite3.SQLITE_BUSY
        cases = [
            (locked, "session_busy_retry_when_idle"),
            (MODULE.SessionBusy("database_busy"), "session_busy_retry_when_idle"),
            (MODULE.SessionBusy("source_changed"), "session_busy_retry_when_idle"),
            (MODULE.SessionBusy("carry_in_progress"), "session_busy_retry_when_idle"),
            (TimeoutError("backup_deadline"), "session_busy_retry_when_idle"),
            (ValueError("linked_copy_input"), "staging_or_backup_unavailable"),
            (
                OSError(2, "missing /home/secret/config.toml password=hunter2"),
                "staging_or_backup_unavailable",
            ),
        ]
        for error, label in cases:
            with self.subTest(label=label, error=type(error).__name__):
                self.assertEqual(MODULE.refusal_label(error), label)
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    code = MODULE.refuse(error)
                text = stderr.getvalue()
                self.assertEqual(code, 1)
                self.assertEqual(text.strip(), f"reload rehearsal refused: {label}")
                self.assertNotIn("/var", text)
                self.assertNotIn("/home", text)
                self.assertNotIn("SELECT", text)
                self.assertNotIn("password", text)
                self.assertNotIn("hunter2", text)
                self.assertNotIn("token", text)

    def test_vanished_copy_is_source_changed(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source = base / "config.toml"
            source.write_text("fixture")

            def vanished(*_args, **_kwargs):
                raise FileNotFoundError(2, "No such file", str(base / "secrets" / "token"))

            with patch.object(MODULE.shutil, "copyfile", vanished):
                with self.assertRaises(MODULE.SessionBusy) as raised:
                    MODULE.copy_plain(source, base / "out")
            self.assertEqual(str(raised.exception), "source_changed")
            self.assertNotIn("secrets", str(raised.exception))

    def test_session_in_two_keys_is_carry_in_progress(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            for key in ("r21.e19.p27.t3", "r22.e20.p28.t3"):
                state = home / "state" / key
                state.mkdir(parents=True)
                connection = sqlite3.connect(state / "product.sqlite3")
                connection.execute("CREATE TABLE sessions(id TEXT)")
                connection.execute("INSERT INTO sessions VALUES(?)", (session,))
                connection.commit()
                connection.close()
            with self.assertRaises(MODULE.SessionBusy) as raised:
                MODULE.session_state_key(home, session, "r22.e24.p30.t4")
            self.assertEqual(str(raised.exception), "carry_in_progress")

    def test_usage_line_names_installed_and_overlay(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = MODULE.main([])
        self.assertEqual(code, 1)
        self.assertEqual(
            stderr.getvalue().strip(),
            "usage: just reload-rehearsal <full-session-uuid>"
            " [--installed <path>] [--config-overlay <dir>]"
            " [--wait-idle [--timeout S] [--interval S]] [--detach --log <path>] [--install]",
        )

    def test_scrub_removes_absolute_paths_and_keeps_the_refusal_text(self):
        text = "template worker: may_spawn was removed (/var/plak-rehearsal-private/token)"
        scrubbed = MODULE.scrub(text, [Path("/tmp/plak-home")])
        self.assertIn("may_spawn was removed", scrubbed)
        self.assertNotIn("/var/plak-rehearsal-private", scrubbed)
        self.assertNotIn(
            "/tmp/plak-home",
            MODULE.scrub("see /tmp/plak-home/secrets/token", [Path("/tmp/plak-home")]),
        )

    def write_carry_binary(self, path, role, import_status, live, import_failure=None, export_failures=()):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            f"ROLE = {role!r}\n"
            f"IMPORT_STATUS = {import_status}\n"
            f"IMPORT_FAILURE = {import_failure!r}\n"
            f"EXPORT_FAILURES = {list(export_failures)!r}\n"
            f"LIVE = {str(live)!r}\n"
            "PRIVATE = '/var/plak-rehearsal-private/token'\n"
            "OLD = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa'\n"
            "NEW = 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb'\n"
            "HOME = Path(os.environ['HOME'])\n"
            "argv = sys.argv[1:]\n"
            "record = {\n"
            "    'role': ROLE, 'argv0': sys.argv[0], 'argv': argv,\n"
            "    'plak_home': os.environ.get('PLAK_HOME'), 'cwd': os.getcwd(),\n"
            "    'env_keys': sorted(os.environ),\n"
            "}\n"
            "with (HOME / 'calls.jsonl').open('a') as handle:\n"
            "    handle.write(json.dumps(record) + '\\n')\n"
            "forbidden = {'OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'PLAK_TEST_SCRIPT', 'ANTHROPIC_BASE_URL'}\n"
            "if forbidden & set(os.environ):\n"
            "    print('credential leaked', file=sys.stderr); sys.exit(9)\n"
            "def fail(message):\n"
            "    print(message, file=sys.stderr); sys.exit(1)\n"
            "def config_message(home):\n"
            "    agent = home / 'agents' / 'worker.toml'\n"
            "    if agent.is_file() and 'may_spawn' in agent.read_text():\n"
            "        return (str(agent) + ': template worker: may_spawn was removed; '\n"
            "                'list the templates it may spawn in spawns = [...] (' + PRIVATE + ') ('\n"
            "                + LIVE + '/secrets/token)')\n"
            "    return None\n"
            "if '--reload-probe' in argv:\n"
            "    message = config_message(Path(os.environ['PLAK_HOME']))\n"
            "    print(json.dumps({'state_key': 'key', 'export_versions': [4, 5], 'config_error': message}))\n"
            "    sys.exit(0)\n"
            "if argv == ['version']:\n"
            "    print('plak fixture (build fixture-id)'); sys.exit(0)\n"
            "if argv[:2] == ['config', 'show']:\n"
            "    message = config_message(Path(os.environ['PLAK_HOME']))\n"
            "    if message: fail(message)\n"
            "    print('config_path=' + str(Path(os.environ['PLAK_HOME']) / 'config.toml')); sys.exit(0)\n"
            "if argv[:3] == ['session', 'export', OLD]:\n"
            "    if ROLE != 'installed': fail('staged build must not export')\n"
            "    if Path.cwd() != HOME / 'workspace': fail('wrong export workspace')\n"
            "    counter = HOME / 'export-count'\n"
            "    attempt = int(counter.read_text()) if counter.exists() else 0\n"
            "    counter.write_text(str(attempt + 1))\n"
            "    if attempt < len(EXPORT_FAILURES):\n"
            "        fail('plak: session ' + OLD + ' cannot be exported: ' + EXPORT_FAILURES[attempt])\n"
            "    out = Path(argv[argv.index('--out') + 1])\n"
            "    home = Path(os.environ['PLAK_HOME'])\n"
            "    if not (home / 'state' / 'r21.e19.p27.t3' / 'product.sqlite3').is_file():\n"
            "        fail('source state missing')\n"
            "    if (home / 'state' / 'key').exists(): fail('destination key leaked into export home')\n"
            "    if (home / 'secrets').exists(): fail('secrets copied')\n"
            "    if str(home) == LIVE: fail('export used the live home')\n"
            "    out.write_text(json.dumps({'workspace_root': str(HOME / 'workspace'), 'format': 'plak-session-export'}))\n"
            "    print(out); sys.exit(0)\n"
            "if argv[:2] == ['session', 'import']:\n"
            "    if ROLE != 'staged': fail('installed build must not import')\n"
            "    home = Path(os.environ['PLAK_HOME'])\n"
            "    if (home / 'state').exists(): fail('state copied into destination')\n"
            "    if (home / 'secrets').exists(): fail('secrets copied')\n"
            "    if str(home) == LIVE: fail('import used the live home')\n"
            "    if IMPORT_STATUS != 0:\n"
            "        message = IMPORT_FAILURE\n"
            "        if message is None:\n"
            "            message = 'already carried ' + LIVE + '/secrets/token ' + PRIVATE\n"
            "        fail(message)\n"
            "    print('session ' + NEW + ' linked from ' + OLD + ' (r21.e19.p27.t3, producer binary fixture)')\n"
            "    print('resume with: plak --resume ' + NEW); sys.exit(0)\n"
            "if argv[:2] == ['session', 'rehearse']:\n"
            "    if argv[2:] != [NEW]: fail('rehearse id')\n"
            "    print('root: config=valid')\n"
            "    print('c1: skipped:closed'); sys.exit(0)\n"
            "fail('unexpected argv ' + ' '.join(argv))\n"
        )
        path.chmod(0o700)

    def tree_snapshot(self, root):
        found = {}
        for path in sorted(root.rglob("*")):
            rel = path.relative_to(root).as_posix()
            if path.is_symlink():
                found[rel] = ("link", os.readlink(path))
            elif path.is_file():
                found[rel] = ("file", path.read_bytes())
            elif path.is_dir():
                found[rel] = ("dir",)
        return found

    def prepare_carry(self, base, _repo, home, env, output, import_status, overlay=False, on_path=False, import_failure=None):
        source = home / "state" / "r21.e19.p27.t3"
        source.mkdir(parents=True)
        with contextlib.closing(sqlite3.connect(source / "product.sqlite3")) as db:
            db.execute("CREATE TABLE sessions(id TEXT, workspace_root TEXT)")
            db.execute("INSERT INTO sessions VALUES(?,?)",
                       ("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", str(base / "workspace")))
            db.commit()
        (home / "config.toml").write_text("# live\n")
        (home / "agents").mkdir()
        (home / "agents" / "worker.toml").write_text('account = "codex"\nmodel = "m"\n')
        (home / "secrets").mkdir()
        (home / "secrets" / "token").write_text("synthetic-secret")
        (base / "workspace").mkdir()
        if on_path:
            installed = base / "bin" / "plak"
            extra = []
        else:
            installed = base / "installed" / "plak"
            extra = ["--installed", str(installed)]
            decoy = base / "bin" / "plak"
            decoy.write_text("#!/bin/sh\necho path plak invoked >&2\nexit 9\n")
            decoy.chmod(0o700)
        self.write_carry_binary(installed, "installed", import_status, home, import_failure)
        self.write_carry_binary(output / "plak", "staged", import_status, home, import_failure)
        env = env.copy()
        env["OPENAI_API_KEY"] = "synthetic"
        env["ANTHROPIC_API_KEY"] = "synthetic"
        env["PLAK_TEST_SCRIPT"] = "fixture"
        env["ANTHROPIC_BASE_URL"] = "https://example.invalid"
        if overlay:
            migrated = base / "overlay"
            (migrated / "agents").mkdir(parents=True)
            (migrated / "agents" / "worker.toml").write_text("may_spawn = true\n")
            (migrated / "config.toml").write_text("# migrated\n")
            (migrated / "profiles").mkdir()
            extra.extend(["--config-overlay", str(migrated)])
        return env, extra, self.tree_snapshot(home)

    def calls(self, base):
        return [json.loads(line) for line in (base / "calls.jsonl").read_text().splitlines()]

    def assert_live_untouched(self, home, before, result):
        self.assertEqual(self.tree_snapshot(home), before, result.stderr)
        self.assertNotIn(str(home), result.stdout)
        self.assertNotIn(str(home), result.stderr)
        self.assertNotIn("synthetic-secret", result.stdout)
        self.assertNotIn("synthetic-secret", result.stderr)
        self.assertNotIn("plak-reload-rehearsal-", result.stdout)
        self.assertNotIn("/var/plak-rehearsal-private", result.stdout + result.stderr)
        self.assertNotIn("path plak invoked", result.stderr)

    def test_cross_key_happy_path_rehearses_carry_and_leaves_live_home_untouched(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, before = self.prepare_carry(base, repo, home, env, output, 0)
            result = self.run_wrapper(repo, env, extra)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("fixture-id", result.stdout)
            self.assertIn(
                "state key changed r21.e19.p27.t3 -> key: rehearsing carry-forward",
                result.stdout,
            )
            self.assertIn("export=ok", result.stdout)
            self.assertIn("config=valid", result.stdout)
            self.assertIn("carried_session=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", result.stdout)
            self.assertIn(
                "session bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb linked from "
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa (r21.e19.p27.t3, producer binary fixture)",
                result.stdout,
            )
            self.assertIn("root: config=valid", result.stdout)
            self.assertIn("c1: skipped:closed", result.stdout)
            calls = self.calls(base)
            kinds = [(call["role"], call["argv"][:2]) for call in calls]
            self.assertEqual(
                kinds,
                [
                    ("staged", ["version"]),
                    ("staged", ["version", "--reload-probe"][:2]),
                    ("installed", ["session", "export"]),
                    ("staged", ["config", "show"]),
                    ("staged", ["version", "--reload-probe"][:2]),
                    ("staged", ["session", "import"]),
                    ("staged", ["session", "rehearse"]),
                ],
            )
            export, shown, imported = calls[2], calls[3], calls[5]
            self.assertTrue(export["plak_home"].endswith("/source-home"))
            self.assertTrue(shown["plak_home"].endswith("/destination"))
            self.assertNotEqual(export["plak_home"], shown["plak_home"])
            self.assertNotEqual(export["plak_home"], str(home))
            self.assertEqual(Path(export["argv0"]), base / "installed" / "plak")
            out = Path(export["argv"][export["argv"].index("--out") + 1])
            self.assertTrue(out.is_absolute())
            self.assertFalse(str(out).startswith(str(home) + os.sep))
            self.assertTrue(shown["cwd"].endswith("/workspace"))
            self.assertTrue(imported["cwd"].endswith("/workspace"))
            self.assertEqual(imported["argv"][2:], [str(out)])
            self.assertEqual(calls[6]["argv"][2:], ["bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"])
            for call in calls:
                self.assertNotIn("OPENAI_API_KEY", call["env_keys"])
                self.assertNotIn("PLAK_TEST_SCRIPT", call["env_keys"])
                self.assertNotEqual(call["plak_home"], str(home))
            self.assert_live_untouched(home, before, result)
            self.assertFalse(Path(export["plak_home"]).exists())
            self.assertFalse(Path(shown["plak_home"]).exists())

    def test_cross_key_export_uses_plak_on_path(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, before = self.prepare_carry(
                base, repo, home, env, output, 0, on_path=True
            )
            result = self.run_wrapper(repo, env, extra)
            self.assertEqual(extra, [])
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            export = next(call for call in self.calls(base) if call["argv"][:2] == ["session", "export"])
            self.assertEqual(export["role"], "installed")
            self.assertEqual(Path(export["argv0"]), base / "bin" / "plak")
            self.assert_live_untouched(home, before, result)

    def test_cross_key_config_refusal_is_scrubbed_and_skips_import(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, before = self.prepare_carry(base, repo, home, env, output, 0, overlay=True)
            result = self.run_wrapper(repo, env, extra)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("may_spawn was removed", result.stdout)
            self.assertIn("config refused:", result.stdout)
            self.assertNotIn(str(base / "overlay"), result.stdout + result.stderr)
            self.assertNotIn("session import", result.stdout)
            self.assertFalse(any(call["argv"][:2] == ["session", "import"] for call in self.calls(base)))
            self.assert_live_untouched(home, before, result)

    def test_cross_key_import_refusal_is_scrubbed_and_skips_rehearse(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, before = self.prepare_carry(base, repo, home, env, output, 1)
            result = self.run_wrapper(repo, env, extra)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("import refused:", result.stdout)
            self.assertIn("already carried", result.stdout)
            self.assertNotIn("rehearse", result.stdout)
            calls = self.calls(base)
            self.assertFalse(any(call["argv"][:2] == ["session", "rehearse"] for call in calls))
            self.assertTrue(any(call["argv"][:2] == ["session", "import"] for call in calls))
            self.assert_live_untouched(home, before, result)

    def test_credential_refusal_matches_only_the_typed_import_line(self):
        def result(code, stderr):
            return subprocess.CompletedProcess([], code, "", stderr)

        typed = "plak: session has no exact effort binding\n"
        self.assertTrue(MODULE.import_lacks_credentials(result(1, typed)))
        self.assertFalse(MODULE.import_lacks_credentials(result(1, "plak: session has no exact account binding\n")))
        self.assertFalse(MODULE.import_lacks_credentials(result(1, "session has no exact effort binding\n")))
        self.assertFalse(MODULE.import_lacks_credentials(result(1, typed + "plak: also this\n")))
        self.assertFalse(MODULE.import_lacks_credentials(result(3, typed)))
        self.assertFalse(MODULE.import_lacks_credentials(result(1, "already carried /tmp/home/secrets/token\n")))
        self.assertFalse(MODULE.import_lacks_credentials(result(
            1, "plak: session has no exact effort binding; see /tmp/plak-home/secrets/token\n",
        )))

    def test_cross_key_credential_binding_refusal_is_not_rehearsed(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, before = self.prepare_carry(
                base, repo, home, env, output, 1,
                import_failure="plak: session has no exact effort binding",
            )
            result = self.run_wrapper(repo, env, extra)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("export=ok", result.stdout)
            self.assertIn("config=valid", result.stdout)
            self.assertIn(
                "import=not rehearsed: credentials unavailable in the disposable home "
                "(export, config and document conversion rehearsed)",
                result.stdout,
            )
            self.assertNotIn("import refused:", result.stdout)
            self.assertNotIn("carried_session=", result.stdout)
            calls = self.calls(base)
            self.assertTrue(any(call["argv"][:2] == ["session", "import"] for call in calls))
            self.assertFalse(any(call["argv"][:2] == ["session", "rehearse"] for call in calls))
            self.assert_live_untouched(home, before, result)

    def test_wrapper_cleans_private_copies_on_hup_int_and_term(self):
        for signum in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=signum), self.wrapper_fixture() as (base, repo, _home, env, output):
                binary = output / "plak"
                binary.write_text(binary.read_text().replace(
                    "print('fixture refusal'); sys.exit(1)",
                    "print('fixture ready', flush=True); import time; time.sleep(60)"))
                process = subprocess.Popen(["bash", str(repo / "scripts/reload-rehearsal.sh"),
                                            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"],
                                           env=env, text=True, start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    # Read through the build identity line to the child's readiness.
                    with selectors.DefaultSelector() as ready:
                        ready.register(process.stdout, selectors.EVENT_READ)
                        self.assertTrue(ready.select(timeout=15), "fixture did not become ready")
                        for line in process.stdout:
                            if line.strip() == "fixture ready":
                                break
                        else:
                            self.fail("fixture exited without becoming ready")
                    process.send_signal(signum)
                    _, stderr = process.communicate(timeout=15)
                    self.assertEqual(process.returncode, 128 + signum, stderr)
                    self.assertFalse(Path((base / "observed").read_text()).exists())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate(timeout=15)


class ReloadRehearsalWaitTests(unittest.TestCase):
    wrapper_fixture = ReloadRehearsalTests.wrapper_fixture
    run_wrapper = ReloadRehearsalTests.run_wrapper
    prepare_carry = ReloadRehearsalTests.prepare_carry
    write_carry_binary = ReloadRehearsalTests.write_carry_binary
    tree_snapshot = ReloadRehearsalTests.tree_snapshot

    RUNNING = ("a turn is running or parked with an unresolved operation; let it finish or run "
               "`plak session recover` first")
    BEHIND = "the runtime advanced past the session record; resume the session once first"
    BACKGROUND = ("background process outcomes are not settled; resume the session "
                  "and send a prompt to deliver their notices before exporting")

    def test_default_idle_retry_interval_is_fifteen_seconds(self):
        options = MODULE.parse_args(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "--wait-idle"])
        self.assertEqual(options.interval, 15)
        self.assertEqual(options.timeout, 300)

    def test_session_workspace_uses_read_only_record_and_refuses_missing_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            state = home / "state/key"
            state.mkdir(parents=True)
            database = state / "product.sqlite3"
            with contextlib.closing(sqlite3.connect(database)) as connection:
                connection.execute("CREATE TABLE sessions(id TEXT, workspace_root TEXT)")
                for workspace in ("", "relative", str(home / "missing"), str(home)):
                    connection.execute("DELETE FROM sessions")
                    connection.execute("INSERT INTO sessions VALUES('a',?)", (workspace,))
                    connection.commit()
                    before = database.read_bytes()
                    if workspace == str(home):
                        self.assertEqual(MODULE.session_workspace(home, "key", "a"), home.resolve())
                    else:
                        with self.assertRaisesRegex(ValueError, "session_workspace_unavailable"):
                            MODULE.session_workspace(home, "key", "a")
                    self.assertEqual(database.read_bytes(), before)
                with self.assertRaisesRegex(ValueError, "session_workspace_unavailable"):
                    MODULE.session_workspace(home, "key", "absent")

    def test_missing_recorded_workspace_refuses_without_export(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, _ = self.prepare_carry(base, repo, home, env, output, 0)
            (base / "workspace").rmdir()
            result = self.run_wrapper(repo, env, extra)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("session_workspace_unavailable", result.stdout)
            self.assertNotIn(str(base), result.stdout + result.stderr)
            calls = [json.loads(line) for line in (base / "calls.jsonl").read_text().splitlines()]
            self.assertFalse(any(call["argv"][:2] == ["session", "export"] for call in calls))

    def test_wait_retries_only_exact_idle_export_refusals_then_installs(self):
        with self.wrapper_fixture() as (base, repo, home, env, output):
            env, extra, _ = self.prepare_carry(base, repo, home, env, output, 0)
            self.write_carry_binary(Path(extra[1]), "installed", 0, home,
                                    export_failures=(self.RUNNING, self.BEHIND, self.BACKGROUND))
            just = base / "bin/just"
            just.write_text('#!/bin/sh\nprintf "%s\n%s\n" "$PWD" "$*" > "$HOME/install-call"\n')
            just.chmod(0o700)
            result = self.run_wrapper(repo, env, [*extra, "--wait-idle", "--timeout", "10", "--interval", "0.01", "--install"])
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((base / "export-count").read_text(), "4")
            self.assertEqual((base / "install-call").read_text().splitlines(), [str(repo), "build-maintainer"])

    def test_unknown_export_refusal_and_no_wait_fail_fast_without_install(self):
        for reason, wait in ((self.BEHIND, []), ("the session has no canonical runtime yet", ["--wait-idle"]),
                             ("prefix " + self.BEHIND, ["--wait-idle"])):
            with self.subTest(reason=reason, wait=wait), self.wrapper_fixture() as (base, repo, home, env, output):
                env, extra, _ = self.prepare_carry(base, repo, home, env, output, 0)
                self.write_carry_binary(Path(extra[1]), "installed", 0, home, export_failures=(reason,))
                result = self.run_wrapper(repo, env, [*extra, *wait, "--install"])
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertEqual((base / "export-count").read_text(), "1")
                self.assertNotIn("import=", result.stdout)
                self.assertFalse((base / "install-call").exists())

    def test_timeout_is_bounded_and_recaptures_instead_of_reusing_stale_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source = base / "source"
            exported = subprocess.CompletedProcess([], 1, "", "plak: session a cannot be exported: " + self.BEHIND)
            ticks = [0.0]
            def capture(_live, target, _key):
                target.mkdir()
            def sleep(seconds):
                ticks[0] += seconds
            with patch.object(MODULE, "capture_home", side_effect=capture) as captures, \
                    patch.object(MODULE, "session_workspace", return_value=base), \
                    patch.object(MODULE, "run_command", return_value=exported) as commands, \
                    patch.object(MODULE.time, "monotonic", side_effect=lambda: ticks[0]), \
                    patch.object(MODULE.time, "sleep", side_effect=sleep), contextlib.redirect_stdout(io.StringIO()) as report:
                result = MODULE.export_copy(base, source, "key", "a", base / "plak", base / "export", [], True, 2, interval=1)
            self.assertFalse(result)
            self.assertEqual(captures.call_count, 2)
            self.assertEqual(commands.call_count, 2)
            self.assertEqual(ticks[0], 2)
            self.assertIn("wait_idle_timeout", report.getvalue())

    def test_detached_child_uses_setsid_private_log_and_absolute_arguments(self):
        with self.wrapper_fixture() as (base, repo, home, env, _output):
            with patch.dict(os.environ, env), patch.object(MODULE, "ROOT", repo), \
                    patch.object(MODULE.subprocess, "Popen") as spawn, \
                    contextlib.redirect_stdout(io.StringIO()):
                spawn.return_value.pid = 42
                result = MODULE.main(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "--wait-idle",
                                      "--detach", "--log", str(base / "rehearsal.log"),
                                      "--installed", "relative-plak", "--interval", "2", "--install"])
            self.assertEqual(result, 0)
            args, kwargs = spawn.call_args
            self.assertTrue(kwargs["start_new_session"])
            self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
            self.assertEqual(kwargs["cwd"], repo)
            self.assertNotIn("--detach", args[0])
            self.assertNotIn("--log", args[0])
            self.assertIn(str(Path.cwd() / "relative-plak"), args[0])
            self.assertIn("--install", args[0])
            self.assertEqual(args[0][args[0].index("--interval") + 1], "2.0")
            self.assertEqual((base / "rehearsal.log").stat().st_mode & 0o777, 0o600)

    def test_invalid_wait_or_detach_options_fail_before_build(self):
        for options in (["--timeout", "2"], ["--interval", "1"],
                        ["--wait-idle", "--interval", "inf"], ["--wait-idle", "--interval", "0"], ["--wait-idle", "--timeout", "nan"],
                        ["--wait-idle", "--timeout", "0"], ["--detach"],
                        ["--wait-idle", "--detach"], ["--log", "file"]):
            with self.subTest(options=options), contextlib.redirect_stderr(io.StringIO()), \
                    patch.object(MODULE.subprocess, "run") as run:
                result = MODULE.main(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", *options])
            self.assertEqual(result, 1)
            run.assert_not_called()

    def test_detach_refuses_existing_log_and_log_inside_live_home(self):
        with self.wrapper_fixture() as (base, repo, home, env, _output):
            existing = base / "existing.log"
            existing.write_text("keep")
            for path, error in ((existing, FileExistsError), (home / "log", ValueError)):
                with patch.dict(os.environ, env), patch.object(MODULE, "ROOT", repo), \
                        patch.object(MODULE.subprocess, "Popen") as spawn, self.assertRaises(error):
                    MODULE.main(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "--wait-idle", "--detach",
                                 "--log", str(path)])
                spawn.assert_not_called()
            self.assertEqual(existing.read_text(), "keep")

    def test_detached_real_child_has_its_own_session_and_no_input_terminal(self):
        with self.wrapper_fixture() as (base, repo, _home, env, _output):
            runner = repo / "detached-fixture.py"
            runner.write_text("import json, os\nprint(json.dumps([os.getsid(0), os.getpid(), os.isatty(0)]))\n")
            children = []
            popen = subprocess.Popen
            def launch(*args, **kwargs):
                child = popen(*args, **kwargs)
                children.append(child)
                return child
            with patch.dict(os.environ, env), patch.object(MODULE, "ROOT", repo), \
                    patch.object(MODULE, "__file__", str(runner)), \
                    patch.object(MODULE.subprocess, "Popen", side_effect=launch), \
                    contextlib.redirect_stdout(io.StringIO()):
                result = MODULE.main(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "--wait-idle",
                                      "--detach", "--log", str(base / "log")])
            self.assertEqual(result, 0)
            self.assertEqual(children[0].wait(timeout=10), 0)
            sid, pid, terminal = json.loads((base / "log").read_text())
            self.assertEqual(sid, pid)
            self.assertNotEqual(sid, os.getsid(0))
            self.assertFalse(terminal)

    def test_install_requires_success_and_propagates_installer_status(self):
        for status in (0, 1):
            with self.subTest(status=status), self.wrapper_fixture() as (base, repo, _home, env, _output), \
                    patch.dict(os.environ, env), patch.object(MODULE, "ROOT", repo), \
                    patch.object(MODULE, "rehearse", return_value=status), \
                    patch.object(MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 7)) as run:
                result = MODULE.main(["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "--install"])
            self.assertEqual(result, 7 if status == 0 else 1)
            if status == 0:
                run.assert_called_once_with(["just", "build-maintainer"], cwd=repo, check=False)
            else:
                run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
