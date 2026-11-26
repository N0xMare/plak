"""Host sizing must preserve lane coverage, strict separation and watchdogs."""

from pathlib import Path
import sys
import tempfile
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
                ("durability", [quarter], [90]),
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

    def test_cgroup_v2_quota_walks_ancestors_and_ignores_max(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            proc_cgroup = root / "cgroup"
            proc_cgroup.write_text("0::/user.slice/child\n", encoding="utf-8")
            cg = root / "sys"
            (cg / "user.slice/child").mkdir(parents=True)
            (cg / "cpu.max").write_text("max 100000\n", encoding="utf-8")
            (cg / "user.slice/cpu.max").write_text("400000 100000\n", encoding="utf-8")
            (cg / "user.slice/child/cpu.max").write_text("600000 100000\n", encoding="utf-8")
            self.assertEqual(layout._cgroup_cpu_quota(proc_cgroup, cg), 4)

            (cg / "user.slice/cpu.max").write_text("max 100000\n", encoding="utf-8")
            (cg / "user.slice/child/cpu.max").write_text("50000 100000\n", encoding="utf-8")
            self.assertEqual(layout._cgroup_cpu_quota(proc_cgroup, cg), 1)

            (cg / "user.slice/child/cpu.max").write_text("max 100000\n", encoding="utf-8")
            self.assertIsNone(layout._cgroup_cpu_quota(proc_cgroup, cg))

    def test_cgroup_v1_quota_respects_positive_quota_and_skips_unlimited(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            proc_cgroup = root / "cgroup"
            proc_cgroup.write_text("4:cpu,cpuacct:/box\n", encoding="utf-8")
            cg = root / "sys"
            (cg / "cpu/box").mkdir(parents=True)
            (cg / "cpu/cpu.cfs_quota_us").write_text("-1\n", encoding="utf-8")
            (cg / "cpu/cpu.cfs_period_us").write_text("100000\n", encoding="utf-8")
            (cg / "cpu/box/cpu.cfs_quota_us").write_text("300000\n", encoding="utf-8")
            (cg / "cpu/box/cpu.cfs_period_us").write_text("100000\n", encoding="utf-8")
            self.assertEqual(layout._cgroup_cpu_quota(proc_cgroup, cg), 3)

            (cg / "cpu/box/cpu.cfs_quota_us").write_text("-1\n", encoding="utf-8")
            self.assertIsNone(layout._cgroup_cpu_quota(proc_cgroup, cg))


if __name__ == "__main__":
    unittest.main()
