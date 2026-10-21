"""Zero-inference contract tests for the Rung A Pi Codex OAuth arm."""

from __future__ import annotations

import json
import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "eval"))

from harbor_adapter.credentials import (  # noqa: E402
    CredentialError,
    pi_cli_model_name,
    require_ready_pi_auth,
)
from harbor_adapter.controls import harbor_ak_flags  # noqa: E402
from harbor_adapter.tuples import (  # noqa: E402
    PI_CODEX_EFFORT,
    PI_CODEX_MODEL,
    PI_CODEX_PROVIDER,
)
from harbor_adapter.vendor_pins import CLI_PINS  # noqa: E402


class PiCodexContractTests(unittest.TestCase):
    def test_frozen_same_model_contract(self) -> None:
        self.assertEqual(CLI_PINS["pi"], "0.87.1")
        self.assertEqual(PI_CODEX_MODEL, "gpt-6-luna")
        self.assertEqual(PI_CODEX_EFFORT, "low")
        self.assertEqual(PI_CODEX_PROVIDER, "openai-codex")
        self.assertEqual(
            pi_cli_model_name(f"openai/{PI_CODEX_MODEL}"),
            f"{PI_CODEX_PROVIDER}/{PI_CODEX_MODEL}",
        )
        self.assertEqual(
            harbor_ak_flags("pi", effort=PI_CODEX_EFFORT, version="0.87.1"),
            ("version=0.87.1", "thinking=low"),
        )

    def test_codex_auth_preflight_requires_complete_oauth(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            auth = pathlib.Path(tmp) / "auth.json"
            with mock.patch.dict(
                os.environ, {"PI_AUTH_JSON_PATH": str(auth)}, clear=False
            ):
                for entry in (
                    {"type": "api_key", "key": "not-used"},
                    {"type": "oauth", "access": "present"},
                ):
                    auth.write_text(json.dumps({PI_CODEX_PROVIDER: entry}))
                    with self.assertRaises(CredentialError):
                        require_ready_pi_auth(PI_CODEX_PROVIDER)
                auth.write_text(
                    json.dumps(
                        {
                            PI_CODEX_PROVIDER: {
                                "type": "oauth",
                                "access": "access-token",
                                "refresh": "refresh-token",
                            }
                        }
                    )
                )
                self.assertEqual(require_ready_pi_auth(PI_CODEX_PROVIDER), auth)

    def test_rung_a_arm_is_serial_and_credential_isolated(self) -> None:
        script = (ROOT / "eval" / "scripts" / "rung-a.sh").read_text()
        arm = script.split("    pi-codex-oauth)", 1)[1].split("      ;;", 1)[0]
        self.assertIn("require_frozen_cli pi", arm)
        self.assertIn("require_host_auth pi openai-codex", arm)
        self.assertIn("unset OPENAI_API_KEY XAI_API_KEY OPENROUTER_API_KEY", arm)
        vendor = script.split("run_vendor() {", 1)[1].split("\n}", 1)[0]
        self.assertIn("concurrent=1", vendor)
        self.assertIn("openai/gpt-6-luna", arm)
        self.assertIn("pi low", arm)
        self.assertNotIn("subscription_dir", arm)
        self.assertNotIn("credential.json", arm)

    def test_pi_cell_keeps_binary_manifest_and_private_uploaded_auth(self) -> None:
        agents = (ROOT / "eval" / "harbor_adapter" / "vendor_agents.py").read_text()
        pi = agents.split("class FrozenPi", 1)[1].split("class FrozenOpenCode", 1)[0]
        self.assertIn('_record_executable_identity(self, environment, "pi", self._arch)', pi)
        self.assertIn("require_ready_pi_auth(pi_cli_provider(harbor_model))", pi)
        self.assertIn("remote_uploaded_auth_acl_command", pi)
        self.assertIn("must use host auth.json, not provider API keys", pi)


if __name__ == "__main__":
    unittest.main()
