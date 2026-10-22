"""scripts/publish.sh against a fake workspace: preconditions, order, index wait.

The fixture is a three-crate cohort (`plakous-zeta` <- `plakous-mid` <- `plak`,
alphabetical order differing from dependency order) plus a repository-only
harness, in its own git repository. Real `cargo metadata` derives the order; a
`cargo` shim records `package`, `fetch` and `publish` and plays the registry by writing
the sparse index entry a real upload would produce.
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
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / f"tools/release/{name}.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


publish_plan = load("publish_plan")
publish_shape = load("publish_shape")
review_expiry = load("review_expiry")

ORDER = ["plakous-zeta", "plakous-mid", "plak"]

CARGO_SHIM = r'''
import io, json, os, sys, tarfile, tomllib
from pathlib import Path
args = sys.argv[1:]
if args[:1] == ["metadata"]:
    os.execv(os.environ["REAL_CARGO"], [os.environ["REAL_CARGO"], *args])
manifest = Path("bin/plak/Cargo.toml")
with open(os.environ["FAKE_CARGO_LOG"], "a") as log:
    log.write(json.dumps(dict(
        args=args, cwd=os.getcwd(), target_dir=os.environ.get("CARGO_TARGET_DIR"),
        offline=os.environ.get("CARGO_NET_OFFLINE"),
        token=os.environ.get("CARGO_REGISTRY_TOKEN"),
        test_hooks="test-hooks" in manifest.read_text() if manifest.exists() else None,
    )) + "\n")
if args[:1] == ["package"] and "--no-verify" in args:
    if os.environ.get("FAKE_PACKAGE_COLD") == "1" and ("--offline" in args or os.environ.get("CARGO_NET_OFFLINE") == "true"):
        sys.exit(1)
    directory = Path(os.environ["CARGO_TARGET_DIR"]) / "package"
    directory.mkdir(parents=True, exist_ok=True)
    for name in os.environ["FAKE_NAMES"].split(","):
        with tarfile.open(directory / f"{name}-0.0.2.crate", "w:gz") as archive:
            data = f"[package]\nname = '{name}'\nversion = '0.0.2'\n".encode()
            member = tarfile.TarInfo(f"{name}-0.0.2/Cargo.toml")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
if args[:1] == ["fetch"]:
    fetched = tomllib.loads(Path(args[args.index("--manifest-path") + 1]).read_text())
    assert set(fetched["patch"]["crates-io"]) == set(os.environ["FAKE_NAMES"].split(","))
    assert all(Path(value["path"]).is_dir() for value in fetched["patch"]["crates-io"].values())
    if os.environ.get("FAKE_FETCH_FAIL") == "1":
        sys.exit(2)
    if ("--offline" in args or os.environ.get("CARGO_NET_OFFLINE") == "true") and os.environ.get("FAKE_FETCH_COLD") == "1":
        sys.exit(1)
if args[:1] == ["publish"]:
    name = args[args.index("-p") + 1]
    if name not in os.environ.get("FAKE_UNINDEXED", "").split(","):
        entry = Path(os.environ["FAKE_INDEX"]) / os.environ["FAKE_INDEX_PATHS"].split(",")[
            os.environ["FAKE_NAMES"].split(",").index(name)]
        entry.parent.mkdir(parents=True, exist_ok=True)
        with entry.open("a") as stream:
            stream.write(json.dumps(dict(name=name, vers="0.0.2")) + "\n")
'''


def library(name: str, dependencies: str = "", publish: bool = True) -> str:
    return (
        f'[package]\nname = "{name}"\nversion = "0.0.2"\nedition = "2021"\n'
        + ("" if publish else "publish = false\n")
        + f"\n[dependencies]\n{dependencies}"
    )


class PublishScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        real_cargo = shutil.which("cargo")
        if real_cargo is None:
            self.skipTest("cargo is not on PATH")
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        base = Path(scratch.name)
        self.repo = base / "repo"
        self.target = base / "target"
        self.index = base / "index"
        self.log = base / "cargo.jsonl"
        for relative in ("scripts/publish.sh", "tools/release/publish_plan.py",
                         "tools/release/publish_shape.py", "tools/release/review_expiry.py",
                         "tools/release/prefetch_packages.py", "tools/checks/vcs_listing.py"):
            (self.repo / relative).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, self.repo / relative)
        # Freeze only this copied fixture's invocation, never the maintainer CLI.
        checker = self.repo / "tools/release/review_expiry.py"
        source = checker.read_text()
        assert source.count("sys.exit(main(note=args.note))") == 1
        checker.write_text(source.replace("sys.exit(main(note=args.note))", "sys.exit(main(now=1_000, note=args.note))"))
        for _, relative, constant in review_expiry.SURFACES:
            source = self.repo / "crates/providers" / relative
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text(f"const {constant}: u64 = 1_000 + 30 * 86_400;\n")
        files = {
            "Cargo.toml": (
                "[workspace]\nresolver = \"2\"\nmembers = [\"crates/zeta\", \"crates/mid\", "
                "\"bin/plak\", \"tests/harnesses/offline-crash\"]\n"
            ),
            "crates/zeta/Cargo.toml": library("plakous-zeta"),
            "crates/mid/Cargo.toml": library(
                "plakous-mid", 'plakous-zeta = { version = "0.0.2", path = "../zeta" }\n'
            ),
            "bin/plak/Cargo.toml": (
                library("plak", 'plakous-mid = { version = "0.0.2", path = "../../crates/mid" }\n'
                        + publish_shape.HARNESS_DEPENDENCY)
                + '\n[dev-dependencies]\nplakous-zeta = { version = "0.0.2", path = "../../crates/zeta" }\n'
                + "\n[features]\n" + publish_shape.TEST_HOOKS_FEATURE
            ),
            "tests/harnesses/offline-crash/Cargo.toml": library(
                publish_shape.HARNESS, publish=False
            ),
        }
        for relative, text in files.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
            source = path.parent / "src" / ("main.rs" if relative.startswith("bin/") else "lib.rs")
            if relative != "Cargo.toml":
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text("fn main() {}\n" if source.name == "main.rs" else "\n")
        bin_dir = base / "bin"
        bin_dir.mkdir()
        shim = bin_dir / "cargo"
        shim.write_text("#!" + sys.executable + "\n" + CARGO_SHIM)
        shim.chmod(0o700)
        self.git("init", "-q")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "fixture")
        self.tree = self.git("rev-parse", "HEAD^{tree}")
        (base / "tmp").mkdir()
        self.env = {
            key: value for key, value in os.environ.items()
            if key not in {"CARGO_REGISTRY_TOKEN", "CARGO_NET_OFFLINE", "PLAK_PUBLISH_TIMEOUT"}
        }
        self.env.update(
            PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", REAL_CARGO=real_cargo,
            FAKE_CARGO_LOG=str(self.log), FAKE_INDEX=str(self.index),
            FAKE_NAMES=",".join(ORDER),
            FAKE_INDEX_PATHS=",".join(publish_plan.index_path(name) for name in ORDER),
            CARGO_TARGET_DIR=str(self.target), TMPDIR=str(base / "tmp"),
            PLAK_PUBLISH_INDEX=self.index.as_uri(),
        )

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def record_evidence(self, tree: str | None = None) -> None:
        evidence = self.target / "gate-evidence" / "full"
        evidence.parent.mkdir(parents=True, exist_ok=True)
        evidence.write_text(f"{tree or self.tree}\n")

    def publish(self, *args: str) -> tuple[subprocess.CompletedProcess[str], list[dict]]:
        result = subprocess.run(
            ["bash", str(self.repo / "scripts/publish.sh"), *args],
            cwd=self.repo, env=self.env, capture_output=True, text=True, timeout=60,
        )
        lines = self.log.read_text().splitlines() if self.log.exists() else []
        self.log.unlink(missing_ok=True)
        all_calls = [json.loads(line) for line in lines]
        self.prefetch_calls = [call for call in all_calls if call["args"][0] == "fetch"
                               or (call["args"][0] == "package" and "--no-verify" in call["args"])]
        return result, [call for call in all_calls if call not in self.prefetch_calls]

    def index_entry(self, name: str) -> None:
        entry = self.index / publish_plan.index_path(name)
        entry.parent.mkdir(parents=True, exist_ok=True)
        entry.write_text(json.dumps(dict(name=name, vers="0.0.2")) + "\n")

    def assert_packaged_in_order(self, call: dict) -> None:
        self.assertEqual(
            call["args"],
            ["package", "--locked", *[arg for name in ORDER for arg in ("-p", name)]],
        )
        self.assertNotEqual(Path(call["cwd"]).resolve(), self.repo.resolve())
        self.assertIs(call["test_hooks"], False)
        self.assertEqual(call["target_dir"], str(self.target / "package"))

    def test_dry_run_refuses_a_dirty_tree(self) -> None:
        self.record_evidence()
        (self.repo / "untracked.txt").write_text("x")
        result, calls = self.publish()
        self.assertEqual(result.returncode, 1)
        self.assertIn("working tree is dirty", result.stderr)
        self.assertEqual(calls, [])

    def test_dry_run_refuses_absent_or_stale_full_evidence(self) -> None:
        for evidence in (None, "0" * 40):
            with self.subTest(evidence=evidence):
                if evidence is not None:
                    self.record_evidence(evidence)
                result, calls = self.publish()
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"no passing `just full` evidence for tree {self.tree}", result.stderr)
                self.assertEqual(calls, [])

    def test_dry_run_packages_the_published_shape_in_dependency_order(self) -> None:
        self.record_evidence()
        result, calls = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assert_packaged_in_order(calls[0])
        self.assertIn("nothing uploaded", result.stdout)

    def test_gate_mode_needs_no_clean_tree_or_evidence(self) -> None:
        (self.repo / "untracked.txt").write_text("x")
        self.env["CARGO_NET_OFFLINE"] = "true"
        result, calls = self.publish("--gate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assert_packaged_in_order(calls[0])
        self.assertEqual(calls[0]["offline"], "true")

    def test_publication_refuses_expiring_pins_before_packaging(self) -> None:
        source = self.repo / "crates/providers/openai/src/oauth_pins.rs"
        source.write_text("const expires_at_unix: u64 = 1_000 + 21 * 86_400;\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "expiry boundary")
        self.record_evidence(self.git("rev-parse", "HEAD^{tree}"))
        self.env["CARGO_REGISTRY_TOKEN"] = "fixture-token"
        for mode in [(), ("--execute",)]:
            with self.subTest(mode=mode):
                result, calls = self.publish(*mode)
                self.assertEqual(result.returncode, 1)
                self.assertIn("Codex OAuth review expires", result.stderr)
                self.assertIn("more than 21 days remaining", result.stderr)
                self.assertEqual(calls, [])
        result, calls = self.publish("--gate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("21 days remaining (informational)", result.stdout)
        self.assertEqual(len(calls), 1)

    def test_verification_graph_prefetch_is_offline_first(self) -> None:
        result, calls = self.publish("--gate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.prefetch_calls), 4)
        self.assertTrue(all("--offline" in call["args"] or call["offline"] == "true"
                            for call in self.prefetch_calls))
        self.assertEqual(calls[0]["offline"], "true")

    def test_cold_archive_graph_fetches_online_then_verifies_offline(self) -> None:
        self.env.update(FAKE_FETCH_COLD="1", FAKE_PACKAGE_COLD="1")
        result, calls = self.publish("--gate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.prefetch_calls), 8)
        for offline, online in zip(self.prefetch_calls[::2], self.prefetch_calls[1::2]):
            self.assertTrue("--offline" in offline["args"] or offline["offline"] == "true")
            self.assertIsNone(online["offline"])
        self.assertEqual(calls[0]["offline"], "true")

    def test_explicit_offline_mode_does_not_get_overridden_by_prefetch(self) -> None:
        self.env.update(FAKE_FETCH_COLD="1", CARGO_NET_OFFLINE="true")
        result, calls = self.publish("--gate")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertTrue(all(call["offline"] == "true" for call in self.prefetch_calls))

    def test_prefetch_failure_never_starts_verification(self) -> None:
        self.env["FAKE_FETCH_FAIL"] = "1"
        result, calls = self.publish("--gate")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertEqual(len(self.prefetch_calls), 3)

    def test_packaging_drops_unpacked_cohort_copies_from_earlier_runs(self) -> None:
        # Cargo reuses an unpacked name-version directory, so a stale copy of a
        # cohort crate would be verified instead of the tree being packaged.
        cargo_home = self.repo.parent / "cargo-home"
        src = cargo_home / "registry" / "src"
        stale = [src / "-overlay" / f"{name}-0.0.2" for name in ORDER]
        stale.append(src / "index.crates.io-x" / "plakous-mid-0.0.2")
        kept = [src / "-overlay" / "serde-1.0.0", src / "-overlay" / "plakous-mid-0.0.3"]
        for directory in (*stale, *kept):
            directory.mkdir(parents=True)
            (directory / ".cargo-ok").write_text("ok")
        fingerprints = self.target / "package" / "debug" / ".fingerprint"
        stale_builds = [fingerprints / f"{name}-0123abcd" for name in ORDER]
        kept_builds = [fingerprints / "serde-0123abcd"]
        for directory in (*stale_builds, *kept_builds):
            directory.mkdir(parents=True)
        self.env["CARGO_HOME"] = str(cargo_home)
        result, calls = self.publish("--gate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertEqual([path for path in (*stale, *stale_builds) if path.exists()], [])
        self.assertEqual([path for path in (*kept, *kept_builds) if not path.exists()], [])

    def test_execute_needs_a_registry_token(self) -> None:
        self.record_evidence()
        result, calls = self.publish("--execute")
        self.assertEqual(result.returncode, 1)
        self.assertIn("CARGO_REGISTRY_TOKEN", result.stderr)
        self.assertEqual(calls, [])

    def test_execute_publishes_in_order_and_skips_indexed_crates(self) -> None:
        self.record_evidence()
        self.index_entry("plakous-zeta")
        self.env["CARGO_REGISTRY_TOKEN"] = "fake-token"
        result, calls = self.publish("--execute")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_packaged_in_order(calls[0])
        self.assertEqual(
            [call["args"] for call in calls[1:]],
            [["publish", "--locked", "--no-verify", "-p", name] for name in ORDER[1:]],
        )
        self.assertTrue(all(call["offline"] is None for call in calls[1:]))
        self.assertIn("plakous-zeta 0.0.2: already in the index, skipped", result.stdout)
        for name in ORDER:
            self.assertTrue(publish_plan.indexed(self.index.as_uri(), name, "0.0.2"))

    def test_execute_stops_before_dependents_of_a_crate_that_never_appears(self) -> None:
        self.record_evidence()
        self.env.update(CARGO_REGISTRY_TOKEN="fake-token", FAKE_UNINDEXED="plakous-mid",
                        PLAK_PUBLISH_TIMEOUT="0")
        result, calls = self.publish("--execute")
        self.assertEqual(result.returncode, 1)
        self.assertIn("plakous-mid 0.0.2 did not appear", result.stderr)
        self.assertEqual(
            [call["args"][-1] for call in calls if call["args"][0] == "publish"],
            ["plakous-zeta", "plakous-mid"],
        )


class PublishOrderTests(unittest.TestCase):
    @staticmethod
    def metadata(dependencies: dict[str, list[dict]]) -> dict:
        packages = [
            dict(id=name, name=name, version="0.0.2", publish=None, dependencies=edges)
            for name, edges in dependencies.items()
        ]
        return dict(workspace_members=list(dependencies), packages=packages)

    def test_path_only_dev_dependencies_do_not_order_and_cycles_fail(self) -> None:
        stripped = dict(name="b", kind="dev", req="*")
        order = publish_plan.publish_order(self.metadata({
            "a": [stripped], "b": [dict(name="a", kind=None, req="^0.0.2")],
        }))
        self.assertEqual(order, [("a", "0.0.2"), ("b", "0.0.2")])
        with self.assertRaises(publish_plan.PlanError):
            publish_plan.publish_order(self.metadata({
                "a": [dict(stripped, req="^0.0.2")],
                "b": [dict(name="a", kind=None, req="^0.0.2")],
            }))

    def test_index_paths_follow_the_registry_layout(self) -> None:
        self.assertEqual(
            [publish_plan.index_path(name) for name in ("a", "ab", "abc", "Plakous-Model")],
            ["1/a", "2/ab", "3/a/abc", "pl/ak/plakous-model"],
        )


if __name__ == "__main__":
    unittest.main()
