"""Counting-rule tests for `check_prod_loc.prod_lines_in_file`.

The rule these pin: a `mod tests { ... }` block is excluded wherever it sits,
so production code *below* a nested test module is still counted. Before this,
the counter truncated the file at the first line matching `mod tests {` after
`.strip()`, which a nested test module (e.g. one inside `profiles::codex`)
matched just as well as a crate-level one — silently discarding every
production line beneath it.
"""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))
SPEC = importlib.util.spec_from_file_location("check_prod_loc", ROOT / "tools/checks/check_prod_loc.py")
assert SPEC and SPEC.loader
LOC = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = LOC
SPEC.loader.exec_module(LOC)


class ProdLineCountingTests(unittest.TestCase):
    def count(self, source: str) -> int:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lib.rs"
            path.write_text(source, encoding="utf-8")
            return LOC.prod_lines_in_file(path)

    def test_file_without_tests_counts_every_line(self):
        source = "pub fn one() {}\npub fn two() {}\npub fn three() {}\n"
        self.assertEqual(self.count(source), 3)

    def test_top_level_mod_tests_ends_the_production_region(self):
        source = (
            "pub fn one() {}\n"
            "pub fn two() {}\n"
            "\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    use super::*;\n"
            "    #[test]\n"
            "    fn works() {\n"
            "        assert!(one_is_fine());\n"
            "    }\n"
            "}\n"
        )
        self.assertEqual(self.count(source), 3)

    def test_bare_top_level_mod_tests_ends_the_production_region(self):
        source = "pub fn one() {}\nmod tests {\n    fn helper() {}\n}\n"
        self.assertEqual(self.count(source), 1)

    def test_nested_mod_tests_does_not_swallow_production_below_it(self):
        """The facade shape: `profiles::codex` holds tests, real code follows."""
        source = (
            "pub mod profiles {\n"                    # 1
            "    pub mod codex {\n"                   # 2
            "        pub fn recommended() -> u8 {\n"  # 3
            "            1\n"                         # 4
            "        }\n"                             # 5
            "\n"                                      # 6
            "        #[cfg(test)]\n"                  # (excluded)
            "        #[allow(clippy::expect_used, reason = \"unit tests\")]\n"
            "        mod tests {\n"
            "            use super::*;\n"
            "            #[test]\n"
            "            fn recommended_is_one() {\n"
            "                assert_eq!(recommended(), 1);\n"
            "            }\n"
            "        }\n"
            "    }\n"                                 # 7
            "\n"                                      # 8
            "    pub mod grok {\n"                    # 9
            "        pub fn recommended() -> u8 {\n"  # 10
            "            2\n"                         # 11
            "        }\n"                             # 12
            "    }\n"                                 # 13
            "}\n"                                     # 14
        )
        self.assertEqual(self.count(source), 14)

    def test_several_nested_test_modules_are_each_excluded(self):
        source = (
            "pub mod a {\n"
            "    pub fn f() {}\n"
            "    mod tests {\n"
            "        #[test]\n"
            "        fn t() {}\n"
            "    }\n"
            "}\n"
            "pub mod b {\n"
            "    pub fn g() {}\n"
            "    mod tests {\n"
            "        #[test]\n"
            "        fn t() {}\n"
            "    }\n"
            "}\n"
            "pub fn tail() {}\n"
        )
        self.assertEqual(self.count(source), 7)

    def test_cfg_test_file_module_declaration_is_not_a_test_block(self):
        """`#[cfg(test)] mod x;` declares a file; it opens no block to skip."""
        source = (
            "#[cfg(test)]\n"
            "mod tests;\n"
            "#[cfg(test)]\n"
            "mod test_support;\n"
            "pub fn production() -> u8 {\n"
            "    3\n"
            "}\n"
        )
        self.assertEqual(self.count(source), 7)

    def test_braces_in_strings_and_comments_do_not_shift_depth(self):
        source = (
            "pub mod a {\n"
            "    pub fn f() {\n"
            "        println!(\"{}\", \"{\");\n"
            "        // a stray } in a comment\n"
            "        let brace = '{';\n"
            "        let raw = r#\"{ unbalanced \"#;\n"
            "    }\n"
            "    mod tests {\n"
            "        #[test]\n"
            "        fn t() {}\n"
            "    }\n"
            "}\n"
            "pub fn tail() {}\n"
        )
        self.assertEqual(self.count(source), 9)

    def test_attributes_introducing_a_nested_test_module_are_excluded_with_it(self):
        source = (
            "pub mod a {\n"
            "    pub fn f() {}\n"
            "    #[cfg(test)]\n"
            "    #[allow(\n"
            "        clippy::expect_used,\n"
            "        reason = \"tests fail fast\"\n"
            "    )]\n"
            "    mod tests {\n"
            "        #[test]\n"
            "        fn t() {}\n"
            "    }\n"
            "    pub fn g() {}\n"
            "}\n"
        )
        self.assertEqual(self.count(source), 4)

    def test_named_test_modules_preserve_production_between_and_after(self):
        source = (
            "pub fn first() {}\n"
            "#[cfg(test)]\n"
            "#[allow(dead_code)]\n"
            "mod observation_budget_tests {\n"
            '    fn helper() { let x = "}"; }\n'
            "}\n"
            "pub fn middle() {}\n"
            "#[cfg(test)]\n"
            "mod scheduled_tests { fn helper() {} }\n"
            "pub fn last() {}\n"
        )
        self.assertEqual(self.count(source), 3)

    def test_top_level_tests_do_not_hide_production_tail(self):
        self.assertEqual(self.count("mod tests { fn helper() {} }\npub fn tail() {}\n"), 1)

    def test_platform_and_test_support_modules_remain_counted(self):
        source = (
            "#[cfg(unix)]\nmod platform { fn helper() {} }\n"
            '#[cfg(any(test, feature = "test-support"))]\n'
            "mod support { fn helper() {} }\n"
            "#[cfg(not(test))]\nmod production { fn helper() {} }\n"
        )
        self.assertEqual(self.count(source), 6)

    def test_nested_named_module_and_multiline_attribute(self):
        source = (
            "mod outer {\n"
            "    #[cfg(\n        test\n    )]\n"
            "    mod checks { fn helper() {} }\n"
            "    pub fn remaining() {}\n}\n"
        )
        self.assertEqual(self.count(source), 3)

    def test_identifier_ending_in_mod_tests_is_not_a_test_module(self):
        source = (
            "pub mod a {\n"
            "    pub fn submod_tests() {}\n"
            "    pub fn f() {}\n"
            "}\n"
        )
        self.assertEqual(self.count(source), 4)


if __name__ == "__main__":
    unittest.main()
