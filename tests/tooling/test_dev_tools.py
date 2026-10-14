import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class DevToolsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        (self.root / "scripts").mkdir()
        shutil.copyfile(ROOT / "scripts/dev-tools.sh", self.root / "scripts/dev-tools.sh")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.jsonl"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        DEV_TOOLS_CALLS=str(self.log), DEV_TOOLS_ROOT=str(self.root),
                        FAKE_PYTHON_MODE="ok", FAKE_NEXTEST_VERSION="0.9.100",
                        FAKE_DENY_VERSION="0.19.0")
        self.executable("python3", r'''import os, sys
mode = os.environ['FAKE_PYTHON_MODE']
if mode in {'broken', 'old'}:
    raise SystemExit(1)
if sys.argv[1:] != ['-c', 'import sys; raise SystemExit(sys.version_info < (3, 11))']:
    import json
    with open(os.environ['DEV_TOOLS_CALLS'], 'a') as log:
        log.write(json.dumps(dict(args=sys.argv[1:], env_names=[])) + '\n')
    raise SystemExit(int(os.environ.get('FAKE_FETCH_STATUS', '0')))
''')
        self.executable("cargo", r'''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
env_names = sorted(name for name in ('CARGO_TARGET_DIR',) if name in os.environ)
with open(os.environ['DEV_TOOLS_CALLS'], 'a') as log:
    log.write(json.dumps(dict(args=args, env_names=env_names)) + '\n')
if args == ['nextest', '--version']:
    print('cargo-nextest ' + os.environ['FAKE_NEXTEST_VERSION'])
elif args == ['deny', '--version']:
    print('cargo-deny ' + os.environ['FAKE_DENY_VERSION'])
elif args[:1] == ['install']:
    expected = Path(os.environ['DEV_TOOLS_ROOT']) / '.dev-tools/build'
    if os.environ.get('CARGO_TARGET_DIR') != str(expected):
        raise SystemExit(90)
''')

    def executable(self, name, source, directory=None):
        directory = directory or self.bin
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / name
        path.write_text("#!" + sys.executable + "\n" + source)
        path.chmod(0o700)

    def run_script(self, *args, env=None):
        return subprocess.run(["/bin/bash", str(self.root / "scripts/dev-tools.sh"), *args],
                              env=env or self.env, text=True, capture_output=True, timeout=15)

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_missing_and_broken_python_are_rejected(self):
        isolated = self.root / "isolated-bin"
        isolated.mkdir()
        os.symlink("/usr/bin/dirname", isolated / "dirname")
        missing_env = dict(self.env, PATH=str(isolated))
        result = self.run_script("check", env=missing_env)
        self.assertEqual(result.returncode, 2)
        self.assertIn("Python 3.11+ is required", result.stderr)
        for mode in ("broken", "old"):
            with self.subTest(mode=mode):
                result = self.run_script("check", env=dict(self.env, FAKE_PYTHON_MODE=mode))
                self.assertEqual(result.returncode, 2)
                self.assertIn("python3 is unusable or older than 3.11", result.stderr)

    def test_old_and_unparseable_nextest_are_rejected(self):
        for version in ("0.9.99", "0.8.999"):
            with self.subTest(version=version):
                result = self.run_script("check", env=dict(self.env, FAKE_NEXTEST_VERSION=version))
                self.assertEqual(result.returncode, 2)
                self.assertIn("cargo-nextest 0.9.100 or newer is required", result.stderr)
        result = self.run_script("check", env=dict(self.env, FAKE_NEXTEST_VERSION="unknown"))
        self.assertEqual(result.returncode, 2)
        self.assertIn("unrecognized version", result.stderr)

    def test_current_and_newer_nextest_are_accepted(self):
        for version in ("0.9.100", "0.10.0", "1.0.0"):
            with self.subTest(version=version):
                result = self.run_script("check", env=dict(self.env, FAKE_NEXTEST_VERSION=version))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Development prerequisites are available.", result.stdout)

    def test_old_deny_fails_before_policy_check(self):
        result = self.run_script("deny", env=dict(self.env, FAKE_DENY_VERSION="0.18.999"))
        self.assertEqual(result.returncode, 2)
        self.assertIn("cargo-deny 0.19.0 or newer is required", result.stderr)
        self.assertEqual([call["args"] for call in self.calls()], [["deny", "--version"]])

    def test_supported_deny_forwards_exact_policy_arguments(self):
        result = self.run_script("deny")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call["args"] for call in self.calls()], [
            ["deny", "--version"],
            ["deny", "check", "advisories", "bans", "licenses", "sources"],
        ])

    def test_install_uses_exact_pins_and_isolated_directories(self):
        result = self.run_script("install")
        self.assertEqual(result.returncode, 0, result.stderr)
        prefix = str(self.root / ".dev-tools")
        calls = self.calls()
        self.assertEqual([call["args"] for call in calls], [
            ["install", "--locked", "--root", prefix, "--version", "0.9.100", "cargo-nextest"],
            ["install", "--locked", "--root", prefix, "--version", "0.19.0", "cargo-deny"],
            ["tools/checks/check_public_compile.py", "--fetch"],
        ])
        self.assertTrue(all(call["env_names"] == ["CARGO_TARGET_DIR"] for call in calls[:2]))
        self.assertEqual(calls[-1]["env_names"], [])
        self.assertNotIn("--force", result.stdout + result.stderr + json.dumps(calls))
        self.assertTrue((self.root / ".dev-tools/build").is_dir())

    def test_install_prefetch_is_repeatable_and_failures_are_not_success(self):
        for _ in range(2):
            result = self.run_script("install")
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sum(call["args"] == ["tools/checks/check_public_compile.py", "--fetch"]
                             for call in self.calls()), 2)
        result = self.run_script("install", env=dict(self.env, FAKE_FETCH_STATUS="71"))
        self.assertEqual(result.returncode, 71)
        self.assertNotIn("Installed verification tools", result.stdout)

    def test_install_rejects_broken_python_before_installing(self):
        result = self.run_script("install", env=dict(self.env, FAKE_PYTHON_MODE="broken"))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.calls(), [])

    def test_check_and_help_never_install(self):
        check = self.run_script("check")
        help_result = self.run_script("--help")
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertEqual(help_result.returncode, 0, help_result.stderr)
        self.assertFalse(any(call["args"][:1] == ["install"] for call in self.calls()))
        self.assertFalse(any("--fetch" in call["args"] for call in self.calls()))

    def test_local_prefix_cargo_wins_over_path(self):
        self.env["FAKE_NEXTEST_VERSION"] = "0.9.99"
        local_bin = self.root / ".dev-tools/bin"
        self.executable("cargo", """import sys
if sys.argv[1:] == ['nextest', '--version']:
    print('cargo-nextest 0.9.100')
""", local_bin)
        result = self.run_script("check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [])

    def test_invalid_mode_is_rejected(self):
        result = self.run_script("invalid")
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage: dev-tools.sh", result.stderr)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
