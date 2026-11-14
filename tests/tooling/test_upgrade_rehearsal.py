"""Isolation boundaries and strict observations of the opt-in binary rehearsal."""

import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


DRIVER = load("upgrade_rehearsal_test", "tools/dev/upgrade_rehearsal.py")
CASES = load("upgrade_rehearsal_cases_test", "tools/dev/upgrade_rehearsal_cases.py")


class UpgradeRehearsalTests(unittest.TestCase):
    def test_terminal_inventory_uses_only_readonly_lines_and_refuses_lost_or_consumed_jobs(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            (home / "host-id").write_text("scratch-host")
            endpoint = home / "builds/fixture/retention-v1.sock"
            endpoint.parent.mkdir(parents=True)
            endpoint.touch()
            job = {
                "number": 1,
                "session": "original-job",
                "terminal": True,
                "status": {"status": "exited", "code": 0},
                "output": "retained-rehearsal-output",
            }
            for jobs, floor, accepted in [([job], 2, True), ([], 2, False), ([job], 1, False)]:
                row = {
                    "origin": {"instance": "original-worker"},
                    "owner": {"line": "scratch-line"},
                    "next_number": floor,
                    "jobs": jobs,
                }
                replies = [
                    {"origin": {"instance": "original-worker"}},
                    {"answer": "ok"},
                    {
                        "id": "rehearsal-lines-0",
                        "answer": {"answer": "lines", "lines": [row], "next": None},
                    },
                ]
                payload = b""
                for reply in replies:
                    body = json.dumps(reply).encode()
                    payload += struct.pack(">I", len(body)) + body
                connection = MagicMock()
                connection.__enter__.return_value = connection
                connection.makefile.return_value = io.BytesIO(payload)
                with patch.object(CASES.socket, "socket", return_value=connection):
                    if accepted:
                        result = CASES.terminal_inventory(home, "scratch-line", DRIVER)
                        self.assertEqual(result[0]["session"], "original-job")
                        self.assertEqual(result[0]["output"], job["output"])
                    else:
                        with self.assertRaises(DRIVER.Refused):
                            CASES.terminal_inventory(home, "scratch-line", DRIVER)
                sent = [json.loads(call.args[0][4:]) for call in connection.sendall.call_args_list]
                self.assertEqual(len(sent), 2)
                self.assertEqual(sent[0]["version"], 1)
                self.assertEqual(sent[1]["request"], {"verb": "lines", "after": 0})
                self.assertIsNone(sent[1]["mutation"])

    def test_runtime_environment_does_not_forward_credentials_or_state_overrides(self):
        with tempfile.TemporaryDirectory() as temporary:
            case = Path(temporary).resolve()
            with patch.dict(
                os.environ,
                {
                    "OPENAI_API_KEY": "not-a-real-key",
                    "PLAK_HOME": "/operator",
                    "PLAK_TEST_STATE_KEY": "foreign",
                    "XDG_CONFIG_HOME": "/operator-config",
                },
            ):
                env = DRIVER.environment(case)
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("PLAK_TEST_STATE_KEY", env)
            for key in [
                "HOME",
                "PLAK_HOME",
                "TMPDIR",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "XDG_STATE_HOME",
                "XDG_RUNTIME_DIR",
            ]:
                self.assertIn(case, Path(env[key]).parents, key)
                self.assertEqual(Path(env[key]).stat().st_mode & 0o077, 0, key)
            self.assertEqual(env["PLAK_TEST_MODEL"], "offline-loopback-v1")
            self.assertEqual(Path(env["GIT_CONFIG_GLOBAL"]).read_text(), "")

    def test_existing_foreign_scratch_directory_is_not_adopted_or_removed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "owned").mkdir()
            sentinel = root / "owned/foreign.txt"
            sentinel.write_text("leave me")
            with self.assertRaisesRegex(DRIVER.Refused, "unrecognized"):
                DRIVER.prepare_root(root)
            self.assertEqual(sentinel.read_text(), "leave me")

    def test_scratch_root_cannot_overlap_the_operator_home(self):
        with tempfile.TemporaryDirectory() as temporary:
            operator = Path(temporary).resolve() / "operator"
            operator.mkdir()
            with patch.dict(os.environ, {"PLAK_HOME": str(operator)}):
                with self.assertRaisesRegex(DRIVER.Refused, "operator home"):
                    DRIVER.prepare_root(operator / "nested")
            self.assertEqual(list(operator.iterdir()), [])

    def test_symlink_root_is_refused_without_writing_through_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary).resolve()
            target = parent / "target"
            target.mkdir()
            alias = parent / "alias"
            alias.symlink_to(target, target_is_directory=True)
            with self.assertRaisesRegex(DRIVER.Refused, "symlinks"):
                DRIVER.prepare_root(alias)
            self.assertEqual(list(target.iterdir()), [])

    def test_no_subcommand_is_a_usage_error_not_an_action(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = subprocess.run(
                [sys.executable, str(ROOT / "tools/dev/upgrade_rehearsal.py")],
                cwd=temporary,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_child_history_ignores_only_presentation_entry_keys(self):
        first = [
            {
                "entry_id": "old-checkpoint-entry",
                "kind": "text",
                "text": "result",
                "tool_calls": 1,
                "tool_results": 1,
            }
        ]
        second = json.loads(json.dumps(first))
        second[0]["entry_id"] = "new-checkpoint-entry"
        self.assertEqual(CASES.history_content(first), CASES.history_content(second))
        for field, value in [("text", "lost result"), ("tool_calls", 0), ("tool_results", 0)]:
            changed = json.loads(json.dumps(second))
            changed[0][field] = value
            self.assertNotEqual(CASES.history_content(first), CASES.history_content(changed), field)
        self.assertNotEqual(CASES.history_content(first), CASES.history_content(first + first))

    def test_source_probe_shape_refuses_unknown_destination_capability_before_bootstrap(self):
        source = {
            "protocol": 3,
            "receipt_version": 1,
            "binary_id": "fixture",
            "state_key": "r27.e37.p37.t10",
            "export_versions": [12, 11],
            "host_available": True,
            "runner_protocol": 8,
            "runner_version": {"protocol": 8, "source_revision": "old", "flavor": "maintainer"},
            "config_error": None,
        }
        destination = json.loads(json.dumps(source))
        destination.update(state_key="r28.e38.p38.t11", export_versions=[13, 12])
        DRIVER.check_probe_shape(source, destination)
        for field, value, detail in [
            ("live_move_protocol", 1, "unknown fields.*live_move_protocol"),
            ("protocol", True, "probe.protocol: expected u32"),
            ("config_error", 1, "probe.config_error: expected null/string"),
        ]:
            changed = dict(destination, **{field: value})
            with self.subTest(field=field), self.assertRaisesRegex(DRIVER.Refused, detail):
                DRIVER.check_probe_shape(source, changed)
        destination.pop("config_error")
        DRIVER.check_probe_shape(source, destination)
        destination["runner_version"]["unexpected"] = 1
        with self.assertRaisesRegex(DRIVER.Refused, "runner_version: source rejects unknown"):
            DRIVER.check_probe_shape(source, destination)
        destination.pop("runner_protocol")
        with self.assertRaisesRegex(DRIVER.Refused, "missing source fields.*runner_protocol"):
            DRIVER.check_probe_shape(source, destination)

    def test_failed_case_is_not_presented_as_qualified(self):
        report = {
            "images": {name: {"revision": name, "binary_id": "fixture"} for name in ["A", "B"]},
            "increment4": "live cases omitted",
            "cases": [{"case": "repro-child", "status": "failed", "error": "grant is missing"}],
        }
        summary = CASES.operator_summary(report)
        self.assertIn("NOT QUALIFIED", summary)
        self.assertIn("grant is missing", summary)
        self.assertIn("simulated old key", summary)

    def test_zero_selected_cases_refuses_instead_of_qualifying_nothing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            images = {}
            for name in ["A", "B"]:
                binary = root / "owned/images" / name / "plak"
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b"not executed")
                images[name] = {
                    "binary": str(binary),
                    "sha256": DRIVER.hashlib.sha256(binary.read_bytes()).hexdigest(),
                }
            DRIVER.write_json(root / "builds.json", images)
            with self.assertRaisesRegex(DRIVER.Refused, "zero"):
                CASES.rehearse(SimpleNamespace(arm="all", case=["no-such-case"]), root, DRIVER)

    def test_all_removes_build_artifacts_even_when_rehearsal_refuses(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "owned").mkdir()
            (root / "owned/build-artifact").touch()

            def refuse(*_):
                raise DRIVER.Refused("not qualified")

            with (
                patch.object(DRIVER, "build"),
                patch.object(DRIVER, "owned_processes", return_value={}),
                patch.dict(
                    sys.modules, {"upgrade_rehearsal_cases": SimpleNamespace(rehearse=refuse)}
                ),
            ):
                with self.assertRaisesRegex(DRIVER.Refused, "not qualified"):
                    DRIVER.execute(SimpleNamespace(command="all", keep_builds=False), root)
            self.assertFalse((root / "owned").exists())

    @unittest.skipUnless(sys.platform == "linux", "the real-binary process guard is Linux-only")
    def test_cleanup_stops_owned_processes_and_preserves_a_foreign_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary).resolve()
            case = parent / "case"
            case.mkdir()
            command = [sys.executable, "-c", "import time; time.sleep(60)"]
            own = subprocess.Popen(command, cwd=case, start_new_session=True)
            foreign = subprocess.Popen(command, cwd=parent, start_new_session=True)
            try:
                DRIVER.wait_for(
                    lambda: own.pid in DRIVER.owned_processes(case), "owned process observed"
                )
                DRIVER.cleanup(case, [own])
                self.assertIsNotNone(own.poll())
                self.assertIsNone(foreign.poll())
                self.assertFalse(case.exists())
            finally:
                for child in [own, foreign]:
                    if child.poll() is None:
                        os.killpg(child.pid, signal.SIGKILL)
                    child.wait()


if __name__ == "__main__":
    unittest.main()
