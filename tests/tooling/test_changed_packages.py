"""`just fast` maps working-tree paths to the packages it lints and tests."""

import importlib.util
import io
import sys
import os
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "changed_packages", ROOT / "tools/gate/changed_packages.py"
)
assert SPEC and SPEC.loader
CHANGED = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHANGED
SPEC.loader.exec_module(CHANGED)
from run_tooling_tests import TREE_READERS, selected_modules  # noqa: E402

DIRS = {
    "bin/plakd": "plakd",
    "crates/core/model": "plakous-model",
    "crates/runtimes/py-worker": "plakous-py-worker",
}


class TouchedPackagesTests(unittest.TestCase):
    def test_reverse_scope_includes_optional_target_and_dev_edges_and_cycles(self):
        def package(name, *dependencies):
            return {
                "name": name,
                "dependencies": [{"name": dependency} for dependency in dependencies],
            }

        packages = [
            package("core"),
            package("leaf", "external"),
            package("host", "core", "harness"),
            package("product", "host"),
            package("harness", "host"),
        ]
        packages[2]['dependencies'][0].update(optional=True, target='cfg(windows)', kind='dev')
        packages[4]['dependencies'][0]['kind'] = 'build'
        self.assertEqual(
            CHANGED.reverse_dependencies(["core"], packages), ["core", "harness", "host", "product"]
        )
        self.assertEqual(CHANGED.reverse_dependencies(["leaf"], packages), ["leaf"])
        self.assertEqual(CHANGED.reverse_dependencies([], packages), [])
        with self.assertRaisesRegex(ValueError, "unknown workspace packages: typo"):
            CHANGED.reverse_dependencies(["typo"], packages)

    def test_ignored_proptest_regressions_are_package_inputs(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(os.environ, {"PATH": "/usr/bin:/bin"}, clear=True),
        ):
            root = Path(temporary)

            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True)

            git("init", "-q")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            (root / ".gitignore").write_text("proptest-regressions/\nignored.rs\n")
            git("add", ".")
            git("commit", "-qm", "base")
            regression = "crates/core/model/proptest-regressions/canonical.txt"
            (root / regression).parent.mkdir(parents=True)
            (root / regression).write_text("seed")
            (root / "ignored.rs").write_text("ignored")
            paths = CHANGED.changed_paths(root)
            self.assertEqual(paths, [regression])
            self.assertEqual(CHANGED.touched_packages(paths, DIRS), ["plakous-model"])
            self.assertEqual(CHANGED.batch_changes(root, "HEAD"), [])
            git("add", "-f", regression)
            self.assertEqual(CHANGED.batch_changes(root, "HEAD"), [regression])

    def test_unbased_batches_ignore_dirt_and_never_fall_back_to_fast_base(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(os.environ, {"PATH": "/usr/bin:/bin"}, clear=True),
        ):
            root = Path(temporary)

            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True)

            git("init", "-q")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            (root / ".gitignore").write_text("proptest-regressions/\n")
            git("add", ".")
            git("commit", "-qm", "base")
            seed = root / "crates/core/model/proptest-regressions/seed.txt"
            seed.parent.mkdir(parents=True)
            seed.write_text("ignored seed")
            (root / "notes.txt").write_text("untracked note")
            for fast_env in ({}, {"PLAK_FAST_BASE": "nonexistent-revision"}):
                with (
                    self.subTest(fast_env=fast_env),
                    patch.dict(os.environ, fast_env),
                    patch.object(CHANGED, "REPO_ROOT", root),
                    redirect_stdout(io.StringIO()) as output,
                ):
                    self.assertEqual(CHANGED.main(["--batch-filter"]), 0)
                    self.assertEqual(output.getvalue().strip(), "all()")
                    self.assertEqual(CHANGED.batch_changes(root, "HEAD"), ["notes.txt"])
            with patch.dict(os.environ, {"PLAK_FAST_BASE": "nonexistent-revision"}):
                with self.assertRaises(subprocess.CalledProcessError):
                    CHANGED.changed_paths(root)

    def touched(self, *paths):
        return CHANGED.touched_packages(list(paths), DIRS)

    def test_a_file_belongs_to_its_innermost_package(self):
        self.assertEqual(
            self.touched(
                "crates/core/model/src/lib.rs",
                "bin/plakd/tests/goldens/a.frame",
                "crates/core/model/Cargo.toml",
            ),
            ["plakd", "plakous-model"],
        )

    def test_sibling_directory_prefixes_do_not_match(self):
        self.assertEqual(
            self.touched("crates/runtimes/py-worker/src/cell.rs"), ["plakous-py-worker"]
        )
        self.assertEqual(self.touched("crates/core/model-extra/src/lib.rs"), [])

    def test_files_outside_every_package_touch_nothing(self):
        self.assertEqual(
            self.touched(
                "docs/spec/05-testing-and-evidence.md", "tools/gate/check_features.py", "Justfile"
            ),
            [],
        )

    def test_files_a_package_reads_outside_it_select_that_package(self):
        for path in (
            "docs/spec/07-config-reference.md",
            "docs/reference-config/config.toml",
            "docs/reference-config/agents/worker.toml",
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    self.touched(path, "crates/core/model/src/lib.rs"), ["plakd", "plakous-model"]
                )
        self.assertEqual(
            self.touched(
                "docs/reference-configs/config.toml",
                "docs/spec/05-testing-and-evidence.md",
            ),
            [],
        )

    def test_workspace_inputs_select_the_whole_workspace(self):
        for path in (
            "Cargo.toml",
            "Cargo.lock",
            ".config/nextest.toml",
            ".cargo/config.toml",
            "rust-toolchain.toml",
            ".clippy.toml",
            ".rustfmt.toml",
            "tools/gate/test-env.sh",
            "tools/gate/test-processes.py",
            "tools/gate/test-tmpdir.sh",
            "tools/gate/dev-tools.pins",
        ):
            with self.subTest(path=path):
                self.assertEqual(self.touched("bin/plakd/src/lib.rs", path), ["--workspace"])

    def test_vendored_patch_sources_select_the_whole_workspace(self):
        self.assertEqual(self.touched("vendor/unclassified/src/lib.rs"), ["--workspace"])

    def test_loose_test_fixtures_select_all_but_fixture_crates_keep_their_owners(self):
        self.assertEqual(self.touched("tests/fixtures/config-scenarios/home.toml"), ["--workspace"])
        self.assertEqual(
            self.touched_in(
                {"tests/fixtures/mcp-fake-server": "plakous-mcp-fake-server"},
                "tests/fixtures/mcp-fake-server/src/main.rs",
            ),
            ["plakous-mcp-fake-server"],
        )

    def test_runtime_edges_add_consumers_of_changed_runtime_binaries(self):
        packages = [
            {"name": "dependency", "dependencies": []},
            {
                "name": "plakous-js-runtime",
                "dependencies": [{"name": "dependency"}],
            },
            *(
                {"name": name, "dependencies": []}
                for name in (
                    "plakous-py-worker",
                    "plakous-cell",
                    "plakd",
                    "plakous-mcp-fake-server",
                )
            ),
        ]
        crash_harness = "package(=plakous-offline-crash-harness)"
        cell_consumers = (
            f"package(=plakd) | package(=plakous) | package(=plakous-hosted) | {crash_harness}"
        )
        for producer, consumers in (
            ("dependency", cell_consumers),
            ("plakous-js-runtime", cell_consumers),
            ("plakous-py-worker", cell_consumers),
            ("plakous-cell", cell_consumers),
            ("plakd", crash_harness),
            (
                "plakous-mcp-fake-server",
                f"package(=plakd) | package(=plakous-hosted) | package(=plakous-mcp) | {crash_harness}",
            ),
        ):
            with self.subTest(producer=producer):
                self.assertEqual(
                    CHANGED.batch_filter([producer], packages), f"rdeps(={producer}) | {consumers}"
                )
        self.assertEqual(CHANGED.batch_filter(["--workspace"], packages), "all()")
        self.assertEqual(CHANGED.batch_filter([], packages), "none()")

        scoped = selected_modules(["bin/plakd/src/lib.rs", "toolshed/unrelated.py"])
        self.assertEqual({module.stem for module in scoped}, TREE_READERS)
        full = selected_modules(None)
        self.assertGreater(len(full), len(scoped))
        for path in (
            "tools/gate/changed_packages.py",
            "tests/tooling/test_changed_packages.py",
            "tests/live/driver.py",
            "eval/adapter.py",
            "Justfile",
            ".config/nextest.toml",
            "tests/fixtures/config-scenarios/fixture.toml",
        ):
            with self.subTest(path=path):
                self.assertEqual(selected_modules([path]), full)

    def test_the_real_workspace_maps_every_manifest_to_its_package(self):
        dirs = CHANGED.package_dirs()
        self.assertEqual(dirs["bin/plakd"], "plakd")
        for directory, name in dirs.items():
            expected = ["--workspace"] if directory.startswith("vendor/") else [name]
            self.assertEqual(self.touched_in(dirs, f"{directory}/Cargo.toml"), expected)

    @staticmethod
    def touched_in(dirs, path):
        return CHANGED.touched_packages([path], dirs)


if __name__ == "__main__":
    unittest.main()
