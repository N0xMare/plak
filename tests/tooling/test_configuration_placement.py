"""Configuration placement ratchet: syntax, exemptions and fail-closed ceilings."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/checks"))
import check_workspace as checker


class ConfigurationPlacementTests(unittest.TestCase):
    def debt(self, source, path="src/logic.rs", package="library", rule=None):
        return checker.configuration_debt(package, {path: source}, rule or {})

    def test_module_swaps_are_allowed_everywhere(self):
        source = '''
        #[cfg(feature = "real")] pub(crate) mod real;
        #[cfg(not(feature = "real"))] #[path = "disabled.rs"] mod real;
        '''
        self.assertFalse(any(self.debt(source).values()))

    def test_all_three_cfg_spellings_and_nested_predicates(self):
        source = '''
        #[cfg(any(feature = "one", feature = "two"))] fn a() {}
        #[cfg_attr(feature = "one", derive(Debug))] struct A;
        fn b() { if cfg!(feature = "two") {} }
        '''
        self.assertEqual(len(self.debt(source)["feature"]), 3)

    def test_comments_and_literals_are_not_configuration(self):
        source = '''
        // #[cfg(feature = "x")] fn a() {}
        const S: &str = r###"cfg!(feature = "x"); std::env::var("X")"###;
        /* std::env::vars(); */
        '''
        self.assertFalse(any(self.debt(source).values()))

    def test_test_only_items_do_not_hide_later_production(self):
        source = '''
        #[cfg(test)] mod tests { fn a() { std::env::var("X"); } }
        #[cfg(all(test, feature = "x"))] fn test() { std::env::vars(); }
        #[cfg(feature = "x")] fn production() { std::env::var_os("X"); }
        '''
        debt = self.debt(source)
        self.assertEqual(len(debt["feature"]), 1)
        self.assertEqual(len(debt["env"]), 1)

    def test_disjunctive_test_feature_still_ships(self):
        source = '#[cfg(any(test, feature = "test-support"))] fn seam() {}'
        debt = self.debt(source)
        self.assertEqual(len(debt["feature"]), 1)
        self.assertEqual(len(debt["test-seam"]), 1)

    def test_test_seams_only_on_support_modules_in_libraries(self):
        source = '''
        #[cfg(feature = "test-support")] mod test_support;
        #[cfg(feature = "fault-injection")] mod transport;
        '''
        self.assertEqual(len(self.debt(source)["test-seam"]), 1)

    def test_product_seams_only_at_composition_root(self):
        source = '#[cfg(feature = "test-hooks")] fn install() {}'
        self.assertFalse(self.debt(source, "src/lib.rs", "plak")["test-seam"])
        self.assertEqual(len(self.debt(source, package="plak")["test-seam"]), 1)

    def test_registry_and_explicit_feature_sites(self):
        source = 'static CAPS: &[&dyn Capability] = &[#[cfg(feature = "x")] &x::CAPABILITY,];'
        self.assertFalse(any(self.debt(source, "src/registry.rs").values()))
        self.assertFalse(any(self.debt(source, rule={"feature-sites": ["src/logic.rs"]}).values()))

    def test_registry_file_does_not_allow_gated_function_bodies(self):
        source = 'fn branch() { #[cfg(feature = "x")] &x::CAPABILITY; }'
        self.assertEqual(len(self.debt(source, "src/registry.rs")["feature"]), 1)
        source = 'static CAPS: &[&dyn Capability] = &[{ #[cfg(feature = "x")] &x::CAPABILITY }];'
        self.assertEqual(len(self.debt(source, "src/registry.rs")["feature"]), 1)

    def test_environment_imports_aliases_and_function_items(self):
        source = '''
        use std::env::{var as read, var_os, vars, vars_os};
        fn a() { let f = std::env::var; env::var_os("X"); }
        '''
        self.assertEqual(len(self.debt(source)["env"]), 6)
        self.assertFalse(self.debt(source, rule={"env-sites": ["src/logic.rs"]})["env"])

    def test_lib_is_not_a_blanket_item_or_statement_exemption(self):
        source = '#[cfg(feature = "x")] fn branch() {}'
        self.assertEqual(len(self.debt(source, "src/lib.rs")["feature"]), 1)

    def test_test_file_conventions_are_excluded(self):
        source = 'fn test() { std::env::vars(); }'
        for path in ("src/tests/a.rs", "src/unit_tests.rs", "src/test_support.rs"):
            self.assertFalse(any(self.debt(source, path).values()))

    def test_new_packages_default_to_zero_debt(self):
        errors = checker.validate_configuration_rules("new", {
            "src/logic.rs": '#[cfg(feature = "x")] fn a() {}',
        }, {})
        self.assertIn("exceeds ceiling 0", errors[0])

    def test_ceiling_requires_reason_and_reductions_lower_it(self):
        source = {"src/logic.rs": 'fn a() { std::env::var("X"); }'}
        self.assertTrue(checker.validate_configuration_rules("x", source, {"env-ceiling": 1}))
        rule = {"env-ceiling": 1, "reason": "Existing debt"}
        self.assertEqual(checker.validate_configuration_rules("x", source, rule), [])
        errors = checker.validate_configuration_rules("x", {}, rule)
        self.assertIn("lower env-ceiling", errors[0])

    def test_invalid_metadata_fails_closed(self):
        for rule in ({"env-ceiling": True}, {"env-ceiling": -1},
                     {"env-sites": "src/x.rs"}, {"feature-sites": ["src/../x.rs"]},
                     {"env-ceilling": 1}, {"reason": 42}, []):
            with self.subTest(rule=rule):
                self.assertTrue(checker.validate_configuration_rules("x", {}, rule))


if __name__ == "__main__":
    unittest.main()
