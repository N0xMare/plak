"""The standalone sweep prepares the same workers as the offline test gate."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("crash_sweep", ROOT / "scripts/crash-sweep.py")
SWEEP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SWEEP)


class CrashSweepTests(unittest.TestCase):
    def run_sweep(self, scratch, results):
        with patch("sys.argv", ["crash-sweep.py", "--strict", "--jobs", "8",
                                "--shapes", "code_mode_cell_final"]), \
                patch.object(SWEEP.subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, stdout=str(scratch) + "\n")), \
                patch.object(SWEEP.subprocess, "call", side_effect=results) as calls, \
                patch.dict("os.environ", {"CARGO_TARGET_DIR": "custom-target",
                                          "CARGO_BUILD_TARGET": "host-triple"}):
            return SWEEP.main(), calls.call_args_list

    def test_builds_workers_and_product_before_running_sweep_with_the_same_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [0, 0])
            self.assertEqual(code, 0)
            self.assertEqual(calls[0].args[0], ["cargo", "test", "--no-run", "--locked",
                                             "--workspace", "--all-features"])
            self.assertEqual(calls[1].args[0], ["cargo", "test", "--locked", "-p", "plak",
                                             "--all-features", "--test", "integration",
                                             "crash_sweep::full_sweep", "--", "--ignored",
                                             "--exact", "--nocapture"])
            env = calls[0].kwargs["env"]
            self.assertEqual(env, calls[1].kwargs["env"])
            self.assertEqual(env["CARGO_TARGET_DIR"], "custom-target")
            self.assertEqual(env["CARGO_BUILD_TARGET"], "host-triple")
            self.assertEqual(env["PLAK_CRASH_SWEEP_STRICT"], "1")
            self.assertEqual(env["PLAK_CRASH_SWEEP_JOBS"], "8")
            self.assertEqual(env["PLAK_CRASH_SWEEP_SHAPES"], "code_mode_cell_final")

    def test_build_failure_never_runs_sweep_against_leftover_workers(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [37])
            self.assertEqual(code, 37)
            self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
