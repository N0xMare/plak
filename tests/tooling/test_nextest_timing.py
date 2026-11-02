"""tools/perf/nextest_timing.py over synthetic nextest JUnit reports."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/tooling/fixtures/nextest_timing"
SPEC = importlib.util.spec_from_file_location(
    "nextest_timing", ROOT / "tools/perf/nextest_timing.py"
)
assert SPEC is not None and SPEC.loader is not None
timing = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = timing
SPEC.loader.exec_module(timing)


class NextestTimingTests(unittest.TestCase):
    def test_every_lane_profile_writes_junit_under_the_nextest_store(self):
        config = tomllib.loads((ROOT / ".config/nextest.toml").read_text())
        self.assertEqual(config["store"]["dir"], "target/nextest")
        for lane in timing.LANES:
            with self.subTest(lane=lane):
                self.assertEqual(config["profile"][lane]["junit"], {"path": "junit.xml"})

    def test_parses_cases_binaries_packages_failures_and_timestamps(self):
        lane = timing.parse("offline", FIXTURE / "offline/junit.xml")
        self.assertEqual(lane.wall, 10.0)
        self.assertEqual(
            [test.name for test in lane.tests],
            [
                "native_auth::tests::fast",
                "native_auth::tests::slow",
                "offline_e2e::longest",
                "cli_contract::quick",
            ],
        )
        self.assertEqual({test.package for test in lane.tests}, {"plakous-xai", "plak"})
        self.assertEqual([test.failed for test in lane.tests], [False, False, True, False])
        span, last = timing.test_span(lane)
        self.assertAlmostEqual(span, 9.5)
        self.assertEqual(last.name, "offline_e2e::longest")

    def test_report_names_slowest_tests_totals_and_lane_paths(self):
        lanes = [timing.parse(name, FIXTURE / name / "junit.xml") for name in ("offline", "serial")]
        lines = timing.report(lanes, 2)
        self.assertIn(
            "lane offline: 4 tests, wall 10.0 s, summed 16.0 s, parallelism 1.6x, 1 failed, 1 flaky, "
            "1 retries",
            lines,
        )
        self.assertIn(
            "  test span: 9.5 s from the first start, ending with plak::integration offline_e2e::longest",
            lines,
        )
        self.assertIn(
            "  test span: no per-test timestamps; at least 4.0 s (plak::bin/plak budget::one)",
            lines,
        )
        slowest = lines.index("slowest 2 tests:")
        self.assertEqual(lines[slowest + 1].split()[:3], ["8.50", "s", "offline"])
        self.assertEqual(lines[slowest + 2].split()[-1], "native_auth::tests::slow")
        self.assertIn("top 2 binaries by summed test time:", lines)
        packages = lines.index("top 2 packages by summed test time:")
        self.assertTrue(lines[packages + 1].endswith(" plak"))
        self.assertIn("16.5 s", lines[packages + 1])

    def test_a_trimmed_real_nextest_report_parses(self):
        lane = timing.parse("offline", FIXTURE / "real-offline/junit.xml")
        self.assertEqual(lane.wall, 166.044)
        self.assertEqual(len(lane.tests), 6)
        self.assertEqual(
            {test.binary for test in lane.tests}, {"plak", "plak::integration", "plakous-xai"}
        )
        self.assertEqual({test.package for test in lane.tests}, {"plak", "plakous-xai"})
        self.assertTrue(all(test.started is not None and test.seconds > 0 for test in lane.tests))
        self.assertFalse(any(test.failed or test.retries for test in lane.tests))
        self.assertTrue(timing.report([lane], 3))

    def test_missing_lanes_are_noted_and_no_report_at_all_fails(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(timing.main(["--dir", str(FIXTURE), "durability", "serial"]), 0)
        self.assertIn("lane durability: no JUnit report", output.getvalue())
        self.assertIn("lane serial: 2 tests", output.getvalue())
        with tempfile.TemporaryDirectory() as empty, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(timing.main(["--dir", empty]), 1)


if __name__ == "__main__":
    unittest.main()
