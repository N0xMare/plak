"""The standalone sweep prepares the same workers as the offline test gate."""

import importlib.util
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
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
UNSET_NAMES = subprocess.check_output(
    ["bash", str(ROOT / "tools/gate/test-env.sh"), "--unset-names"], text=True
)


def test_env_output(command, **kwargs):
    return UNSET_NAMES if command[-1] == "--unset-names" else TEST_ENV


SPEC = importlib.util.spec_from_file_location("crash_sweep", ROOT / "tools/dev/crash-sweep.py")
SWEEP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SWEEP)


class CrashSweepTests(unittest.TestCase):
    def run_sweep(self, scratch, results, args=None, listed_groups=16, missing_binary=None):
        if args is None:
            args = ["--strict", "--jobs", "8", "--shapes", "code_mode_cell_final"]

        build_calls = []

        def run(command, **kwargs):
            if command[:3] == ["cargo", "test", "--no-run"]:
                build_calls.append(unittest.mock.call(command, **kwargs))
                rows = []
                for binary in ("plakous-py-host", "plakous-js-host", "mcp-fake-server", "plakd"):
                    if binary == missing_binary:
                        continue
                    executable = scratch / "custom target" / binary
                    executable.parent.mkdir(exist_ok=True)
                    executable.touch()
                    executable.chmod(0o700)
                    for is_test in (True, False):
                        rows.append(
                            json.dumps(
                                {
                                    "reason": "compiler-artifact",
                                    "target": {"name": binary, "kind": ["bin"]},
                                    "profile": {"test": is_test},
                                    "executable": str(executable) + ("-harness" if is_test else ""),
                                }
                            )
                        )
                return subprocess.CompletedProcess(command, results[0], stdout="\n".join(rows))
            if command[-1] != "--list":
                return subprocess.CompletedProcess(command, 0, stdout=str(scratch) + "\n")
            selector = command[command.index("--") - 1]
            rows = [
                f"{selector}{kind}_{group}: test"
                for kind in ["root", "child"]
                for group in range(8)
            ]
            return subprocess.CompletedProcess(command, 0, stdout="\n".join(rows[:listed_groups]))

        with (
            patch("sys.argv", ["crash-sweep.py", *args]),
            patch.object(
                SWEEP.subprocess,
                "run",
                side_effect=run,
            ),
            patch.object(SWEEP.subprocess, "check_output", side_effect=test_env_output),
            patch.object(SWEEP.subprocess, "call", side_effect=results[1:]) as calls,
            patch.dict(
                "os.environ",
                {
                    "CARGO_TARGET_DIR": "custom-target",
                    "CARGO_BUILD_TARGET": "host-triple",
                    "PLAK_CRASH_SWEEP_SHAPES": "final_only",
                    "PLAK_TEST_SESSION_LEASE_SECONDS": "1",
                    "PLAK_TEST_CELL_HELLO_MS": "1",
                    "PLAK_TEST_HOST_IDLE_EXIT_SECONDS": "1",
                    "OPENAI_API_KEY": "leaked",
                    "PLAK_PY_HOST": "stale-worker",
                    "PLAK_JS_HOST": "stale-worker",
                    "PLAK_MCP_SERVER": "stale-server",
                    "PLAK_TEST_BINARY": "stale-product",
                },
            ),
        ):
            return SWEEP.main(), build_calls + calls.call_args_list

    def test_builds_workers_and_product_before_running_sweep_with_the_same_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(Path(directory), [0, 0])
            self.assertEqual(code, 0)
            self.assertEqual(
                calls[0].args[0],
                [
                    "cargo",
                    "test",
                    "--no-run",
                    "--locked",
                    "--workspace",
                    "--all-features",
                    "--message-format=json",
                ],
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
            for variable, binary in (
                ("PLAK_PY_HOST", "plakous-py-host"),
                ("PLAK_JS_HOST", "plakous-js-host"),
                ("PLAK_MCP_SERVER", "mcp-fake-server"),
                ("PLAK_TEST_BINARY", "plakd"),
            ):
                self.assertEqual(
                    env[variable], str(Path(directory).resolve() / "custom target" / binary)
                )
            self.assertEqual(env["CARGO_TARGET_DIR"], "custom-target")
            self.assertEqual(env["CARGO_BUILD_TARGET"], "host-triple")
            self.assertEqual(env["PLAK_CRASH_SWEEP_STRICT"], "1")
            self.assertNotIn("PLAK_TEST_SESSION_LEASE_SECONDS", env)
            self.assertNotIn("PLAK_TEST_CELL_HELLO_MS", env)
            self.assertNotIn("PLAK_TEST_HOST_IDLE_EXIT_SECONDS", env)
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertEqual(env["PLAK_CRASH_SWEEP_JOBS"], "8")
            self.assertEqual(env["PLAK_CRASH_SWEEP_SHAPES"], "code_mode_cell_final")

    def test_strict_default_defers_to_all_registered_shapes_even_with_inherited_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(
                Path(directory), [0] * (len(SWEEP.TERMINAL_SHAPES) + 2), ["--strict"]
            )
            self.assertEqual(code, 0)
            for call in calls:
                self.assertNotIn("PLAK_CRASH_SWEEP_SHAPES", call.kwargs["env"])
                self.assertEqual(call.kwargs["env"]["PLAK_CRASH_SWEEP_STRICT"], "1")
            self.assertEqual(
                [call.args[0][call.args[0].index("--") - 1] for call in calls[2:]],
                list(SWEEP.TERMINAL_SHAPES.values()),
            )

    def test_terminal_shape_is_selected_and_repeated_without_the_generic_sweep(self):
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(
                Path(directory),
                [0, 0, 0],
                ["--strict", "--shapes", "wire_host_restart", "--repeat", "2"],
            )
            self.assertEqual(code, 0)
            self.assertEqual(len(calls), 3)
            for call in calls[1:]:
                self.assertIn("host_skeleton::crash::wire::cuts::wire_host_restart::", call.args[0])
                self.assertNotIn("crash_sweep::full_sweep", call.args[0])

    def test_strict_restart_families_select_all_groups_instead_of_zero_exact_matches(self):
        shapes = [
            "wire_host_restart",
            "wire_host_restart_switch",
        ]
        with tempfile.TemporaryDirectory() as directory:
            code, calls = self.run_sweep(
                Path(directory), [0] * 3, ["--strict", "--jobs", "2", "--shapes", ",".join(shapes)]
            )
            self.assertEqual(code, 0)
            self.assertEqual(len(calls), 3)
            for shape, call in zip(shapes, calls[1:], strict=True):
                selector = call.args[0][call.args[0].index("--") - 1]
                self.assertTrue(selector.endswith(f"::{shape}::"))
                self.assertNotIn("--exact", call.args[0])
                self.assertEqual(call.args[0][call.args[0].index("--test-threads") + 1], "2")

    def test_strict_restart_family_refuses_empty_or_incomplete_test_listings(self):
        for count in [0, 15]:
            with self.subTest(groups=count), tempfile.TemporaryDirectory() as directory:
                code, calls = self.run_sweep(
                    Path(directory),
                    [0],
                    ["--strict", "--shapes", "wire_host_restart"],
                    listed_groups=count,
                )
                self.assertEqual(code, 1)
                self.assertEqual(
                    len(calls), 1, "no incomplete family may be reported as a successful sweep"
                )

    def test_terminal_shape_failure_is_not_reported_as_a_success(self):
        with tempfile.TemporaryDirectory() as directory:
            code, _ = self.run_sweep(Path(directory), [0, 42], ["--shapes", "wire_host_restart"])
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

    def test_missing_fixture_record_never_starts_a_sweep(self):
        for binary in ('plakous-py-host', 'plakous-js-host', 'mcp-fake-server', 'plakd'):
            with self.subTest(binary=binary), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(SystemExit, f'built {binary} executable'):
                    self.run_sweep(Path(directory), [0], missing_binary=binary)

    def test_invalid_shard_and_merge_args_are_refused_before_running_tools(self):
        cases = (
            (["--shard", "1/2", "--output", "path"], {}, "requires --strict"),
            (["--strict", "--shard", "0/2", "--output", "path"], {}, "1-based"),
            (["--strict", "--shard", "1/2"], {}, "require --output"),
            (["--strict", "--shard", "1/2", "--repeat", "2", "--output", "path"], {}, "one repeat"),
            (["--merge", "receipt", "--output", "path"], {}, "requires --expect-commit"),
            (
                ["--strict", "--shard", "1/2", "--output", "path"],
                {"PLAK_TEST_WORKING_PRUNE_MARGIN": "1"},
                "default pruning",
            ),
        )
        for argv, env, reason in cases:
            errors = StringIO()
            with (
                self.subTest(argv=argv),
                patch("sys.argv", ["crash-sweep.py", *argv]),
                patch.dict("os.environ", env, clear=True),
                patch.object(SWEEP.subprocess, "check_output") as subprocess_call,
                redirect_stderr(errors),
                self.assertRaises(SystemExit) as error,
            ):
                SWEEP.main()
            self.assertEqual(error.exception.code, 2)
            self.assertIn(reason, errors.getvalue())
            subprocess_call.assert_not_called()

    def test_refuses_to_replace_existing_receipt(self):
        with tempfile.TemporaryDirectory() as scratch:
            output = Path(scratch) / "receipt.json.gz"
            output.write_text("prior evidence")
            with (
                patch.object(SWEEP, "clean_checkout") as checkout,
                self.assertRaisesRegex(ValueError, "already exists"),
            ):
                SWEEP.run_shard({}, SimpleNamespace(output=output), (1, 2))
            checkout.assert_not_called()
            self.assertEqual(output.read_text(), "prior evidence")

    def test_uses_rustc_host_triple_if_target_is_not_set(self):
        with patch.object(
            SWEEP.subprocess, "check_output", return_value="host: aarch64-apple-darwin\n"
        ) as rustc:
            self.assertEqual(SWEEP.build_target({}), "aarch64-apple-darwin")
            rustc.assert_called_once()
        self.assertEqual(
            SWEEP.build_target({"CARGO_BUILD_TARGET": "x86_64-unknown-linux-musl"}),
            "x86_64-unknown-linux-musl",
        )

    def test_streamed_output_is_kept_and_checks_result_lines(self):
        lines = [
            "test result: ok. 0 passed; 0 failed; 0 ignored\n",
            "test result: ok. 16 passed; 0 failed; 0 ignored\n",
        ]
        command = [*SWEEP.COMMAND, "test_name", "--"]
        with (
            patch.object(SWEEP.subprocess, "Popen") as process,
            redirect_stdout(StringIO()) as output,
        ):
            child = process.return_value.__enter__.return_value
            child.stdout = lines
            child.wait.return_value = 0
            self.assertEqual(SWEEP.cargo_evidence(command, {}, 16), "".join(lines))
            self.assertEqual(output.getvalue(), "".join(lines))
        for log, status in (("".join(lines), 101), (lines[0], 0)):
            with self.assertRaisesRegex(ValueError, "expected 16 successful tests"):
                SWEEP.require_pass_count(log, status, 16, "test_name")

    def test_wire_completion_line_can_follow_concurrent_test_output(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "fixture row.jsonl"
            row = {"invariants": True, "store": "tree", "cut": 1}
            path.write_text(json.dumps(row) + "\n")
            prefix = "test cuts::root_0 ... wire_host_restart: child=false group=0:"
            groups = SWEEP.wire_rows("wire_host_restart", f"{prefix} 1 cuts; {path}\n")
            self.assertEqual(
                groups, [dict(family="wire_host_restart", child=0, group=0, rows=[row])]
            )
            with self.assertRaisesRegex(ValueError, "row count disagrees"):
                SWEEP.wire_rows("wire_host_restart", f"{prefix} 2 cuts; {path}\n")


if __name__ == "__main__":
    unittest.main()
