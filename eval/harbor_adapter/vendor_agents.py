"""Harbor installed agents that upload the pinned vendor CLIs.

Stock Harbor `run()` remains for Codex after frozen install. Grok/Pi/OpenCode
`run()` inject host OAuth `auth.json` (never host interactive config:
Grok `config.toml`, Pi picker/packages, OpenCode `opencode.jsonc`).
Only `install()` is replaced elsewhere so trials do not download Node, float
the CLI version, or pipe an install script into bash on the critical path.
Effort is pinned by Harbor `--ak`, not by copying the operator's interactive
settings. Provider API keys in the Harbor process environment are refused.
"""

from __future__ import annotations

import json
import asyncio
import os
import re
import shlex
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths
from harbor.agents.installed.codex import Codex
from harbor.agents.installed.grok_build import GrokBuild
from harbor.agents.installed.opencode import OpenCode
from harbor.agents.installed.pi import Pi
from harbor.environments.base import BaseEnvironment

from harbor_adapter.credentials import (
    CredentialError,
    opencode_auth_provider,
    pi_cli_model_name,
    pi_cli_provider,
    remote_grok_auth_acl_command,
    remote_uploaded_auth_acl_command,
    replace_pi_codex_auth,
    require_ready_grok_auth,
    require_ready_opencode_auth,
    require_ready_pi_auth,
    replace_grok_auth,
    validate_grok_auth,
)
from harbor_adapter.controls import NON_STOCK_KWARGS
from harbor_adapter.artifact_identity import artifact_record
from harbor_adapter.arch import (
    installed_root,
    platform_record_command,
    resolve_arch,
)
from harbor_adapter.vendor_install import grok_install_command, node_cli_install_command
from harbor_adapter.native_usage import extract_agent_usage
from harbor_adapter.vendor_pins import (
    CLI_PINS,
    REMOTE_VENDOR,
    FrozenArtifacts,
    VendorArtifactError,
    require_frozen_artifacts,
)


def _vendor_selection(kwargs: dict[str, Any]) -> tuple[str, Path]:
    """The cell architecture and the vendor prefix root that matches it."""
    arch = resolve_arch(kwargs.pop("arch", None))
    raw = kwargs.pop("vendor_root", None)
    if raw is None:
        return arch, installed_root(arch) / "vendor"
    return arch, Path(raw)


def _force_pin(cli: str, kwargs: dict[str, Any]) -> None:
    pinned = CLI_PINS[cli]
    incoming = kwargs.get("version")
    if incoming is None:
        kwargs["version"] = pinned
        return
    if str(incoming) != pinned:
        raise VendorArtifactError(
            f"{cli} version {incoming!r} is not the bakeoff pin {pinned}"
        )


def _reject_host_overlays(cli: str, kwargs: dict[str, Any]) -> None:
    """Fail closed if a cell would inherit host Pi/OpenCode/Grok config."""
    for key in NON_STOCK_KWARGS.get(cli, ()):
        if kwargs.get(key) not in (None, {}, ""):
            raise VendorArtifactError(
                f"{cli} {key} is not stock bakeoff behavior; Harbor cells "
                "must not inherit host agent config"
            )


async def _install_node_cli(
    agent: BaseInstalledAgent,
    environment: BaseEnvironment,
    artifacts: FrozenArtifacts,
) -> None:
    cli = artifacts.cli
    await agent.exec_as_root(
        environment, command=f"mkdir -p {shlex.quote(REMOTE_VENDOR)}"
    )
    await environment.upload_file(
        str(artifacts.node_tarball.resolve()), f"{REMOTE_VENDOR}/node.tar.gz"
    )
    await environment.upload_file(
        str(artifacts.cli_archive.resolve()), f"{REMOTE_VENDOR}/{cli}.tar.gz"
    )
    await agent.exec_as_root(environment, command=node_cli_install_command(cli))


async def _assert_version(
    agent: BaseInstalledAgent,
    environment: BaseEnvironment,
    pinned: str,
) -> None:
    command = agent.get_version_command()
    if not command:
        raise VendorArtifactError("frozen vendor CLI has no version command")
    # Launch in the real task image before credentials or paid dispatch. A
    # matching manifest cannot establish loader/libc compatibility there.
    try:
        result = await asyncio.wait_for(
            environment.exec(
                command=f"timeout -k 2s 15s sh -c {shlex.quote(command)}"
            ),
            20,
        )
    except Exception:
        raise VendorArtifactError("frozen vendor CLI cell version probe failed") from None
    if result.return_code != 0:
        raise VendorArtifactError(
            f"frozen vendor CLI cell version probe returned {result.return_code}"
        )
    installed = agent.parse_version(result.stdout or "")
    if installed != pinned and pinned not in installed:
        raise VendorArtifactError(
            f"frozen CLI reported {installed!r}, pin is {pinned}"
        )


async def _record_executable_identity(
    agent: BaseInstalledAgent, environment: BaseEnvironment, cli: str, arch: str,
    *, prerequisites: dict[str, Any] | None = None,
) -> None:
    """Record actual in-cell executable bytes after extraction and version probe."""
    names = [cli, f"{cli}-archive"]
    paths = [f"/usr/local/bin/{cli}", f"{REMOTE_VENDOR}/{cli}" + ("" if cli == "grok" else ".tar.gz")]
    if cli != "grok":
        names.extend(("node", "node-archive"))
        paths.extend((f"{REMOTE_VENDOR}/node/bin/node", f"{REMOTE_VENDOR}/node.tar.gz"))
    binaries: dict[str, dict[str, Any]] = {}
    for name, path in zip(names, paths, strict=True):
        try:
            result = await asyncio.wait_for(
                environment.exec(command=f"sha256sum {shlex.quote(path)} && wc -c < {shlex.quote(path)}"),
                10,
            )
            lines = (result.stdout or "").splitlines()
            sha = lines[0].split()[0] if lines else ""
            size = int(lines[1]) if len(lines) > 1 else 0
        except Exception:
            raise VendorArtifactError(f"frozen {cli} cell identity probe failed") from None
        if result.return_code != 0 or len(sha) != 64 or any(c not in "0123456789abcdef" for c in sha) or size <= 0:
            raise VendorArtifactError(f"frozen {cli} cell identity probe is incomplete")
        binaries[name] = {"sha256": sha, "bytes": size}
    record = artifact_record(harness=cli, binaries=binaries, arch=arch)
    record["version"] = CLI_PINS[cli]
    if prerequisites:
        record["prerequisites"] = prerequisites
    (agent.logs_dir / "vendor-artifact.json").write_text(
        json.dumps(record, indent=2) + "\n", encoding="utf-8"
    )


_CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"


async def _ensure_codex_ca_bundle(
    agent: BaseInstalledAgent, environment: BaseEnvironment,
) -> dict[str, Any]:
    """Ensure and bind the trust store seen by the native Codex process."""
    command = (
        f"test -s {_CA_BUNDLE} && test -r {_CA_BUNDLE} && "
        f"sha256sum {_CA_BUNDLE} && wc -c < {_CA_BUNDLE}"
    )

    async def probe() -> tuple[str, int] | None:
        try:
            result = await asyncio.wait_for(environment.exec(command=command), 10)
            lines = (result.stdout or "").splitlines()
            sha = lines[0].split()[0] if lines else ""
            size = int(lines[1]) if len(lines) > 1 else 0
        except Exception:
            return None
        if (result.return_code != 0 or len(sha) != 64 or
                any(c not in "0123456789abcdef" for c in sha) or size <= 0):
            return None
        return sha, size

    found = await probe()
    if found is None:
        await agent.ensure_system_dependencies(environment, ("ca_certificates",))
        found = await probe()
    if found is None:
        raise VendorArtifactError("native codex cell has no readable nonempty CA bundle")
    sha, size = found
    return {"path": _CA_BUNDLE, "sha256": sha, "bytes": size}


class _CanonicalNativeUsage:
    """Attach content-free canonical token buckets after Harbor's stock parser."""

    _native_harness: str

    def populate_context_post_run(self, context: AgentContext) -> None:
        try:
            super().populate_context_post_run(context)
        finally:
            usage = extract_agent_usage(Path(self.logs_dir), self._native_harness)
            record = usage.as_dict()
            (Path(self.logs_dir) / "native-usage.json").write_text(
                json.dumps(record, indent=2) + "\n", encoding="utf-8"
            )
            metadata = dict(context.metadata or {})
            metadata["native_usage"] = record
            metadata["usage_status"] = usage.status
            metadata["missing_usage_buckets"] = list(usage.missing_buckets)
            metadata["cache_write_tokens"] = usage.cache_write_tokens
            metadata["reasoning_tokens"] = usage.reasoning_tokens
            context.metadata = metadata
            if usage.usage_records:
                context.n_input_tokens = usage.input_tokens
                context.n_cache_tokens = usage.cache_read_tokens
                context.n_output_tokens = usage.output_tokens


class FrozenCodex(_CanonicalNativeUsage, Codex):
    _native_harness = "codex"
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self._arch, self._vendor_root = _vendor_selection(kwargs)
        _reject_host_overlays("codex", kwargs)
        _force_pin("codex", kwargs)
        super().__init__(*args, **kwargs)

    async def install(self, environment: BaseEnvironment) -> None:
        artifacts = require_frozen_artifacts("codex", self._vendor_root, self._arch)
        await _install_node_cli(self, environment, artifacts)
        await self.ensure_system_dependencies(environment, ("ripgrep",))
        ca_bundle = await _ensure_codex_ca_bundle(self, environment)
        await self.exec_as_root(
            environment, command=platform_record_command(self._arch)
        )
        await _assert_version(self, environment, artifacts.version)
        await _record_executable_identity(
            self, environment, "codex", self._arch,
            prerequisites={"ca_bundle": ca_bundle},
        )


class FrozenGrokBuild(_CanonicalNativeUsage, GrokBuild):
    _native_harness = "grok-build"
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self._arch, self._vendor_root = _vendor_selection(kwargs)
        _reject_host_overlays("grok", kwargs)
        _force_pin("grok", kwargs)
        super().__init__(*args, **kwargs)

    async def install(self, environment: BaseEnvironment) -> None:
        artifacts = require_frozen_artifacts("grok", self._vendor_root, self._arch)
        await self.exec_as_root(
            environment, command=f"mkdir -p {shlex.quote(REMOTE_VENDOR)}"
        )
        await environment.upload_file(
            str(artifacts.cli_archive.resolve()), f"{REMOTE_VENDOR}/grok"
        )
        await self.exec_as_root(environment, command=grok_install_command())
        await self.exec_as_root(
            environment, command=platform_record_command(self._arch)
        )
        await _assert_version(self, environment, artifacts.version)
        await _record_executable_identity(self, environment, "grok", self._arch)

    _REMOTE_GROK_SECRETS_DIR = "/tmp/grok-secrets"

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if self._get_env("XAI_API_KEY"):
            raise VendorArtifactError(
                "grok-build native-home must use SuperGrok OAuth auth.json, "
                "not XAI_API_KEY"
            )
        if self._provider:
            raise VendorArtifactError(
                "FrozenGrokBuild is SuperGrok OAuth only; non-xAI model routes "
                "are refused"
            )

        auth_json = require_ready_grok_auth()
        auth_snapshot = auth_json.read_bytes()
        validate_grok_auth(auth_snapshot)
        identity = await self.exec_as_agent(environment, command="id -u")
        uid_text = (identity.stdout or "").strip()
        if not re.fullmatch(r"[0-9]+", uid_text):
            raise CredentialError("native Grok agent UID could not be established")
        agent_uid = int(uid_text)
        escaped_instruction = shlex.quote(instruction)
        remote_auth = f"{self._REMOTE_GROK_SECRETS_DIR}/auth.json"

        await self.exec_as_root(
            environment,
            command=f"mkdir -p {shlex.quote(self._REMOTE_GROK_SECRETS_DIR)}",
        )
        await environment.upload_file(str(auth_json), remote_auth)
        if auth_json.read_bytes() != auth_snapshot:
            raise CredentialError("Grok OAuth host credential changed during upload")
        await self.exec_as_root(
            environment,
            command=remote_grok_auth_acl_command(
                agent_uid, remote_auth
            ),
        )
        await self.exec_as_agent(
            environment,
            command=(
                'mkdir -p "$HOME/.grok" && '
                f"ln -sf {shlex.quote(remote_auth)} \"$HOME/.grok/auth.json\" && "
                'chmod 0700 "$HOME/.grok" && '
                f"chmod 0600 {shlex.quote(remote_auth)} \"$HOME/.grok/auth.json\""
            ),
        )

        env = {
            "GROK_DISABLE_AUTOUPDATER": "1",
            "GROK_LOG_FILE": (
                EnvironmentPaths.agent_dir / self._CLI_LOG_FILENAME
            ).as_posix(),
            # The pinned CLI's WARN refresh diagnostic prints a token prefix.
            "RUST_LOG": "error",
        }

        await self.exec_as_agent(
            environment, command=self._build_write_config_command()
        )

        try:
            await self.exec_as_agent(
                environment,
                command=self._build_run_script(escaped_instruction),
                env=env,
            )
        finally:
            primary_error = sys.exc_info()[0]
            writeback_error: Exception | None = None
            try:
                await self._persist_refreshed_auth(
                    environment, auth_json, auth_snapshot
                )
            except Exception as exc:
                if primary_error is None:
                    writeback_error = exc
                else:
                    self.logger.warning(
                        "Grok OAuth refresh was not persisted: %s",
                        type(exc).__name__,
                    )
            try:
                sessions_target = EnvironmentPaths.agent_dir / "sessions"
                await self.exec_as_agent(
                    environment,
                    command=(
                        f"mkdir -p {EnvironmentPaths.agent_dir.as_posix()}\n"
                        'if [ -d "$HOME/.grok/sessions" ]; then\n'
                        f"  rm -rf {sessions_target.as_posix()}\n"
                        f'  cp -R "$HOME/.grok/sessions" {sessions_target.as_posix()}\n'
                        "fi"
                    ),
                )
            except Exception:
                self.logger.debug("Failed to copy grok session files", exc_info=True)
            if writeback_error is not None:
                raise writeback_error

    async def _persist_refreshed_auth(
        self, environment: BaseEnvironment, auth_json: Path, snapshot: bytes
    ) -> None:
        remote_candidate = f"{self._REMOTE_GROK_SECRETS_DIR}/auth-refreshed.json"
        await self.exec_as_agent(
            environment,
            command=(
                'test -f "$HOME/.grok/auth.json" && '
                f'cp -- "$HOME/.grok/auth.json" {shlex.quote(remote_candidate)} && '
                f'chmod 0600 {shlex.quote(remote_candidate)}'
            ),
        )
        descriptor, candidate_name = tempfile.mkstemp(
            prefix=".auth.json.cell-", dir=auth_json.parent
        )
        os.close(descriptor)
        candidate = Path(candidate_name)
        try:
            await environment.download_file(remote_candidate, str(candidate))
            replace_grok_auth(auth_json, candidate, snapshot)
        finally:
            candidate.unlink(missing_ok=True)


class FrozenPi(_CanonicalNativeUsage, Pi):
    _native_harness = "pi"
    _REMOTE_PI_SECRETS_DIR = "/tmp/pi-secrets"

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self._arch, self._vendor_root = _vendor_selection(kwargs)
        _reject_host_overlays("pi", kwargs)
        _force_pin("pi", kwargs)
        super().__init__(*args, **kwargs)

    async def install(self, environment: BaseEnvironment) -> None:
        artifacts = require_frozen_artifacts("pi", self._vendor_root, self._arch)
        await _install_node_cli(self, environment, artifacts)
        await self.ensure_system_dependencies(environment, ("coreutils",))
        await self.exec_as_root(
            environment, command=platform_record_command(self._arch)
        )
        await _assert_version(self, environment, artifacts.version)
        await _record_executable_identity(self, environment, "pi", self._arch)

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if self._get_env("OPENAI_API_KEY") or self._get_env("XAI_API_KEY") or self._get_env(
            "OPENROUTER_API_KEY"
        ):
            raise VendorArtifactError(
                "pi native-home must use host auth.json, not provider API keys"
            )
        harbor_model = self.model_name or ""
        provider = pi_cli_provider(harbor_model)
        auth_json = require_ready_pi_auth(pi_cli_provider(harbor_model))
        auth_snapshot = auth_json.read_bytes()
        remote_auth = f"{self._REMOTE_PI_SECRETS_DIR}/auth.json"
        await self.exec_as_root(
            environment,
            command=f"mkdir -p {shlex.quote(self._REMOTE_PI_SECRETS_DIR)}",
        )
        await environment.upload_file(str(auth_json), remote_auth)
        if auth_json.read_bytes() != auth_snapshot:
            raise CredentialError("Pi OAuth host credential changed during upload")
        await self.exec_as_root(
            environment,
            command=remote_uploaded_auth_acl_command(
                environment.default_user, remote_auth
            ),
        )
        await self.exec_as_agent(
            environment,
            command=(
                'mkdir -p "$HOME/.pi/agent" && '
                f"ln -sf {shlex.quote(remote_auth)} \"$HOME/.pi/agent/auth.json\" && "
                'chmod 0700 "$HOME/.pi" "$HOME/.pi/agent" && '
                f"chmod 0600 {shlex.quote(remote_auth)} \"$HOME/.pi/agent/auth.json\""
            ),
        )
        original = self.model_name
        self.model_name = pi_cli_model_name(harbor_model)
        try:
            await super().run(instruction, environment, context)
        finally:
            self.model_name = original
            if provider == "openai-codex":
                primary_error = sys.exc_info()[0]
                try:
                    await self._persist_refreshed_codex_auth(
                        environment, auth_json, auth_snapshot
                    )
                except Exception as exc:
                    if primary_error is None:
                        raise
                    self.logger.warning(
                        "Pi Codex OAuth refresh was not persisted: %s",
                        type(exc).__name__,
                    )

    async def _persist_refreshed_codex_auth(
        self, environment: BaseEnvironment, auth_json: Path, snapshot: bytes
    ) -> None:
        remote_candidate = f"{self._REMOTE_PI_SECRETS_DIR}/auth-refreshed.json"
        await self.exec_as_agent(
            environment,
            command=(
                'test -f "$HOME/.pi/agent/auth.json" && '
                f'cp -- "$HOME/.pi/agent/auth.json" {shlex.quote(remote_candidate)} && '
                f'chmod 0600 {shlex.quote(remote_candidate)}'
            ),
        )
        descriptor, candidate_name = tempfile.mkstemp(
            prefix=".auth.json.pi-cell-", dir=auth_json.parent
        )
        os.close(descriptor)
        candidate = Path(candidate_name)
        try:
            await environment.download_file(remote_candidate, str(candidate))
            replace_pi_codex_auth(auth_json, candidate, snapshot)
        finally:
            candidate.unlink(missing_ok=True)


class FrozenOpenCode(_CanonicalNativeUsage, OpenCode):
    _native_harness = "opencode"
    _REMOTE_OPENCODE_SECRETS_DIR = "/tmp/opencode-secrets"
    _ATTEMPTS_FILENAME = "opencode-attempts.jsonl"
    # Must match Harbor OpenCode.run XDG_DATA_HOME.
    _HARBOR_OPENCODE_XDG_DATA = "/logs/agent/opencode/xdg-data"

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self._arch, self._vendor_root = _vendor_selection(kwargs)
        _reject_host_overlays("opencode", kwargs)
        _force_pin("opencode", kwargs)
        super().__init__(*args, **kwargs)

    async def install(self, environment: BaseEnvironment) -> None:
        artifacts = require_frozen_artifacts("opencode", self._vendor_root, self._arch)
        await _install_node_cli(self, environment, artifacts)
        await self.ensure_system_dependencies(environment, ("coreutils",))
        await self.exec_as_root(
            environment, command=platform_record_command(self._arch)
        )
        await _assert_version(self, environment, artifacts.version)
        await _record_executable_identity(self, environment, "opencode", self._arch)

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if self._get_env("OPENAI_API_KEY") or self._get_env("XAI_API_KEY") or self._get_env(
            "OPENROUTER_API_KEY"
        ):
            raise VendorArtifactError(
                "opencode native-home must use host auth.json, not provider API keys"
            )
        harbor_model = self.model_name or ""
        auth_json = require_ready_opencode_auth(opencode_auth_provider(harbor_model))
        remote_auth = f"{self._REMOTE_OPENCODE_SECRETS_DIR}/auth.json"
        xdg_auth_dir = f"{self._HARBOR_OPENCODE_XDG_DATA}/opencode"
        await self.exec_as_root(
            environment,
            command=f"mkdir -p {shlex.quote(self._REMOTE_OPENCODE_SECRETS_DIR)}",
        )
        await environment.upload_file(str(auth_json), remote_auth)
        await self.exec_as_root(
            environment,
            command=remote_uploaded_auth_acl_command(
                environment.default_user, remote_auth
            ),
        )
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p {shlex.quote(xdg_auth_dir)} && "
                f"ln -sf {shlex.quote(remote_auth)} "
                f"{shlex.quote(xdg_auth_dir)}/auth.json && "
                f"chmod 0700 {shlex.quote(self._HARBOR_OPENCODE_XDG_DATA)} "
                f"{shlex.quote(xdg_auth_dir)} && "
                f"chmod 0600 {shlex.quote(remote_auth)} "
                f"{shlex.quote(xdg_auth_dir)}/auth.json"
            ),
        )
        started = time.time()
        try:
            await super().run(instruction, environment, context)
        except NonZeroAgentExitCodeError as exc:
            detail = str(exc)
            try:
                detail = "; ".join(self._error_messages()) or detail
            except Exception:
                pass
            self._record_attempt(
                attempt=1,
                started=started,
                outcome="failed",
                error=detail,
            )
            raise
        else:
            self._record_attempt(
                attempt=1, started=started, outcome="ok", error=None
            )

    def _record_attempt(
        self,
        *,
        attempt: int,
        started: float,
        outcome: str,
        error: str | None,
    ) -> None:
        """Append one attempt to the trial's OpenCode run history."""
        finished = time.time()
        row = {
            "agent": "opencode",
            "attempt": attempt,
            "outcome": outcome,
            "started_epoch_s": round(started, 3),
            "finished_epoch_s": round(finished, 3),
            "duration_s": round(finished - started, 3),
            "error": error,
        }
        try:
            path = Path(self.logs_dir) / self._ATTEMPTS_FILENAME
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(row, sort_keys=True) + "\n")
        except OSError:
            # Evidence is best-effort; it must never fail the trial.
            pass
