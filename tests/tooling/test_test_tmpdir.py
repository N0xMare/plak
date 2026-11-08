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
        (self.checkout / "tools/gate").mkdir(parents=True)
        shutil.copyfile(
            ROOT / "tools/gate/test-tmpdir.sh", self.checkout / "tools/gate/test-tmpdir.sh"
        )
        self.shared = self.base / "shared"
        self.shared.mkdir()
        self.fallback = self.checkout / "target/plak-test-tmp"
        self.env = dict(os.environ)
        self.env.pop("PLAK_TEST_TMPDIR", None)

    def select(self, *args):
        result = subprocess.run(
            ["bash", str(self.checkout / "tools/gate/test-tmpdir.sh"), *args],
            cwd=self.base,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return Path(result.stdout.strip()), result.stderr

    def per_checkout_name(self):
        digest = subprocess.run(
            ["cksum"], input=str(self.checkout), capture_output=True, text=True, check=True
        ).stdout.split()[0]
        return f"plak-test-{os.getuid()}-{digest}"

    def test_linux_and_darwin_use_a_private_per_checkout_shared_directory(self):
        for system in ("Linux", "Darwin"):
            with self.subTest(system=system):
                selected, stderr = self.select(
                    "--system", system, "--shared-root", str(self.shared)
                )
                self.assertEqual(selected, self.shared / self.per_checkout_name())
                self.assertEqual(stat.S_IMODE(selected.stat().st_mode), 0o700)
                self.assertFalse(selected.is_symlink())
                self.assertEqual(stderr, "")
                shutil.rmtree(selected)

    def test_fresh_empties_the_shared_directory_and_plain_runs_keep_it(self):
        selected, _ = self.select("--system", "Darwin", "--shared-root", str(self.shared))
        (selected / "left-over").write_text("x")
        self.assertEqual(
            self.select("--system", "Darwin", "--shared-root", str(self.shared))[0], selected
        )
        self.assertTrue((selected / "left-over").exists())
        self.assertEqual(
            self.select("--fresh", "--system", "Darwin", "--shared-root", str(self.shared))[0],
            selected,
        )
        self.assertEqual(list(selected.iterdir()), [])

    def test_default_shared_roots_are_dev_shm_and_private_tmp(self):
        for system, shared_root in (("Linux", Path("/dev/shm")), ("Darwin", Path("/private/tmp"))):
            with self.subTest(system=system):
                usable = (
                    shared_root.is_dir()
                    and not shared_root.is_symlink()
                    and shared_root.resolve() == shared_root
                    and os.access(shared_root, os.W_OK)
                )
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
                selected, stderr = self.select(
                    "--system", "Darwin", "--shared-root", str(shared_root)
                )
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
        self.assertIn("test-tmpdir.sh", (ROOT / "tools/gate/test-env.sh").read_text())
        for path in (
            "tools/gate/dev-check.sh",
            "tools/perf/perf-smoke.sh",
            "tools/dev/crash-sweep.py",
            "tools/dev/jj-tools.py",
            "tools/dev/coverage.sh",
            "tests/live/live.py",
        ):
            with self.subTest(path=path):
                self.assertIn("test-env.sh", (ROOT / path).read_text())
        dev = (ROOT / "tools/dev/dev.just").read_text()
        self.assertIn("tools/dev/coverage.sh", dev)
        self.assertIn(
            "tools/gate/test-env.sh -- python3 tools/gate/test-processes.py -- tools/dev/crash-sweep.py",
            dev,
        )
        self.assertNotIn('TMPDIR="$PWD/target/plak-test-tmp"', dev)
        justfile = (ROOT / "Justfile").read_text()
        self.assertNotIn('TMPDIR="$PWD/target/plak-test-tmp"', justfile)
        self.assertIn("[positional-arguments]", justfile)
        self.assertIn(
            'tools/gate/test-env.sh -- python3 tools/gate/test-processes.py -- cargo nextest "$@"',
            justfile,
        )
        self.assertIn("plak_dev_tools_prefix", justfile)
        self.assertIn("dev-tools.sh check-nextest", justfile)
        self.assertNotIn("\ntest:\n", justfile)
        coverage = (ROOT / "tools/dev/coverage.sh").read_text()
        self.assertIn("plak_dev_tools_prefix", coverage)
        self.assertIn("dev-tools.sh check-nextest", coverage)

    def test_test_env_exports_home_and_empty_git_config(self):
        script = ROOT / "tools/gate/test-env.sh"
        env = dict(os.environ, PLAK_TEST_TMPDIR=str(self.base / "explicit-tmp"))
        result = subprocess.run(
            ["bash", str(script), "--json"], env=env, capture_output=True, text=True, timeout=20
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        import json

        values = json.loads(result.stdout)
        scratch = (self.base / "explicit-tmp").resolve()
        self.assertEqual(Path(values["TMPDIR"]), scratch)
        home = Path(values["PLAK_HOME"])
        self.assertEqual(home, scratch / "plak-isolated-home")
        self.assertEqual(stat.S_IMODE(home.stat().st_mode), 0o700)
        gitconfig = Path(values["GIT_CONFIG_GLOBAL"])
        self.assertEqual(gitconfig, scratch / "gitconfig")
        self.assertEqual(gitconfig.read_text(), "")
        self.assertEqual(values["GIT_CONFIG_NOSYSTEM"], "1")

    def test_nextest_filter_expression_reaches_cargo_as_one_argument(self):
        copy = self.base / "repo"
        for relative in (
            "Justfile",
            "tools/musl/musl.just",
            "tests/live/live.just",
            "tools/perf/perf.just",
            "tools/prompts/prompts.just",
            "tools/tui/tui.just",
            "tools/dev/dev.just",
            "eval/harbor.just",
            "tools/gate/test-env.sh",
            "tools/gate/test-tmpdir.sh",
            "tools/gate/test-processes.py",
            "tools/gate/dev-tools.sh",
            "tools/gate/dev-tools-prefix.sh",
        ):
            destination = copy / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)
            if destination.suffix == ".sh":
                destination.chmod(0o755)
        bindir = copy / ".dev-tools/bin"
        bindir.mkdir(parents=True)
        log = copy / "cargo-argv.jsonl"
        cargo = bindir / "cargo"
        cargo.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "if sys.argv[1:3] == ['nextest', '--version']:\n"
            "    print('cargo-nextest 0.9.100')\n"
            "    raise SystemExit(0)\n"
            "with open(os.environ['CARGO_ARGV_LOG'], 'a') as handle:\n"
            "    handle.write(json.dumps(sys.argv) + '\\n')\n"
            "raise SystemExit(0)\n"
        )
        cargo.chmod(0o755)
        ruff = bindir / "ruff"
        ruff.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            "if '--version' in sys.argv:\n"
            "    print('ruff 9.9.9')\n"
            "raise SystemExit(0)\n"
        )
        ruff.chmod(0o755)
        result = subprocess.run(
            ["just", "nextest", "-E", "package(plakous-model)", "list"],
            cwd=copy,
            env=dict(
                os.environ,
                CARGO_ARGV_LOG=str(log),
                PLAK_TEST_TMPDIR=str(self.base / "nextest-tmp"),
            ),
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("syntax error", result.stderr)
        import json

        recorded = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(len(recorded), 1)
        self.assertTrue(recorded[0][0].endswith("/cargo"), recorded[0][0])
        self.assertEqual(
            recorded[0][1:],
            ["nextest", "-E", "package(plakous-model)", "list"],
        )

    def test_eval_of_test_env_does_not_fail_open(self):
        failing = self.base / "fail-env.sh"
        failing.write_text("#!/bin/bash\necho boom >&2\nexit 9\n")
        failing.chmod(0o755)
        closed = subprocess.run(
            [
                "bash",
                "-c",
                'set -euo pipefail; test_env="$("$1")"; eval "$test_env"; echo OPEN',
                "closed",
                str(failing),
            ],
            capture_output=True,
            text=True,
        )
        self.assertEqual(closed.returncode, 9, closed.stderr)
        self.assertNotIn("OPEN", closed.stdout)
        opened = subprocess.run(
            [
                "bash",
                "-c",
                'set -euo pipefail; eval "$("$1")"; echo OPEN',
                "opened",
                str(failing),
            ],
            capture_output=True,
            text=True,
        )
        self.assertEqual(opened.returncode, 0, opened.stderr)
        self.assertIn("OPEN", opened.stdout)
        for relative in (
            "tools/gate/dev-check.sh",
            "tools/dev/coverage.sh",
            "tools/perf/perf-smoke.sh",
        ):
            text = (ROOT / relative).read_text()
            self.assertNotIn('eval "$(tools/gate/test-env.sh', text)
            self.assertIn('test_env="$(tools/gate/test-env.sh', text)
            self.assertIn('eval "$test_env"', text)


if __name__ == "__main__":
    unittest.main()
