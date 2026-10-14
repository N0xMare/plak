"""Harbor installed agent: Full-cell `pmux run` (default Claude Code).

This is the Claude native-home comparator. It does not call the minified
ask pool, Harbor stock print-mode Claude, or set ANTHROPIC_API_KEY.
"""

from __future__ import annotations

import json
import platform
import shlex
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from harbor_adapter.credentials import (
    remote_claude_securestorage_acl_command,
    refuse_stripped_env,
    replace_claude_code_store,
    require_ready_claude_code_store,
)
from harbor_adapter.pmux_args import PmuxArgumentError, pmux_run_arguments
from harbor_adapter.artifact_identity import digest_all, pmux_artifact_record
from harbor_adapter.pmux_pins import require_frozen_pmux
from harbor_adapter.pmux_trial import pmux_prompt_write_command, pmux_trial_command
from harbor_adapter.tuples import (
    REMOTE_CLAUDE_SECURESTORAGE,
    REMOTE_PMUX,
    REMOTE_PMUXD,
    REMOTE_PMUX_HOOK,
    REMOTE_PMUX_LAUNCHER,
    REMOTE_PMUX_PROMPT,
    REMOTE_PMUX_RMUXD,
    REMOTE_WORKSPACE,
    STRIP_ENV,
)
from harbor_adapter.arch import (
    installed_root,
    platform_record_command,
    resolve_arch,
)
from harbor_adapter.overload import should_score_through_rosetta
from harbor_adapter.usage import UsageTotals, parse_pmux_json, parse_pmux_jsonl
from harbor_adapter.vendor_install import node_cli_install_command
from harbor_adapter.vendor_pins import (
    CLI_PINS,
    REMOTE_VENDOR,
    FrozenArtifacts,
    VendorArtifactError,
    require_frozen_artifacts,
)


# Harness termination states recorded on every trial, scored through or not.
TERMINATION_NOT_RUN = "not-run"
TERMINATION_OK = "ok"
TERMINATION_CRASH = "harness-crash"
TERMINATION_CRASH_SCORED_THROUGH = "harness-crash-scored-through"
TERMINATION_ERROR = "harness-error"


def _apply_usage(
    context: AgentContext,
    usage: UsageTotals,
    *,
    harness_termination: str = TERMINATION_NOT_RUN,
    artifact: dict[str, Any] | None = None,
) -> None:
    context.n_input_tokens = usage.n_input_tokens
    context.n_cache_tokens = usage.n_cache_tokens
    context.n_output_tokens = usage.n_output_tokens
    context.cost_usd = usage.cost_usd if usage.resolved else None
    context.metadata = {
        "cost_status": usage.cost_status,
        "usage_resolved": usage.resolved,
        "usage_status": usage.usage_status,
        "missing_usage_buckets": usage.missing_buckets,
        "harness": "pmux-run",
        "claude_pin": CLI_PINS["claude"],
        "cache_write_tokens": usage.n_cache_write_tokens,
        "harness_termination": harness_termination,
        "artifact": artifact or {},
    }


class FrozenPmux(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "pmux"

    def __init__(
        self,
        logs_dir: Path,
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        extra_env = dict(extra_env or {})
        for key in STRIP_ENV:
            extra_env.pop(key, None)
            extra_env.pop(key.upper(), None)
        self._arch = resolve_arch(kwargs.pop("arch", None))
        root = installed_root(self._arch)
        self._vendor_root = Path(
            kwargs.pop("vendor_root", None) or root / "vendor"
        )
        self._pmux_root = Path(
            kwargs.pop("pmux_root", None) or root / "pmux"
        )
        self._model = str(kwargs.pop("model", "claude-sonnet-5"))
        self._effort = str(kwargs.pop("effort", "low"))
        self._turn_timeout_ms = int(
            kwargs.pop("turn_timeout_ms", 900_000)
        )
        self._last_usage: UsageTotals | None = None
        self._harness_termination = TERMINATION_NOT_RUN
        self._artifact: dict[str, Any] = {}
        super().__init__(logs_dir, extra_env=extra_env, **kwargs)

    def get_version_command(self) -> str:
        return "claude --version"

    async def install(self, environment: BaseEnvironment) -> None:
        artifacts = require_frozen_artifacts(
            "claude", self._vendor_root, self._arch
        )
        bins = require_frozen_pmux(self._pmux_root, self._arch)
        # Record which bytes this trial uploads, before it uploads them. A
        # trial that passes in the morning and times out at night must be
        # answerable on "did the artifact change" from its own record.
        self._artifact = pmux_artifact_record(bins, arch=self._arch)
        self._artifact["binaries"].update(digest_all((
            ("claude-archive", artifacts.cli_archive),
            ("node-archive", artifacts.node_tarball),
        )))
        self._artifact["version"] = artifacts.version
        await self._install_claude_prefix(environment, artifacts)
        remotes = (
            (bins.pmux, REMOTE_PMUX),
            (bins.pmuxd, REMOTE_PMUXD),
            (bins.rmuxd, REMOTE_PMUX_RMUXD),
            (bins.launcher, REMOTE_PMUX_LAUNCHER),
            (bins.hook, REMOTE_PMUX_HOOK),
        )
        for local, remote in remotes:
            await environment.upload_file(str(local.resolve()), remote)
        quoted = " ".join(shlex.quote(remote) for _, remote in remotes)
        await self.exec_as_root(
            environment,
            command=f"chmod 0755 {quoted}",
        )
        await self.exec_as_root(
            environment, command=platform_record_command(self._arch)
        )
        await self._assert_claude_pin(environment, artifacts.version)

    async def _install_claude_prefix(
        self, environment: BaseEnvironment, artifacts: FrozenArtifacts
    ) -> None:
        await self.exec_as_root(
            environment, command=f"mkdir -p {shlex.quote(REMOTE_VENDOR)}"
        )
        await environment.upload_file(
            str(artifacts.node_tarball.resolve()), f"{REMOTE_VENDOR}/node.tar.gz"
        )
        await environment.upload_file(
            str(artifacts.cli_archive.resolve()), f"{REMOTE_VENDOR}/claude.tar.gz"
        )
        await self.exec_as_root(
            environment, command=node_cli_install_command("claude")
        )

    async def _assert_claude_pin(
        self, environment: BaseEnvironment, pinned: str
    ) -> None:
        result = await environment.exec(command="claude --version")
        if result.return_code != 0:
            raise VendorArtifactError(
                f"claude --version failed: {result.stderr or result.stdout}"
            )
        reported = (result.stdout or "").strip()
        if pinned not in reported:
            raise VendorArtifactError(
                f"frozen Claude Code reported {reported!r}, pin is {pinned}"
            )

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        refuse_stripped_env(self.extra_env)
        if not instruction.strip():
            raise PmuxArgumentError("prompt is empty")
        store = require_ready_claude_code_store()
        credential = store / ".credentials.json"
        remote_cred = f"{REMOTE_CLAUDE_SECURESTORAGE}/.credentials.json"
        await self.exec_as_root(
            environment,
            command=f"mkdir -p {shlex.quote(REMOTE_CLAUDE_SECURESTORAGE)}",
        )
        await environment.upload_file(str(credential.resolve()), remote_cred)
        await self.exec_as_root(
            environment,
            command=remote_claude_securestorage_acl_command(
                environment.default_user, REMOTE_CLAUDE_SECURESTORAGE
            ),
        )
        await self.exec_as_root(
            environment,
            command=pmux_prompt_write_command(instruction),
        )
        argv = pmux_run_arguments(
            model=self._model,
            effort=self._effort,
            prompt_file=REMOTE_PMUX_PROMPT,
            cwd=REMOTE_WORKSPACE,
        )
        quoted = " ".join(shlex.quote(part) for part in argv)
        stdout_path = "/logs/agent/pmux-stdout.txt"
        stderr_path = "/logs/agent/pmux-stderr.log"
        command = pmux_trial_command(
            quoted,
            stdout_path,
            stderr_path,
            turn_timeout_ms=self._turn_timeout_ms,
        )
        try:
            try:
                await self.exec_as_agent(environment, command=command)
                self._harness_termination = TERMINATION_OK
            except NonZeroAgentExitCodeError:
                # Record the crash before deciding whether to score through it:
                # a harness death is never invisible in the trial record.
                self._harness_termination = TERMINATION_CRASH
                if not await self._score_through_rosetta(environment):
                    raise
                self._harness_termination = TERMINATION_CRASH_SCORED_THROUGH
            except BaseException as error:
                # Any other harness death names itself in the trial record
                # rather than leaving the run indistinguishable from not-run.
                self._harness_termination = (
                    f"{TERMINATION_ERROR}:{type(error).__name__}"
                )
                raise
        finally:
            await self._persist_refreshed_pin(environment, store)
            await self._capture_usage(environment, context)

    async def _score_through_rosetta(self, environment: BaseEnvironment) -> bool:
        stderr = await self._read_agent_file(
            environment, "/logs/agent/pmux-stderr.log"
        )
        listed = await self.exec_as_agent(
            environment,
            command=(
                "ls /logs/agent/claude-transcripts/*.jsonl "
                "2>/dev/null || true"
            ),
        )
        has_transcripts = bool((listed.stdout or "").strip())
        return should_score_through_rosetta(
            stderr,
            has_transcripts=has_transcripts,
            cell_arch=self._arch,
            host_system=platform.system(),
        )

    async def _read_agent_file(
        self, environment: BaseEnvironment, path: str
    ) -> str:
        result = await self.exec_as_agent(
            environment,
            command=f"cat {shlex.quote(path)} 2>/dev/null || true",
        )
        return result.stdout or ""

    async def _persist_refreshed_pin(
        self, environment: BaseEnvironment, store: Path
    ) -> None:
        """Copy the in-cell pin back so the next trial is not NeedsLogin."""
        remote_cred = f"{REMOTE_CLAUDE_SECURESTORAGE}/.credentials.json"
        candidate = store / ".credentials.json.cell"
        try:
            await environment.download_file(remote_cred, str(candidate))
            replace_claude_code_store(store, candidate)
        except Exception:
            if candidate.is_file():
                candidate.unlink()

    async def _capture_usage(
        self, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        result = await self.exec_as_agent(
            environment,
            command="cat /logs/agent/pmux-stdout.txt 2>/dev/null || true",
        )
        try:
            self._last_usage = parse_pmux_json(result.stdout or "")
        except Exception:
            self._last_usage = parse_pmux_json("{}")
        if not self._last_usage.resolved:
            jsonl = await self.exec_as_agent(
                environment,
                command=(
                    "cat /logs/agent/claude-transcripts/*.jsonl "
                    "2>/dev/null || true"
                ),
            )
            fallback = parse_pmux_jsonl(jsonl.stdout or "")
            if fallback.resolved:
                self._last_usage = fallback
        _apply_usage(
            context,
            self._last_usage,
            harness_termination=self._harness_termination,
            artifact=self._artifact,
        )
        evidence = self.logs_dir / "pmux-usage.json"
        record = dict(self._last_usage.__dict__)
        record["harness_termination"] = self._harness_termination
        record["artifact"] = self._artifact
        evidence.write_text(
            json.dumps(record, indent=2) + "\n",
            encoding="utf-8",
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        if self._last_usage is not None:
            _apply_usage(
                context,
                self._last_usage,
                harness_termination=self._harness_termination,
                artifact=self._artifact,
            )
