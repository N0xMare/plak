"""The nextest lane profiles partition the offline suite.

`.config/nextest.toml` names the serial (wall-clock budget) and durability
members once each, as those profiles' `default-filter`, and the `offline`
(parallel) profile excludes exactly their union. A predicate that drifts out
of the exclusion would run a budget test under parallel load; one that only
the exclusion names would never run at all.
"""

from __future__ import annotations

import re
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / ".config/nextest.toml"
SOURCE_ROOTS = ("crates", "bin", "tests")
PREDICATE = re.compile(r"test\(([^)]+)\)")


def profiles() -> dict:
    with CONFIG.open("rb") as handle:
        return tomllib.load(handle)["profile"]


def predicates(profile: str) -> list[str]:
    return PREDICATE.findall(profiles()[profile]["default-filter"])


class NextestLaneTests(unittest.TestCase):
    def test_offline_excludes_exactly_the_serial_and_durability_lanes(self) -> None:
        serial, durability = predicates("serial"), predicates("durability")
        self.assertTrue(serial and durability)
        self.assertEqual(set(serial) & set(durability), set())
        for lane in (serial, durability):
            self.assertEqual(len(lane), len(set(lane)), "duplicate lane predicate")
        offline = profiles()["offline"]["default-filter"]
        self.assertRegex(offline, r"^\s*not \(")
        self.assertEqual(predicates("offline"), serial + durability)
        for name in ("serial", "durability"):
            body = profiles()[name]["default-filter"]
            self.assertEqual(PREDICATE.sub("", body).replace("|", "").split(), [], name)

    def test_lane_profiles_are_single_threaded_with_the_budget_timeout(self) -> None:
        for name in ("serial", "durability"):
            profile = profiles()[name]
            self.assertEqual(profile["test-threads"], 1, name)
            self.assertEqual(profile["slow-timeout"], {"period": "240s", "terminate-after": 2})
        self.assertNotIn("retries", CONFIG.read_text())

    def test_every_lane_predicate_names_a_test_or_module_in_the_tree(self) -> None:
        names = predicates("serial") + predicates("durability")
        functions: set[str] = set()
        modules: set[str] = set()
        qualified: set[str] = set()
        for root in SOURCE_ROOTS:
            for path in (ROOT / root).rglob("*.rs"):
                text = path.read_text()
                file_functions = set(re.findall(r"\bfn\s+([a-z0-9_]+)\s*\(", text))
                functions |= file_functions
                modules |= set(re.findall(
                    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z0-9_]+)\s*[;{]", text, re.MULTILINE))
                module_path = path.parent.parts if path.stem in {"main", "mod"} else path.with_suffix("").parts
                for name in names:
                    if "::" in name and not name.endswith("::"):
                        prefix, function = name.rsplit("::", 1)
                        parts = tuple(prefix.split("::"))
                        if module_path[-len(parts):] == parts and function in file_functions:
                            qualified.add(name)
        missing = [
            name for name in names
            if not (name[:-2] in modules if name.endswith("::")
                    else name in qualified if "::" in name
                    else name in functions)
        ]
        self.assertEqual(missing, [], f"lane predicates with no definition under {SOURCE_ROOTS}")


if __name__ == "__main__":
    unittest.main()
