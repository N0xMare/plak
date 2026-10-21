#!/usr/bin/env python3
"""Opt-in, zero-inference MCP OAuth interoperability lane. No operator accounts."""

from __future__ import annotations

import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
CONTROL = "http://127.0.0.1:18082/control"
KC = "http://127.0.0.1:18081"
RESOURCE = "http://127.0.0.1:18082/mcp/"
SCENARIOS = (
    "dcr_loopback_keycloak",
    "dcr_loopback_strict",
    "dcr_paste",
    "dcr_device",
    "confidential_issuer_pin",
    "device_confidential",
    "refresh_cadence",
    "rotation_reuse",
    "forced_401",
    "fault_401",
    "concurrent_refresh",
    "step_up",
    "hostile_scope",
    "server_revocation",
    "logout_revoke",
    "logout_503",
    "logout_local",
    "resource_a_at_b",
    "shared_origin_tenant",
    "issuer_mismatch",
    "as_restart",
    "state_key_carry",
)


def request(url, data=None, headers=None, method=None):
    if isinstance(data, dict):
        data = json.dumps(data).encode()
        headers = {"content-type": "application/json", **(headers or {})}
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(
            req, timeout=10
        ) as response:
            return response.status, json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        try:
            value = json.loads(error.read())
        except ValueError:
            value = {}
        return error.code, value


def authorize_url(output):
    # OSC 52 clipboard content is deliberately not decoded.
    for line in output.splitlines():
        value = line.strip()
        if value.startswith("http://127.0.0.1:") and "code_challenge=" in value:
            parsed = urllib.parse.urlsplit(value)
            query = urllib.parse.parse_qs(parsed.query)
            if query.get("response_type") == ["code"] and query.get(
                "code_challenge_method"
            ) == ["S256"]:
                return value
    return None


def device_prompt(output):
    url = re.search(r"1\. Open:\s+(http://127\.0\.0\.1:\S+)", output)
    code = re.search(r"2\. Enter code:\s+([A-Za-z0-9-]+)", output)
    return (url.group(1), code.group(1)) if url and code else None


def redact(output):
    output = re.sub(
        r"\x1b\]52;[^\x07\x1b]*(?:\x07|\x1b\\)", "[clipboard omitted]", output
    )
    output = re.sub(
        r"https?://[^\s<>]+",
        lambda m: m.group(0).split("?")[0]
        + ("?[query omitted]" if "?" in m.group(0) else ""),
        output,
    )
    output = re.sub(r"(Enter code:\s+)\S+", r"\1[omitted]", output)
    return output


def remedy_scopes(output):
    match = re.search(r"plak mcp login srv --scope '([A-Za-z0-9._:/+ \-]+)'", output)
    return set(match.group(1).split()) if match else set()


def session_id(output):
    match = re.search(r"^session_id=([0-9a-f-]{36})$", output, re.MULTILINE)
    return match.group(1) if match else None


def refresh_count(events, issuer="strict"):
    return sum(
        e.get("as") == issuer and e.get("grant") == "refresh_token" for e in events
    )


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class Harness:
    def __init__(self, args):
        self.args = args
        self.binary = Path(args.binary).resolve()
        self.root = Path(tempfile.mkdtemp(prefix="plak-mcp-oauth-"))
        self.project = f"plak-oauth-{os.getpid()}"
        self.compose = [
            "docker",
            "compose",
            "--profile",
            "driver",
            "-p",
            self.project,
            "-f",
            str(HERE / "compose.yaml"),
        ]
        self.output = []
        self.processes = set()
        self.shims = self.root / "bin"
        self.shims.mkdir()
        for name in ("xdg-open", "open"):
            path = self.shims / name
            path.write_text(
                f'#!{sys.executable}\nimport os, pathlib, sys\npathlib.Path(os.environ["OAUTH_BROWSER_URL"]).write_text(sys.argv[1])\n'
            )
            path.chmod(0o700)
        for name in ("pbcopy", "xclip", "xsel", "wl-copy"):
            path = self.shims / name
            path.write_text("#!/bin/sh\nexit 1\n")
            path.chmod(0o700)

    def compose_run(self, *args, **kwargs):
        command = self.compose + list(args)
        timeout = kwargs.pop("timeout", 180)
        payload = kwargs.pop("input", None)
        with subprocess.Popen(
            command,
            stdin=subprocess.PIPE if payload is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
            **kwargs,
        ) as proc:
            try:
                stdout, stderr = proc.communicate(payload, timeout=timeout)
            except BaseException:
                # Docker's CLI can spawn a Compose plugin. Retire the whole group
                # before teardown so it cannot create a late orphan container.
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    proc.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    proc.communicate()
                raise
            if proc.returncode:
                raise subprocess.CalledProcessError(
                    proc.returncode, command, stdout, stderr
                )
            return subprocess.CompletedProcess(command, proc.returncode, stdout, stderr)

    def start(self):
        for port in (18080, 18081, 18082, 18083, 18084, 18765):
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                probe.bind(("127.0.0.1", port))
        self.compose_run("up", "-d", "node")
        self.compose_run(
            "exec",
            "--interactive=false",
            "-T",
            "node",
            "npm",
            "ci",
            "--ignore-scripts",
            timeout=300,
        )
        self.compose_run("up", "-d", "keycloak", "services", "browser")
        self.wait_url(f"{KC}/realms/mcp/.well-known/openid-configuration")
        self.wait_url("http://127.0.0.1:18083/.well-known/openid-configuration")
        self.admin_token = self.admin_login()
        base = f"{KC}/admin/realms/mcp"
        headers = {"authorization": f"Bearer {self.admin_token}"}
        status, policies = request(
            base
            + "/components?type=org.keycloak.services.clientregistration.policy.ClientRegistrationPolicy",
            headers=headers,
        )
        require(status == 200, "Keycloak policy query failed")
        for policy in policies:
            if policy.get("subType") == "anonymous":
                request(
                    base + "/components/" + policy["id"],
                    headers=headers,
                    method="DELETE",
                )
        realm_id = request(base, headers=headers)[1]["id"]
        status, policy_result = request(
            base + "/components",
            {
                "name": "loopback-only-dcr",
                "providerId": "trusted-hosts",
                "parentId": realm_id,
                "providerType": "org.keycloak.services.clientregistration.policy.ClientRegistrationPolicy",
                "subType": "anonymous",
                "config": {
                    "trusted-hosts": ["127.0.0.1"],
                    "host-sending-registration-request-must-match": ["true"],
                    "client-uris-must-match": ["true"],
                },
            },
            headers=headers,
        )
        require(
            status == 201,
            "Keycloak anonymous DCR policy setup failed: "
            + str(status)
            + " "
            + str(policy_result),
        )
        # Every dynamically registered client is forced to S256 and consent.
        status, policy_result = request(
            base + "/components",
            {
                "name": "consent-required",
                "providerId": "consent-required",
                "parentId": realm_id,
                "providerType": "org.keycloak.services.clientregistration.policy.ClientRegistrationPolicy",
                "subType": "anonymous",
                "config": {},
            },
            headers=headers,
        )
        require(
            status == 201,
            "Keycloak consent policy setup failed: "
            + str(status)
            + " "
            + str(policy_result),
        )

    def wait_url(self, url):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            try:
                if request(url)[0] == 200:
                    return
            except (OSError, ValueError):
                pass
            time.sleep(0.5)
        raise RuntimeError("stack readiness deadline exceeded")

    def admin_login(self):
        status, result = request(
            f"{KC}/realms/master/protocol/openid-connect/token",
            urllib.parse.urlencode(
                {
                    "grant_type": "password",
                    "client_id": "admin-cli",
                    "username": "admin",
                    "password": "fixture-admin",
                }
            ).encode(),
        )
        require(status == 200, "Keycloak admin login failed")
        return result["access_token"]

    def fixture(self, name, tenant="strict", confidential=False):
        self.output = []
        self.tenant = tenant
        self.home = self.root / name / "home"
        (self.home / "agents").mkdir(parents=True)
        self.home.chmod(0o700)
        self.workspace = self.root / name / "workspace"
        self.workspace.mkdir()
        self.script = self.root / name / "script.json"
        self.script.write_text(
            json.dumps({"version": 1, "steps": [{"output": {"text": "done"}}]})
        )
        (self.home / "agents" / "lead.toml").write_text(
            'account = "codex"\nmodel = "offline-loopback-v1"\neffort = "medium"\ncomposition = "direct"\ntools = ["mcp:srv"]\n'
        )
        self.config = (
            '[root]\nagent = "lead"\n[orchestration]\nsubagents = false\n[compaction]\nenabled = false\n'
            f'[mcp_servers.srv]\ntransport = "http"\nurl = "{RESOURCE}{tenant}"\nauth = "oauth"\n'
        )
        if confidential:
            client_id = (
                "device-confidential"
                if name == "device_confidential"
                else "confidential"
            )
            self.config += (
                f'oauth_issuer = "{KC}/realms/mcp"\noauth_client_id = "{client_id}"\n'
                'oauth_client_secret = "fixture-client"\noauth_callback_port = 18765\n'
            )
            (self.home / "secrets" / "named").mkdir(mode=0o700, parents=True)
            (self.home / "secrets").chmod(0o700)
            secret = self.home / "secrets" / "named" / "fixture-client.secret"
            secret.write_text("fixture-client-secret")
            secret.chmod(0o600)
        self.write_config()
        for t in ("strict", "keycloak", "tenantb"):
            request(
                CONTROL,
                {
                    "tenant": t,
                    "force401": False,
                    "hostile": False,
                    "resource": None,
                    "issuer": None,
                    "refreshDelayMs": 0,
                },
            )
        self.before = len(self.events())

    def write_config(self):
        (self.home / "config.toml").write_text(self.config)

    def env(self, faults=""):
        env = {
            "PATH": str(self.shims) + os.pathsep + os.defpath,
            "HOME": str(self.root / "fake-home"),
            "TMPDIR": str(self.root),
            "XDG_STATE_HOME": str(self.root / "xdg-state"),
            "XDG_CONFIG_HOME": str(self.root / "xdg-config"),
            "PLAK_HOME": str(self.home),
            "PLAK_TEST_MODEL": "offline-loopback-v1",
            "PLAK_TEST_SCRIPT": str(self.script),
            "PLAK_TEST_EVIDENCE": str(self.home.parent / "model.jsonl"),
            "OAUTH_BROWSER_URL": str(self.home.parent / "browser-url"),
            "BROWSER": str(self.shims / "xdg-open"),
            "PLAK_TEST_FAULTS": faults,
            "TERM": "dumb",
        }
        return env

    def command(self, *args, faults="", binary=None):
        result = subprocess.run(
            [str(binary or self.binary), *args],
            cwd=self.workspace,
            env=self.env(faults),
            input="y\n",
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=90,
        )
        self.output.append(
            {
                "args": list(args),
                "exit": result.returncode,
                "output": redact(result.stdout),
            }
        )
        return result

    def browser(self, url, paste=False, code=None):
        try:
            result = self.compose_run(
                "exec",
                "-T",
                "browser",
                "node",
                "browser.mjs",
                input=(
                    json.dumps({"url": url, "paste": paste, "code": code}) + "\n"
                ).encode(),
                timeout=90,
            )
        except subprocess.CalledProcessError as error:
            raise RuntimeError(
                "browser failed: " + redact(error.stderr.decode())[-2500:]
            ) from None
        return json.loads(result.stdout)

    def login(self, remote=False, expect_device=False, scope=None, tamper=False):
        args = [str(self.binary), "mcp", "login", "srv"]
        if remote:
            args.append("--remote")
        if scope:
            args.extend(["--scope", scope])
        proc = subprocess.Popen(
            args,
            cwd=self.workspace,
            env=self.env(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        self.processes.add(proc)
        lines = queue.Queue()

        def read():
            for line in proc.stdout:
                lines.put(line)

        thread = threading.Thread(target=read, daemon=True)
        thread.start()
        output = ""
        driven = False
        deadline = time.monotonic() + 110
        try:
            while time.monotonic() < deadline:
                try:
                    output += lines.get(timeout=0.2)
                except queue.Empty:
                    pass
                device = device_prompt(output)
                url = authorize_url(output)
                shim = self.home.parent / "browser-url"
                if not remote and shim.exists():
                    url = shim.read_text()
                if not driven and device:
                    driven = True
                    self.browser(device[0], code=device[1])
                elif not driven and url and (remote or shim.exists()):
                    driven = True
                    if expect_device:
                        raise AssertionError(
                            "AS rejected DCR device grant; client fell back to paste"
                        )
                    browser_result = self.browser(url, paste=remote)
                    callback = browser_result["callback"]
                    self.output.append(
                        {"browser_callback": browser_result["safe_callback"]}
                    )
                    if remote:
                        if tamper:
                            p = urllib.parse.urlsplit(callback)
                            q = urllib.parse.parse_qs(p.query)
                            q["iss"] = ["https://wrong.example.invalid"]
                            callback = urllib.parse.urlunsplit(
                                p._replace(query=urllib.parse.urlencode(q, doseq=True))
                            )
                        proc.stdin.write(callback + "\n")
                        proc.stdin.flush()
                if proc.poll() is not None:
                    thread.join(timeout=2)
                    while not lines.empty():
                        output += lines.get()
                    break
            else:
                raise RuntimeError("Plak login deadline exceeded")
            self.output.append(
                {"args": args[1:], "exit": proc.returncode, "output": redact(output)}
            )
            if tamper:
                require(
                    proc.returncode != 0 and "expected authorization server" in output,
                    "issuer mismatch not refused",
                )
            else:
                require(
                    proc.returncode == 0 and "logged in" in output,
                    "login failed: " + redact(output)[-1800:],
                )
        finally:
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            self.processes.discard(proc)
            shim = self.home.parent / "browser-url"
            shim.unlink(missing_ok=True)

    def events(self):
        return request(CONTROL)[1]["events"]

    def delta(self):
        return self.events()[self.before :]

    def control(self, **values):
        return request(CONTROL, {"tenant": self.tenant, **values})

    def test(self, ok=True, faults=""):
        result = self.command("mcp", "test", "srv", faults=faults)
        require(
            (result.returncode == 0) == ok,
            "unexpected MCP test result: " + redact(result.stdout)[-1000:],
        )
        return result.stdout

    def tool(self, name="read", faults="", session=None):
        steps = [{"output": {"text": "done"}}]
        if name is not None:
            steps.insert(
                0,
                {
                    "output": {"tool": {"name": "mcp__srv__" + name, "arguments": {}}},
                    "finish": "tool_calls",
                },
            )
        self.script.write_text(json.dumps({"version": 1, "steps": steps}))
        args = ["run", "--print-session-id"]
        if session:
            args += ["--session", session]
        else:
            args += [
                "--account",
                "codex",
                "--model",
                "offline-loopback-v1",
                "--effort",
                "medium",
                "--composition",
                "direct",
            ]
        result = self.command(*args, "--", "exercise MCP fixture", faults=faults)
        sid = session_id(result.stdout)
        if sid:
            log = self.command("session", "log", sid, "--json")
            require(log.returncode == 0, "session log unavailable")
            result.stdout += "\n" + log.stdout
        return result, sid

    def scenario(self, name):
        tenant = (
            "keycloak"
            if name
            in (
                "dcr_loopback_keycloak",
                "confidential_issuer_pin",
                "device_confidential",
                "server_revocation",
                "as_restart",
            )
            else "strict"
        )
        self.fixture(
            name,
            tenant,
            confidential=name
            in (
                "confidential_issuer_pin",
                "device_confidential",
                "server_revocation",
                "as_restart",
            ),
        )
        if name == "state_key_carry":
            if not self.args.previous_binary:
                return (
                    "BLOCKED",
                    "requires --previous-binary with test-hooks and a different state key",
                )
            previous = Path(self.args.previous_binary).resolve()
            require(
                "test-hooks" in self.command("version", binary=previous).stdout,
                "previous binary must have test-hooks",
            )
            oldkey = self.command(
                "version", "--state-key", binary=previous
            ).stdout.strip()
            newkey = self.command("version", "--state-key").stdout.strip()
            require(oldkey != newkey, "carry binaries must have different state keys")
            self.login()
            current = self.binary
            self.binary = previous
            template = self.home / "agents/lead.toml"
            new_template = template.read_text()
            template.write_text(
                new_template.replace('tools = ["mcp:srv"]', "tools = []")
            )
            (self.home / "config.toml").write_text(
                self.config.split("[mcp_servers.srv]")[0]
            )
            carry = self.home.parent / "carry.json"
            try:
                result, sid = self.tool(name=None)
                require(
                    result.returncode == 0 and sid, "old-build session creation failed"
                )
                exported = self.command(
                    "session", "export", sid, "--out", str(carry), binary=previous
                )
                require(exported.returncode == 0, "export failed")
            finally:
                self.binary = current
                template.write_text(new_template)
                self.write_config()
            imported = self.command("session", "import", str(carry))
            require(imported.returncode == 0, "import failed")
            self.test()
            imported_id = re.search(r"session ([0-9a-f-]{36})", imported.stdout)
            require(imported_id is not None, "imported session id absent")
            result, _ = self.tool(session=imported_id.group(1))
            require(
                result.returncode == 0
                and any(e.get("event") == "effect" for e in self.delta()),
                "carried session did not use stored login",
            )
            return "PASS", "state-key move retained home-scoped login"
        if name == "dcr_device":
            self.login(remote=True, expect_device=True)
        elif name == "dcr_paste":
            self.login(remote=True)
        elif name == "device_confidential":
            self.login(remote=True, expect_device=True)
        elif name == "issuer_mismatch":
            self.login(remote=True, tamper=True)
            require(
                not any(e.get("issued") for e in self.delta()),
                "token issued after issuer mismatch",
            )
            return "PASS", "callback refused before token dispatch"
        else:
            self.login()
        if name.startswith("dcr_") or name in (
            "confidential_issuer_pin",
            "device_confidential",
        ):
            self.test()
            if name != "dcr_device":
                result, _ = self.tool()
                require(result.returncode == 0, "scripted tool turn failed")
                require(
                    any(e.get("event") == "effect" for e in self.delta()),
                    "no MCP effect observed",
                )
        elif name in ("refresh_cadence", "rotation_reuse", "concurrent_refresh"):
            self.test()
            self.test()
            require(
                refresh_count(self.delta()) == 0,
                "60s token refreshed before half its lifetime",
            )
            # This is the behavior under test, not a readiness poll.
            time.sleep(32)
            if name == "concurrent_refresh":
                self.control(refreshDelayMs=500)
                with concurrent.futures.ThreadPoolExecutor(2) as pool:
                    results = list(
                        pool.map(lambda _: self.command("mcp", "test", "srv"), range(2))
                    )
                require(
                    refresh_count(self.delta()) == 1,
                    "concurrent processes did not collapse refresh",
                )
                for result in results:
                    require(
                        result.returncode == 0
                        or "could not renew its login" in result.stdout
                        or "renewing its login in another process" in result.stdout,
                        "refresh contender did not fail closed on the in-flight claim",
                    )
                require(
                    any(result.returncode == 0 for result in results),
                    "refresh owner did not complete",
                )
                self.test()
                require(
                    refresh_count(self.delta()) == 1,
                    "contender refreshed the new generation again",
                )
            else:
                self.test()
                self.test()
                require(
                    refresh_count(self.delta()) == 1,
                    "refresh cadence was not once per half-lifetime",
                )
                if name == "rotation_reuse":
                    status, reuse = self.control(reuse="strict")
                    require(
                        status == 200 and reuse.get("error") == "invalid_grant",
                        "AS did not detect predecessor refresh-token reuse",
                    )
                    time.sleep(32)
                    self.test(ok=False)
        elif name in ("forced_401", "fault_401"):
            if name == "forced_401":
                self.control(force401=True)
                result, _ = self.tool()
            else:
                # Calibrate the call's physical ordinal against this exact binary/SDK.
                self.tool()
                calibration = [e for e in self.delta() if e.get("event") == "mcp"]
                call_index = next(
                    i + 1
                    for i, e in enumerate(calibration)
                    if e.get("method") == "tools/call"
                )
                self.before = len(self.events())
                result, _ = self.tool(
                    faults=f"host=127.0.0.1,path=/mcp/strict,nth={call_index},fault=401"
                )
            events = self.delta()
            require(refresh_count(events) == 1, "401 did not cause exactly one refresh")
            require(
                sum(e.get("method") == "tools/call" for e in events)
                == (1 if name == "forced_401" else 0),
                "401 call was resent",
            )
            require(
                not any(e.get("event") == "effect" for e in events), "rejected tool ran"
            )
            require(
                "call again" in result.stdout or "renewed" in result.stdout,
                "missing renewed-login remedy",
            )
            self.tool()
            require(
                sum(e.get("event") == "effect" for e in self.delta()) == 1,
                "next explicit call did not succeed exactly once",
            )
        elif name in ("step_up", "hostile_scope"):
            if name == "hostile_scope":
                self.control(hostile=True)
            result, _ = self.tool("write")
            require(
                any(e.get("status") == 403 for e in self.delta()),
                "no 403 challenge observed",
            )
            require(
                not any(e.get("event") == "effect" for e in self.delta()),
                "under-scoped call ran",
            )
            require("plak mcp login srv" in result.stdout, "no login remedy")
            require("$(touch" not in result.stdout, "hostile scope reached remedy")
            if name == "step_up":
                require(
                    {"read", "write", "offline_access"} <= remedy_scopes(result.stdout),
                    "step-up remedy omitted the stored/challenged scope union",
                )
                self.login(scope="read write")
                self.tool("write")
                require(
                    any(
                        e.get("event") == "effect" and e.get("tool") == "write"
                        for e in self.delta()
                    ),
                    "step-up did not authorize tool",
                )
        elif name == "server_revocation":
            self.admin_token = self.admin_login()
            headers = {"authorization": f"Bearer {self.admin_token}"}
            base = f"{KC}/admin/realms/mcp"
            user = request(base + "/users?username=alice", headers=headers)[1][0]
            status, _ = request(
                base + f"/users/{user['id']}/consents/confidential",
                headers=headers,
                method="DELETE",
            )
            require(status == 204, "Keycloak offline-grant revocation failed")
            client = request(base + "/clients?clientId=confidential", headers=headers)[
                1
            ][0]
            status, remaining = request(
                base + f"/users/{user['id']}/offline-sessions/{client['id']}",
                headers=headers,
            )
            require(
                status == 200 and not remaining,
                "offline grant remains after revocation",
            )
            self.output.append(
                {
                    "admin_revocation": {
                        "status": 204,
                        "remaining_offline_sessions": len(remaining),
                    }
                }
            )
            # Keycloak JWTs remain valid until expiry; refresh then witnesses invalid_grant.
            time.sleep(32)
            output = self.test(ok=False)
            require(
                refresh_count(self.delta(), "keycloak") == 1,
                "revocation did not produce exactly one refresh",
            )
            require(
                "invalid_grant" in output
                and output.count("remedy: plak mcp login srv") == 1,
                "missing single invalid_grant remedy",
            )
            mark = len(self.events())
            self.test(ok=False)
            require(not self.events()[mark:], "retired login sent another request")
        elif name in ("logout_revoke", "logout_503", "logout_local"):
            faults = (
                "host=127.0.0.1,path=/token/revocation,nth=1,fault=503"
                if name == "logout_503"
                else ""
            )
            args = ["mcp", "logout", "srv"] + (
                ["--local"] if name == "logout_local" else []
            )
            result = self.command(*args, faults=faults)
            if name == "logout_503":
                require(result.returncode != 0, "503 logout succeeded")
                shown = self.command("mcp", "show", "srv").stdout
                require("quarantined" in shown, "failed revocation did not quarantine")
                self.test(ok=False)
                require(
                    self.command("mcp", "logout", "srv", "--local").returncode == 0,
                    "local retirement failed",
                )
            else:
                require(result.returncode == 0, "logout failed")
                revokes = [e for e in self.delta() if e.get("revoked")]
                require(
                    len(revokes) == (1 if name == "logout_revoke" else 0),
                    "unexpected revocation dispatch count",
                )
            require(
                "not logged in" in self.command("mcp", "show", "srv").stdout,
                "login not retired",
            )
        elif name == "resource_a_at_b":
            # In-memory fixture handoff, never print or persist the bearer outside its private store.
            self.test()
            status, value = self.control(probe_audience=True)
            require(
                status == 200 and value.get("status") == 401,
                "resource A token was not refused at B",
            )
        elif name == "shared_origin_tenant":
            expectations = json.loads(Path(self.args.resource_rules).read_text())
            self.control(resource=RESOURCE + "tenantb")
            before = len(self.events())
            try:
                self.login()
            except AssertionError:
                require(
                    expectations["tenant_metadata"] == "refuse",
                    "resource rules expected tenant login",
                )
                require(
                    not any(e.get("issued") for e in self.events()[before:]),
                    "tenant attack reached token issuance",
                )
            else:
                require(
                    expectations["tenant_metadata"] == "accept",
                    "shared-origin tenant metadata accepted",
                )
        elif name == "as_restart":
            result, sid = self.tool()
            require(
                result.returncode == 0 and sid,
                "session did not start before AS restart",
            )
            self.compose_run("restart", "keycloak")
            self.wait_url(f"{KC}/realms/mcp/.well-known/openid-configuration")
            result, _ = self.tool(session=sid)
            require(result.returncode == 0, "session did not resume after AS restart")
            time.sleep(32)
            result, _ = self.tool(session=sid)
            require(
                result.returncode == 0, "session could not refresh after AS restart"
            )
            require(
                sum(e.get("event") == "effect" for e in self.delta()) == 3,
                "restarted session missed or repeated a tool call",
            )
            require(
                refresh_count(self.delta(), "keycloak") >= 1,
                "AS restart did not exercise refresh",
            )
        return "PASS", "external AS and MCP witnesses verified"

    def close(self):
        for proc in list(self.processes):
            if proc.poll() is None:
                proc.kill()
                proc.wait()
        try:
            self.compose_run("down", "--volumes", "--remove-orphans", timeout=90)
            remaining = subprocess.check_output(
                [
                    "docker",
                    "ps",
                    "-aq",
                    "--filter",
                    f"label=com.docker.compose.project={self.project}",
                ],
                text=True,
            )
            require(not remaining.strip(), "owned containers remain after teardown")
        finally:
            shutil.rmtree(self.root)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=str(ROOT / "target/release/plak"))
    parser.add_argument("--previous-binary")
    parser.add_argument("--only", help="comma-separated scenario names")
    parser.add_argument(
        "--report", default=str(ROOT / "target/live/mcp-oauth/report.json")
    )
    parser.add_argument("--resource-rules", default=str(HERE / "resource-rules.json"))
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args(argv)
    if args.list:
        print("\n".join(SCENARIOS))
        return 0
    rules = json.loads(Path(args.resource_rules).read_text())
    if set(rules) != {"tenant_metadata"} or rules["tenant_metadata"] not in (
        "accept",
        "refuse",
    ):
        parser.error("resource rules require tenant_metadata = accept or refuse")
    if args.previous_binary and not Path(args.previous_binary).is_file():
        parser.error("--previous-binary does not exist")
    selected = args.only.split(",") if args.only else list(SCENARIOS)
    if set(selected) - set(SCENARIOS):
        parser.error("unknown scenario")
    # Fixed loopback ports, guarded across checkouts; never interfere with another lane.
    lock = open(Path(tempfile.gettempdir()) / "plak-mcp-oauth.lock", "a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    os.umask(0o077)
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        parser.error("--binary does not exist; build the test-hooks binary first")
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    harness = Harness(args)
    report = {
        "binary_sha256": digest,
        "source_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "source_dirty": bool(
            subprocess.check_output(
                ["git", "status", "--porcelain"], cwd=ROOT, text=True
            ).strip()
        ),
        "package_lock_sha256": hashlib.sha256(
            (HERE / "package-lock.json").read_bytes()
        ).hexdigest(),
        "resource_rules": rules,
        "scenarios": [],
        "paid_inference": False,
    }
    if args.previous_binary:
        report["previous_binary_sha256"] = hashlib.sha256(
            Path(args.previous_binary).resolve().read_bytes()
        ).hexdigest()

    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    try:
        probe_home = harness.root / "version-home"
        probe_home.mkdir(mode=0o700)
        version = subprocess.check_output(
            [str(harness.binary), "version"],
            text=True,
            env={
                "HOME": str(harness.root / "fake-home"),
                "PLAK_HOME": str(probe_home),
                "XDG_STATE_HOME": str(harness.root / "xdg-state"),
                "PATH": os.defpath,
            },
        )
        require("test-hooks" in version, "binary must be built with test-hooks")
        report["version"] = version.strip()
        harness.start()
        for name in selected:
            start = time.monotonic()
            try:
                result, detail = harness.scenario(name)
            except AssertionError as error:
                result, detail = "FAIL", str(error)
            except Exception as error:
                result, detail = (
                    "HARNESS",
                    type(error).__name__ + ": " + redact(str(error)),
                )
            row = {
                "name": name,
                "result": result,
                "detail": detail,
                "seconds": round(time.monotonic() - start, 2),
                "commands": list(harness.output),
                "events": harness.delta(),
            }
            report["scenarios"].append(row)
            print(f"{name}: {result}: {detail}", flush=True)
    except KeyboardInterrupt:
        report["interrupted"] = True
    except Exception as error:
        report["setup_error"] = type(error).__name__ + ": " + redact(str(error))
        print(report["setup_error"], file=sys.stderr)
    finally:
        try:
            harness.close()
            report["teardown"] = "complete"
        except Exception as error:
            report["teardown"] = "failed: " + type(error).__name__
        report["binary_unchanged"] = (
            hashlib.sha256(harness.binary.read_bytes()).hexdigest() == digest
        )
        if args.previous_binary:
            report["previous_binary_unchanged"] = (
                hashlib.sha256(
                    Path(args.previous_binary).resolve().read_bytes()
                ).hexdigest()
                == report["previous_binary_sha256"]
            )
        path = Path(args.report)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(report, indent=2) + "\n")
        path.chmod(0o600)
        print(f"report: {path}")
    return (
        0
        if len(report["scenarios"]) == len(selected)
        and all(s["result"] == "PASS" for s in report["scenarios"])
        and report.get("teardown") == "complete"
        and report["binary_unchanged"]
        and report.get("previous_binary_unchanged", True)
        else 1
    )


if __name__ == "__main__":
    raise SystemExit(main())
