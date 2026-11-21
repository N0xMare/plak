"""Host sizing must preserve lane coverage, strict separation and watchdogs."""

from pathlib import Path
import sys
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/gate"))
import nextest_layout as layout  # noqa: E402


class NextestLayoutTests(unittest.TestCase):
    def test_supported_pool_sizes_preserve_every_non_resource_setting(self):
        source = (ROOT / ".config/nextest.toml").read_text()
        for available, width, quarter in [
            (0, 1, 1),
            (1, 1, 1),
            (2, 2, 1),
            (4, 4, 1),
            (8, 8, 2),
            (10, 10, 3),
            (12, 12, 3),
            (16, 16, 4),
            (24, 16, 4),
        ]:
            whole = "num-test-threads"
            for profile, weights, priorities in (
                ("durability", [whole, quarter], [-50, 90]),
                ("install", [whole, whole, whole, quarter, quarter], [-50, 100, -50, 95, 90]),
            ):
                with self.subTest(profile=profile, available=available):
                    rendered, actual_width = layout.render(source, available, profile)
                    self.assertEqual(actual_width, width)
                    actual = tomllib.loads(rendered)
                    sized = actual["profile"].pop(profile)
                    expected = tomllib.loads(source)
                    baseline = expected["profile"].pop(profile)
                    self.assertEqual(actual, expected)
                    self.assertEqual(sized["test-threads"], width)
                    for key in ["default-filter", "slow-timeout", "junit"]:
                        self.assertEqual(sized[key], baseline[key])
                    rules = sized["overrides"]
                    self.assertEqual([r["threads-required"] for r in rules], weights)
                    self.assertEqual([r.get("priority", 0) for r in rules], priorities)
                    self.assertEqual(
                        [r["filter"] for r in rules], [r["filter"] for r in baseline["overrides"]]
                    )

    def test_unrecognized_rewrite_fails_instead_of_using_the_wrong_width(self):
        source = (ROOT / ".config/nextest.toml").read_text()
        source = source.replace("test-threads = 16", "test-threads\t=16")
        with self.assertRaises(ValueError):
            layout.render(source, 10)


if __name__ == "__main__":
    unittest.main()
