import hashlib
import json
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

from tests.tooling.shipping_fixture import LINUX, REVISION, copy_emitter, executable, snapshot_block


ROOT = pathlib.Path(__file__).resolve().parents[2]


def executable_fixture(text: str) -> str:
    if text.split()[0] != "plakd":
        return text
    return (
        '#!/bin/sh\n# ' + text + '\n'
        'case "$2" in --write-binary-manifest) echo fixture >"$3/binary-id"; '
        'chmod 0444 "$3/binary-id" ;; *) exit 1 ;; esac\n'
    )


def snapshot_env(home: str) -> dict[str, str]:
    """Environment for the snapshot install block over text fixtures.

    A defensive PATH stub keeps an operator build out of fixture execution.
    """
    stub = pathlib.Path(home).parent / "path-stub"
    stub.mkdir(exist_ok=True)
    (stub / "plakd").write_text("#!/bin/sh\nexit 1\n")
    (stub / "plakd").chmod(0o755)
    env = os.environ.copy()
    env["HOME"] = home
    env["PATH"] = str(stub) + os.pathsep + env["PATH"]
    return env


class BuildPlakTests(unittest.TestCase):
    def test_installers_remove_only_owned_obsolete_worker_links(self) -> None:
        for installer in ("install.sh",):
            source = (ROOT / installer).read_text()
            function = (
                "remove_obsolete_worker_link() {"
                + source.split("remove_obsolete_worker_link() {", 1)[1].split("\n}", 1)[0]
                + "\n}"
            )
            with tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory).resolve()
                worker = home / ".local/bin/monty"
                worker.parent.mkdir(parents=True)
                targets = [
                    home / f".local/lib/plak/{root}/old/monty"
                    for root in ("sidecars", "maintainer", "release")
                ]
                for target in targets + [home / "foreign/monty"]:
                    with self.subTest(installer=installer, target=target):
                        worker.symlink_to(target)
                        result = subprocess.run(
                            ["/bin/sh", "-c", function + "\nremove_obsolete_worker_link"],
                            env={**os.environ, "HOME": str(home)},
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(worker.is_symlink(), target not in targets)
                        if worker.is_symlink():
                            self.assertEqual(worker.readlink(), target)
                            worker.unlink()
                worker.write_text("independent installation")
                result = subprocess.run(
                    ["/bin/sh", "-c", function + "\nremove_obsolete_worker_link"],
                    env={**os.environ, "HOME": str(home)},
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(worker.read_text(), "independent installation")

    def test_install_link_refuses_foreign_destinations(self) -> None:
        source = (ROOT / "install.sh").read_text()
        function = (
            "check_link() ("
            + source.split("check_link() (", 1)[1].split('bindir="$HOME/.local/bin"', 1)[0]
        )
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            home = base / "home"
            target = base / "target"
            target.mkdir()
            owned = home / ".local/lib/plak/maintainer/snapshot/plakd"
            owned.parent.mkdir(parents=True)
            owned.write_text("snapshot")
            released = home / ".local/lib/plak/release/snapshot/plakd"
            released.parent.mkdir(parents=True)
            released.write_text("snapshot")
            dest = home / ".local/bin/plakd"
            dest.parent.mkdir(parents=True)
            env = {**os.environ, "HOME": str(home)}
            for kind in (
                "file",
                "directory",
                "foreign-link",
                "owned-link",
                "release-link",
                "target-link",
                "missing",
            ):
                with self.subTest(kind=kind):
                    if dest.is_symlink() or dest.is_file():
                        dest.unlink()
                    elif dest.is_dir():
                        dest.rmdir()
                    if kind == "file":
                        dest.write_text("foreign")
                    elif kind == "directory":
                        dest.mkdir()
                    elif kind.endswith("link"):
                        dest.symlink_to(
                            {
                                "foreign-link": base / "foreign",
                                "owned-link": owned,
                                "release-link": released,
                                "target-link": target / "plakd",
                            }[kind]
                        )
                    result = subprocess.run(
                        [
                            "bash",
                            "-c",
                            'set -eu; mode=maintainer; lib="$HOME/.local/lib/plak"; source_link_target=$1; '
                            'die() { echo "$*" >&2; exit 1; }; ' + function + '\nlink "$2" "$3"',
                            "link",
                            str(target),
                            str(dest),
                            str(owned),
                        ],
                        capture_output=True,
                        text=True,
                        check=False,
                        env=env,
                    )
                    success = kind in ("owned-link", "release-link", "target-link", "missing")
                    self.assertEqual(result.returncode == 0, success, result.stderr)
                    if success:
                        self.assertEqual(dest.readlink(), owned)
                    else:
                        self.assertIn("refusing to replace", result.stderr)
                        if kind == "file":
                            self.assertEqual(dest.read_text(), "foreign")
                        elif kind == "foreign-link":
                            self.assertEqual(dest.readlink(), base / "foreign")

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

    def test_product_build_keeps_monty_features_out_of_parent(self) -> None:
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
        self.assertNotIn('Monty linkage not verified', installer)

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

    def test_snapshot_install_is_content_addressed_immutable_and_retained(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
            + '\nprintf "%s\\n" "$bin" "$js_host" "$py_host"'
        )
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            out = base / "target"
            out.mkdir()
            env = snapshot_env(str(base / "home"))
            root = base / "home/.local/lib/plak/maintainer"

            def install(content: str) -> list[str]:
                for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                    (out / name).write_text(executable_fixture(f"{name} {content}"))
                result = subprocess.run(
                    [
                        "bash",
                        "-c",
                        script,
                        "snapshot",
                        str(out / "plakd"),
                        str(out / "plakous-js-host"),
                        str(out / "plakous-py-host"),
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                    env=env,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                return result.stdout.split()

            def digest(text: str) -> str:
                return hashlib.sha256(text.encode()).hexdigest()

            def snapshot_id(plakd: str, js_host: str, monty: str) -> str:
                manifest = (
                    f"plakd {digest(executable_fixture(plakd))}\nplakous-js-host {digest(js_host)}\n"
                    f"plakous-py-host {digest(monty)}\n"
                )
                return digest(manifest)

            first = install("one")
            first_id = snapshot_id("plakd one", "plakous-js-host one", "plakous-py-host one")
            artifacts = root.parent / "sidecars"
            self.assertEqual(
                first,
                [
                    str(root / first_id / "plakd"),
                    str(
                        artifacts
                        / "plakous-js-host"
                        / digest("plakous-js-host one")
                        / "plakous-js-host"
                    ),
                    str(
                        artifacts
                        / "plakous-py-host"
                        / digest("plakous-py-host one")
                        / "plakous-py-host"
                    ),
                ],
            )
            for path in first:
                self.assertEqual(pathlib.Path(path).stat().st_mode & 0o777, 0o555)
            self.assertEqual(install("one"), first)  # same bytes, same snapshot
            # Rebuilding only the parent leaves both canonical worker paths unchanged.
            (out / "plakd").write_text(executable_fixture("plakd parent-only rebuild"))
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    script,
                    "snapshot",
                    str(out / "plakd"),
                    str(out / "plakous-js-host"),
                    str(out / "plakous-py-host"),
                ],
                capture_output=True,
                text=True,
                check=False,
                env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            rebuilt = result.stdout.split()
            self.assertNotEqual(rebuilt[0], first[0])
            self.assertEqual(rebuilt[1:], first[1:])
            self.assertEqual(
                (pathlib.Path(rebuilt[0]).parent / "plakous-js-host").resolve(),
                pathlib.Path(first[1]),
            )
            (out / "plakd").write_text(executable_fixture("plakd one"))
            # A rebuild that changes only a sidecar is a new snapshot.
            (out / "plakous-js-host").write_text("plakous-js-host two")
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    script,
                    "snapshot",
                    str(out / "plakd"),
                    str(out / "plakous-js-host"),
                    str(out / "plakous-py-host"),
                ],
                capture_output=True,
                text=True,
                check=False,
                env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            sidecar_only = snapshot_id("plakd one", "plakous-js-host two", "plakous-py-host one")
            self.assertEqual(result.stdout.split()[0], str(root / sidecar_only / "plakd"))
            self.assertEqual(
                result.stdout.split()[1],
                str(
                    artifacts
                    / "plakous-js-host"
                    / digest("plakous-js-host two")
                    / "plakous-js-host"
                ),
            )
            (root / ".staging.abandoned").mkdir()
            os.utime(root / ".staging.abandoned", (0, 0))
            for index in range(10):
                install(f"build {index}")
            snapshots = [entry for entry in root.iterdir() if not entry.name.startswith(".")]
            self.assertEqual(len(snapshots), 13)
            self.assertIn(
                snapshot_id("plakd build 9", "plakous-js-host build 9", "plakous-py-host build 9"),
                {entry.name for entry in snapshots},
            )
            self.assertEqual(
                [entry.name for entry in root.iterdir() if entry.name.startswith(".")],
                [".staging.abandoned"],
            )
            self.assertEqual(pathlib.Path(first[0]).read_text(), executable_fixture("plakd one"))
            self.assertTrue(all((entry / "plakd").is_file() for entry in snapshots))
            self.assertEqual(pathlib.Path(first[1]).read_text(), "plakous-js-host one")
            self.assertEqual(pathlib.Path(first[2]).read_text(), "plakous-py-host one")
            # Cache corruption must be refused, not overwritten or silently reused.
            current = (
                artifacts
                / "plakous-js-host"
                / digest("plakous-js-host build 9")
                / "plakous-js-host"
            )
            current.chmod(0o755)
            current.write_text("corrupt current")
            failed = subprocess.run(
                [
                    "bash",
                    "-c",
                    script,
                    "snapshot",
                    str(out / "plakd"),
                    str(out / "plakous-js-host"),
                    str(out / "plakous-py-host"),
                ],
                capture_output=True,
                text=True,
                check=False,
                env=env,
            )
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("sidecar artifact integrity refused", failed.stderr)
            self.assertEqual(current.read_text(), "corrupt current")

    def test_sidecar_cache_refuses_symlinked_directories_and_changed_permissions(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; umask 002; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
            + '\nprintf "%s\\n" "$js_host"'
        )
        faults = ("root", "engine", "digest", "file", "non-executable", "writable")
        faults += tuple(
            f"{level}-{kind}-writable"
            for level in ("root", "engine", "digest")
            for kind in ("group", "world")
        )
        for fault in faults:
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as directory:
                base = pathlib.Path(directory).resolve()
                out = base / "target"
                out.mkdir()
                for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                    (out / name).write_text(executable_fixture(name))
                env = snapshot_env(str(base / "home"))
                command = [
                    "bash",
                    "-c",
                    script,
                    "snapshot",
                    str(out / "plakd"),
                    str(out / "plakous-js-host"),
                    str(out / "plakous-py-host"),
                ]
                installed = subprocess.run(command, capture_output=True, text=True, env=env)
                self.assertEqual(installed.returncode, 0, installed.stderr)
                worker = pathlib.Path(installed.stdout.strip())
                for relative in (
                    ".",
                    ".local",
                    ".local/lib",
                    ".local/lib/plak",
                    ".local/lib/plak/maintainer",
                ):
                    ancestor = pathlib.Path(env["HOME"]) / relative
                    self.assertEqual(ancestor.stat().st_mode & 0o777, 0o755)
                if fault in ("non-executable", "writable"):
                    worker.chmod(0o444 if fault == "non-executable" else 0o755)
                elif fault.endswith("-writable"):
                    level, kind, _ = fault.split("-")
                    target = {
                        "digest": worker.parent,
                        "engine": worker.parent.parent,
                        "root": worker.parent.parent.parent,
                    }[level]
                    target.chmod(0o775 if kind == "group" else 0o757)
                else:
                    target = {
                        "file": worker,
                        "digest": worker.parent,
                        "engine": worker.parent.parent,
                        "root": worker.parent.parent.parent,
                    }[fault]
                    moved = base / "relocated"
                    target.rename(moved)
                    target.symlink_to(moved, target_is_directory=fault != "file")
                refused = subprocess.run(command, capture_output=True, text=True, env=env)
                self.assertNotEqual(refused.returncode, 0)
                self.assertIn("refused", refused.stderr)

    def test_installer_refuses_unsafe_physical_ancestors_without_repair(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; umask 002; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
        )
        for relative in (
            ".",
            ".local",
            ".local/lib",
            ".local/lib/plak",
            ".local/lib/plak/maintainer",
        ):
            for mode in (0o775, 0o757):
                with (
                    self.subTest(ancestor=relative, mode=oct(mode)),
                    tempfile.TemporaryDirectory() as directory,
                ):
                    base = pathlib.Path(directory).resolve()
                    home = base / "physical-home"
                    ancestor = home / relative
                    ancestor.mkdir(parents=True)
                    ancestor.chmod(mode)
                    linked = base / "linked-home"
                    linked.symlink_to(home, target_is_directory=True)
                    out = base / "target"
                    out.mkdir()
                    for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                        (out / name).write_text(executable_fixture(name))
                    env = snapshot_env(str(linked))
                    result = subprocess.run(
                        [
                            "bash",
                            "-c",
                            script,
                            "snapshot",
                            str(out / "plakd"),
                            str(out / "plakous-js-host"),
                            str(out / "plakous-py-host"),
                        ],
                        capture_output=True,
                        text=True,
                        env=env,
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("refused", result.stderr)
                    self.assertEqual(ancestor.stat().st_mode & 0o777, mode)
                    self.assertFalse((home / ".local/lib/plak/sidecars").exists())

    def test_concurrent_install_keeps_one_immutable_canonical_worker(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
            + '\nprintf "%s\\n" "$js_host" "$py_host"'
        )
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            out = base / "target"
            out.mkdir()
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                (out / name).write_text(executable_fixture(name))
            env = snapshot_env(str(base / "home"))
            command = [
                "bash",
                "-c",
                script,
                "snapshot",
                str(out / "plakd"),
                str(out / "plakous-js-host"),
                str(out / "plakous-py-host"),
            ]
            processes = [
                subprocess.Popen(
                    command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env
                )
                for _ in range(4)
            ]
            results = []
            for process in processes:
                stdout, stderr = process.communicate(timeout=30)
                self.assertEqual(process.returncode, 0, stderr)
                results.append(stdout.splitlines())
            self.assertTrue(all(result == results[0] for result in results))
            workers = [pathlib.Path(path) for path in results[0]]
            inodes = [worker.stat().st_ino for worker in workers]
            repeated = subprocess.run(command, capture_output=True, text=True, env=env)
            self.assertEqual(repeated.returncode, 0, repeated.stderr)
            self.assertEqual(inodes, [worker.stat().st_ino for worker in workers])
            for worker in workers:
                self.assertEqual(worker, worker.resolve())
                self.assertEqual(worker.read_text(), worker.name)
                self.assertEqual(
                    worker.parent.name, hashlib.sha256(worker.read_bytes()).hexdigest()
                )
            cache = base / "home/.local/lib/plak"
            self.assertEqual(list(cache.rglob(".staging.*")), [])
            # These fixtures deliberately decline the state-key probe.
            self.assertEqual(list(cache.rglob("state-key")), [])

    def test_install_preserves_legacy_producer_and_canonical_workers(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
        )
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            root = base / "home/.local/lib/plak/maintainer"
            legacy = root / ("a" * 64)
            legacy.mkdir(parents=True)
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                (legacy / name).write_text(f"legacy {name}")
                (legacy / name).chmod(0o555)
            os.utime(legacy, (0, 0))
            out = base / "target"
            out.mkdir()
            env = snapshot_env(str(base / "home"))
            for index in range(10):
                for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                    (out / name).write_text(executable_fixture(f"{name} build {index}"))
                result = subprocess.run(
                    [
                        "bash",
                        "-c",
                        script,
                        "snapshot",
                        str(out / "plakd"),
                        str(out / "plakous-js-host"),
                        str(out / "plakous-py-host"),
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                    env=env,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                self.assertEqual((legacy / name).read_text(), f"legacy {name}")
                self.assertFalse((legacy / name).is_symlink())
            self.assertEqual(sum((entry / "plakd").is_file() for entry in root.iterdir()), 11)

    def test_snapshot_paths_are_physical_under_a_linked_home(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            physical = base / "physical-home"
            physical.mkdir()
            linked = base / "linked-home"
            linked.symlink_to(physical, target_is_directory=True)
            out = base / "target"
            out.mkdir()
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                (out / name).write_text(
                    '#!/bin/sh\ncase "$2" in --state-key) echo r1.e1.p1.t1 ;; --write-binary-manifest) echo fixture >"$3/binary-id"; chmod 0444 "$3/binary-id" ;; esac\n'
                )
            env = snapshot_env(str(linked))
            result = subprocess.run(
                [
                    "sh",
                    str(ROOT / "install.sh"),
                    "--source-build",
                    str(out),
                    "maintainer",
                    str(out),
                ],
                capture_output=True,
                text=True,
                check=False,
                env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            installed = (linked / ".local/bin/plakd").readlink()
            self.assertEqual(installed, installed.resolve())
            self.assertTrue(installed.is_relative_to(physical))
            self.assertEqual((linked / ".local/bin/plakd").resolve(), installed)

    def test_snapshot_install_accepts_a_binary_without_the_manifest_probe(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
            + '\nprintf "%s\\n" "$bin"'
        )
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            binary = base / "plakd"
            binary.write_text(
                '#!/bin/sh\ncase "$2" in --write-binary-manifest) echo partial >"$3/binary-id"; exit 2 ;; --state-key) echo r1.e1.p1.t1 ;; esac\n'
            )
            for name in ("plakous-js-host", "plakous-py-host"):
                (base / name).write_text(name)
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    script,
                    "snapshot",
                    str(binary),
                    str(base / "plakous-js-host"),
                    str(base / "plakous-py-host"),
                ],
                capture_output=True,
                text=True,
                env=snapshot_env(str(base / "home")),
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            snapshot = pathlib.Path(result.stdout.strip()).parent
            self.assertTrue((snapshot / "plakd").is_file())
            self.assertFalse((snapshot / "binary-id").exists())
            self.assertEqual(list(snapshot.parent.glob(".staging.*")), [])

    def test_snapshot_records_its_state_key_and_preserves_all_keys(self) -> None:
        block = snapshot_block()
        script = (
            'set -euo pipefail; bin="$1"; js_host="$2"; py_host="$3"; snapshot_flavor=maintainer; '
            + block
            + '\nprintf "%s\\n" "$bin"'
        )
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            out = base / "target"
            out.mkdir()
            env = snapshot_env(str(base / "home"))

            def install(key: str, build: int) -> pathlib.Path:
                (out / "plakd").write_text(
                    f'#!/bin/sh\n# build {build}\n'
                    f'case "$1 $2" in\n"version --state-key") echo {key} ;;\n'
                    '"version --export-version") echo 4 ;;\n"version --write-binary-manifest") echo fixture >"$3/binary-id"; chmod 0444 "$3/binary-id" ;;\nesac\n'
                )
                for name in ("plakous-js-host", "plakous-py-host"):
                    (out / name).write_text(f"{name} {build}")
                result = subprocess.run(
                    [
                        "bash",
                        "-c",
                        script,
                        "snapshot",
                        str(out / "plakd"),
                        str(out / "plakous-js-host"),
                        str(out / "plakous-py-host"),
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                    env=env,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                return pathlib.Path(result.stdout.strip()).parent

            old = install("r1.e1.p1.t1", 0)
            self.assertEqual((old / "state-key").read_text().strip(), "r1.e1.p1.t1")
            self.assertEqual((old / "export-version").read_text().strip(), "4")
            self.assertEqual((old / "binary-id").read_text(), "fixture\n")
            self.assertEqual((old / "binary-id").stat().st_mode & 0o777, 0o444)
            for build in range(1, 11):
                install("r2.e1.p1.t1", build)
            root = base / "home/.local/lib/plak/maintainer"
            snapshots = {entry for entry in root.iterdir() if not entry.name.startswith(".")}
            self.assertIn(old, snapshots)
            self.assertEqual(len(snapshots), 11)
            # Historical producers remain available beyond eight builds and three keys.
            for build, key in enumerate(("r3.e1.p1.t1", "r4.e1.p1.t1"), start=11):
                install(key, build)
            for build in range(13, 21):
                install("r5.e1.p1.t1", build)
            snapshots = {entry for entry in root.iterdir() if not entry.name.startswith(".")}
            keys = sorted((entry / "state-key").read_text().strip() for entry in snapshots)
            self.assertIn(old, snapshots)
            self.assertEqual(len(snapshots), 21)
            self.assertEqual(keys.count("r5.e1.p1.t1"), 8)
            self.assertEqual(
                sorted(set(keys)),
                ["r1.e1.p1.t1", "r2.e1.p1.t1", "r3.e1.p1.t1", "r4.e1.p1.t1", "r5.e1.p1.t1"],
            )

    def test_install_mode_builds_plain_release_and_installs_a_release_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory).resolve()
            repo = base / "repo"
            (repo / "tools/build").mkdir(parents=True)
            shutil.copy(ROOT / "tools/build/build-plakd.sh", repo / "tools/build/build-plakd.sh")
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
            for name, body in (
                (
                    "rustc",
                    f"printf '%s\\n' 'host: {LINUX}' 'release: 1.100.0' 'commit-hash: {'b' * 40}' 'LLVM version: 23.1.5'",
                ),
                ("uname", 'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac'),
            ):
                (fake / name).write_text(f"#!/bin/sh\n{body}\n")
                (fake / name).chmod(0o755)
            # Fake cargo: record the invocation and write the named executables
            # whose bytes depend on the build arguments.
            (fake / "cargo").write_text(f"""#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == -V ]]; then echo 'cargo 1.100.0 (aaaaaaaaa 2026-11-09)'; exit; fi
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
    printf '%s\\n# %s\\n' {shlex.quote(executable())} "$*" >"$out/$name"
    chmod 0755 "$out/$name"
  done
done
""")
            (fake / "cargo").chmod(0o755)
            (fake / "strip").write_text(
                f"#!/bin/sh\nprintf 'strip %s\\n' \"$1\" >>{log}\nprintf '# stripped\\n' >>\"$1\"\n"
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
            result = run("--install", PLAK_SOURCE_REVISION="spoofed")
            self.assertEqual(result.returncode, 0, result.stderr)
            raw = base / "target" / LINUX / "release"
            self.assertEqual(
                log.read_text().splitlines(),
                [
                    "cargo build --locked -p plakd --profile release",
                    "cargo build --locked -p plakous-js-runtime -p plakous-py-worker --profile release",
                    f"linkage {raw}",
                    f"strip {raw / 'stripped/plakd'}",
                    f"strip {raw / 'stripped/plakous-js-host'}",
                    f"strip {raw / 'stripped/plakous-py-host'}",
                ],
            )
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                self.assertNotIn("# stripped", (raw / name).read_text())
                self.assertIn("# stripped", (raw / "stripped" / name).read_text())
            installed = (home / ".local/bin/plakd").resolve()
            self.assertEqual(installed.parent.parent, lib / "release")
            self.assertEqual(len(installed.parent.name), 64)
            self.assertEqual((installed.parent / "state-key").read_text().strip(), "r1.e1.p1.t1")
            self.assertEqual((installed.parent / "binary-id").read_text(), "fixture\n")
            self.assertFalse((lib / "maintainer").exists())
            for name in ("plakous-js-host", "plakous-py-host"):
                worker = (home / ".local/bin" / name).resolve()
                self.assertEqual(worker.parent.parent, lib / "sidecars" / name)
                self.assertEqual((installed.parent / name).resolve(), worker)
            self.assertNotIn("maintainer", installed.read_text())

            result = run("--artifacts")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(raw / "stripped"))
            self.assertNotIn("--features", log.read_text())
            self.assertEqual((home / ".local/bin/plakd").resolve(), installed)
            for name in ("plakd", "plakous-js-host", "plakous-py-host"):
                data = (raw / "stripped" / name).read_bytes()
                for path in (repo, base / "cargo-home"):
                    self.assertNotIn(os.fsencode(path), data, name)

            checker = repo / "tools/build/check-artifact-linkage.py"
            checker_source = checker.read_text()
            checker.write_text(checker_source + "raise SystemExit(2)\n")
            result = run("--artifacts")
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("strip ", log.read_text())
            self.assertEqual(result.stdout, "")
            checker.write_text(checker_source)

            # A maintainer install may replace the release links and back again;
            # both snapshots are retained.
            result = run("--install-maintainer")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("--features plakd/maintainer --profile maintainer", log.read_text())
            maintainer = (home / ".local/bin/plakd").resolve()
            self.assertEqual(maintainer.parent.parent, lib / "maintainer")
            result = run("--install")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((home / ".local/bin/plakd").resolve(), installed)
            self.assertTrue(maintainer.is_file())

            # A foreign destination is never clobbered.
            (home / ".local/bin/plakd").unlink()
            (home / ".local/bin/plakd").write_text("foreign")
            result = run("--install")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing to replace", result.stderr)
            self.assertEqual((home / ".local/bin/plakd").read_text(), "foreign")

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
            self.assertEqual((home / ".local/bin/plakd").read_text(), "foreign")
            result = run("--release")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("--features plakd/maintainer --profile release", log.read_text())
            self.assertNotIn("strip ", log.read_text())
            self.assertEqual((home / ".local/bin/plakd").read_text(), "foreign")
            result = run("--install", "--release")
            self.assertEqual(result.returncode, 2)
            (repo / ".git").rmdir()
            (repo / ".git").write_text("gitdir: /unused\n")
            result = run("--maintainer")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("--features plakd/maintainer --profile maintainer", log.read_text())
            self.assertEqual((home / ".local/bin/plakd").read_text(), "foreign")
            result = run("--install-maintainer")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("linked worktree", result.stderr)

    def test_justfile_exposes_build_and_install(self) -> None:
        justfile = (ROOT / "Justfile").read_text(encoding="utf-8")
        self.assertIn("\nbuild profile:", justfile)
        self.assertIn("tools/build/recipe-build.sh", justfile)
        self.assertIn("\ninstall flavor:", justfile)
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
        self.assertIn('exec sh "$repo_root/install.sh"', source)
        self.assertIn("plakous-js-host", source)
        self.assertNotIn("npm", source)
        self.assertNotIn("@openai/codex", source)


if __name__ == "__main__":
    unittest.main()
