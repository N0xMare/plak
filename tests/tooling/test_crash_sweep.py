"""The standalone sweep prepares the same workers as the offline test gate."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

TEST_ENV = json.dumps(
    {
        "TMPDIR": "/tmp/plak-test",
        "PLAK_HOME": "/tmp/plak-test/home",
        "GIT_CONFIG_GLOBAL": "/tmp/plak-test/gitconfig",
        "GIT_CONFIG_NOSYSTEM": "1",
    }
)

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("crash_sweep", ROOT / "tools/dev/crash-sweep.py")
SWEEP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SWEEP)


class CrashSweepTests(unittest.TestCase):
    def run_sweep(self, scratch, results, args=None):
        if args is None:
            args = ["--strict", "--jobs", "8", "--shapes", "code_mode_cell_final"]
        with (
            patch("sys.argv", ["crash-sweep.py", *args]),
            patch.object(
                SWEEP.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, stdout=str(scratch) + "\n"),
            ),
            patch.object(SWEEP.subprocess, "check_output", return_value=TEST_ENV),
            patch.object(SWEEP.subprocess, "call", side_effect=results) as calls,
            patch.dict(
                "os.environ",
                {
                    "CARGO_TARGET_DIR": "custom-target",
                    "CARGO_BUILD_TARGET": "host-triple",
                    "PLAK_CRASH_SWEEP_SHAPES": "final_only",
                    "PLAK_TEST_SESSION_LEASE_SECONDS": "1",
                    "PLAK_TEST_CELL_HELLO_MS": "1",
                },
            ),
        ):
            return SWEEP.main(), calls.call_args_list

    def test_builds_workers_and_product_before_running_sweep_with_the_same_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [0, 0])
            self.assertEqual(code, 0)
            self.assertEqual(
                calls[0].args[0],
                ["cargo", "test", "--no-run", "--locked", "--workspace", "--all-features"],
            )
            self.assertEqual(
                calls[1].args[0],
                [
                    "cargo",
                    "test",
                    "--locked",
                    "--workspace",
                    "--all-features",
                    "--test",
                    "integration",
                    "crash_sweep::full_sweep",
                    "--",
                    "--ignored",
                    "--exact",
                    "--nocapture",
                ],
            )
            env = calls[0].kwargs["env"]
            self.assertEqual(env, calls[1].kwargs["env"])
            self.assertEqual(env["CARGO_TARGET_DIR"], "custom-target")
            self.assertEqual(env["CARGO_BUILD_TARGET"], "host-triple")
            self.assertEqual(env["PLAK_CRASH_SWEEP_STRICT"], "1")
            self.assertNotIn("PLAK_TEST_SESSION_LEASE_SECONDS", env)
            self.assertNotIn("PLAK_TEST_CELL_HELLO_MS", env)
            self.assertEqual(env["PLAK_CRASH_SWEEP_JOBS"], "8")
            self.assertEqual(env["PLAK_CRASH_SWEEP_SHAPES"], "code_mode_cell_final")

    def test_strict_default_defers_to_all_registered_shapes_even_with_inherited_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [0, 0, 0], ["--strict"])
            self.assertEqual(code, 0)
            for call in calls:
                self.assertNotIn("PLAK_CRASH_SWEEP_SHAPES", call.kwargs["env"])
                self.assertEqual(call.kwargs["env"]["PLAK_CRASH_SWEEP_STRICT"], "1")
            self.assertEqual(
                calls[-1].args[0][-4:],
                ["host_skeleton::crash::tui::tui_host_restart", "--", "--exact", "--nocapture"],
            )

    def test_terminal_shape_is_selected_and_repeated_without_the_generic_sweep(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(
                Path(directory),
                [0, 0, 0],
                ["--strict", "--shapes", "tui_host_restart", "--repeat", "2"],
            )
            self.assertEqual(code, 0)
            self.assertEqual(len(calls), 3)
            for call in calls[1:]:
                self.assertIn("host_skeleton::crash::tui::tui_host_restart", call.args[0])
                self.assertNotIn("crash_sweep::full_sweep", call.args[0])

    def test_terminal_shape_failure_is_not_reported_as_a_success(self):
        with tempfile.TemporaryDirectory() as directory:
            code, _ = self.run_sweep(Path(directory), [0, 42], ["--shapes", "tui_host_restart"])
            self.assertEqual(code, 42)

    def test_non_strict_default_keeps_the_quick_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [0, 0], [])
            self.assertEqual(code, 0)
            for call in calls:
                self.assertEqual(
                    call.kwargs["env"]["PLAK_CRASH_SWEEP_SHAPES"],
                    "final_only,direct_tool_then_final",
                )
                self.assertEqual(call.kwargs["env"]["PLAK_CRASH_SWEEP_STRICT"], "0")

    def test_explicit_non_strict_selection_overrides_the_quick_default(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(
                Path(directory), [0, 0], ["--shapes", "tree_refresh,code_mode_cell_final"]
            )
            self.assertEqual(code, 0)
            for call in calls:
                self.assertEqual(
                    call.kwargs["env"]["PLAK_CRASH_SWEEP_SHAPES"],
                    "tree_refresh,code_mode_cell_final",
                )

    def test_build_failure_never_runs_sweep_against_leftover_workers(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [37])
            self.assertEqual(code, 37)
            self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
