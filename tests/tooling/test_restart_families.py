"""A lane split must not silently discard a restart family's union evidence."""

import copy
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/gate"))
from check_restart_families import FAMILIES, check  # noqa: E402


class RestartFamilyTests(unittest.TestCase):
    def inventory(self):
        return {
            "rust-suites": {
                "plak::integration": {
                    "package-name": "plakd",
                    "binary-name": "integration",
                    "testcases": {
                        f"{family}::{role}_{group}": {
                            "ignored": False,
                            "filter-match": {"status": "matches"},
                        }
                        for family in FAMILIES
                        for role in ("root", "child")
                        for group in range(8)
                    },
                }
            }
        }

    def test_complete_selection_passes(self):
        check(self.inventory())

    def test_missing_excluded_or_ignored_group_refuses_each_family(self):
        for family in FAMILIES:
            for mode in ("missing", "excluded", "ignored"):
                with self.subTest(family=family, mode=mode):
                    inventory = self.inventory()
                    tests = inventory["rust-suites"]["plak::integration"]["testcases"]
                    name = family + "::child_7"
                    if mode == "missing":
                        del tests[name]
                    elif mode == "excluded":
                        tests[name]["filter-match"]["status"] = "mismatch"
                    else:
                        tests[name]["ignored"] = True
                    with self.assertRaisesRegex(ValueError, "expected all 16 restart groups"):
                        check(inventory)

    def test_another_binary_or_package_cannot_supply_the_missing_group(self):
        for field in ("binary-name", "package-name"):
            inventory = self.inventory()
            suite = inventory["rust-suites"]["plak::integration"]
            other = copy.deepcopy(suite)
            other[field] = "other"
            inventory["rust-suites"]["other"] = other
            del suite["testcases"][FAMILIES[0] + "::root_0"]
            with self.assertRaisesRegex(ValueError, "expected all 16 restart groups"):
                check(inventory)


if __name__ == "__main__":
    unittest.main()
