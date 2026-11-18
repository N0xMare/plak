"""Real Unix socket paths and retained evidence, without a binary or inference."""

import json
import os
from pathlib import Path
import shutil
import socket
import stat
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/live"))
import live
import qualification


class LiveHomeTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.artifacts = Path(directory.name).resolve() / ("checkout-" + "x" * 100) / "target/live"
        self.homes = []
        self.addCleanup(self.remove_homes)

    def remove_homes(self):
        for home in self.homes:
            shutil.rmtree(home, ignore_errors=True)

    def allocate(self, artifacts, name="main"):
        home = live.scratch_home(artifacts, name)
        self.homes.append(home)
        return home

    def test_every_route_lane_and_attempt_fits_both_platforms_with_current_formats(self):
        self.assertEqual(live.socket_layout(), ("r28.e39.p39.t11", 9, 1))
        seen = set()
        for route in live.ROUTES:
            for lane in qualification.LANES:
                for attempt in (1, 2):
                    artifacts = (
                        self.artifacts
                        / f"qualification-{lane}-{route}-XXXXXXXX"
                        / f"attempt-{attempt}"
                    )
                    for label in ("main", "scenario-" + "y" * 100 + "/second-stage"):
                        with self.subTest(route=route, lane=lane, attempt=attempt, label=label):
                            home = self.allocate(artifacts, label)
                            self.assertNotIn(home, seen)
                            seen.add(home)
                            self.assertEqual(home.parent, Path("/tmp").resolve())
                            self.assertEqual(home, home.resolve())
                            self.assertEqual(stat.S_IMODE(home.stat().st_mode), 0o700)
                            self.assertEqual(home.stat().st_uid, os.getuid())
                            for path in live.socket_paths(home):
                                for limit in (104, 108):
                                    self.assertLessEqual(len(os.fsencode(path)) + 1 + 8, limit)
                            self.assertEqual(live.scratch_home(artifacts, label), home)
                            self.assertEqual(live.scratch_homes(artifacts)[label], str(home))

    def test_real_host_and_runner_sockets_bind_from_retained_home(self):
        home = self.allocate(self.artifacts)
        for path in live.socket_paths(home):
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(path))
                self.assertTrue(stat.S_ISSOCK(path.stat().st_mode))
        self.assertEqual(live.scratch_homes(self.artifacts), {"main": str(home)})
        self.assertEqual(stat.S_IMODE((self.artifacts / "homes.json").stat().st_mode), 0o600)

    def test_overlong_temp_root_refuses_cell_without_paid_launch_or_rerun(self):
        long_home = self.artifacts / "too-long-home"
        long_home.mkdir(parents=True, mode=0o700)
        paid = Mock()

        def attempt(path):
            live.scratch_home(path)
            paid()
            return 0, {}

        with patch("live.tempfile.mkdtemp", return_value=str(long_home)):
            result = qualification.run_cell(self.artifacts / "cell", attempt)
        paid.assert_not_called()
        self.assertEqual(result["outcome"], live.HARNESS)
        self.assertEqual(len(result["attempts"]), 1)
        self.assertIn("SUN_LEN", result["attempts"][0]["reason"])
        self.assertIn("no paid attempt", result["attempts"][0]["reason"])
        report = json.loads((self.artifacts / "cell/attempt-1/report.json").read_text())
        self.assertIn("shorter physical system temp root", report["pre_attempt_error"])
        self.assertFalse(long_home.exists())

    def test_byte_length_not_character_count_and_live_state_key_override(self):
        home = Path("/tmp").resolve() / ("é" * 25)
        self.assertLess(len(str(max(live.socket_paths(home), key=lambda p: len(str(p))))) + 9, 104)
        with self.assertRaises(live.ScenarioFailure):
            live.check_socket_home(home)
        with patch("live.plak_child_source", return_value={"PLAK_TEST_STATE_KEY": "r" * 100}):
            with self.assertRaises(live.ScenarioFailure):
                self.allocate(self.artifacts)

    def test_reused_home_refuses_public_directory_or_symlink(self):
        home = self.allocate(self.artifacts)
        home.chmod(0o755)
        with self.assertRaises(live.ScenarioFailure):
            live.scratch_home(self.artifacts)
        home.chmod(0o700)
        link = self.artifacts / "linked"
        link.symlink_to(home, target_is_directory=True)
        (self.artifacts / "homes.json").write_text(json.dumps({"main": str(link)}))
        with self.assertRaises(live.ScenarioFailure):
            live.scratch_home(self.artifacts)

    def test_usage_follows_all_scenario_stage_homes_not_artifact_layout(self):
        for label, tokens in (("case", 2), ("case/second", 3), ("unrelated", 100)):
            home = self.allocate(self.artifacts, label)
            logs = home / "state/r/sessions"
            logs.mkdir(parents=True)
            (logs / "s.events.jsonl").write_text(
                json.dumps({"kind": "turn.usage", "input_tokens": tokens}) + "\n"
            )
        self.assertEqual(live.scenario_home_usage(self.artifacts, "case").input_tokens, 5)


if __name__ == "__main__":
    unittest.main()
