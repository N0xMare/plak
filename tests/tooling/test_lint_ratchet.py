"""Lint and size ratchet: suppression and large-file inventory, fail-closed ceilings."""

import sys
import tempfile
import tomllib
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/gate"))
import check_workspace as checker

REASON = {"reason": "frozen debt"}


def long_file(lines: int, tests: int = 0) -> str:
    body = "".join(f"fn f{index}() {{}}\n" for index in range(lines))
    if tests:
        body += (
            "#[cfg(test)]\nmod tests {\n"
            + "".join(f"    fn t{index}() {{}}\n" for index in range(tests))
            + "}\n"
        )
    return body


class LintRatchetInventoryTests(unittest.TestCase):
    def inventory(self, files, roots=("src/lib.rs",)):
        return checker.lint_ratchet_inventory(files, set(roots))

    def test_missing_docs_counts_module_item_and_conditional_suppressions(self):
        inventory = self.inventory(
            {
                "src/lib.rs": (
                    "//! Library.\n#![allow(missing_docs, reason = \"debt\")]\n"
                    "#[allow(missing_docs, reason = \"module contract pending\")]\nmod a;\n"
                    "mod inline { #![expect(missing_docs, reason = \"pending\")] }\n"
                    "#[cfg_attr(test, allow(missing_docs))]\nmod b;\n"
                    "// #[allow(missing_docs)]\n"
                    '#[doc = "#[allow(missing_docs)]"]\n'
                    "#[deny(missing_docs)]\nmod c;\n"
                ),
                "src/a.rs": '#![allow(missing_docs, reason = "module contract pending")]\n',
                "tests/suite.rs": "#![expect(missing_docs)]\n",
            },
            roots=("src/lib.rs", "tests/suite.rs"),
        )
        self.assertEqual(
            inventory["missing-docs"],
            ["src/a.rs:1", "src/lib.rs:3", "src/lib.rs:5", "src/lib.rs:6"],
        )
        self.assertEqual(inventory["crate-wide-missing-docs"], ["src/lib.rs:2", "tests/suite.rs:1"])

    def test_item_level_and_crate_wide_suppressions_are_counted_apart(self):
        inventory = self.inventory(
            {
                "src/lib.rs": (
                    "//! Crate docs.\n#![allow(\n    clippy::too_many_lines,\n    reason = \"split // pending\"\n)]\n"
                    "#[allow(clippy::too_many_arguments, reason = \"wire shape\")]\nfn a() {}\n"
                ),
                "src/logic.rs": (
                    "#![expect(clippy::too_many_lines, reason = \"module\")]\n"
                    "#[expect(clippy::too_many_lines, clippy::too_many_arguments, reason = \"x\")]\nfn b() {}\n"
                    "#[cfg_attr(test, allow(clippy::too_many_lines))]\nfn c() {}\n"
                ),
                "tests/suite.rs": "#[allow(clippy::too_many_lines)]\nfn d() {}\n",
            }
        )
        self.assertEqual(inventory["crate-wide"], ["src/lib.rs:too_many_lines"])
        self.assertEqual(
            inventory["items"]["too_many_lines"],
            ["src/logic.rs:1", "src/logic.rs:2", "src/logic.rs:4", "tests/suite.rs:1"],
        )
        self.assertEqual(
            inventory["items"]["too_many_arguments"], ["src/lib.rs:6", "src/logic.rs:2"]
        )

    def test_groups_suppress_their_member_lints(self):
        inventory = self.inventory(
            {
                "src/lib.rs": "#![allow(clippy::pedantic, clippy::all)]\n",
                "src/a.rs": "#[expect(clippy::complexity)]\nfn a() {}\n#[allow(clippy::pedantic)]\nfn b() {}\n",
            }
        )
        self.assertEqual(
            inventory["crate-wide"], ["src/lib.rs:too_many_arguments", "src/lib.rs:too_many_lines"]
        )
        self.assertEqual(
            inventory["items"],
            {"too_many_lines": ["src/a.rs:3"], "too_many_arguments": ["src/a.rs:1"]},
        )

    def test_only_leading_inner_attributes_of_a_crate_root_are_crate_wide(self):
        inventory = self.inventory(
            {
                "src/lib.rs": "mod inner {\n    #![allow(clippy::too_many_lines)]\n}\n",
                "src/main.rs": "#![allow(clippy::too_many_arguments)]\nfn main() {}\n",
                "src/module.rs": "#![allow(clippy::too_many_lines)]\n",
            },
            roots=("src/lib.rs", "src/main.rs"),
        )
        self.assertEqual(inventory["crate-wide"], ["src/main.rs:too_many_arguments"])
        self.assertEqual(inventory["items"]["too_many_lines"], ["src/lib.rs:2", "src/module.rs:1"])

    def test_cfg_attr_counts_only_when_it_applies_an_allow_or_expect(self):
        inventory = self.inventory(
            {
                "src/a.rs": (
                    "#[cfg_attr(test, deny(clippy::too_many_lines))]\nfn a() {}\n"
                    "#[cfg_attr(feature = \"x\", expect(clippy::too_many_lines))]\nfn b() {}\n"
                    '#[doc = "// #[allow(clippy::too_many_arguments)]"]\nfn c() {}\n'
                )
            }
        )
        self.assertEqual(
            inventory["items"], {"too_many_lines": ["src/a.rs:3"], "too_many_arguments": []}
        )

    def test_comments_other_lints_and_other_attributes_are_not_suppressions(self):
        inventory = self.inventory(
            {
                "src/lib.rs": (
                    "// #[allow(clippy::too_many_lines)]\n"
                    "#[allow(clippy::too_many_lines_ish, clippy::unwrap_used)]\n"
                    "#[doc = \"see too_many_lines\"]\n"
                    "#[deny(clippy::too_many_arguments)]\nfn a() {}\n"
                )
            }
        )
        self.assertEqual(inventory["items"], {"too_many_lines": [], "too_many_arguments": []})
        self.assertEqual(inventory["crate-wide"], [])

    def test_large_files_count_non_blank_non_test_lines_only(self):
        inventory = self.inventory(
            {
                "src/big.rs": long_file(1501),
                "src/limit.rs": long_file(1500, tests=400) + "\n\n\n",
                "src/inline.rs": long_file(1400, tests=300),
                "src/lib.rs": "#[cfg(test)]\nmod tests;\n",
                "src/tests.rs": long_file(1600),
                "src/module.rs": "#[cfg(test)]\nmod suite;\n",
                "src/module/suite.rs": long_file(1600),
                "tests/suite.rs": long_file(1600),
                "benches/b.rs": long_file(1600),
                # Not cfg(test): a feature-gated support module and an example
                # count like any production file.
                "src/test_support.rs": long_file(1600),
                "examples/demo.rs": long_file(1600),
            }
        )
        self.assertEqual(
            inventory["large-files"],
            ["examples/demo.rs (1600)", "src/big.rs (1501)", "src/test_support.rs (1600)"],
        )

    def test_compound_nested_and_redirected_test_files_are_not_large(self):
        inventory = self.inventory(
            {
                "src/foo.rs": '#[cfg(feature = "x")] mod outer { mod inner { '
                '#[cfg(all(test, any(unix, windows)))] mod suite; } }',
                "src/foo/outer/inner/suite.rs": long_file(1600),
                "src/foo/outer/inner/suite/child.rs": long_file(1600),
                "src/compound.rs": '#[cfg(all(test, not(feature = "x")))] mod suite;',
                "src/compound/suite.rs": long_file(1600),
                "src/reversed.rs": '#[cfg(all(feature = "x", test))] mod suite;',
                "src/reversed/suite.rs": long_file(1600),
                "src/inline.rs": long_file(1450)
                + '#[cfg(all(test, unix))] mod tests {\n'
                + long_file(100)
                + '}\n',
                "src/redirect.rs": '#[path = "model_tests.rs"] #[cfg(test)] mod tests;',
                "src/model_tests.rs": long_file(1600),
                "src/lib.rs": '#[cfg(any(test, feature = "x"))] mod suite;\n'
                '// #[cfg(test)] mod commented;',
                "src/suite.rs": long_file(1600),
                "src/commented.rs": long_file(1600),
            }
        )
        self.assertEqual(
            inventory["large-files"], ["src/commented.rs (1600)", "src/suite.rs (1600)"]
        )


class LintRatchetValidationTests(unittest.TestCase):
    INVENTORY = {
        "items": {"too_many_lines": ["src/a.rs:1", "src/b.rs:2"], "too_many_arguments": []},
        "crate-wide": ["src/lib.rs:too_many_lines"],
        "large-files": ["src/big.rs (1600)"],
    }
    EXACT = {
        "too-many-lines-ceiling": 2,
        "large-file-ceiling": 1,
        "crate-wide-allows": ["src/lib.rs:too_many_lines"],
        **REASON,
    }

    def validate(self, rule, inventory=None):
        return checker.validate_lint_ratchet("pkg", inventory or self.INVENTORY, rule)

    def test_exact_ceilings_pass(self):
        self.assertEqual(self.validate(self.EXACT), [])

    def test_missing_docs_is_bounded_and_rejects_crate_wide_opt_outs(self):
        inventory = {
            **self.INVENTORY,
            "missing-docs": ["src/lib.rs:8"],
            "crate-wide-missing-docs": [],
        }
        exact = {**self.EXACT, "missing-docs-ceiling": 1}
        self.assertEqual(self.validate(exact, inventory), [])
        self.assertTrue(
            any(
                "missing-docs count 1 exceeds its ceiling 0" in error
                for error in self.validate({**exact, "missing-docs-ceiling": 0}, inventory)
            )
        )
        self.assertEqual(self.validate({**exact, "missing-docs-ceiling": 2}, inventory), [])
        self.assertTrue(
            any(
                "must be a nonnegative integer" in error
                for error in self.validate({**exact, "missing-docs-ceiling": True}, inventory)
            )
        )
        self.assertTrue(
            any(
                "crate-wide missing_docs suppression src/lib.rs:1 is not permitted" in error
                for error in self.validate(
                    exact, {**inventory, "crate-wide-missing-docs": ["src/lib.rs:1"]}
                )
            )
        )

    def test_removing_enrollment_and_restoring_a_crate_wide_allow_fails_static(self):
        inventory = checker.lint_ratchet_inventory(
            {"src/lib.rs": '#![allow(missing_docs, reason = "restored opt-out")]\n'},
            {"src/lib.rs"},
        )
        self.assertEqual(
            checker.validate_lint_ratchet("plakous-hosted", inventory, {}),
            [
                "plakous-hosted: documentation ratchet enrollment requires missing-docs-ceiling; "
                "removing the key is not an opt-out"
            ],
        )
        self.assertTrue(
            any(
                "crate-wide missing_docs suppression" in error
                for error in checker.validate_lint_ratchet(
                    "plakous-hosted", inventory, {"missing-docs-ceiling": 0}
                )
            )
        )

    def test_above_a_ceiling_fails_and_below_passes(self):
        errors = self.validate({**self.EXACT, "too-many-lines-ceiling": 1, "large-file-ceiling": 2})
        self.assertIn(
            "pkg: too-many-lines count 2 exceeds its ceiling 1; split the item or remove the allow. "
            "Ceilings are not raised without review (spec 06 sec. 4): src/a.rs:1; src/b.rs:2",
            errors,
        )
        self.assertEqual(len(errors), 1)
        self.assertEqual(self.validate({**self.EXACT, "too-many-arguments-ceiling": 3}), [])

    def test_an_unrecorded_crate_wide_allow_fails_and_a_removed_one_must_be_dropped(self):
        errors = self.validate({**self.EXACT, "crate-wide-allows": []})
        self.assertTrue(
            any("crate-wide allow src/lib.rs:too_many_lines is not recorded" in e for e in errors)
        )
        inventory = {**self.INVENTORY, "crate-wide": []}
        errors = self.validate(self.EXACT, inventory)
        self.assertIn(
            "pkg: crate-wide allow src/lib.rs:too_many_lines is gone; remove it from crate-wide-allows "
            "in this package's [package.metadata.plakous-lint]",
            errors,
        )

    def test_a_new_package_starts_at_zero(self):
        errors = self.validate({})
        self.assertTrue(any("too-many-lines count 2 exceeds its ceiling 0" in e for e in errors))
        self.assertTrue(any("is not recorded" in e for e in errors))
        self.assertEqual(
            self.validate(
                {},
                {
                    "items": {"too_many_lines": [], "too_many_arguments": []},
                    "crate-wide": [],
                    "large-files": [],
                },
            ),
            [],
        )

    def test_shape_and_reason_fail_closed(self):
        self.assertEqual(
            self.validate([]), ["pkg: [package.metadata.plakous-lint] must be a table"]
        )
        self.assertIn(
            "pkg: unknown [package.metadata.plakous-lint] key 'too-many-line-ceiling'",
            self.validate({**self.EXACT, "too-many-line-ceiling": 1}),
        )
        self.assertIn(
            "pkg: plakous-lint large-file-ceiling must be a nonnegative integer",
            self.validate({**self.EXACT, "large-file-ceiling": -1}),
        )
        self.assertIn(
            "pkg: nonzero plakous-lint ceilings require a reviewed reason",
            self.validate({key: value for key, value in self.EXACT.items() if key != "reason"}),
        )

    def test_clippy_thresholds_are_pinned(self):
        self.assertEqual(checker.validate_ratchet_clippy_thresholds(checker.ROOT), [])
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / ".clippy.toml").write_text("too-many-lines-threshold = 150\n")
            errors = checker.validate_ratchet_clippy_thresholds(root)
        self.assertEqual(len(errors), 2, errors)

    def test_the_workspace_ratchet_is_bounded_and_lives_in_package_manifests(self):
        workspace = checker.load_workspace(checker.ROOT)
        self.assertIn(
            "missing-docs-ceiling",
            checker.package_lint_rule(workspace.packages["plakous-hosted"]),
        )
        self.assertNotIn("lint", workspace.root_manifest["workspace"]["metadata"]["plakous"])
        directories = {package.directory for package in workspace.packages.values()}
        for name, package in sorted(workspace.packages.items()):
            nested = {
                other
                for other in directories
                if other != package.directory and other.startswith(f"{package.directory}/")
            }
            with self.subTest(package=name):
                files = checker.package_rust_files(checker.ROOT, package, nested)
                inventory = checker.package_lint_inventory(package, files)
                rule = checker.package_lint_rule(package)
                self.assertEqual(checker.validate_lint_ratchet(name, inventory, rule), [])


class TightenCeilingsTests(unittest.TestCase):
    def test_tighten_recounts_every_ceiling_and_never_raises_one(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch).resolve()
            package = root / "pkg"
            source = package / "src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text(
                '#[cfg(feature = "test-hooks")]\n'
                "#[allow(clippy::too_many_lines, clippy::too_many_arguments, missing_docs)]\n"
                'pub fn gate() { std::env::var("TEST"); }\n' + long_file(1501)
            )
            (root / ".clippy.toml").write_text("too-many-arguments-threshold = 7\n")
            manifest = package / "Cargo.toml"
            manifest.write_text(
                '[package]\nname = "pkg"\n'
                '[package.metadata.plakous]\nlayer = "bin"\nclass = "supported"\n'
                '[package.metadata.plakous-lint]\n'
                'too-many-lines-ceiling = 2\n'
                'too-many-arguments-ceiling = 3\n'
                'large-file-ceiling = 4\n'
                'missing-docs-ceiling = 5 # keeps enrollment\n'
                'reason = "Existing debt"\n'
            )
            root_manifest = root / "Cargo.toml"
            root_manifest.write_text(
                '[workspace.metadata.plakous]\nlayers = ["bin"]\n'
                '[workspace.metadata.plakous.edges]\nbin = []\n'
                '[workspace.metadata.plakous.layout]\nbin = []\n'
                '[workspace.metadata.plakous.classes]\nbin = ["supported"]\n'
                '[workspace.metadata.plakous.cfg.pkg]\n'
                'feature-sites = []\nfeature-ceiling = 3\ntest-seam-ceiling = 3\nenv-ceiling = 3\n'
                'reason = "Existing debt"\n'
                '[workspace.metadata.plakous.modules.pkg.a]\n'
                'may-use = ["crate::a"]\nmay-use-ceiling = 3\nreason = "Existing debt"\n'
            )
            graph = {
                "workspace_members": ["pkg"],
                "packages": [
                    {
                        "id": "pkg",
                        "name": "pkg",
                        "manifest_path": str(manifest),
                        "targets": [{"kind": ["lib"], "src_path": str(source)}],
                    }
                ],
            }

            def tighten():
                with redirect_stdout(StringIO()), redirect_stderr(StringIO()) as errors:
                    result = checker.tighten_ceilings(checker.load_workspace(root, graph))
                return result, errors.getvalue()

            before = manifest.read_text(), root_manifest.read_text()
            self.assertEqual(tighten(), (0, ""))
            lint = tomllib.loads(manifest.read_text())["package"]["metadata"]["plakous-lint"]
            self.assertEqual(
                lint,
                {
                    "too-many-lines-ceiling": 1,
                    "too-many-arguments-ceiling": 1,
                    "large-file-ceiling": 1,
                    "missing-docs-ceiling": 1,
                    "reason": "Existing debt",
                },
            )
            self.assertIn("missing-docs-ceiling = 1 # keeps enrollment", manifest.read_text())
            rules = tomllib.loads(root_manifest.read_text())["workspace"]["metadata"]["plakous"]
            self.assertEqual(
                rules["cfg"]["pkg"],
                {
                    "feature-sites": [],
                    "feature-ceiling": 1,
                    "test-seam-ceiling": 1,
                    "env-ceiling": 1,
                    "reason": "Existing debt",
                },
            )
            self.assertEqual(rules["modules"]["pkg"]["a"]["may-use-ceiling"], 1)
            after = manifest.read_text(), root_manifest.read_text()
            self.assertNotEqual(before, after)
            self.assertEqual(tighten(), (0, ""))
            self.assertEqual((manifest.read_text(), root_manifest.read_text()), after)
            source.write_text(source.read_text() + 'pub fn grow() { std::env::var("TEST"); }\n')
            result, errors = tighten()
            self.assertEqual(result, 1)
            self.assertIn("env placement debt 2 exceeds ceiling 1", errors)
            self.assertEqual((manifest.read_text(), root_manifest.read_text()), after)


if __name__ == "__main__":
    unittest.main()
