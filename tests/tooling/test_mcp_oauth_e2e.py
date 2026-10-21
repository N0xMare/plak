"""Offline parsing/redaction for the opt-in MCP OAuth driver, never a live run."""

import importlib.util
from pathlib import Path
import unittest

PATH = Path(__file__).resolve().parents[1] / "live/mcp_oauth/run.py"
spec = importlib.util.spec_from_file_location("mcp_oauth_e2e", PATH)
lane = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lane)


class ParsingTests(unittest.TestCase):
    def test_authorize_url_requires_loopback_pkce(self):
        good = "http://127.0.0.1:18083/auth?response_type=code&code_challenge=hidden&code_challenge_method=S256"
        self.assertEqual(lane.authorize_url("header\n" + good + "\nfooter"), good)
        self.assertIsNone(lane.authorize_url(good.replace("S256", "plain")))
        self.assertIsNone(lane.authorize_url(good.replace("127.0.0.1", "evil.invalid")))
        self.assertIsNone(
            lane.authorize_url("http://127.0.0.1:18083/callback?code=secret")
        )

    def test_device_prompt_waits_for_both_fields(self):
        text = "  1. Open:        http://127.0.0.1:18083/device\n  2. Enter code:  ABCD-EFGH\n"
        self.assertEqual(
            lane.device_prompt(text), ("http://127.0.0.1:18083/device", "ABCD-EFGH")
        )
        self.assertIsNone(lane.device_prompt(text.splitlines()[0]))

    def test_redacts_queries_clipboard_and_device_codes(self):
        output = "\x1b]52;c;c2VjcmV0\x07\nhttp://127.0.0.1:18083/auth?state=secret&code=secret\n  2. Enter code: ABCD-EFGH\n"
        redacted = lane.redact(output)
        for secret in ("secret", "c2VjcmV0", "ABCD-EFGH"):
            self.assertNotIn(secret, redacted)
        self.assertIn("[query omitted]", redacted)
        self.assertIn("[clipboard omitted]", redacted)

    def test_redacts_st_terminated_osc(self):
        self.assertNotIn("c2VjcmV0", lane.redact("\x1b]52;c;c2VjcmV0\x1b\\"))

    def test_refresh_counter_is_issuer_scoped(self):
        events = [
            {"as": "strict", "grant": "refresh_token", "status": 200},
            {"as": "keycloak", "grant": "refresh_token"},
            {"as": "strict", "grant": "authorization_code"},
            {"event": "mcp"},
        ]
        self.assertEqual(lane.refresh_count(events), 1)

    def test_step_up_scope_union_ignores_order_and_retains_offline_access(self):
        self.assertEqual(
            lane.remedy_scopes(
                "remedy: plak mcp login srv --scope 'offline_access read write'"
            ),
            {"read", "write", "offline_access"},
        )
        self.assertEqual(
            lane.remedy_scopes(
                "remedy: plak mcp login srv --scope 'write read offline_access'"
            ),
            {"read", "write", "offline_access"},
        )
        self.assertEqual(
            lane.remedy_scopes("remedy: plak mcp login srv --scope '$(id)'"), set()
        )

    def test_session_id_comes_from_cli_marker(self):
        sid = "12345678-1234-1234-1234-123456789abc"
        self.assertEqual(lane.session_id("notice\nsession_id=" + sid + "\ndone"), sid)
        self.assertIsNone(lane.session_id("other=" + sid))

    def test_matrix_has_no_duplicate_names(self):
        self.assertEqual(len(lane.SCENARIOS), len(set(lane.SCENARIOS)))
        self.assertIn("state_key_carry", lane.SCENARIOS)


class ComposeProcessTests(unittest.TestCase):
    def test_browser_error_does_not_retain_playwright_input_values(self):
        from unittest import mock
        import subprocess

        harness = lane.Harness.__new__(lane.Harness)
        error = subprocess.CalledProcessError(
            1, "browser", stderr=b'locator.fill("PRIVATE-DEVICE-CODE")'
        )
        with mock.patch.object(harness, "compose_run", side_effect=error):
            with self.assertRaises(RuntimeError) as raised:
                harness.browser("http://127.0.0.1:18083/device")
        self.assertNotIn("PRIVATE-DEVICE-CODE", str(raised.exception))
        self.assertIn("stderr omitted", str(raised.exception))

    def test_timeout_retires_the_cli_and_plugin_process_group(self):
        from unittest import mock
        import signal
        import subprocess

        harness = lane.Harness.__new__(lane.Harness)
        harness.compose = ["docker", "compose"]
        proc = mock.MagicMock()
        proc.pid = 12345
        proc.communicate.side_effect = [
            subprocess.TimeoutExpired("docker", 1),
            (b"", b""),
        ]
        with (
            mock.patch.object(lane.subprocess, "Popen") as popen,
            mock.patch.object(lane.os, "killpg") as killpg,
        ):
            popen.return_value.__enter__.return_value = proc
            with self.assertRaises(subprocess.TimeoutExpired):
                harness.compose_run("up", timeout=1)
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            killpg.assert_called_once_with(proc.pid, signal.SIGTERM)

    def test_compose_failure_preserves_bounded_stderr(self):
        from unittest import mock
        import subprocess

        harness = lane.Harness.__new__(lane.Harness)
        harness.compose = ["docker", "compose"]
        proc = mock.MagicMock()
        proc.returncode = 1
        proc.communicate.return_value = (b"", b"fixture error")
        with mock.patch.object(lane.subprocess, "Popen") as popen:
            popen.return_value.__enter__.return_value = proc
            with self.assertRaises(subprocess.CalledProcessError) as error:
                harness.compose_run("up")
            self.assertEqual(error.exception.stderr, b"fixture error")


if __name__ == "__main__":
    unittest.main()
