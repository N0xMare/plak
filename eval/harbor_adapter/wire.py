"""Comparator recorder lifecycle; Plak uses its first-party wire captures."""
from __future__ import annotations

import asyncio
from pathlib import Path
import shlex
import time

from harbor_adapter.preflight import PreflightError

DIRECTORY = "/tmp/harbor-wire-cell"
HOSTS = {
    "codex": ("chatgpt.com", "api.openai.com"),
    "grok-build": ("api.x.ai",),
    "pmux": ("api.anthropic.com",),
    "pi": ("chatgpt.com", "api.openai.com"),
    "opencode": ("openrouter.ai",),
}


def bounded_command(command: str, seconds: float) -> str:
    if seconds <= 0:
        raise PreflightError("no remaining agent budget")
    return f"timeout --signal=TERM --kill-after=2s {seconds:.3f}s bash -c {shlex.quote(command)}"


class WireAgent:
    async def exec_as_agent(self, environment, command, **kwargs):
        if getattr(self, "_native_harness", None) == "pi":
            command = command.replace("/logs/agent/pi/sessions", "/tmp/pi-cell/sessions")
        elif getattr(self, "_native_harness", None) == "opencode":
            command = command.replace("/logs/agent/opencode/xdg-data", "/tmp/opencode-cell/xdg-data")
        from harbor_adapter.argv import native_argv, write_argv
        instruction = getattr(self, "_eval_instruction", None)
        if instruction is not None:
            argv = native_argv(command, instruction, getattr(self, "_native_harness", "pmux"))
            if argv:
                write_argv(self.logs_dir, argv)
        env = dict(kwargs.pop("env", None) or {})
        env.update(getattr(self, "_wire_env", {}))
        if getattr(self, "_native_harness", None) == "opencode" and "XDG_DATA_HOME" in env:
            env["XDG_DATA_HOME"] = "/tmp/opencode-cell/xdg-data"
        deadline = getattr(self, "_eval_deadline", None)
        if deadline is not None:
            command = bounded_command(command, max(1, deadline - time.monotonic()))
        return await super().exec_as_agent(environment, command=command, env=env, **kwargs)


async def start_recording(agent, environment) -> None:
    harness = getattr(agent, "_native_harness", "pmux")
    hosts = HOSTS[harness]
    if harness == "opencode":
        provider = (agent.model_name or "").split("/", 1)[0]
        hosts = {"openai": HOSTS["codex"], "xai": HOSTS["grok-build"],
                 "openrouter": HOSTS["opencode"]}.get(provider)
        if hosts is None:
            raise PreflightError("no exact recording hosts for this OpenCode route")
    package = "/tmp/harbor-wire-code/harbor_adapter"
    await agent.exec_as_root(environment, command=f"mkdir -p {package}; touch {package}/__init__.py")
    for name in ("observed_config.py", "recording_proxy.py"):
        await environment.upload_file(str(Path(__file__).with_name(name)), f"{package}/{name}")
    args = " ".join("--host " + shlex.quote(host) for host in hosts)
    # No proxy stdout/stderr reaches a harvested transcript, even on a TLS fault.
    command = (
        f"umask 077; PYTHONPATH=/tmp/harbor-wire-code python3 -m harbor_adapter.recording_proxy "
        f"--directory {DIRECTORY} --passthrough {args} >/dev/null 2>&1 & echo $! > /tmp/harbor-wire.pid"
    )
    started = await agent.exec_as_root(environment, command=command)
    if started.return_code:
        raise PreflightError("wire recorder could not start")
    async def ready():
        for _ in range(50):
            result = await environment.exec(command=f"test -f {DIRECTORY}/ready")
            if not result.return_code:
                return
            await asyncio.sleep(.1)
        raise PreflightError("wire recorder readiness timeout")
    await ready()
    await agent.exec_as_root(environment, command=(
        f"cat /etc/ssl/certs/ca-certificates.crt {DIRECTORY}/ca.pem > {DIRECTORY}/bundle.pem"
    ))
    agent._wire_env = {
        "HTTPS_PROXY": "http://127.0.0.1:18080",
        "https_proxy": "http://127.0.0.1:18080",
        "NO_PROXY": "localhost,127.0.0.1", "no_proxy": "localhost,127.0.0.1",
        "NODE_EXTRA_CA_CERTS": DIRECTORY + "/ca.pem",
        "SSL_CERT_FILE": DIRECTORY + "/bundle.pem",
        "REQUESTS_CA_BUNDLE": DIRECTORY + "/bundle.pem",
    }


async def harvest_recording(agent, environment) -> None:
    # Keep the proxy alive for inherited tool environments through verification.
    # The Harbor container teardown, not agent completion, owns its lifetime.
    agent._eval_deadline = None
    agent._wire_env = {}
    result = await agent.exec_as_root(environment, command=(
        f"test -s {DIRECTORY}/observed-config.json && "
        f"cp {DIRECTORY}/observed-config.json /logs/agent/observed-config.json"
    ))
    if result.return_code:
        raise PreflightError("no first wire request recorded; this CLI did not use the proxy")
