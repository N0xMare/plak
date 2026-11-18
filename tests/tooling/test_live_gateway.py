"""The credential-gateway live recipe prints a plan and does not call a provider."""

import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tests" / "live" / "gateway.py"
ROUTES = (
    "codex",
    "claude-haiku55-low",
    "grok",
    "api-openai",
    "api-anthropic",
    "api-xai",
    "or-nemotron-free",
)


class LiveGatewayPlan(unittest.TestCase):
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
        self.assertIn("in-process", completed.stdout)
        self.assertIn("unix-socket", completed.stdout)
        self.assertIn("tailnet TCP", completed.stdout)
        self.assertIn("just live estimate --rate-card RATE_CARD --soak-row grok", completed.stdout)
        self.assertIn("hop=gateway", completed.stdout)
        self.assertIn("no provider call", completed.stdout)

    def test_paid_refuses_before_a_provider_call(self):
        completed = subprocess.run(
            [sys.executable, str(SCRIPT), "--paid", "--binary", "/usr/bin/false"],
            check=False,
            capture_output=True,
            text=True,
            cwd=ROOT,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("no provider call", completed.stderr)
        self.assertIn("codex", completed.stdout)


if __name__ == "__main__":
    unittest.main()
