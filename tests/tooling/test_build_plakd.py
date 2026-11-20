import json
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

from tests.tooling.shipping_fixture import LINUX, REVISION, copy_emitter, executable


ROOT = pathlib.Path(__file__).resolve().parents[2]


class BuildPlakTests(unittest.TestCase):
    def test_playground_uses_home_capture_and_preserves_execution(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            repo = base / "repo"
            (repo / "tools/dev").mkdir(parents=True)
            (repo / "docs").mkdir(parents=True)
            (repo / "tests").mkdir()
            shutil.copy(ROOT / "tools/dev/plakd-dev.sh", repo / "tools/dev/plakd-dev.sh")
            shutil.copytree(ROOT / "docs/reference-config", repo / "docs/reference-config")
            binary = repo / "target/release/plakd"
            binary.parent.mkdir(parents=True)
            binary.write_text(
                '#!/usr/bin/env python3\nimport json, os, sys\n'
                'print(json.dumps([os.getcwd(), sys.argv[1:], '
                'os.environ["PLAK_HOME"], '
                'os.environ.get("PLAK_TEST_EXCHANGE_CAPTURE_DIR"), '
                'os.environ.get("OPENAI_API_KEY"), os.getpid()]))\n'
            )
            binary.chmod(0o700)
            env = os.environ.copy()
            # Use this test runner's real interpreter, not host pyenv shims.
            python = base / "python-bin"
            python.mkdir()
            (python / "python3").symlink_to(sys.executable)
            env["PATH"] = str(python) + os.pathsep + env["PATH"]
            env.pop("CARGO_BUILD_TARGET", None)
            env.pop("CARGO_TARGET_DIR", None)
            env.pop("PLAK_DEV_PROFILE", None)
            env.update(
                PLAK_DEV_PLAK_HOME=str(base / "home"),
                PLAK_TEST_EXCHANGE_CAPTURE_DIR="stale",
                OPENAI_API_KEY="synthetic",
            )
            for _ in range(2):
                process = subprocess.Popen(
                    ["bash", str(repo / "tools/dev/plakd-dev.sh"), "run", "two words", ""],
                    cwd=base,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                stdout, stderr = process.communicate(timeout=15)
                self.assertEqual(process.returncode, 0, stderr)
                cwd, argv, home, capture, key, pid = json.loads(stdout)
                self.assertEqual(cwd, str(base))
                self.assertEqual(argv, ["run", "two words", ""])
                self.assertEqual(pid, process.pid)  # exec, not a signal-interposing wrapper
                self.assertIsNone(key)
                self.assertIsNone(capture)  # the maintainer binary owns automatic capture
                self.assertEqual(home, str(base / "home"))
                self.assertEqual(pathlib.Path(home).stat().st_mode & 0o777, 0o700)
                self.assertTrue((pathlib.Path(home) / "config.toml").is_file())
                self.assertFalse((repo / "tests/provider-authority").exists())

    def test_installer_refuses_to_relink_from_a_linked_worktree(self) -> None:
        source = (ROOT / "tools/build/build-plakd.sh").read_text()
        block = source.split("# BEGIN LINKED WORKTREE GUARD", 1)[1].split(
            "# END LINKED WORKTREE GUARD", 1
        )[0]
        for layout in ("primary", "linked", "jj-secondary"):
            with tempfile.TemporaryDirectory() as temporary:
                root = pathlib.Path(temporary)
                if layout == "primary":
                    (root / ".git").mkdir()
                elif layout == "linked":
                    (root / ".git").write_text("gitdir: /unused\n")
                else:
                    (root / ".jj").mkdir()
                result = subprocess.run(
                    ["bash", "-c", "set -euo pipefail; repo_root=$1; " + block, "guard", str(root)],
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode == 0, layout == "primary", result.stderr)
                if layout != "primary":
                    self.assertIn("linked worktree", result.stderr)

    def test_product_build_builds_evaluators_separately(self) -> None:
        import tomllib

        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
        self.assertEqual(manifest["workspace"]["default-members"], ["bin/plakd"])
        source = (ROOT / "tools/build/build-plakd.sh").read_text()
        builds = [line for line in source.splitlines() if line.startswith("cargo build ")]
        self.assertEqual(len(builds), 2)
        self.assertIn('-p plakd ${plak_features[@]+"${plak_features[@]}"}', builds[0])
        self.assertIn("plak_features=(--features plakd/maintainer)", source)
        self.assertIn("-p plakous-js-runtime -p plakous-py-worker", builds[1])
        self.assertNotIn("--features", builds[1])

    def test_installer_and_build_gate_require_cross_platform_linkage(self) -> None:
        installer = (ROOT / "tools/build/build-plakd.sh").read_text()
        gate = (ROOT / "tools/gate/dev-check.sh").read_text()
        self.assertIn('check-artifact-linkage.py "$output_dir"', installer)
        self.assertIn('check-artifact-linkage.py', gate)
        self.assertNotIn('micro-monty linkage not verified', installer)

    def test_product_build_profile_arguments_work_with_system_bash(self) -> None:
        source = (ROOT / "tools/build/build-plakd.sh").read_text()
        builds = [line for line in source.splitlines() if line.startswith("cargo build ")]
        script = (
            'set -euo pipefail; profile="$1"; profile_flag=(); '
            'plak_features=(); [[ -z "$2" ]] || plak_features=(--features "$2"); '
            'if [[ "$profile" != debug ]]; then profile_flag=(--profile "$profile"); fi; '
            'cargo() { printf "%s\\n" "$*"; };\n' + "\n".join(builds)
        )
        for profile, features in (
            ("debug", "plakd/maintainer"),
            ("release", "plakd/maintainer"),
            ("maintainer", "plakd/maintainer"),
            ("release", ""),
        ):
            with self.subTest(profile=profile, features=features):
                result = subprocess.run(
                    ["/bin/bash", "-c", script, "builds", profile, features],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                suffix = "" if profile == "debug" else f" --profile {profile}"
                plakd = " --features plakd/maintainer" if features else ""
                self.assertEqual(
                    result.stdout.splitlines(),
                    [
                        "build --locked -p plakd" + plakd + suffix,
                        "build --locked -p plakous-js-runtime -p plakous-py-worker" + suffix,
                    ],
                )

    def test_install_mode_builds_plain_release_and_installs_a_release_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            repo = base / "repo"
            (repo / "tools/build").mkdir(parents=True)
            shutil.copy(ROOT / "tools/build/build-plakd.sh", repo / "tools/build/build-plakd.sh")
            (repo / "tools/musl").mkdir()
            shutil.copy(ROOT / "tools/musl/musl-toolchain.sh", repo / "tools/musl")
            shutil.copy(ROOT / "install.sh", repo / "install.sh")
            copy_emitter(repo)
            log = base / "calls.log"
            (repo / "tools/build/check-artifact-linkage.py").write_text(
                f"import sys\nopen({str(log)!r}, 'a').write('linkage ' + sys.argv[1] + '\\n')\n"
            )
            fake = base / "fake-bin"
            fake.mkdir()
            (fake / "python3").symlink_to(sys.executable)
            # Only a primary checkout can relink installed binaries.
            (repo / ".git").mkdir()
            (fake / "git").write_text(
                f'#!/bin/sh\ncase "$1" in rev-parse) echo {REVISION} ;; status) printf "%s" "${{FAKE_DIRTY:-}}" ;; esac\n'
            )
            (fake / "git").chmod(0o755)
            gnu = "x86_64-unknown-linux-gnu"
            for name, body in (
                (
                    "rustc",
                    f"printf '%s\\n' 'host: {gnu}' 'release: 1.100.0' 'commit-hash: {'b' * 40}' 'LLVM version: 23.1.5'",
                ),
                ("uname", 'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac'),
                ("rustup", f"echo {LINUX}"),
                ("musl-gcc", "exit 0"),
            ):
                (fake / name).write_text(f"#!/bin/sh\n{body}\n")
                (fake / name).chmod(0o755)
            # Fake cargo: record the invocation and write the named executables
            # whose bytes depend on the build arguments.
            probe = executable().replace(
                '  *) exit 2 ;;',
                f"""  "snapshot install "*) printf 'delegate %s\\n' "$*" >>{log} ;;
  *) exit 2 ;;""",
            )
            (fake / "cargo").write_text(f"""#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == -V ]]; then echo 'cargo 1.100.0 (aaaaaaaaa 2026-11-09)'; exit; fi
[[ "${{PLAK_SOURCE_REVISION:-}}" != spoofed ]] || exit 2
printf 'cargo %s\\n' "$*" >>{log}
profile=debug; packages=(); args=("$@")
for ((i = 0; i < ${{#args[@]}}; i++)); do
  case "${{args[i]}}" in
    --profile) profile="${{args[i+1]}}" ;;
    -p) packages+=("${{args[i+1]}}") ;;
  esac
done
out="$CARGO_TARGET_DIR/${{CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}}$profile"
mkdir -p "$out"
for package in "${{packages[@]}}"; do
  case "$package" in
    plakd) names=(plakd) ;;
    plakous-js-runtime) names=(plakous-js-host) ;;
    plakous-py-worker) names=(plakous-py-host) ;;
  esac
  for name in "${{names[@]}}"; do
    printf '%s\\n# %s\\n' {shlex.quote(probe)} "$*" >"$out/$name"
    chmod 0755 "$out/$name"
  done
done
""")
            (fake / "cargo").chmod(0o755)
            (fake / "strip").write_text(
                f"""#!/bin/sh
printf 'strip %s\\n' "$1" >>{log}
printf '# stripped\\n' >>"$1"
if [ "${{FAKE_LEAK_FILE:-}}" = "${{1##*/}}" ]; then
    printf '# leaked %s\\n' "$FAKE_LEAK_PATH" >>"$1"
fi
"""
            )
            (fake / "strip").chmod(0o755)
            home = base / "home"
            env = {
                k: v for k, v in os.environ.items() if k not in ("CARGO_BUILD_TARGET", "RUSTFLAGS")
            }
            env.update(
                HOME=str(home),
                CARGO_TARGET_DIR=str(base / "target"),
                CARGO_HOME=str(base / "cargo-home"),
                PATH=str(fake) + os.pathsep + env["PATH"],
            )
            lib = home / ".local/lib/plak"

            def run(*args: str, **overrides: str) -> subprocess.CompletedProcess[str]:
                log.write_text("")
                return subprocess.run(
                    ["bash", str(repo / "tools/build/build-plakd.sh"), *args],
                    cwd=base,
                    env={**env, **overrides},
                    capture_output=True,
                    text=True,
                    check=False,
                )

            for mode in ("--install", "--artifacts"):
                for overrides, message in (
                    ({"FAKE_DIRTY": "?? untracked"}, "not clean"),
                    ({"RUSTFLAGS": ""}, "not RUSTFLAGS"),
                ):
                    result = run(mode, **overrides)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn(message, result.stderr)
                    self.assertEqual(log.read_text(), "")
                    self.assertFalse(lib.exists())
            result = run("--artifacts", CARGO_BUILD_TARGET=gnu)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn(f"Linux --artifacts requires {LINUX}", result.stderr)
            self.assertEqual(log.read_text(), "")
            result = run("--install", PLAK_SOURCE_REVISION="spoofed")
            self.assertEqual(result.returncode, 0, result.stderr)
            raw = base / "target" / gnu / "release"
            self.assertEqual(
                log.read_text().splitlines(),
                [
                    "cargo build --locked -p plakd --profile release",
                    "cargo build --locked -p plakous-js-runtime -p plakous-py-worker --profile release",
                    f"linkage {raw}",
                    f"strip {raw / 'stripped/plakd'}",
                    f"strip {raw / 'stripped/plakous-js-host'}",
                    f"strip {raw / 'stripped/plakous-py-host'}",
                    f"delegate snapshot install --from {raw / 'stripped'} --source-build {base / 'target'} --flavor release",
                ],
            )
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                self.assertNotIn("# stripped", (raw / name).read_text())
                self.assertIn("# stripped", (raw / "stripped" / name).read_text())
            self.assertFalse(lib.exists())

            result = run("--artifacts")
            self.assertEqual(result.returncode, 0, result.stderr)
            shipping_dir = base / "target" / LINUX / "release/stripped"
            self.assertEqual(result.stdout.strip(), str(shipping_dir))
            self.assertNotIn("--features", log.read_text())
            self.assertNotIn("delegate ", log.read_text())
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                for path in (repo, base / "cargo-home"):
                    with self.subTest(binary=name, leaked_path=path):
                        refused = run("--artifacts", FAKE_LEAK_FILE=name, FAKE_LEAK_PATH=str(path))
                        self.assertEqual(refused.returncode, 1, refused.stderr)
                        self.assertIn(f"{name} contains a physical build path", refused.stderr)
                        self.assertIn(os.fsencode(path), (shipping_dir / name).read_bytes())
                        self.assertFalse((shipping_dir / "manifest.json").exists())

            checker = repo / "tools/build/check-artifact-linkage.py"
            checker_source = checker.read_text()
            checker.write_text(checker_source + "raise SystemExit(2)\n")
            result = run("--artifacts")
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("strip ", log.read_text())
            self.assertEqual(result.stdout, "")
            checker.write_text(checker_source)

            # The unpublished maintainer build uses the same native entry point.
            result = run("--install-maintainer")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("--features plakd/maintainer --profile maintainer", log.read_text())
            self.assertIn(
                f"delegate snapshot install --from {base / 'target/maintainer'} "
                f"--source-build {base / 'target'} --flavor maintainer",
                log.read_text(),
            )

            result = run("--bogus")
            self.assertEqual(result.returncode, 2)
            self.assertIn("--install-maintainer", result.stderr)
            self.assertNotIn("--no-install", result.stderr)

            # Build-only leaves PATH and snapshots alone, including from a
            # linked worktree. Install still refuses that layout.
            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                "cargo build --locked -p plakd --features plakd/maintainer\n", log.read_text()
            )
            self.assertNotIn("--profile", log.read_text())
            self.assertNotIn("strip ", log.read_text())
            self.assertNotIn("delegate ", log.read_text())
            result = run("--release")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("--features plakd/maintainer --profile release", log.read_text())
            self.assertNotIn("strip ", log.read_text())
            self.assertNotIn("delegate ", log.read_text())
            result = run("--install", "--release")
            self.assertEqual(result.returncode, 2)
            (repo / ".git").rmdir()
            (repo / ".git").write_text("gitdir: /unused\n")
            result = run("--maintainer")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("--features plakd/maintainer --profile maintainer", log.read_text())
            self.assertNotIn("delegate ", log.read_text())
            result = run("--install-maintainer")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("linked worktree", result.stderr)

    def test_justfile_exposes_build_and_install(self) -> None:
        justfile = (ROOT / "Justfile").read_text(encoding="utf-8")
        self.assertIn("\nbuild profile:", justfile)
        self.assertIn("tools/build/recipe-build.sh", justfile)
        self.assertIn('\ninstall flavor="":', justfile)
        self.assertIn("tools/gate/dev-check.sh --install", justfile)
        self.assertIn("tools/build/recipe-install.sh", justfile)
        self.assertNotIn("\nbuild-release:", justfile)
        self.assertNotIn("\nbuild-maintainer:", justfile)
        self.assertNotIn("\nproduct:\n", justfile)
        self.assertNotIn("\nplak:\n", justfile)
        self.assertNotIn("\nplak-dev:\n", justfile)
        self.assertNotIn("\nplak-minimal:\n", justfile)
        self.assertNotIn("--no-install", justfile)
        recipe = (ROOT / "tools/build/recipe-build.sh").read_text(encoding="utf-8")
        install = (ROOT / "tools/build/recipe-install.sh").read_text(encoding="utf-8")
        self.assertIn("debug|release|maintainer|profiling", recipe)
        self.assertIn("release|maintainer", install)
        self.assertIn("exec tools/build/build-plakd.sh ;;", recipe)
        self.assertIn("exec tools/build/build-plakd.sh --release ;;", recipe)
        self.assertIn("exec tools/build/build-plakd.sh --maintainer ;;", recipe)
        self.assertIn("exec tools/build/build-plakd.sh --profiling ;;", recipe)
        self.assertIn("exec tools/build/build-plakd.sh --install ;;", install)
        self.assertIn("exec tools/build/build-plakd.sh --install-maintainer ;;", install)
        source = (ROOT / "tools/build/build-plakd.sh").read_text(encoding="utf-8")
        self.assertIn("profile=profiling", source)

    def test_maintainer_build_ships_js_host_without_official_v8(self) -> None:
        source = (ROOT / "tools/build/build-plakd.sh").read_text(encoding="utf-8")
        self.assertNotIn("--features embedded", source)
        self.assertNotIn("-p plakd -p plakous-js-eval", source)
        self.assertIn("JS_NewRuntime", (ROOT / "tools/gate/check_workspace.py").read_text())
        self.assertNotIn("fetch-official-code-mode-host.sh", source)
        self.assertNotIn("build-or-locate-compose-host.sh", source)
        self.assertNotIn("PLAK_BUNDLED_PYTHON_HOST", source)
        self.assertNotIn("PLAK_BUNDLED_JAVASCRIPT_HOST", source)
        self.assertIn("--features plakd/maintainer", source)
        self.assertIn('exec "$bin" snapshot install --from "$output_dir"', source)
        self.assertIn("plakous-js-host", source)
        self.assertNotIn("npm", source)
        self.assertNotIn("@openai/codex", source)


if __name__ == "__main__":
    unittest.main()
