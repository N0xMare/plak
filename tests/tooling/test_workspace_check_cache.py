"""Hit, miss and invalidation for the workspace checker's result cache.

The full checker is not what these tests time. They hash a fixture and drive
`check_root` with the real checker replaced, which is the skip decision.
"""

from __future__ import annotations

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/gate"))


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


checker = load_module("check_workspace_cache", ROOT / "tools/gate/check_workspace.py")

LINE = (
    "validated 1 workspace packages across layers (core=1), "
    "dependency boundaries, and the plak/plakous-js-host build split; "
    "artifact linkage not checked"
)


def git(root: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-C", str(root), "-c", "user.name=t", "-c", "user.email=t@t", *args],
        check=True,
        capture_output=True,
        text=True,
    )


def same_mtime(path: Path, like: Path) -> None:
    stamp = like.stat()
    os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))


class WorkspaceCheckCache(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)
        self.root = self.scratch / "repo"
        self.root.mkdir()
        self.target = self.scratch / "target"
        self.cargo_home = self.scratch / "cargo-home"
        self.cargo_home.mkdir()
        git(self.root, "init", "-q")
        (self.root / "Cargo.toml").write_text('[workspace]\nresolver = "3"\n', encoding="utf-8")
        (self.root / ".gitignore").write_text("target/\ndocs/local/\n", encoding="utf-8")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", "fixture")
        self.env = patch.dict(
            os.environ,
            {"CARGO_TARGET_DIR": str(self.target), "CARGO_HOME": str(self.cargo_home)},
        )
        self.env.start()
        self.addCleanup(self.env.stop)

    def digest(self) -> str:
        return checker.global_cache_digest(self.root)

    def drive(self, body, *, code: int = 0) -> int:
        calls = {"n": 0}

        def fake() -> int:
            calls["n"] += 1
            body()
            if code == 0:
                checker._success_line = LINE
                print(LINE)
            return code

        with (
            patch.object(checker, "_check_workspace", side_effect=fake),
            redirect_stdout(StringIO()),
        ):
            result = checker.check_root(self.root)
        self.calls = calls["n"]
        return result

    def test_global_digest_tracks_toolchain_root_env_and_not_terminal_decoration(self) -> None:
        first = self.digest()
        self.assertEqual(first, self.digest())
        other = self.scratch / "other"
        other.mkdir()
        # Without config paths, which also name the root, only the root label differs.
        with patch.object(checker.cargo_key, "config_candidates", return_value=[]):
            self.assertNotEqual(
                checker.global_cache_digest(self.root), checker.global_cache_digest(other)
            )
        with patch.dict(os.environ, {"RUSTFLAGS": "--cfg e6_cache_probe"}):
            self.assertNotEqual(first, self.digest())
        with patch.dict(os.environ, {"CARGO_ENCODED_RUSTFLAGS": "e6"}):
            self.assertNotEqual(first, self.digest())
        with patch.dict(os.environ, {"CARGO_BUILD_TARGET": "x86_64-unknown-linux-gnu"}):
            self.assertNotEqual(first, self.digest())
        with patch.dict(
            os.environ, {"CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS": "-Dwarnings"}
        ):
            self.assertNotEqual(first, self.digest())
        with patch.dict(os.environ, {"CARGO_TERM_COLOR": "always"}):
            self.assertEqual(first, self.digest())
        real = subprocess.check_output

        for tool in ("rustc", "cargo"):

            def fake(command, tool=tool, **kwargs):
                if command[:1] == [tool]:
                    return f"{tool} 0.0.0-e6\n"
                return real(command, **kwargs)

            with (
                self.subTest(tool=tool),
                patch.object(subprocess, "check_output", side_effect=fake),
            ):
                self.assertNotEqual(first, self.digest())

    def test_checker_source_bytes_change_the_digest(self) -> None:
        source = self.scratch / "check_workspace.py"
        source.write_text("# one\n", encoding="utf-8")
        with patch.object(checker, "_CHECKER_SOURCES", (source,)):
            before = self.digest()
            source.write_text("# two\n", encoding="utf-8")
            self.assertNotEqual(before, self.digest())

    def test_a_walk_that_lists_other_entries_than_the_directory_is_unstable(self) -> None:
        walked = self.root / "walked"
        walked.mkdir()
        (walked / "kept.rs").write_text("", encoding="utf-8")
        with checker.recording_reads() as log:
            checker._note_dir(str(walked), ["kept.rs"])
            self.assertNotEqual(log.dirs[str(walked)], checker._UNSTABLE_DIR)
            checker._note_dir(str(walked), ["kept.rs", "vanished.rs"])
            self.assertEqual(log.dirs[str(walked)], checker._UNSTABLE_DIR)
            checker._note_dir(str(walked), ["kept.rs"])
            self.assertEqual(log.dirs[str(walked)], checker._UNSTABLE_DIR)

    def test_ancestor_and_cargo_home_config_change_the_digest(self) -> None:
        before = self.digest()
        home_config = self.cargo_home / "config.toml"
        home_config.write_text("[net]\noffline = true\n", encoding="utf-8")
        self.assertNotEqual(before, self.digest())
        home_config.unlink()
        self.assertEqual(before, self.digest())
        ancestor = self.root.parent / ".cargo" / "config.toml"
        ancestor.parent.mkdir()
        ancestor.write_text("[build]\n", encoding="utf-8")
        self.assertNotEqual(before, self.digest())

    def test_symbol_tool_bytes_change_the_digest(self) -> None:
        tool = self.scratch / "nm"
        tool.write_bytes(b"nm-a")
        tool.chmod(0o755)

        def which(name: str) -> str | None:
            return str(tool) if name == "nm" else None

        with patch.object(shutil, "which", side_effect=which):
            before = self.digest()
            tool.write_bytes(b"nm-b")
            self.assertNotEqual(before, self.digest())

    def test_a_read_file_mtime_and_hygiene_content_miss(self) -> None:
        cargo = self.root / "Cargo.toml"
        hygiene = self.root / "docs/local/hygiene-private-names.txt"

        def read() -> None:
            cargo.read_text(encoding="utf-8")
            if hygiene.exists():
                hygiene.read_bytes()

        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 0)
        cargo.write_text(cargo.read_text(encoding="utf-8") + "\n", encoding="utf-8")
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        stamp = cargo.stat().st_mtime + 50
        os.utime(cargo, (stamp, stamp))
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        hygiene.parent.mkdir(parents=True)
        self.assertEqual(self.drive(read), 0)
        before_calls = self.calls
        hygiene.write_text("secret-name\n", encoding="utf-8")
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(before_calls, 0)
        self.assertEqual(self.calls, 1)

    def test_an_ignored_rust_source_the_checker_reads_misses(self) -> None:
        source = self.root / "crates/demo/src"
        source.mkdir(parents=True)
        (source / "lib.rs").write_text("mod e6probe;\n", encoding="utf-8")
        probe = source / "e6probe.rs"
        probe.write_text("fn kept() {}\n", encoding="utf-8")
        (self.root / ".git/info/exclude").write_text(
            "crates/demo/src/e6probe.rs\n", encoding="utf-8"
        )
        ignored = subprocess.run(
            ["git", "-C", str(self.root), "check-ignore", "-q", "crates/demo/src/e6probe.rs"],
            check=False,
        )
        self.assertEqual(ignored.returncode, 0)

        def walk() -> None:
            for current, _dirnames, names in os.walk(source):
                for name in names:
                    if name.endswith(".rs"):
                        (Path(current) / name).read_text(encoding="utf-8")

        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(self.calls, 1)
        probe.write_text("fn kept() { let _ = std::env::var(\"E6\"); }\n", encoding="utf-8")
        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(self.calls, 1)

    def test_replacing_a_file_with_a_symlink_of_the_same_bytes_and_mtime_misses(self) -> None:
        listed = self.root / "listed.txt"
        listed.write_text("same", encoding="utf-8")
        target = self.root / "same-target.txt"
        shutil.copy2(listed, target)
        same_mtime(target, listed)

        def read() -> None:
            listed.read_bytes()

        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 0)
        # Following the link would see the same type, size, mtime and bytes.
        stamp = listed.stat()
        listed.unlink()
        listed.symlink_to(target.name)
        os.utime(listed, ns=(stamp.st_atime_ns, stamp.st_mtime_ns), follow_symlinks=False)
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)

    def test_retargeting_a_symlink_between_identical_files_misses(self) -> None:
        first = self.root / "target-a.txt"
        second = self.root / "target-b.txt"
        first.write_text("same", encoding="utf-8")
        shutil.copy2(first, second)
        same_mtime(second, first)
        link = self.root / "link.txt"
        link.symlink_to(first.name)

        def read() -> None:
            link.read_bytes()

        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        stamp = link.lstat()
        link.unlink()
        # Same link length and lstat times: only the readlink text differs.
        link.symlink_to(second.name)
        os.utime(link, ns=(stamp.st_atime_ns, stamp.st_mtime_ns), follow_symlinks=False)
        self.assertEqual(link.lstat().st_size, stamp.st_size)
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)

    def test_a_link_chain_rewritten_past_its_first_hop_misses(self) -> None:
        final = self.root / "final.txt"
        final.write_text("one", encoding="utf-8")
        middle = self.root / "middle.txt"
        middle.symlink_to(final.name)
        link = self.root / "link.txt"
        link.symlink_to(middle.name)

        def read() -> None:
            link.read_bytes()

        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        stamp = final.stat()
        final.write_text("two", encoding="utf-8")
        os.utime(final, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)

    def test_same_size_same_mtime_different_bytes_misses(self) -> None:
        listed = self.root / "listed.txt"
        listed.write_text("aaaa", encoding="utf-8")

        def read() -> None:
            listed.read_bytes()

        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        stamp = listed.stat()
        listed.write_text("bbbb", encoding="utf-8")
        os.utime(listed, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.assertEqual(listed.stat().st_mtime_ns, stamp.st_mtime_ns)
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)

    def test_each_reader_records_a_missing_path_it_tried(self) -> None:
        readers = {
            "read_bytes": lambda path: path.read_bytes(),
            "read_text": lambda path: path.read_text(encoding="utf-8"),
            "open": lambda path: path.open("rb").close(),
        }
        for name, reader in readers.items():
            with self.subTest(reader=name):
                checker.workspace_cache_path(self.root).unlink(missing_ok=True)
                missing = self.root / f"absent-{name}.txt"

                def read(missing: Path = missing, reader=reader) -> None:
                    try:
                        reader(missing)
                    except FileNotFoundError:
                        pass

                self.assertEqual(self.drive(read), 0)
                self.assertEqual(self.calls, 1)
                self.assertEqual(self.drive(read), 0)
                self.assertEqual(self.calls, 0)
                missing.write_text("later\n", encoding="utf-8")
                self.assertEqual(self.drive(read), 0)
                self.assertEqual(self.calls, 1)

    def test_each_reader_records_content(self) -> None:
        readers = {
            "read_bytes": lambda path: path.read_bytes(),
            "read_text": lambda path: path.read_text(encoding="utf-8"),
            "open": lambda path: path.open("rb").close(),
        }
        for name, reader in readers.items():
            with self.subTest(reader=name):
                checker.workspace_cache_path(self.root).unlink(missing_ok=True)
                listed = self.root / f"listed-{name}.txt"
                listed.write_text("aaaa", encoding="utf-8")

                def read(listed: Path = listed, reader=reader) -> None:
                    reader(listed)

                self.assertEqual(self.drive(read), 0)
                self.assertEqual(self.calls, 1)
                listed.write_text("bbbb", encoding="utf-8")
                self.assertEqual(self.drive(read), 0)
                self.assertEqual(self.calls, 1)

    def test_a_walked_directory_entry_misses(self) -> None:
        extra = self.root / "walked"
        extra.mkdir()

        def walk() -> None:
            for _current, _dirnames, _names in os.walk(extra):
                pass

        self.assertEqual(self.drive(walk), 0)
        (extra / "new.rs").write_text("fn new() {}\n", encoding="utf-8")
        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(self.calls, 1)

    def test_a_nested_walked_directory_entry_misses(self) -> None:
        # Only the per-directory record of the walk sees a file added below the top.
        nested = self.root / "walked" / "nested"
        nested.mkdir(parents=True)

        def walk() -> None:
            for _current, _dirnames, _names in os.walk(nested.parent):
                pass

        self.assertEqual(self.drive(walk), 0)
        (nested / "new.rs").write_text("fn new() {}\n", encoding="utf-8")
        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(self.calls, 1)

    def test_a_listed_files_result_misses_when_a_path_appears(self) -> None:
        # A stub listing touches no file, so only the stored result can miss.
        listing = ["Cargo.toml"]

        def listed_files(root: Path) -> list[str]:
            del root
            return list(listing)

        def read() -> None:
            checker.vcs_listing.listed_files(self.root)

        with patch.object(checker.vcs_listing, "listed_files", side_effect=listed_files):
            self.assertEqual(self.drive(read), 0)
            self.assertEqual(self.calls, 1)
            self.assertEqual(self.drive(read), 0)
            self.assertEqual(self.calls, 0)
            listing.append("added.rs")
            self.assertEqual(self.drive(read), 0)
            self.assertEqual(self.calls, 1)

    def test_a_real_git_listing_misses_when_a_path_appears(self) -> None:
        def listing() -> None:
            checker.vcs_listing.listed_files(self.root)

        self.assertEqual(self.drive(listing), 0)
        self.assertEqual(self.calls, 1)
        added = self.root / "added.txt"
        added.write_text("added\n", encoding="utf-8")
        git(self.root, "add", "-A")
        self.assertEqual(self.drive(listing), 0)
        self.assertEqual(self.calls, 1)

    def test_existence_probe_result_and_path_label_miss(self) -> None:
        missing = self.root / "missing.txt"

        def probe() -> None:
            missing.exists()

        self.assertEqual(self.drive(probe), 0)
        self.assertEqual(self.calls, 1)
        missing.write_text("now", encoding="utf-8")
        self.assertEqual(self.drive(probe), 0)
        self.assertEqual(self.calls, 1)
        left = self.root / "left.txt"
        right = self.root / "right.txt"
        left.write_text("same", encoding="utf-8")
        right.write_text("same", encoding="utf-8")

        def observe(path: Path) -> dict[str, object]:
            with checker.recording_reads() as log:
                path.read_bytes()
                return checker._observation(log)

        self.assertNotEqual(observe(left)["files"], observe(right)["files"])

    def test_release_artifact_content_is_part_of_the_observation(self) -> None:
        artifact = self.target / "release" / "plak"
        artifact.parent.mkdir(parents=True)
        artifact.write_bytes(b"one")

        def read() -> None:
            for path in checker.built_product_artifacts(self.root).values():
                path.read_bytes()

        self.assertEqual(self.drive(read), 0)
        self.assertEqual(checker.built_product_artifacts(self.root)["plak"], artifact)
        artifact.write_bytes(b"two")
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 1)
        ignored = self.root / "target" / "release" / "plak"
        ignored.parent.mkdir(parents=True)
        ignored.write_bytes(b"repo-local target is not the gate target")
        self.assertEqual(self.drive(read), 0)
        self.assertEqual(self.calls, 0)

    def test_without_a_target_dir_the_repo_release_dir_is_the_product(self) -> None:
        with patch.dict(os.environ, {"CARGO_TARGET_DIR": ""}):
            artifact = self.root / "target" / "release" / "plak"

            def read() -> None:
                for path in checker.built_product_artifacts(self.root).values():
                    path.read_bytes()

            self.assertEqual(self.drive(read), 0)
            self.assertEqual(self.calls, 1)
            artifact.parent.mkdir(parents=True)
            artifact.write_bytes(b"product")
            self.assertEqual(self.drive(read), 0)
            self.assertEqual(self.calls, 1)
            self.assertEqual(checker.built_product_artifacts(self.root)["plak"], artifact)

    def test_the_hygiene_names_file_is_in_the_global_digest(self) -> None:
        hygiene = self.root / checker.HYGIENE_PRIVATE_NAMES_FILE
        before = self.digest()
        self.assertEqual(self.drive(lambda: None), 0)
        self.assertEqual(self.calls, 1)
        hygiene.parent.mkdir(parents=True)
        hygiene.write_text("private-name\n", encoding="utf-8")
        self.assertNotEqual(before, self.digest())
        self.assertEqual(self.drive(lambda: None), 0)
        self.assertEqual(self.calls, 1)
        changed = self.digest()
        hygiene.write_text("other-name\n", encoding="utf-8")
        self.assertNotEqual(changed, self.digest())

    def test_the_real_hygiene_loader_records_an_absent_names_file(self) -> None:
        def load() -> None:
            checker.load_hygiene_private_names(self.root)

        hygiene = self.root / checker.HYGIENE_PRIVATE_NAMES_FILE
        with patch.object(checker, "global_cache_digest", return_value="0" * 64):
            self.assertEqual(self.drive(load), 0)
            self.assertEqual(self.calls, 1)
            self.assertEqual(self.drive(load), 0)
            self.assertEqual(self.calls, 0)
            hygiene.parent.mkdir(parents=True)
            hygiene.write_text("private-name\n", encoding="utf-8")
            self.assertEqual(self.drive(load), 0)
            self.assertEqual(self.calls, 1)

    def test_credentials_are_not_keyed_and_no_receipt_holds_an_env_value(self) -> None:
        before = self.digest()
        secrets = {
            "CARGO_REGISTRY_TOKEN": "e6-registry-token-value",
            "CARGO_REGISTRIES_MIRROR_TOKEN": "e6-mirror-token-value",
            "CARGO_REGISTRY_SECRET_KEY": "e6-secret-key-value",
            "CARGO_REGISTRY_CREDENTIAL_PROVIDER": "e6-provider-value",
        }
        with patch.dict(os.environ, secrets):
            names = {name for name, _value in checker.cargo_key.tracked_env()}
            self.assertFalse(names & set(secrets))
            self.assertEqual(before, self.digest())
            with patch.dict(os.environ, {"RUSTFLAGS": "--cfg e6_raw_env_value"}):
                self.assertEqual(self.drive(lambda: None), 0)
                self.assertEqual(self.calls, 1)
        receipt = checker.workspace_cache_path(self.root).read_bytes()
        for value in [*secrets.values(), "e6_raw_env_value"]:
            self.assertNotIn(value.encode(), receipt)

    def test_a_cargo_environment_change_during_the_run_writes_no_receipt(self) -> None:
        def change() -> None:
            os.environ["RUSTFLAGS"] = "--cfg e6_during"

        with patch.dict(os.environ, {"RUSTFLAGS": ""}):
            self.assertEqual(self.drive(change), 0)
        self.assertFalse(checker.workspace_cache_path(self.root).exists())

    def test_a_file_read_twice_with_different_bytes_writes_no_receipt(self) -> None:
        listed = self.root / "listed.txt"
        listed.write_text("one", encoding="utf-8")

        def change_between_reads() -> None:
            listed.read_bytes()
            with open(listed, "w", encoding="utf-8") as stream:
                stream.write("two")
            listed.read_bytes()

        self.assertEqual(self.drive(change_between_reads), 0)
        self.assertFalse(checker.workspace_cache_path(self.root).exists())

    def test_an_unreadable_walk_writes_no_receipt(self) -> None:
        missing = self.root / "never-created"
        errors: list[OSError] = []

        def walk() -> None:
            for _entry in os.walk(missing, onerror=errors.append):
                pass

        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(len(errors), 1)
        self.assertFalse(checker.workspace_cache_path(self.root).exists())

    def test_a_walk_that_cannot_enter_a_subdirectory_writes_no_receipt(self) -> None:
        walked = self.root / "walked"
        locked = walked / "locked"
        locked.mkdir(parents=True)
        locked.chmod(0)
        self.addCleanup(locked.chmod, 0o700)
        if os.access(locked, os.R_OK):
            self.skipTest("permissions do not restrict this user")

        def walk() -> None:
            for _entry in os.walk(walked):
                pass

        self.assertEqual(self.drive(walk), 0)
        self.assertFalse(checker.workspace_cache_path(self.root).exists())

    def test_a_walked_entry_that_changes_type_misses(self) -> None:
        walked = self.root / "walked"
        walked.mkdir()
        entry = walked / "entry"
        entry.write_text("", encoding="utf-8")

        def walk() -> None:
            for _current, dirnames, _names in os.walk(walked):
                dirnames[:] = []

        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(self.calls, 1)
        stamp = walked.stat()
        entry.unlink()
        entry.mkdir()
        os.utime(walked, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.assertEqual(self.drive(walk), 0)
        self.assertEqual(self.calls, 1)

    def test_identity_keeps_type_and_size_apart(self) -> None:
        # Same size and mtime: only the file type differs.
        fifo = self.scratch / "fifo"
        os.mkfifo(fifo)
        here = Path.cwd()
        os.chdir(self.scratch)
        try:
            import socket

            server = socket.socket(socket.AF_UNIX)
            self.addCleanup(server.close)
            server.bind("sock")
        finally:
            os.chdir(here)
        sock = self.scratch / "sock"
        same_mtime(sock, fifo)
        one = checker.cargo_key.path_identity(fifo)
        two = checker.cargo_key.path_identity(sock)
        self.assertEqual(one["size"], two["size"])
        self.assertNotEqual(one, two)
        # Same type and mtime: only the directory size differs.
        grown = self.scratch / "grown"
        grown.mkdir()
        before = checker.cargo_key.path_identity(grown)
        stamp = grown.stat()
        for index in range(400):
            (grown / f"entry-with-a-long-name-{index:04}").mkdir()
        os.utime(grown, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        after = checker.cargo_key.path_identity(grown)
        if before["size"] == after["size"]:
            self.skipTest("this filesystem does not report directory sizes")
        self.assertNotEqual(before, after)

    def test_replay_refuses_an_operation_outside_the_probe_kinds(self) -> None:
        victim = self.root / "victim.txt"
        victim.write_text("kept", encoding="utf-8")
        forged = {
            "files": {},
            "dirs": {},
            "probes": {f"unlink\0{victim}": {"result": False, "identity": {"state": "absent"}}},
            "listings": [],
        }
        with self.assertRaises(checker._CacheDoubt):
            checker.replay_observation(forged, self.root)
        digest = self.digest()
        checker.write_cached_success(self.root, digest, forged, LINE)
        self.assertIsNone(checker.read_cached_success(self.root, digest))
        self.assertTrue(victim.is_file())

    def test_a_file_changed_during_the_run_writes_no_receipt(self) -> None:
        cargo = self.root / "Cargo.toml"

        def mutate() -> None:
            cargo.read_text(encoding="utf-8")
            cargo.write_text(cargo.read_text(encoding="utf-8") + "\n# during\n", encoding="utf-8")

        self.assertEqual(self.drive(mutate), 0)
        self.assertFalse(checker.workspace_cache_path(self.root).exists())
        self.assertEqual(self.drive(lambda: cargo.read_text(encoding="utf-8")), 0)
        self.assertEqual(self.calls, 1)

    def test_miss_corrupt_and_mismatched_receipts_recompute(self) -> None:
        digest = self.digest()
        observation: dict[str, object] = {"files": {}, "dirs": {}, "probes": {}, "listings": []}
        self.assertIsNone(checker.read_cached_success(self.root, digest))
        checker.write_cached_success(self.root, digest, observation, LINE)
        self.assertEqual(checker.read_cached_success(self.root, digest), LINE)
        path = checker.workspace_cache_path(self.root)
        path.write_text("{", encoding="utf-8")
        self.assertIsNone(checker.read_cached_success(self.root, digest))
        path.write_text("[]", encoding="utf-8")
        self.assertIsNone(checker.read_cached_success(self.root, digest))
        checker.write_cached_success(self.root, digest, observation, LINE)
        payload = json.loads(path.read_text(encoding="utf-8"))
        payload["line"] = LINE + " tampered"
        path.write_text(json.dumps(payload), encoding="utf-8")
        self.assertIsNone(checker.read_cached_success(self.root, digest))
        checker.write_cached_success(self.root, digest, observation, LINE)
        self.assertIsNone(checker.read_cached_success(self.root, "0" * 64))

    def test_a_hit_skips_the_checker(self) -> None:
        digest = self.digest()
        checker.write_cached_success(
            self.root,
            digest,
            {"files": {}, "dirs": {}, "probes": {}, "listings": []},
            LINE,
        )
        output = StringIO()
        with (
            patch.object(checker, "_check_workspace", side_effect=AssertionError("checker ran")),
            redirect_stdout(output),
        ):
            code = checker.check_root(self.root)
        self.assertEqual(code, 0)
        self.assertEqual(output.getvalue(), LINE + "\n")

    def test_a_failure_is_not_cached(self) -> None:
        self.assertEqual(self.drive(lambda: None, code=1), 1)
        self.assertEqual(self.drive(lambda: None, code=1), 1)
        self.assertEqual(self.calls, 1)
        self.assertFalse(checker.workspace_cache_path(self.root).exists())

    def test_doubt_recomputes_instead_of_reading_a_receipt(self) -> None:
        digest = self.digest()
        checker.write_cached_success(
            self.root,
            digest,
            {"files": {}, "dirs": {}, "probes": {}, "listings": []},
            LINE,
        )

        def boom(root: Path = checker.ROOT) -> str:
            del root
            raise checker._CacheDoubt("unreadable input")

        self.assertEqual(self.drive(lambda: None), 0)
        self.assertEqual(self.calls, 0)
        with (
            patch.object(checker, "global_cache_digest", side_effect=boom),
            patch.object(checker, "_check_workspace", return_value=1),
        ):
            self.assertEqual(checker.check_root(self.root), 1)


if __name__ == "__main__":
    unittest.main()
