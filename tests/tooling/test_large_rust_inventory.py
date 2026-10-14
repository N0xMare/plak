"""Physical inventory categories must reconcile without hiding conditional code."""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/checks"))
from inventory_large_rust import inspect_source


class LargeRustInventoryTests(unittest.TestCase):
    def inspect(self, source, external=False, benchmark=False):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lib.rs"
            path.write_text(source)
            return inspect_source(path, external, benchmark)

    def test_categories_reconcile_and_conditions_remain_visible(self):
        result = self.inspect(
            '#[cfg(test)]\nmod checks { fn test_case() {} }\n'
            '#[cfg(any(test, feature = "test-support"))]\n'
            'fn helper() {}\ninclude_str!("fixture.txt");\n'
        )
        self.assertEqual(result['physical_lines'], 5)
        self.assertEqual(result['production_candidate_lines'], 3)
        self.assertEqual(result['inline_test_module_lines'], 2)
        self.assertEqual(result['external_test_lines'], 0)
        self.assertEqual([row['line'] for row in result['conditional_sites_in_candidates']], [3])
        self.assertEqual([row['line'] for row in result['source_or_data_inclusions']], [5])
        self.assertEqual([row['name'] for row in result['declarations']], ['helper'])

    def test_external_tests_are_not_double_counted(self):
        result = self.inspect('#[cfg(test)]\nmod checks {}\n', external=True)
        self.assertEqual(result['external_test_lines'], 2)
        self.assertEqual(result['inline_test_module_lines'], 0)
        self.assertEqual(result['production_candidate_lines'], 0)

    def test_benchmarks_are_distinct_nonproduction_sources(self):
        result = self.inspect('fn benchmark() {}\n', benchmark=True)
        self.assertEqual(result['benchmark_lines'], 1)
        self.assertEqual(result['production_candidate_lines'], 0)
        self.assertEqual(result['inline_test_module_lines'], 0)

    def test_strings_and_comments_do_not_invent_dependencies(self):
        result = self.inspect('// include!("fake.rs");\nconst S: &str = "include!(fake)";\n')
        self.assertEqual(result['source_or_data_inclusions'], [])


if __name__ == '__main__':
    unittest.main()
