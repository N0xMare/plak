"""The credential-gateway live driver: its offline plan, its refusal, and a
zero-inference paid run against a fake `plakd`."""

import contextlib
import io
import json
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tests" / "live" / "gateway.py"
sys.path.insert(0, str(ROOT / "tests" / "live"))

import gateway  # noqa: E402
import live  # noqa: E402

ROUTES = (
    "codex",
    "claude-haiku55-low",
    "grok",
    "api-openai",
    "api-anthropic",
    "api-xai",
    "or-nemotron-free",
)

# A `plakd` that answers every command the paid run sends, without a
# provider. `gateway serve` marks its home until SIGTERM; a host whose
# `[gateway] address` names that home's socket audits one line per run.
FAKE_PLAKD = """\
#!/bin/sh
# capture root: {manifest}
gw_dir() {{ sed -n 's|^address = "unix:\\(.*\\)/gateway.sock"$|\\1|p' "$PLAK_HOME/config.toml"; }}
audit() {{ d=$(gw_dir); if [ -n "$d" ]; then echo "relay $1" >> "$d/audit.log"; fi; }}
case "$1 $2" in
  "gateway --help") echo "  plakd gateway serve"; exit 0 ;;
  "gateway serve")
    n=$(cat "$PLAK_HOME/starts" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$PLAK_HOME/starts"
    trap 'rm -f "$PLAK_HOME/serving"; exit 0' TERM
    echo "instance-$n" > "$PLAK_HOME/serving"
    while :; do sleep 0.1; done ;;
  "gateway check")
    d=$(gw_dir); [ -f "$d/serving" ] || exit 1
    echo "gateway $(cat "$d/serving") protocol 1"; exit 0 ;;
  "gateway audit") cat "$PLAK_HOME/audit.log" 2>/dev/null; exit 0 ;;
esac
if [ "$1" = doctor ]; then
  [ -f "$(gw_dir)/serving" ] || exit 1
  for kind in codex claude grok; do
    printf 'OAuth %s\tOK\tready · record held by the gateway\n' "$kind"
  done
  exit 0
fi
if [ "$1" = status ]; then
  [ -f "$(gw_dir)/serving" ] || exit 1
  echo "via gateway unix:$(gw_dir)/gateway.sock"
  echo "$2: credential=ready"
  exit 0
fi
if [ "$1" = refresh ]; then echo "$2: credential=ready"; exit 0; fi
if [ "$1" = run ] && [ "$2" = --session ]; then audit reopen; echo MAGENTA; exit 0; fi
if [ "$1" = run ]; then audit turn; echo "session_id=s$$"; echo ok; exit 0; fi
exit 1
"""


class LiveGatewayPlan(unittest.TestCase):
    def test_live_faults_go_only_to_the_hop_that_owns_them(self):
        home = Path("/fixture/driver-home")
        env = live.Env(Path("/fixture/plakd"), home, home)
        source = {
            "PLAK_HOME": str(home),
            "PLAK_TEST_FAULTS": "host=example.test,fault=503;hop=gateway,fault=connect",
            "PLAK_TEST_EXCHANGE_CAPTURE_DIR": "/fixture/capture",
        }
        with patch.object(live, "running_gateway") as start:
            self.addCleanup(env.close_gateway)
            caller = env.gateway_env(source)
            self.assertEqual(caller["PLAK_TEST_FAULTS"], "hop=gateway,fault=connect")
            self.assertEqual(caller["PLAK_TEST_GATEWAY"], "external")
            self.assertEqual(start.call_args.kwargs["faults"], "host=example.test,fault=503")
            self.assertNotIn("PLAK_TEST_EXCHANGE_CAPTURE_DIR", start.call_args.args[3])

    def test_plan_names_routes_topologies_and_the_estimate_line(self):
        completed = subprocess.run(
            [sys.executable, str(SCRIPT)],
            check=False,
            capture_output=True,
            text=True,
            cwd=ROOT,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        for route in ROUTES:
            self.assertIn(route, completed.stdout)
        self.assertNotIn("in-process", completed.stdout)
        self.assertIn("unix-socket", completed.stdout)
        self.assertIn("reopen every session across a gateway restart", completed.stdout)
        self.assertIn("forced refresh of each OAuth kind", completed.stdout)
        self.assertIn("just live estimate --rate-card RATE_CARD --soak-row grok", completed.stdout)
        # Four API-key routes, a turn and a reopen each.
        self.assertIn("gateway_api_turns 8 input 175296 output 4000", completed.stdout)
        self.assertIn("(6 turns)", completed.stdout)
        self.assertIn("plakd gateway serve", completed.stdout)
        self.assertIn("[gateway] address", completed.stdout)
        self.assertIn("audit line count", completed.stdout)
        self.assertIn(
            "unix-socket doctor and status must report every OAuth kind Ready", completed.stdout
        )
        self.assertIn("not driven: tailnet TCP", completed.stdout)
        self.assertIn("no provider call", completed.stdout)

    def test_paid_without_serve_is_a_harness_error_before_any_call(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "plakd"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IEXEC)
            artifacts = Path(directory) / "artifacts"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--paid",
                    "--binary",
                    str(binary),
                    "--artifacts",
                    str(artifacts),
                ],
                check=False,
                capture_output=True,
                text=True,
                cwd=ROOT,
            )
            self.assertFalse(artifacts.exists())
        self.assertEqual(completed.returncode, 2, completed.stderr)
        self.assertIn("harness: ", completed.stderr)
        self.assertIn("has no `plakd gateway serve`", completed.stderr)
        self.assertIn("no provider call", completed.stderr)
        self.assertIn("codex", completed.stdout)


class LiveGatewayPaid(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        (self.root / "tests").mkdir()
        operator = self.root / "operator" / ".plak" / "secrets"
        operator.mkdir(parents=True)
        (operator / "openai.key").write_text("fixture-key\n")
        self.binary = self.root / "plakd"
        manifest = self.root / "bin" / "plakd"
        self.binary.write_text(FAKE_PLAKD.format(manifest=manifest), encoding="utf-8")
        self.binary.chmod(0o755)
        for patcher in (
            patch.object(live, "ROOT", self.root),
            patch.object(live.Path, "home", return_value=self.root / "operator"),
            patch.object(live, "isolate_test_environment", lambda: None),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_separate_gateway_turn_restart_reopen_and_refresh(self):
        artifacts = self.root / "artifacts"

        def remove_homes():
            for home in live.scratch_homes(artifacts).values():
                shutil.rmtree(home, ignore_errors=True)

        self.addCleanup(remove_homes)
        with contextlib.redirect_stdout(io.StringIO()):
            code = gateway.run_paid(self.binary, artifacts)
        steps = json.loads((artifacts / "report.json").read_text())["steps"]
        self.assertEqual([step for step in steps if step["outcome"] != live.PASS], [])
        self.assertEqual(code, 0)

        def subjects(topology, step):
            return [s["subject"] for s in steps if s["topology"] == topology and s["step"] == step]

        self.assertEqual(subjects(gateway.UNIX_SOCKET, "turn"), list(ROUTES))
        self.assertEqual(subjects(gateway.UNIX_SOCKET, "reopen"), list(ROUTES))
        self.assertEqual(subjects(gateway.UNIX_SOCKET, "refresh"), list(gateway.OAUTH_KINDS))
        self.assertEqual(subjects(gateway.UNIX_SOCKET, "doctor"), ["oauth"])
        self.assertEqual(subjects(gateway.UNIX_SOCKET, "status"), list(gateway.OAUTH_KINDS))
        restart = next(
            s for s in steps if s["topology"] == gateway.UNIX_SOCKET and s["step"] == "restart"
        )
        self.assertEqual(restart["detail"], "instance instance-1 -> instance-2")
        relayed = [
            s
            for s in steps
            if s["topology"] == gateway.UNIX_SOCKET and s["step"] in ("turn", "reopen")
        ]
        self.assertTrue(all(s["audit_lines"] == 1 for s in relayed), relayed)

        # The service adopts the keyed home in place; the relaying host has
        # no key and names the service's socket.
        homes = live.scratch_homes(artifacts)
        keyed, keyless = Path(homes[gateway.SERVICE_HOME]), Path(homes[gateway.UNIX_SOCKET])
        socket = keyed / "gateway.sock"
        self.assertTrue((keyed / "secrets" / "openai.key").is_file())
        self.assertFalse((keyless / "secrets").exists())
        self.assertEqual((keyed / "gateway.toml").read_text(), f'socket = "{socket}"\n')
        self.assertIn(
            f'[gateway]\naddress = "unix:{socket}"\n', (keyless / "config.toml").read_text()
        )
        self.assertFalse((keyed / "serving").exists(), "the service was left running")


if __name__ == "__main__":
    unittest.main()
