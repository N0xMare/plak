"""Lint and size ratchet: suppression and large-file inventory, fail-closed ceilings."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/gate"))
import check_workspace as checker

REASON = {"reason": "frozen debt"}


def long_file(lines: int, tests: int = 0) -> str:
    body = "".join(f"fn f{index}() {{}}\n" for index in range(lines))
    if tests:
        body += "#[cfg(test)]\nmod tests {\n" + "".join(f"    fn t{index}() {{}}\n" for index in range(tests)) + "}\n"
    return body


class LintRatchetInventoryTests(unittest.TestCase):
    def inventory(self, files, roots=("src/lib.rs",)):
        return checker.lint_ratchet_inventory(files, set(roots))

    def test_item_level_and_crate_wide_suppressions_are_counted_apart(self):
        inventory = self.inventory({
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
        })
        self.assertEqual(inventory["crate-wide"], ["src/lib.rs:too_many_lines"])
        self.assertEqual(inventory["items"]["too_many_lines"],
                         ["src/logic.rs:1", "src/logic.rs:2", "src/logic.rs:4", "tests/suite.rs:1"])
        self.assertEqual(inventory["items"]["too_many_arguments"], ["src/lib.rs:6", "src/logic.rs:2"])

    def test_groups_suppress_their_member_lints(self):
        inventory = self.inventory({
            "src/lib.rs": "#![allow(clippy::pedantic, clippy::all)]\n",
            "src/a.rs": "#[expect(clippy::complexity)]\nfn a() {}\n#[allow(clippy::pedantic)]\nfn b() {}\n",
        })
        self.assertEqual(inventory["crate-wide"], ["src/lib.rs:too_many_arguments", "src/lib.rs:too_many_lines"])
        self.assertEqual(inventory["items"], {"too_many_lines": ["src/a.rs:3"], "too_many_arguments": ["src/a.rs:1"]})

    def test_only_leading_inner_attributes_of_a_crate_root_are_crate_wide(self):
        inventory = self.inventory({
            "src/lib.rs": "mod inner {\n    #![allow(clippy::too_many_lines)]\n}\n",
            "src/main.rs": "#![allow(clippy::too_many_arguments)]\nfn main() {}\n",
            "src/module.rs": "#![allow(clippy::too_many_lines)]\n",
        }, roots=("src/lib.rs", "src/main.rs"))
        self.assertEqual(inventory["crate-wide"], ["src/main.rs:too_many_arguments"])
        self.assertEqual(inventory["items"]["too_many_lines"], ["src/lib.rs:2", "src/module.rs:1"])

    def test_cfg_attr_counts_only_when_it_applies_an_allow_or_expect(self):
        inventory = self.inventory({"src/a.rs": (
            "#[cfg_attr(test, deny(clippy::too_many_lines))]\nfn a() {}\n"
            "#[cfg_attr(feature = \"x\", expect(clippy::too_many_lines))]\nfn b() {}\n"
            '#[doc = "// #[allow(clippy::too_many_arguments)]"]\nfn c() {}\n'
        )})
        self.assertEqual(inventory["items"], {"too_many_lines": ["src/a.rs:3"], "too_many_arguments": []})

    def test_comments_other_lints_and_other_attributes_are_not_suppressions(self):
        inventory = self.inventory({"src/lib.rs": (
            "// #[allow(clippy::too_many_lines)]\n"
            "#[allow(clippy::too_many_lines_ish, clippy::unwrap_used)]\n"
            "#[doc = \"see too_many_lines\"]\n"
            "#[deny(clippy::too_many_arguments)]\nfn a() {}\n"
        )})
        self.assertEqual(inventory["items"], {"too_many_lines": [], "too_many_arguments": []})
        self.assertEqual(inventory["crate-wide"], [])

    def test_large_files_count_non_blank_non_test_lines_only(self):
        inventory = self.inventory({
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
        })
        self.assertEqual(inventory["large-files"],
                         ["examples/demo.rs (1600)", "src/big.rs (1501)", "src/test_support.rs (1600)"])


    def test_compound_nested_and_redirected_test_files_are_not_large(self):
        inventory = self.inventory({
            "src/foo.rs": '#[cfg(feature = "x")] mod outer { mod inner { '
                          '#[cfg(all(test, any(unix, windows)))] mod suite; } }',
            "src/foo/outer/inner/suite.rs": long_file(1600),
            "src/foo/outer/inner/suite/child.rs": long_file(1600),
            "src/compound.rs": '#[cfg(all(test, not(feature = "x")))] mod suite;',
            "src/compound/suite.rs": long_file(1600),
            "src/reversed.rs": '#[cfg(all(feature = "x", test))] mod suite;',
            "src/reversed/suite.rs": long_file(1600),
            "src/inline.rs": long_file(1450) + '#[cfg(all(test, unix))] mod tests {\n'
                             + long_file(100) + '}\n',
            "src/redirect.rs": '#[path = "model_tests.rs"] #[cfg(test)] mod tests;',
            "src/model_tests.rs": long_file(1600),
            "src/lib.rs": '#[cfg(any(test, feature = "x"))] mod suite;\n'
                          '// #[cfg(test)] mod commented;',
            "src/suite.rs": long_file(1600),
            "src/commented.rs": long_file(1600),
        })
        self.assertEqual(inventory["large-files"],
                         ["src/commented.rs (1600)", "src/suite.rs (1600)"])


class LintRatchetValidationTests(unittest.TestCase):
    INVENTORY = {
        "items": {"too_many_lines": ["src/a.rs:1", "src/b.rs:2"], "too_many_arguments": []},
        "crate-wide": ["src/lib.rs:too_many_lines"],
        "large-files": ["src/big.rs (1600)"],
    }
    EXACT = {
        "too-many-lines-ceiling": 2, "large-file-ceiling": 1,
        "crate-wide-allows": ["src/lib.rs:too_many_lines"], **REASON,
    }

    def validate(self, rule, inventory=None):
        return checker.validate_lint_ratchet("pkg", inventory or self.INVENTORY, rule)

    def test_exact_ceilings_pass(self):
        self.assertEqual(self.validate(self.EXACT), [])

    def test_above_a_ceiling_fails_and_below_must_be_lowered(self):
        errors = self.validate({**self.EXACT, "too-many-lines-ceiling": 1, "large-file-ceiling": 2})
        self.assertIn("pkg: too-many-lines count 2 exceeds its ceiling 1; split the item or remove the allow. "
                      "Ceilings are not raised without review (spec 06 sec. 4): src/a.rs:1; src/b.rs:2", errors)
        self.assertIn("pkg: large-file count 1 is below its ceiling 2; set large-file-ceiling = 1 in this "
                      "package's [package.metadata.plakous-lint]", errors)
        errors = self.validate({**self.EXACT, "too-many-arguments-ceiling": 3})
        self.assertEqual(len(errors), 1)
        self.assertIn("set too-many-arguments-ceiling = 0", errors[0])

    def test_an_unrecorded_crate_wide_allow_fails_and_a_removed_one_must_be_dropped(self):
        errors = self.validate({**self.EXACT, "crate-wide-allows": []})
        self.assertTrue(any("crate-wide allow src/lib.rs:too_many_lines is not recorded" in e for e in errors))
        inventory = {**self.INVENTORY, "crate-wide": []}
        errors = self.validate(self.EXACT, inventory)
        self.assertIn("pkg: crate-wide allow src/lib.rs:too_many_lines is gone; remove it from crate-wide-allows "
                      "in this package's [package.metadata.plakous-lint]", errors)

    def test_a_new_package_starts_at_zero(self):
        errors = self.validate({})
        self.assertTrue(any("too-many-lines count 2 exceeds its ceiling 0" in e for e in errors))
        self.assertTrue(any("is not recorded" in e for e in errors))
        self.assertEqual(self.validate({}, {"items": {"too_many_lines": [], "too_many_arguments": []},
                                            "crate-wide": [], "large-files": []}), [])

    def test_shape_and_reason_fail_closed(self):
        self.assertEqual(self.validate([]), ["pkg: [package.metadata.plakous-lint] must be a table"])
        self.assertIn("pkg: unknown [package.metadata.plakous-lint] key 'too-many-line-ceiling'",
                      self.validate({**self.EXACT, "too-many-line-ceiling": 1}))
        self.assertIn("pkg: plakous-lint large-file-ceiling must be a nonnegative integer",
                      self.validate({**self.EXACT, "large-file-ceiling": -1}))
        self.assertIn("pkg: nonzero plakous-lint ceilings require a reviewed reason",
                      self.validate({key: value for key, value in self.EXACT.items() if key != "reason"}))

    def test_clippy_thresholds_are_pinned(self):
        self.assertEqual(checker.validate_ratchet_clippy_thresholds(checker.ROOT), [])
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / ".clippy.toml").write_text("too-many-lines-threshold = 150\n")
            errors = checker.validate_ratchet_clippy_thresholds(root)
        self.assertEqual(len(errors), 2, errors)

    def test_the_workspace_ratchet_is_exact_and_lives_in_package_manifests(self):
        workspace = checker.load_workspace(checker.ROOT)
        self.assertNotIn("lint", workspace.root_manifest["workspace"]["metadata"]["plakous"])
        directories = {package.directory for package in workspace.packages.values()}
        for name, package in sorted(workspace.packages.items()):
            nested = {other for other in directories
                      if other != package.directory and other.startswith(f"{package.directory}/")}
            with self.subTest(package=name):
                files = checker.package_rust_files(checker.ROOT, package, nested)
                inventory = checker.package_lint_inventory(package, files)
                rule = checker.package_lint_rule(package)
                self.assertEqual(checker.validate_lint_ratchet(name, inventory, rule), [])


if __name__ == "__main__":
    unittest.main()
