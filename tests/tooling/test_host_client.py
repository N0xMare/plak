"""The guide's stdlib reference client against a real, offline-scripted host."""

import importlib.util
import json
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[2]
EXAMPLE = ROOT / "tools/examples/host_client.py"
spec = importlib.util.spec_from_file_location("host_client", EXAMPLE)
host_client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(host_client)


class ReferenceClientTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # The tooling gate supplies Cargo's actual all-feature test artifact,
        # not a guessed debug path or an operator's installed binary.
        cls.binary = Path(os.environ["PLAK_TOOLING_BINARY"])

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="hc-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        home = self.root / "p"
        agents = home / "agents"
        agents.mkdir(parents=True, mode=0o700)
        home.chmod(0o700)
        (home / "config.toml").write_text(
            '[root]\nagent="lead"\n[orchestration]\nsubagents=false\n'
        )
        (agents / "lead.toml").write_text(
            'account="codex"\nmodel="offline-loopback-v1"\neffort="medium"\ncomposition="direct"\n'
        )
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        script = self.root / "script.json"
        script.write_text('{"version":1,"steps":[]}')
        scratch = self.root / "tmp"
        scratch.mkdir()
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.root / "fake-home"),
            "PLAK_HOME": str(home),
            "TMPDIR": str(scratch),
            "GIT_CONFIG_GLOBAL": os.environ["GIT_CONFIG_GLOBAL"],
            "GIT_CONFIG_NOSYSTEM": "1",
            "PLAK_TEST_RUNNER_DETACHED": "1",
            "PLAK_TEST_MODEL": "offline-loopback-v1",
            "PLAK_TEST_SCRIPT": str(script),
            "PLAK_TEST_EVIDENCE": str(self.root / "evidence.jsonl"),
        }
        for name in ("PLAK_PY_HOST", "PLAK_JS_HOST"):
            self.env[name] = os.environ[name]
        self.host = subprocess.Popen(
            [self.binary, "host"],
            cwd=self.workspace,
            env=self.env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self.addCleanup(self.stop_host)
        self.lines = queue.Queue()

        def drain():
            for line in self.host.stderr:
                self.lines.put(line)
            self.lines.put(None)

        self.drain = threading.Thread(target=drain, daemon=True)
        self.drain.start()
        while True:
            line = self.lines.get(timeout=15)
            self.assertIsNotNone(line, "host exited before serving")
            if line.startswith("plak host: serving on "):
                self.socket = line.removeprefix("plak host: serving on ").split(" (host ")[0]
                break

    def stop_host(self):
        if self.host.poll() is None:
            self.host.terminate()
            try:
                self.host.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(self.host.pid, signal.SIGKILL)
                self.host.wait(timeout=5)
        self.drain.join(timeout=5)
        self.host.stderr.close()

    def create_session(self):
        with host_client.Client(self.socket) as client:
            welcome = client.hello()
            self.assertNotIn("session", welcome)
            answer = client.request(
                1,
                {
                    "op": "new_session",
                    "runner": {"runner_id": "local", "name": "local"},
                    "workspace": str(self.workspace),
                    "request_id": str(uuid.uuid4()),
                },
            )
            return answer["session"]

    def test_guide_client_reads_a_real_attach_snapshot_without_a_turn(self):
        session = self.create_session()
        result = subprocess.run(
            [sys.executable, EXAMPLE, "--socket", self.socket, "--session", session],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        frames = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual(frames[0]["proto"], host_client.WIRE_VERSION)
        answers = [
            frame["outcome"]["ok"]["answer"] for frame in frames if frame["frame"] == "response"
        ]
        self.assertEqual(answers, ["sessions", "attached"])
        self.assertEqual(frames[-1]["frame"], "snapshot")
        self.assertTrue(frames[-1]["last"])
        evidence = self.root / "evidence.jsonl"
        self.assertFalse(evidence.exists() and evidence.read_text().strip())
        schema = subprocess.run(
            [self.binary, "wire", "schema"],
            env=self.env,
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(json.loads(schema.stdout)["wire_version"], host_client.WIRE_VERSION)
        help_page = subprocess.run(
            [self.binary, "--help"], env=self.env, capture_output=True, text=True, check=True
        )
        self.assertIn("wire", help_page.stdout)

    def test_another_version_is_refused_without_negotiation(self):
        with host_client.Client(self.socket) as client:
            client.send(
                {
                    "frame": "hello",
                    "proto": host_client.WIRE_VERSION - 1,
                    "binary_id": "wrong-version",
                    "caps": [],
                }
            )
            refused = client.receive()
            self.assertEqual(refused["frame"], "refused")
            self.assertEqual(refused["kind"], "version_mismatch")

    def test_unknown_session_fails_without_creating_one(self):
        with self.assertRaisesRegex(host_client.ProtocolError, "not in the host directory"):
            host_client.inspect_session(self.socket, str(uuid.uuid4()))

    def test_guide_transcript_matches_the_live_host(self):
        session = self.create_session()
        guide = (ROOT / "docs/user-guide/integrating.md").read_text()
        hello, listing, attach = [
            json.loads(text) for text in re.findall(r"```json\n(.*?)\n```", guide, re.S)
        ]
        with host_client.Client(self.socket) as client:
            client.send(hello)
            self.assertEqual(client.receive()["proto"], host_client.WIRE_VERSION)
            rows = client.request(listing["id"], listing["op"])["rows"]
            node = next(row["node"] for row in rows if row["session"] == session)
            self.assertEqual(set(node), set(attach["op"]["node"]))
            attach["op"]["node"] = node
            self.assertEqual(client.request(attach["id"], attach["op"])["answer"], "attached")


if __name__ == "__main__":
    unittest.main()
