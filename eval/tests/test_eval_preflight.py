"""Zero-inference checks for the actual-cell executable gate."""

from __future__ import annotations

import asyncio
import json
import pathlib
import sys
import tempfile
import unittest
from types import SimpleNamespace

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "eval"))
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from harbor_adapter.preflight import PreflightError, probe_executable  # noqa: E402
from harbor_adapter.lifecycle import parse_sample, sample_cell  # noqa: E402
from test_eval_harbor_adapter import _installed_agent_with_harbor_stub  # noqa: E402


class _Cell:
    def __init__(self, code: int = 0, stdout: str = "plak 0.1.0\n"):
        self.code = code
        self.stdout = stdout
        self.commands: list[str] = []

    async def exec(self, *, command: str):
        self.commands.append(command)
        return SimpleNamespace(return_code=self.code, stdout=self.stdout, stderr="secret")


class PreflightTests(unittest.IsolatedAsyncioTestCase):
    async def test_version_runs_inside_cell_under_timeout(self):
        cell = _Cell()
        await probe_executable(cell, "/installed-agent/plak")
        self.assertIn("timeout -k 2s 15s /installed-agent/plak version", cell.commands[0])

    async def test_worker_starts_with_closed_stdin(self):
        cell = _Cell()
        await probe_executable(cell, "/installed-agent/plakous-js-host", version_argument=None)
        self.assertIn("plakous-js-host </dev/null", cell.commands[0])

    async def test_failure_does_not_publish_child_output(self):
        with self.assertRaises(PreflightError) as found:
            await probe_executable(_Cell(127, "secret"), "/installed-agent/plak")
        self.assertIn("returned 127", str(found.exception))
        self.assertNotIn("secret", str(found.exception))

    async def test_success_without_plak_version_is_refused(self):
        with self.assertRaises(PreflightError):
            await probe_executable(_Cell(0, ""), "/installed-agent/plak")


class LifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_secret_free_process_and_socket_sample(self):
        raw = (
            "PROCESSES\n123 1 123 S\n124 123 123 R\n"
            "SOCKETS\n1F90 98765\n"
            "malformed token private-command --key=secret\n"
        )
        record = parse_sample(raw, "after_agent_run")
        self.assertEqual(record["processes"][0],
                         {"pid": 123, "ppid": 1, "pgid": 123, "state": "S"})
        self.assertEqual(record["listen_sockets"], [{"port": 8080, "inode": 98765}])
        self.assertNotIn("secret", str(record))
        self.assertIn("observed_at_utc", record)

    async def test_failed_sample_is_explicitly_unavailable(self):
        class Broken:
            async def exec(self, *, command: str):
                raise RuntimeError("private data")

        record = await sample_cell(Broken(), "before_agent_run")
        self.assertTrue(record["unavailable"])
        self.assertNotIn("private data", str(record))


class VendorPreflightTests(unittest.IsolatedAsyncioTestCase):
    async def test_shell_preamble_is_inside_the_bounded_version_probe(self):
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            class Vendor:
                def get_version_command(self):
                    return "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex --version"

                def parse_version(self, output):
                    return output.strip()

            class Cell:
                command = ""

                async def exec(self, *, command):
                    self.command = command
                    return SimpleNamespace(return_code=0, stdout="0.159.3\n")

            cell = Cell()
            await module._assert_version(Vendor(), cell, "0.159.3")
            self.assertIn("timeout -k 2s 15s sh -c", cell.command)
            self.assertIn("codex --version", cell.command)

    async def test_vendor_identity_covers_installed_command_and_archive(self):
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            class Cell:
                commands: list[str]

                def __init__(self):
                    self.commands = []

                async def exec(self, *, command):
                    self.commands.append(command)
                    return SimpleNamespace(
                        return_code=0,
                        stdout="a" * 64 + "  /installed-agent/vendor/file\n42\n",
                    )

            with tempfile.TemporaryDirectory() as tmp:
                agent = SimpleNamespace(logs_dir=pathlib.Path(tmp))
                cell = Cell()
                await module._record_executable_identity(agent, cell, "codex", "amd64")
                record = json.loads((pathlib.Path(tmp) / "vendor-artifact.json").read_text())
                self.assertEqual(set(record["binaries"]),
                                 {"codex", "codex-archive", "node", "node-archive"})
                self.assertEqual(record["arch"], "amd64")
                self.assertEqual(record["version"], "0.159.3")

    async def test_vendor_identity_refuses_missing_bytes(self):
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            class Cell:
                async def exec(self, *, command):
                    return SimpleNamespace(return_code=127, stdout="", stderr="secret")

            with tempfile.TemporaryDirectory() as tmp:
                agent = SimpleNamespace(logs_dir=pathlib.Path(tmp))
                with self.assertRaises(module.VendorArtifactError) as found:
                    await module._record_executable_identity(agent, Cell(), "grok", "amd64")
                self.assertNotIn("secret", str(found.exception))

    async def test_transport_failure_is_structural(self):
        class Broken:
            async def exec(self, *, command: str):
                raise RuntimeError("secret")

        with self.assertRaises(PreflightError) as found:
            await probe_executable(Broken(), "/installed-agent/plak")
        self.assertNotIn("secret", str(found.exception))


if __name__ == "__main__":
    unittest.main()
