"""The offline test families whose failures under load are not defects.

A failure here is read differently from a failure anywhere else, so the set is
written down rather than carried in whoever happens to remember it. Twice now a
candidate gate has spent a review cycle on a red run that was a loaded machine,
and each time the diagnosis started from scratch.

This file is a record and a rule, not a test of the product. It asserts only
that the things it names still exist, so the record cannot rot into a list of
families nobody has.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]

NEXTEST_CONFIG = ROOT / ".config/nextest.toml"

# How to read a failure in one of these families.
#
# 1. Look at the machine before the change. A load average near or above the
#    core count is the first thing to rule out, not the last.
# 2. Run the family alone. These pass in isolation when the tree is sound.
# 3. Compare the failing test names across runs. A real regression fails the
#    same test every time; load fails whichever test lost the race, so the
#    names move. Different names on consecutive runs is close to conclusive.
# 4. If it still looks like yours, revert your change on a scratch copy and run
#    it again under the same load. That is the only step that settles it, and
#    it is cheap.
#
# What this rule is NOT: permission to ignore a failure. A family that fails in
# isolation, or fails the same test twice, is a defect and is treated as one.
READING_RULE_STEPS = 4

# The Rust side: nextest's budget lane. These spawn real binaries, real
# pseudoterminals and real process groups, and several assert timing.
BUDGET_LANE_MARKER = "BUDGET LANE"
DURABILITY_LANE_MARKER = "DURABILITY LANE"


class LoadSensitiveFamiliesTests(unittest.TestCase):
    def test_the_budget_lane_still_exists_and_is_declared_twice(self):
        """Once per profile, and the two are kept byte-identical on purpose."""
        config = NEXTEST_CONFIG.read_text()
        self.assertEqual(config.count(BUDGET_LANE_MARKER), 2)
        self.assertEqual(config.count(DURABILITY_LANE_MARKER), 2)
        self.assertIn("profile.default.overrides", config)
        self.assertIn("profile.offline.overrides", config)

    def test_the_budget_lane_carries_a_slow_timeout_rather_than_a_tight_one(self):
        """Its members are allowed to be slow; that is what the lane is for."""
        config = NEXTEST_CONFIG.read_text()
        periods = [int(match) for match in
                   re.findall(r'slow-timeout = \{ period = "(\d+)s"', config)]
        self.assertTrue(periods)
        self.assertGreaterEqual(max(periods), 120)

    def test_the_rule_for_reading_a_failure_is_written_down(self):
        """Four steps, in this file, so nobody re-derives them under pressure."""
        source = Path(__file__).read_text()
        for step in range(1, READING_RULE_STEPS + 1):
            self.assertIn(f"# {step}. ", source)
        self.assertIn("permission to ignore a failure", source)


if __name__ == "__main__":
    unittest.main()
