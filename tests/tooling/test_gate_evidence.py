"""The documentation-only evidence rule: classification and the allowlist audit."""

from __future__ import annotations

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "gate_evidence", ROOT / "tools/release/gate_evidence.py"
)
assert SPEC is not None and SPEC.loader is not None
gate_evidence = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate_evidence)


class ClassifyTests(unittest.TestCase):
    def test_allowlist_package_only_and_everything_else(self):
        for path, expected in (
            ("tests/live/STATUS.md", "docs"),
            ("CHANGELOG.md", "docs"),
            ("docs/spec/05-testing-and-evidence.md", "docs"),
            ("docs/spec/sub/page.md", "docs"),
            ("docs/user-guide/cli.md", "docs"),
            ("docs/spec/07-config-reference.md", "other"),
            ("README.md", "package"),
            ("crates/core/model/README.md", "other"),
            ("docs/local/notes.md", "other"),
            ("docs/specs.md", "other"),
            ("tests/live/live.py", "other"),
            ("AGENTS.md", "other"),
            ("Cargo.toml", "other"),
        ):
            with self.subTest(path=path):
                self.assertEqual(gate_evidence.classify(path), expected)


class VerifyTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name) / "repo"
        self.target = Path(scratch.name) / "target"
        (self.root / "docs/spec").mkdir(parents=True)
        (self.root / "docs/spec/01.md").write_text("spec\n")
        self.git("init", "-q")
        self.commit()
        self.tree = self.git("rev-parse", "HEAD^{tree}")
        (self.target / "gate-evidence").mkdir(parents=True)
        self.record(self.tree)

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@t", *args],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def commit(self) -> None:
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", "c")

    def record(self, text: str) -> None:
        (self.target / "gate-evidence" / "full").write_text(text + "\n")

    def verify(self) -> tuple[bool, str]:
        ok, lines = gate_evidence.verify(self.root, self.target)
        return ok, "\n".join(lines)

    def test_evidence_must_name_a_tree_object_id(self):
        for text in ("HEAD", "--all", "main^{tree}", "0" * 39, self.git("rev-parse", "HEAD")):
            with self.subTest(text=text):
                (self.root / "CHANGELOG.md").write_text(text)
                self.record(text)
                ok, output = self.verify()
                self.assertFalse(ok)
                self.assertIn("evidence names unknown tree", output)

    def test_a_symlink_in_the_delta_invalidates(self):
        (self.root / "docs/spec/link.md").symlink_to("01.md")
        self.commit()
        ok, output = self.verify()
        self.assertFalse(ok)
        self.assertIn("changes a symlink or submodule (docs/spec/link.md)", output)

    def test_a_gitlink_in_the_delta_invalidates_whatever_the_diff_configuration(self):
        self.git("config", "diff.ignoreSubmodules", "all")
        commit = self.git("rev-parse", "HEAD")
        self.git("update-index", "--add", "--cacheinfo", f"160000,{commit},docs/spec/sub")
        self.git("commit", "-q", "-m", "gitlink")
        ok, output = self.verify()
        self.assertFalse(ok)
        self.assertIn("delta docs: docs/spec/sub", output)
        self.assertIn("changes a symlink or submodule (docs/spec/sub)", output)

    def test_differing_trees_with_no_readable_delta_invalidate(self):
        (self.root / "CHANGELOG.md").write_text("changes\n")
        self.commit()
        with mock.patch.object(gate_evidence, "tree_delta", return_value=[]):
            ok, output = self.verify()
        self.assertFalse(ok)
        self.assertIn("the trees differ but no delta was read", output)


class AuditTests(unittest.TestCase):
    def audit(self, files: dict[str, str]) -> list[str]:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            for relative, text in files.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            return gate_evidence.audit(root, sorted(files))

    def test_the_repository_audit_passes(self):
        self.assertEqual(gate_evidence.audit(ROOT, gate_evidence.listed_files(ROOT)), [])

    def test_a_gate_input_naming_an_allowlisted_path_or_component_fails(self):
        errors = self.audit(
            {
                "crates/a/tests/docs.rs": 'let spec = read("../../docs/spec/05-testing.md");\n',
                "tests/tooling/test_x.py": 'text = (ROOT / "CHANGELOG.md").read_text()\n',
                "scripts/x.sh": "cat tests/live/STATUS.md\n",
                "bin/b/build.rs": 'println!("cargo:rerun-if-changed=../../docs/user-guide/cli.md");\n',
                "crates/c/tests/join.rs": 'let page = root.join("../../docs").join("user-guide").join("x.md");\n',
                "tests/tooling/test_y.py": 'pages = sorted((REPO / "docs" / "spec").rglob("*.md"))\n',
                "scripts/y.sh": 'cat "$root/README.md"\n',
            }
        )
        flagged = {
            error.split(": names ")[0] + " " + error.split("'")[1]
            for error in errors
            if ": names " in error
        }
        self.assertEqual(
            flagged,
            {
                "crates/a/tests/docs.rs:1 docs/spec/05-testing.md",
                "tests/tooling/test_x.py:1 CHANGELOG.md",
                "scripts/x.sh:1 STATUS.md",
                "bin/b/build.rs:1 docs/user-guide/cli.md",
                'crates/c/tests/join.rs:1 "../../docs"',
                'crates/c/tests/join.rs:1 "user-guide"',
                'tests/tooling/test_y.py:1 "docs"',
                'tests/tooling/test_y.py:1 "spec"',
                "tests/tooling/test_y.py:1 *.md",
                "scripts/y.sh:1 README.md",
            },
        )

    def test_a_root_readme_read_fails_even_when_the_token_is_reviewed(self):
        reviewed = {"crates/a/src/lib.rs": (frozenset({"README.md"}), "test")}
        sources = {
            "crates/a/Cargo.toml": "[package]\n",
            "crates/a/src/lib.rs": '#![doc = include_str!(\n    "../../../README.md"\n)]\n',
        }
        with mock.patch.dict(gate_evidence.REVIEWED_REFERENCES, reviewed, clear=True):
            errors = self.audit(sources)
            self.assertEqual(
                errors,
                [
                    "crates/a/src/lib.rs: reads the root README.md; PACKAGE_ONLY assumes no gate "
                    "but packaging does"
                ],
            )
            sources["crates/a/src/lib.rs"] = (
                'const R: &str = include_str!(concat!(\n    env!("CARGO_MANIFEST_DIR"),\n    "/../../README.md"));\n'
            )
            self.assertEqual(len(self.audit(sources)), 1)
            sources["crates/a/src/lib.rs"] = '#![doc = include_str!("../README.md")]\n'
            self.assertEqual(self.audit(sources), [])

    def test_comments_urls_documents_and_manifest_readme_keys_are_not_readers(self):
        self.assertEqual(
            self.audit(
                {
                    "crates/a/src/lib.rs": "//! See docs/spec/01-plakous-library.md.\n",
                    "crates/a/Cargo.toml": (
                        'documentation = "https://example.invalid/tree/main/docs/user-guide"\n'
                        'readme = "../../README.md"\n'
                    ),
                    "tools/perf/perf.just": "# See docs/spec/05.\n",
                    "docs/spec/06.md": "CHANGELOG.md and docs/spec/07-config-reference.md\n",
                    "crates/a/README.md": "docs/spec\n",
                }
            ),
            [],
        )

    def test_reviewed_references_are_exact_texts_and_stale_ones_fail(self):
        reviewed = {"src/a.rs": (frozenset({'"docs"', "README.md"}), "fixture")}
        with mock.patch.dict(gate_evidence.REVIEWED_REFERENCES, reviewed, clear=True):
            self.assertEqual(self.audit({"src/a.rs": 'f("docs"); g("README.md");\n'}), [])
            errors = self.audit({"src/a.rs": 'f("docs"); g("docs/spec/x.md");\n'})
        self.assertTrue(any("names 'docs/spec/x.md'" in error for error in errors), errors)
        self.assertIn(
            "REVIEWED_REFERENCES entry src/a.rs: 'README.md' no longer occurs; drop it", errors
        )

    def test_the_excluded_reader_reads_exactly_its_excluded_path(self):
        reader = "bin/plak/src/product_config/tests.rs"
        with mock.patch.dict(gate_evidence.REVIEWED_REFERENCES, {}, clear=True):
            self.assertEqual(
                self.audit({reader: 'join("../../docs/spec/07-config-reference.md")\n'}), []
            )
            for other in ("docs/spec/02-plak-product.md", "docs/spec/07"):
                errors = self.audit({reader: f'join("../../{other}")\n'})
                self.assertEqual(len(errors), 1, errors)

    def test_an_excluded_reader_requires_its_exclusion(self):
        with mock.patch.object(gate_evidence, "EXCLUDED", {}):
            errors = self.audit({})
        self.assertTrue(any("must be EXCLUDED" in error for error in errors), errors)
        with mock.patch.dict(gate_evidence.EXCLUDED, {"src/lib.rs": "not documentation"}):
            errors = self.audit({})
        self.assertTrue(any("not under the allowlist" in error for error in errors), errors)


if __name__ == "__main__":
    unittest.main()
