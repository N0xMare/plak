import gzip
import asyncio
import importlib.util
import json
import os
import socket
import ssl
import shutil
import subprocess
import sys
import tempfile
import unittest
import urllib.request
from http.client import HTTPConnection
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "native_compare_prompts", ROOT / "scripts/native_compare_prompts.py"
)
assert SPEC and SPEC.loader
NCP = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = NCP
SPEC.loader.exec_module(NCP)


class RedactionTests(unittest.TestCase):
    def test_headers_redact_secrets_and_keep_dialect_pins(self):
        rows = NCP.redact_headers(
            [
                ("Authorization", "Bearer sk-ant-secretsecretsecret"),
                ("Cookie", "session=abc"),
                ("x-api-key", "sk-or-v1-secretsecretsecret"),
                ("x-claude-code-session-id", "12345678-1234-4234-8234-123456789abc"),
                ("user-agent", "claude-cli/2.1.263 (external, cli)"),
                ("anthropic-beta", "oauth-2025-04-20"),
            ]
        )
        by_name = {row["name"]: row["value"] for row in rows}
        self.assertEqual(by_name["Authorization"], "[redacted]")
        self.assertEqual(by_name["Cookie"], "[redacted]")
        self.assertEqual(by_name["x-api-key"], "[redacted]")
        self.assertEqual(by_name["x-claude-code-session-id"], "[redacted]")
        self.assertEqual(by_name["user-agent"], "claude-cli/2.1.263 (external, cli)")
        self.assertEqual(by_name["anthropic-beta"], "oauth-2025-04-20")

    def test_json_redacts_emails_secrets_and_session_metadata(self):
        redacted = NCP.redact_json(
            {
                "model": "claude-sonnet-5",
                "api_key": "sk-ant-secretsecretsecret",
                "system": "Contact owner@example.com then continue.",
                "metadata": {"user_id": '{"device_id":"private-device"}'},
                "session_note": "request 12345678-1234-4234-8234-123456789abc",
                "tools": [{"name": "Grep", "description": "search the repo"}],
            }
        )
        self.assertEqual(redacted["model"], "claude-sonnet-5")
        self.assertEqual(redacted["api_key"], "[redacted]")
        self.assertEqual(redacted["metadata"]["user_id"], "[redacted]")
        self.assertIn("[redacted-email]", redacted["system"])
        self.assertNotIn("owner@example.com", redacted["system"])
        self.assertNotIn("12345678-1234-4234-8234-123456789abc", redacted["session_note"])
        self.assertEqual(redacted["tools"][0]["name"], "Grep")


class JsoncTests(unittest.TestCase):
    def test_pretty_jsonc_keeps_real_newlines_inside_strings(self):
        text = NCP.pretty_jsonc({"prompt": "line one\nline two"})
        self.assertIn('"prompt": "line one\nline two"', text)
        self.assertTrue(text.endswith("\n"))


class InferenceDetectionTests(unittest.TestCase):
    def test_messages_path_with_json_is_inference(self):
        self.assertTrue(
            NCP.is_inference_payload("/v1/messages", {"model": "claude-sonnet-5"})
        )

    def test_token_count_control_request_is_not_inference(self):
        self.assertFalse(NCP.is_inference_payload("/v1/messages/count_tokens?beta=true", {"model": "claude-sonnet-5", "tools": [{"name": "Bash"}]}))

    def test_models_list_is_not_inference(self):
        self.assertFalse(NCP.is_inference_payload("/v1/models", {"object": "list"}))
        self.assertTrue(NCP.is_models_path("/api/v1/models?limit=10"))
        self.assertFalse(
            NCP.is_inference_payload(
                "/backend-api/codex/analytics-events/events", {"model": "x"}
            )
        )


class SelectionTests(unittest.TestCase):
    def test_gzip_body_is_decoded_before_json_parse(self):
        payload = gzip.compress(json.dumps({"model": "gpt-5.6-luna"}).encode())
        decoded = NCP.decode_body([("Content-Encoding", "gzip")], payload)
        self.assertEqual(json.loads(decoded)["model"], "gpt-5.6-luna")

    def test_zstd_magic_is_decoded(self):
        if shutil.which("zstd") is None:
            self.skipTest("zstd not installed")
        raw = json.dumps({"model": "gpt-5.6-luna"}).encode()
        compressed = subprocess.check_output(["zstd", "-c"], input=raw)
        decoded = NCP.decode_body([("Content-Encoding", "zstd")], compressed)
        self.assertEqual(json.loads(decoded)["model"], "gpt-5.6-luna")

    def test_selects_request_with_the_most_tools(self):
        title = NCP.CapturedRequest(
            "POST",
            "/v1/responses",
            [],
            json.dumps({"tools": [{"name": "session_title"}]}).encode(),
        )
        main = NCP.CapturedRequest(
            "POST",
            "/v1/responses",
            [],
            json.dumps({"tools": [{"name": "grep"}, {"name": "write"}]}).encode(),
        )
        chosen = NCP.select_inference([title, main])
        self.assertEqual(chosen, main)


class DummyResponseTests(unittest.TestCase):
    def test_messages_sse_ends_the_turn(self):
        content_type, body = NCP.dummy_messages("claude-sonnet-5", stream=True)
        text = body.decode("utf-8")
        self.assertEqual(content_type, "text/event-stream")
        self.assertIn("event: message_stop", text)
        self.assertIn("end_turn", text)

    def test_claude_script_runs_two_tool_rounds_and_one_retry(self):
        script = NCP.ClaudeScript()
        tools = [{"name": "Bash"}]

        def main(*content):
            return {"model": "claude-sonnet-5", "tools": tools,
                    "messages": [{"role": "user", "content": list(content)}]}

        def ask(text):
            return main({"type": "text", "text": text})

        def result(tool_id):
            return main({"type": "tool_result", "tool_use_id": tool_id, "content": "ok"})

        replies = [
            script.reply("/v1/messages?beta=true", ask(NCP.PROMPT), True),
            script.reply("/v1/messages", ask(NCP.CLAUDE_TOOL_PROMPT), True),
            script.reply("/v1/messages", result("toolu_nc_1"), True),
            script.reply("/v1/messages", result("toolu_nc_2"), True),
            script.reply("/v1/messages", ask(NCP.CLAUDE_RETRY_PROMPT), True),
            script.reply("/v1/messages", ask(NCP.CLAUDE_RETRY_PROMPT), True),
            script.reply("/v1/messages", {"model": "claude-haiku-5-5", "messages": []}, False),
        ]
        actions = [reply.summary["action"] for reply in replies]
        self.assertEqual(actions, ["text", "tool_use:1", "tool_use:2", "text",
                                   "overloaded", "text", "text"])
        self.assertEqual(replies[4].status, 529)
        self.assertIn(("x-should-retry", "true"), replies[4].headers)
        self.assertIsNone(replies[4].summary["message_id"])
        self.assertIn(b'"id":"toolu_nc_2"', replies[2].body)
        self.assertIn(b"input_json_delta", replies[2].body)
        ids = [reply.summary["request_id"] for reply in replies]
        self.assertEqual(len(set(ids)), len(ids))
        self.assertFalse(replies[6].summary["main"])
        self.assertIsNone(script.reply("/v1/messages/count_tokens", ask("x"), False))

    def test_multiturn_facts_label_prompt_ids_without_leaking_them(self):
        uuid = "0b2f6a52-1111-4222-8333-944455556666"
        body = {"model": "claude-sonnet-5", "tools": [{"name": "Bash"}],
                "system": [{"type": "text", "text": (
                    "x-anthropic-billing-header: cc_version=2.1.295.05c; cc_entrypoint=cli; "
                    f"cch=00000; cc_prompt_id={uuid}; cc_turn_origin=human; "
                    "cc_prompt_index=1; cc_turn_index=1;")}],
                "messages": [{"role": "user", "content": "hello"}],
                "diagnostics": {"previous_message_id": None}}
        item = NCP.CapturedRequest("POST", "/v1/messages", [], json.dumps(body).encode(),
                                   {"serial": 1, "main": True, "action": "text"})
        [facts] = NCP.multiturn_facts([item, item][:1])
        self.assertIn("cc_prompt_id=p1;", facts["billing"])
        self.assertNotIn(uuid, json.dumps(facts))
        self.assertEqual(facts["diagnostics"], {"previous_message_id": None})

    def test_claude_overrides_select_binary_model_and_effort(self):
        args = NCP.parse_args(["claude", "--claude-bin", "/x/claude", "--claude-model",
                               "claude-haiku-5-5", "--claude-effort", "max",
                               "--claude-scenario", "multiturn"])
        spec = NCP.claude_spec(args)
        self.assertEqual((spec.bin, spec.model, spec.effort),
                         ("/x/claude", "claude-haiku-5-5", "max"))
        self.assertEqual(args.claude_scenario, "multiturn")

    def test_chat_sse_sends_done(self):
        _, body = NCP.dummy_chat("inclusionai/ling-3.0-flash-vl:free", stream=True)
        self.assertIn("data: [DONE]", body.decode("utf-8"))

    def test_responses_json_completes(self):
        content_type, body = NCP.dummy_responses("gpt-5.6-luna", stream=False)
        payload = json.loads(body)
        self.assertEqual(content_type, "application/json")
        self.assertEqual(payload["status"], "completed")

    def test_responses_sse_includes_sequence_numbers(self):
        _, body = NCP.dummy_responses("grok-4.6", stream=True)
        self.assertIn(b'"sequence_number":0', body)
        self.assertIn(b"response.completed", body)

    def test_session_title_tool_choice_returns_function_call(self):
        _, body = NCP.dummy_responses(
            "grok-4.6",
            stream=True,
            payload={
                "tool_choice": {"type": "function", "name": "session_title"},
            },
        )
        text = body.decode("utf-8")
        self.assertIn("function_call", text)
        self.assertIn("session_title", text)


class MockServerTests(unittest.TestCase):
    def test_codex_discovery_and_mcp_on_plain_and_mitm(self):
        cases = [
            ("GET", "/backend-api/wham/accounts/check", None, 200,
             {"accounts": [{"id": "native-compare-account", "workspace_backend_origin": "https://chatgpt.com", "account_routing_override": "NO_CONSTRAINT"}]}),
            ("GET", "/backend-api/codex/models?client_version=0.160.0", None, 200, None),
            ("GET", "/backend-api/plugins", None, 200, {"plugins": [], "next_page_token": None}),
            ("GET", "/backend-api/plugins/suggested", None, 200, {"enabled": False, "plugins": []}),
            ("POST", "/backend-api/ps/mcp", {"jsonrpc": "2.0", "id": 7, "method": "initialize"}, 200, None),
            ("POST", "/backend-api/ps/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"}, 202, {}),
            ("POST", "/backend-api/ps/mcp", {"jsonrpc": "2.0", "id": 8, "method": "tools/list"}, 200, {"jsonrpc": "2.0", "id": 8, "result": {"tools": []}}),
            ("POST", "/backend-api/ps/mcp", {"jsonrpc": "2.0", "id": 9, "method": "unknown"}, 200, {"jsonrpc": "2.0", "id": 9, "error": {"code": -32601, "message": "Method not found"}}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            for mitm in (False, True):
                with self.subTest(mitm=mitm):
                    state = NCP.CaptureState()
                    if mitm:
                        if shutil.which("openssl") is None:
                            continue
                        server = NCP.start_mitm(state, Path(directory))
                    else:
                        server, _, _ = NCP.start_mock(state)
                    try:
                        for method, path, payload, status, expected in cases:
                            address = server.server.server_address if mitm else server.server_address
                            connection = HTTPConnection(*address, timeout=5)
                            if mitm:
                                connection.set_tunnel("chatgpt.com", 443)
                                connection.connect()
                                context = ssl.create_default_context(cafile=str(server.ca_pem))
                                connection.sock = context.wrap_socket(connection.sock, server_hostname="chatgpt.com")
                            connection.request(method, path, body=json.dumps(payload) if payload else None,
                                               headers={"Content-Type": "application/json"})
                            response = connection.getresponse()
                            body = json.loads(response.read())
                            connection.close()
                            self.assertEqual(response.status, status, path)
                            if expected is not None:
                                self.assertEqual(body, expected, path)
                            elif method == "GET":
                                model = body["models"][0]
                                required = {"slug", "display_name", "description", "supported_reasoning_levels", "shell_type", "visibility", "supported_in_api", "priority", "availability_nux", "upgrade", "support_verbosity", "default_verbosity", "apply_patch_tool_type", "truncation_policy", "experimental_supported_tools"}
                                self.assertTrue(required <= model.keys())
                            else:
                                self.assertEqual(body["id"], 7)
                                self.assertEqual(body["jsonrpc"], "2.0")
                                self.assertEqual(set(body["result"]), {"protocolVersion", "capabilities", "serverInfo"})
                        self.assertFalse(state.inferences)
                    finally:
                        if mitm:
                            server.shutdown()
                        else:
                            server.shutdown()
                            server.server_close()

    def test_records_first_inference_and_answers_models(self):
        httpd, state, origin = NCP.start_mock()
        try:
            with urllib.request.urlopen(f"{origin}/v1/models", timeout=5) as response:
                catalog = json.loads(response.read())
            self.assertEqual(catalog["object"], "list")
            payload = json.dumps(
                {
                    "model": "claude-sonnet-5",
                    "messages": [{"role": "user", "content": "hello"}],
                    "tools": [{"name": "Grep"}],
                    "stream": True,
                }
            ).encode("utf-8")
            connection = HTTPConnection("127.0.0.1", httpd.server_address[1], timeout=5)
            connection.request(
                "POST",
                "/v1/messages",
                body=payload,
                headers={
                    "Content-Type": "application/json",
                    "Authorization": "Bearer secret",
                    "Accept": "text/event-stream",
                },
            )
            reply = connection.getresponse()
            body = reply.read()
            connection.close()
            self.assertEqual(reply.status, 200)
            self.assertIn(b"message_stop", body)
            self.assertTrue(state.inferences)
            captured = json.loads(state.inferences[0].body)
            self.assertEqual(captured["tools"][0]["name"], "Grep")
            redacted = NCP.redact_headers(state.inferences[0].headers)
            auth = next(row for row in redacted if row["name"].lower() == "authorization")
            self.assertEqual(auth["value"], "[redacted]")
        finally:
            httpd.shutdown()
            httpd.server_close()

    def test_unset_openai_base_url_is_not_the_mock(self):
        argv = ["exec", "hello", "-c", 'chatgpt_base_url="http://127.0.0.1:9/backend-api"']
        self.assertEqual(
            NCP.codex_inference_url(argv),
            "https://chatgpt.com/backend-api/codex/responses",
        )
        self.assertEqual(NCP.simulate_codex_capture(argv), 2)

    def test_fake_codex_cli_reaches_mock_inference(self):
        httpd, state, origin = NCP.start_mock()
        try:
            with tempfile.TemporaryDirectory() as directory:
                base = Path(directory)
                fake = base / "codex"
                fake.write_text(
                    "#!/usr/bin/env python3\n"
                    "import sys\n"
                    f"sys.path.insert(0, {str(ROOT / 'scripts')!r})\n"
                    "import native_compare_prompts as capture\n"
                    "raise SystemExit(capture.simulate_codex_capture(sys.argv[1:]))\n"
                )
                fake.chmod(0o755)
                fixture = base / "fixture"
                fixture.mkdir()
                command = NCP.vendor_command(NCP.SPECS["codex"], fake, fixture, origin)
                inference = NCP.codex_inference_url(command)
                self.assertEqual(inference, f"{origin}/backend-api/codex/responses")
                env = os.environ.copy()
                env["PATH"] = "/usr/bin:/bin"
                for name in (
                    "HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy",
                    "ALL_PROXY", "all_proxy",
                ):
                    env.pop(name, None)
                result = subprocess.run(
                    command, cwd=fixture, env=env, text=True, capture_output=True, timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(state.inferences, state.requests)
                captured = state.inferences[0]
                self.assertEqual(captured.path.split("?", 1)[0], "/backend-api/codex/responses")
                payload = json.loads(captured.body)
                self.assertEqual(payload["model"], "gpt-5.6-luna")
                self.assertEqual(payload["input"], "hello")
                paths = [path for _method, path in state.requests]
                self.assertIn("/backend-api/wham/accounts/check", paths)
                self.assertIn("/backend-api/codex/models", paths)
        finally:
            httpd.shutdown()
            httpd.server_close()


class FixtureTests(unittest.TestCase):
    def test_opencode_fixture_pins_mock_base_url(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = NCP.prepare_fixture(
                Path(directory), "opencode", "http://127.0.0.1:9"
            )
            config = json.loads((fixture / "opencode.json").read_text())
            self.assertEqual(
                config["provider"]["openrouter"]["options"]["baseURL"],
                "http://127.0.0.1:9/api/v1",
            )
            self.assertTrue((fixture / ".git").is_dir())

    def test_claude_fixture_ships_empty_mcp_config(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = NCP.prepare_fixture(
                Path(directory), "claude", "http://127.0.0.1:9"
            )
            self.assertEqual(
                json.loads((fixture / "mcp.json").read_text()),
                {"mcpServers": {}},
            )


class ClaudeIngestTests(unittest.TestCase):
    CLI_PAYLOAD = {
        "model": "claude-sonnet-5",
        "system": [
            {
                "type": "text",
                "text": NCP.CLAUDE_CLI_IDENTITY,
            }
        ],
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"name": "Bash"}, {"name": "Grep"}],
    }
    SDK_PAYLOAD = {
        "model": "claude-sonnet-5",
        "system": [
            {
                "type": "text",
                "text": NCP.CLAUDE_SDK_IDENTITY,
            }
        ],
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"name": "Bash"}],
    }

    def test_vendor_command_refuses_claude_print_path(self):
        with self.assertRaises(RuntimeError) as raised:
            NCP.vendor_command(
                NCP.SPECS["claude"],
                Path("/bin/claude"),
                Path("/tmp/fixture"),
                "http://127.0.0.1:9",
            )
        self.assertIn("never -p", str(raised.exception))
        self.assertIn("sdk-cli", str(raised.exception))

    def test_default_vendors_include_claude(self):
        args = NCP.parse_args([])
        self.assertEqual(NCP.resolve_vendors(args), ["claude", "codex", "grok", "opencode"])

    def test_from_without_other_vendors_is_claude_only(self):
        args = NCP.parse_args(["--from", "/tmp/claude.log"])
        self.assertEqual(NCP.resolve_vendors(args), ["claude"])

    def test_from_with_non_claude_vendor_is_rejected(self):
        args = NCP.parse_args(["grok", "--from", "/tmp/claude.log"])
        with self.assertRaises(ValueError):
            NCP.resolve_vendors(args)

    def test_leading_double_dash_from_just_is_stripped(self):
        args = NCP.parse_args(
            ["--", "claude", "--from", "/tmp/claude-native.log"]
        )
        self.assertEqual(args.vendors, ["claude"])
        self.assertEqual(args.from_path, Path("/tmp/claude-native.log"))
        self.assertEqual(NCP.resolve_vendors(args), ["claude"])

    def test_ingest_accepts_cli_jsonl_wrapper(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "claude.log"
            log.write_text(
                json.dumps(
                    {
                        "url": "https://api.anthropic.com/v1/messages?beta=true",
                        "headers": {
                            "user-agent": "claude-cli/2.1.263 (external, cli)"
                        },
                        "body": self.CLI_PAYLOAD,
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            captured = NCP.ingest_claude_debug(log)
            payload = json.loads(captured.body)
            self.assertEqual(payload["tools"][1]["name"], "Grep")
            self.assertIn("messages?beta=true", captured.path)
            ua = next(value for name, value in captured.headers if name.lower() == "user-agent")
            self.assertIn("(external, cli)", ua)
            self.assertNotIn("sdk-cli", ua)

    def test_ingest_rejects_sdk_print_dump(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "sdk.log"
            log.write_text(json.dumps(self.SDK_PAYLOAD) + "\n", encoding="utf-8")
            with self.assertRaises(RuntimeError) as raised:
                NCP.ingest_claude_debug(log)
            self.assertIn("sdk-cli", str(raised.exception))

    def test_visible_pty_text_strips_cursor_moves(self):
        raw = (
            b"\x1b[2GYes,\x1b[9GI\x1b[11Gtrust\x1b[17Gthis\x1b[22Gfolder"
        )
        self.assertEqual(NCP.folded_pty_text(raw), "yesitrustthisfolder")
        command = NCP.claude_tui_command(
            NCP.SPECS["claude"],
            Path("/bin/claude"),
            Path("/tmp/fixture"),
            Path("/tmp/claude.debug.log"),
        )
        self.assertNotIn("-p", command)
        self.assertNotIn("--print", command)
        self.assertIn("--debug-file", command)
        self.assertIn("api", command)

    def test_run_ingest_without_from_does_not_spawn_claude(self):
        error = NCP.run_claude_ingest(NCP.SPECS["claude"], Path("/tmp"), None)
        self.assertIsNotNone(error)
        self.assertIn("never -p", error)


class CaptureIsolationTests(unittest.TestCase):
    def test_environment_does_not_inherit_credentials_or_native_config(self):
        inherited = {
            "ANTHROPIC_API_KEY": "private-user-key", "OPENAI_API_KEY": "private-key",
            "CLAUDE_CONFIG_DIR": "/private/account", "GROK_AUTH": "private-login",
            "HTTPS_PROXY": "https://private-proxy", "NODE_OPTIONS": "--require private.js",
        }
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, inherited):
            home = Path(directory) / "home"
            env = NCP.capture_env("http://127.0.0.1:9", home=home)
            # A macOS worktree may legitimately put /private/tmp in PATH.
            for value in inherited.values():
                self.assertNotIn(value, json.dumps(env))
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("XAI_API_KEY", env)
            self.assertNotIn("GROK_AUTH", env)
            self.assertEqual(env["HOME"], str(home))
            self.assertEqual(env["CLAUDE_CONFIG_DIR"], str(home / "claude-1"))
            auth = json.loads((home / "codex/auth.json").read_text())
            self.assertEqual(auth["auth_mode"], "chatgpt")
            self.assertEqual(auth["tokens"]["account_id"], "native-compare-account")
            grok = json.loads((home / "grok/auth.json").read_text())
            self.assertEqual(next(iter(grok.values()))["auth_mode"], "oidc")

    def test_claude_trust_seed_accepts_fixture_and_refuses_real_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / "home"
            project = root / "fixture"
            project.mkdir()
            linked = root / "fixture-link"
            linked.symlink_to(project, target_is_directory=True)
            keys = NCP.claude_trust_project_keys(linked)
            self.assertIn(os.path.normpath(os.path.abspath(linked)), keys)
            self.assertIn(os.path.realpath(project), keys)

            with patch.dict(os.environ, {}, clear=False):
                os.environ.pop("CLAUDE_CONFIG_DIR", None)
                env = NCP.capture_env("http://127.0.0.1:9", home=home)
            self.assertEqual(env["CLAUDE_CONFIG_DIR"], str(home / "claude-1"))
            seeded = NCP.seed_claude_workspace_trust(home / "claude-1", project)
            config = json.loads((home / "claude-1" / ".claude.json").read_text())
            self.assertTrue(config["hasCompletedOnboarding"])
            self.assertTrue(config["bypassPermissionsModeAccepted"])
            for key in seeded:
                entry = config["projects"][key]
                self.assertIs(entry["hasTrustDialogAccepted"], True)
            self.assertNotEqual(home / "claude-1", Path.home() / ".claude")

            operator = root / "operator"
            operator.mkdir()
            (operator / ".claude").mkdir()
            real_config = root / "real-claude-config"
            real_config.mkdir()
            with patch.object(Path, "home", return_value=operator), patch.dict(
                os.environ, {"CLAUDE_CONFIG_DIR": str(real_config)}
            ):
                for blocked in (operator, operator / ".claude", real_config):
                    with self.assertRaisesRegex(RuntimeError, "synthetic config"):
                        NCP.seed_claude_workspace_trust(blocked, project)
                via_home = root / "via-home"
                via_home.symlink_to(operator, target_is_directory=True)
                with self.assertRaisesRegex(RuntimeError, "synthetic config"):
                    NCP.seed_claude_workspace_trust(via_home, project)
                kept = NCP.seed_claude_workspace_trust(home / "claude-1", project)
            self.assertTrue(kept)
            self.assertFalse((operator / ".claude.json").exists())
            self.assertFalse((operator / ".claude" / ".claude.json").exists())
            self.assertFalse((real_config / ".claude.json").exists())

    def test_missing_client_clears_only_managed_previous_artifacts(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(NCP, "which", return_value=None):
            out = Path(directory)
            dest = out / "opencode"
            dest.mkdir()
            for name in ("pin.json", "hello.request.json", "hello.md", "notes.md"):
                (dest / name).write_text("stale")
            error = NCP.run_vendor(NCP.SPECS["opencode"], out, out, 1)
            self.assertIn("not on PATH", error)
            self.assertEqual([p.name for p in dest.iterdir()], ["notes.md"])

    def test_invalid_body_never_writes_raw_headers_or_body(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = NCP.CapturedRequest("POST", "/v1/messages", [("Authorization", "private")], b"private-body")
            with patch.object(NCP, "ROOT", root), self.assertRaises(RuntimeError) as raised:
                NCP.write_capture(root / "out", NCP.SPECS["claude"], Path("/bin/claude"), "v", request, "local", [])
            self.assertNotIn("private", str(raised.exception))
            self.assertFalse(any(p.is_file() for p in root.rglob("*")))

    def test_unknown_connect_is_denied_without_outbound_connection(self):
        if shutil.which("openssl") is None:
            self.skipTest("openssl not installed")
        with tempfile.TemporaryDirectory() as directory:
            proxy = NCP.start_mitm(NCP.CaptureState(), Path(directory))
            try:
                address = proxy.server.server_address
                connection = socket.create_connection(address, timeout=2)
                with connection, patch.object(socket, "create_connection", side_effect=AssertionError("external forwarding")):
                    connection.sendall(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n")
                    self.assertIn(b"403 Forbidden", connection.recv(1024))
            finally:
                proxy.shutdown()

    def test_capture_survives_client_error_and_supports_external_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "native"
            executable.write_text(
                "#!" + sys.executable + "\n"
                "import json, os, sys, urllib.request\n"
                "if '--version' in sys.argv: print('test-native'); sys.exit(0)\n"
                "body = json.dumps({'model':'mock','messages':[{'role':'user','content':'hello'}],'tools':[{'name':'Read'}]}).encode()\n"
                "urllib.request.urlopen(urllib.request.Request(os.environ['ANTHROPIC_BASE_URL']+'/v1/messages', data=body), timeout=3).read()\n"
                "sys.exit(7)\n"
            )
            executable.chmod(0o700)
            work = root / "work"
            work.mkdir()
            with patch.object(NCP, "ROOT", root), patch.object(NCP, "which", return_value=executable):
                error = NCP.run_vendor(NCP.SPECS["opencode"], root / "out", work, 5)
            self.assertIsNone(error)
            pin = json.loads((root / "out/opencode/pin.json").read_text())
            self.assertEqual(pin["capture_status"], "captured")
            self.assertEqual(pin["cli_exit_code"], 7)
            self.assertEqual(pin["client_status"], "exited")
            self.assertEqual(list(work.glob("opencode-home-*")), [])

    def test_reaped_pty_child_cleanup_is_idempotent(self):
        async def check():
            process = NCP.owned_processes.PtyProcess(123)
            cleanup = await NCP.owned_processes.Owned(process).close()
            self.assertIsNone(process.returncode)
            self.assertFalse(cleanup["joined"])
        with patch.object(NCP.owned_processes, "processes", return_value={}), patch.object(NCP.os, "waitpid", side_effect=ChildProcessError):
            asyncio.run(check())

    def test_pty_output_bound_rejects_before_retaining_excess(self):
        async def check():
            read, write = os.pipe()
            try:
                os.write(write, b"a" * 128)
                retained = bytearray()
                with patch.object(NCP.owned_processes, "OUTPUT_LIMIT", 32):
                    with self.assertRaisesRegex(RuntimeError, "output limit"):
                        await NCP.drain_pty(read, retained, 1)
                self.assertEqual(retained, b"a" * 32)
            finally:
                os.close(read)
                os.close(write)
        asyncio.run(check())

    def test_pty_owner_reaps_real_finite_child(self):
        async def check():
            pid, fd = NCP.pty.fork()
            if pid == 0:
                os.execl(sys.executable, sys.executable, "-c", "import time; time.sleep(30)")
            try:
                process = NCP.owned_processes.PtyProcess(pid)
                cleanup = await NCP.owned_processes.Owned(process).close()
                self.assertTrue(cleanup["joined"])
                self.assertTrue(cleanup["forced"])
                with self.assertRaises(ChildProcessError):
                    os.waitpid(pid, os.WNOHANG)
            finally:
                os.close(fd)
        asyncio.run(check())

    def test_headless_output_overflow_refuses_capture_and_joins(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "native"
            executable.write_text(
                "#!" + sys.executable + "\n"
                "import os, sys, time\n"
                "if '--version' in sys.argv: print('fixture'); sys.exit(0)\n"
                "os.write(1, b'x' * 4096)\n"
                "time.sleep(30)\n"
            )
            executable.chmod(0o700)
            work = root / "work"
            work.mkdir()
            capture = NCP.owned_processes.capture_stream
            executions = []
            run = NCP.owned_processes.run_command

            async def bounded(reader, path, exceeded):
                await capture(reader, path, exceeded, limit=512)

            async def recorded(*args, **kwargs):
                result = await run(*args, **kwargs)
                executions.append(result)
                return result

            with patch.object(NCP, "ROOT", root), patch.object(NCP, "which", return_value=executable), patch.object(NCP.owned_processes, "capture_stream", bounded), patch.object(NCP.owned_processes, "run_command", recorded):
                error = NCP.run_vendor(NCP.SPECS["opencode"], root / "out", work, 5)
            self.assertIn("output limit exceeded", error)
            self.assertTrue(executions[-1]["cleanup"]["joined"])
            self.assertTrue(executions[-1]["output_limit"])
            self.assertFalse((root / "out/opencode/pin.json").exists())
            self.assertEqual(list(work.glob("opencode-home-*")), [])


if __name__ == "__main__":
    unittest.main()
