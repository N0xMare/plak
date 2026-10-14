"""Zero-inference tests for Pi openai-codex refresh-token write-back."""

from __future__ import annotations

import base64
import json
import pathlib
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "eval"))

from harbor_adapter.credentials import CredentialError, replace_pi_codex_auth  # noqa: E402


def token(account: str) -> str:
    payload = {"https://api.openai.com/auth": {"chatgpt_account_id": account}}
    encoded = base64.urlsafe_b64encode(json.dumps(payload).encode()).rstrip(b"=").decode()
    return f"header.{encoded}.signature"


def store(account: str = "account-a", *, refresh: str = "refresh-a", expires: int = 100,
          other: str = "unchanged") -> bytes:
    return json.dumps({
        "openai-codex": {"type": "oauth", "access": token(account), "refresh": refresh,
                         "expires": expires, "accountId": account},
        "openrouter": {"type": "api_key", "key": other},
    }).encode()


class PiAuthRefreshTests(unittest.TestCase):
    def test_adapter_harvests_effective_file_only_for_codex_provider(self) -> None:
        agents = (ROOT / "eval" / "harbor_adapter" / "vendor_agents.py").read_text()
        pi = agents.split("class FrozenPi", 1)[1].split("class FrozenOpenCode", 1)[0]
        self.assertIn('if provider == "openai-codex":', pi)
        self.assertIn('"$HOME/.pi/agent/auth.json"', pi)
        self.assertIn("replace_pi_codex_auth(auth_json, candidate, snapshot)", pi)
        self.assertNotIn("replace_pi_codex_auth", agents.split("class FrozenOpenCode", 1)[1])

    def test_replaces_rotated_same_account_atomically_and_privately(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            host, candidate = root / "auth.json", root / "candidate.json"
            original, refreshed = store(), store(refresh="refresh-b", expires=200)
            host.write_bytes(original)
            candidate.write_bytes(refreshed)
            self.assertTrue(replace_pi_codex_auth(host, candidate, original))
            self.assertEqual(host.read_bytes(), refreshed)
            self.assertEqual(host.stat().st_mode & 0o777, 0o600)
            self.assertFalse(candidate.exists())

    def test_rejects_foreign_account_other_provider_and_competing_host(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            host, candidate = root / "auth.json", root / "candidate.json"
            original = store()
            for changed in (
                store("account-b", refresh="refresh-b", expires=200),
                store(refresh="refresh-b", expires=200, other="changed"),
            ):
                host.write_bytes(original)
                candidate.write_bytes(changed)
                with self.assertRaises(CredentialError):
                    replace_pi_codex_auth(host, candidate, original)
                self.assertEqual(host.read_bytes(), original)
            host.write_bytes(store(refresh="host-writer", expires=150))
            candidate.write_bytes(store(refresh="refresh-b", expires=200))
            with self.assertRaises(CredentialError):
                replace_pi_codex_auth(host, candidate, original)

    def test_rejects_malformed_or_regressed_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            host, candidate = root / "auth.json", root / "candidate.json"
            original = store(expires=100)
            host.write_bytes(original)
            for changed in (b"{}", store(refresh="refresh-b", expires=99)):
                candidate.write_bytes(changed)
                with self.assertRaises(CredentialError):
                    replace_pi_codex_auth(host, candidate, original)
                self.assertEqual(host.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
