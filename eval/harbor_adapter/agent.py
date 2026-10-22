"""Harbor installed agent: frozen `plak run` in the task container.

Claude Code native-home is FrozenPmux (`pmux run`), not this class.
Stock Harbor `claude-code` (`claude --print`) stays unused.
"""

from __future__ import annotations

import json
import shutil
import shlex
import tempfile
import sys
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from harbor_adapter.cell_prep import prepare_cell, record_cell
from harbor_adapter.artifact_identity import plak_artifact_record, require_plak_provenance
from harbor_adapter.preflight import probe_executable, probe_run_capabilities, task_budgets
from harbor_adapter.lifecycle import sample_cell
from harbor_adapter.hygiene import harvest_command
from harbor_adapter.arch import (
    installed_root,
    platform_record_command,
    require_elf_arch,
    resolve_arch,
)
from harbor_adapter.arguments import (
    require_js_host,
    require_prompt_variant,
    require_tuple,
    run_arguments,
    run_env,
    sanitize_extra_env,
)
from harbor_adapter.credentials import (
    remote_state_acl_command,
    trial_oauth,
    require_ready_openrouter_key,
)
from harbor_adapter.tuples import (
    REMOTE_BINARY,
    REMOTE_HOME,
    REMOTE_JS_HOST,
    REMOTE_OPENROUTER_KEY,
    REMOTE_XDG,
)
from harbor_adapter.termination import (
    AGENT_EXIT_EXCEPTION,
    TerminationRecord,
    classify_plak,
    parse_exit_code,
    parse_session_events,
)
from harbor_adapter.usage import (
    UsageTotals,
    merge_usage,
    parse_session_id_lines,
    parse_telemetry_json,
    telemetry_query,
)


class PlakAgent(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "plak"

    def __init__(
        self,
        logs_dir: Path,
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        extra_env = sanitize_extra_env(extra_env)
        self._arch = resolve_arch(kwargs.pop("arch", None))
        root = installed_root(self._arch)
        self._binary_path = Path(
            kwargs.pop("binary_path", None) or root / "plak"
        )
        self._js_host_path = Path(
            kwargs.pop("js_host_path", None) or root / "plakous-js-host"
        )
        self._account = str(kwargs.pop("account", "codex"))
        self._model = str(kwargs.pop("model", "gpt-5.6-luna"))
        self._effort = str(kwargs.pop("effort", "low"))
        require_tuple(self._account, self._model, self._effort)
        if kwargs.pop("composition", None) is not None:
            raise ValueError("scored Plak arms use the shipped composition, not an override")
        xdg = kwargs.pop("xdg_state_home", None)
        self._xdg_state_home = Path(xdg) if xdg else None
        plak_home = kwargs.pop("plak_home", None)
        self._plak_home = Path(plak_home) if plak_home else None
        variant = kwargs.pop("prompt_variant", None)
        self._prompt_variant = require_prompt_variant(
            str(variant) if variant is not None else None
        )
        self._last_usage: UsageTotals | None = None
        self._termination: TerminationRecord | None = None
        self._artifact: dict[str, Any] = {}
        super().__init__(logs_dir, extra_env=extra_env, **kwargs)

    def get_version_command(self) -> str:
        return f"{REMOTE_BINARY} version"

    async def install(self, environment: BaseEnvironment) -> None:
        await prepare_cell(self, environment, self._arch)
        if not self._binary_path.is_file():
            raise RuntimeError(
                f"missing Harbor agent binary {self._binary_path}; "
                f"run `just build-harbor-agent` with HARBOR_ARCH={self._arch}"
            )
        require_elf_arch(self._binary_path, self._arch, "frozen plak")
        provenance = require_plak_provenance(self._binary_path, self._js_host_path)
        require_js_host(self._js_host_path)
        require_elf_arch(self._js_host_path, self._arch, "frozen plakous-js-host")
        # Copy once: the digest and upload must refer to the same bytes even if
        # a local build replaces the installed binary during a trial.
        with tempfile.TemporaryDirectory(prefix="plak-harbor-upload-") as directory:
            frozen = Path(directory) / "plak"
            shutil.copyfile(self._binary_path, frozen)
            frozen_js = Path(directory) / "plakous-js-host"
            shutil.copyfile(self._js_host_path, frozen_js)
            self._artifact = plak_artifact_record(frozen, frozen_js, arch=self._arch)
            if any(entry["sha256"] != provenance["binaries"][name]["sha256"]
                   for name, entry in self._artifact["binaries"].items()):
                raise ValueError("Plak build changed during frozen upload")
            self._artifact["source"] = provenance["source"]
            self._artifact["build"] = {key: provenance[key] for key in ("features", "toolchain")}
            await environment.upload_file(str(frozen), REMOTE_BINARY)
            await environment.upload_file(str(frozen_js), REMOTE_JS_HOST)
        for name, remote in (("plak", REMOTE_BINARY), ("plakous-js-host", REMOTE_JS_HOST)):
            entry = self._artifact["binaries"].get(name)
            if entry:
                line = shlex.quote(entry["sha256"] + "  " + remote)
                await self.exec_as_root(environment, command=f"printf '%s\\n' {line} | sha256sum -c - >/dev/null")
        await self.exec_as_root(
            environment, command=f"chmod 0755 {shlex.quote(REMOTE_BINARY)}"
        )
        uploaded = [REMOTE_BINARY, REMOTE_HOME, REMOTE_XDG]
        await self.exec_as_root(
            environment, command=f"chmod 0755 {shlex.quote(REMOTE_JS_HOST)}"
        )
        uploaded.append(REMOTE_JS_HOST)
        await environment.upload_file(
            str(Path(__file__).with_name("observed_config.py")), "/tmp/plak-observed-config.py"
        )
        await probe_executable(environment, REMOTE_BINARY)
        await probe_run_capabilities(environment, REMOTE_BINARY)
        await probe_executable(environment, REMOTE_JS_HOST, version_argument=None)
        await self.exec_as_root(
            environment,
            command=(
                f"mkdir -p {shlex.quote(REMOTE_HOME)} "
                f"{shlex.quote(REMOTE_HOME)}/secrets "
                f"{shlex.quote(REMOTE_HOME)}/logs "
                f"{shlex.quote(REMOTE_XDG)}/plak/subscriptions "
                f"{shlex.quote(REMOTE_XDG)}/plak/subscriptions/compatibility"
            ),
        )
        if self._account == "openrouter":
            key = require_ready_openrouter_key(plak_home=self._plak_home)
            await environment.upload_file(str(key.resolve()), REMOTE_OPENROUTER_KEY)
            uploaded.append(REMOTE_OPENROUTER_KEY)
        else:
            agent_timeout, verifier_timeout = task_budgets(environment)
            with trial_oauth(self._account, agent_timeout=agent_timeout,
                             verifier_timeout=verifier_timeout,
                             xdg_state_home=self._xdg_state_home) as oauth_dir:
                remote_sub = (
                    f"{REMOTE_XDG}/plak/subscriptions/{oauth_dir.name}"
                )
                await environment.upload_dir(str(oauth_dir), remote_sub)
                catalog = oauth_dir.parent / "catalog"
                if catalog.is_dir():
                    await environment.upload_dir(
                        str(catalog), f"{REMOTE_XDG}/plak/subscriptions/catalog"
                    )
                compatibility = oauth_dir.parent / "compatibility"
                if compatibility.is_dir():
                    await environment.upload_dir(
                        str(compatibility),
                        f"{REMOTE_XDG}/plak/subscriptions/compatibility",
                    )
        await self.exec_as_root(
            environment,
            command=remote_state_acl_command(environment.default_user, *uploaded),
        )
        await self.exec_as_root(
            environment, command=platform_record_command(self._arch)
        )

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        import time
        deadline = time.monotonic() + task_budgets(environment)[0] - 10
        await record_cell(self, environment)
        argv = run_arguments(
            account=self._account,
            model=self._model,
            effort=self._effort,
            prompt=instruction,
        )
        from harbor_adapter.argv import write_argv
        write_argv(self.logs_dir, argv[:-1])
        env = self._run_env()
        quoted = " ".join(shlex.quote(part) for part in argv)
        stdout_path = "/logs/agent/plak-stdout.txt"
        stderr_path = "/logs/agent/plak-stderr.log"
        exit_path = "/logs/agent/plak-exit.txt"
        # Do not `plak refresh` in the cell. Refresh rotates the provider
        # refresh token and invalidates every other copy (host, sibling
        # trials, vendor Codex/Grok). Host refreshes under the per-trial
        # upload lock; cells use the uploaded Ready credentials as-is.
        command = _plak_run_command(
            quoted, stdout_path, stderr_path, exit_path
        )
        from harbor_adapter.wire import bounded_command
        command = bounded_command(command, deadline - time.monotonic())
        lifecycle = [await sample_cell(environment, "before_agent_run")]
        exception_type = ""
        try:
            await self.exec_as_agent(environment, command=command, env=env)
        except NonZeroAgentExitCodeError:
            exception_type = AGENT_EXIT_EXCEPTION
            raise
        except BaseException as error:
            # Preserve the first task outcome, including a harness death.
            exception_type = type(error).__name__
            raise
        finally:
            lifecycle.append(await sample_cell(environment, "after_agent_run"))
            try:
                (self.logs_dir / "plak-lifecycle.json").write_text(
                    json.dumps({"schema_version": 1, "samples": lifecycle}, indent=2) + "\n",
                    encoding="utf-8",
                )
            except OSError:
                pass
            primary_error = sys.exc_info()[0]
            try:
                try:
                    await self._record_termination(environment, env, exception_type)
                    await self._capture_usage(environment, context)
                    await self.exec_as_agent(environment, command=(
                        "python3 /tmp/plak-observed-config.py /tmp/plak-cell/home/wire-captures "
                        "/logs/agent/observed-config.json"
                    ))
                finally:
                    await self.exec_as_root(environment, command=harvest_command())
            except Exception:
                if primary_error is None:
                    raise

    def _run_env(self) -> dict[str, str]:
        return run_env(
            extra_env=self.extra_env, prompt_variant=self._prompt_variant
        )

    async def _record_termination(
        self,
        environment: BaseEnvironment,
        env: dict[str, str],
        exception_type: str,
    ) -> None:
        """Writes what ended this turn next to the agent logs.

        Reward alone cannot tell a model miss from a context admission
        refusal, an unavailable credential, or a rejected tool call. This
        record is the evidence that separates them, and it is written on
        every path, including the one where `plak run` never started.
        """
        try:
            stderr = await self._read_agent_file(environment, env, "/logs/agent/plak-stderr.log")
            exit_text = await self._read_agent_file(environment, env, "/logs/agent/plak-exit.txt")
            events, session_ids = await self._read_session_events(environment, env)
            record = classify_plak(
                exit_code=parse_exit_code(exit_text),
                stderr=stderr,
                events=events,
                exception_type=exception_type,
                session_ids=session_ids,
            )
        except Exception:
            record = classify_plak(
                exit_code=None, stderr="", exception_type=exception_type
            )
        self._termination = record
        payload = record.as_dict()
        payload["artifact"] = self._artifact
        payload["prompt_variant"] = self._prompt_variant
        payload["account"] = self._account
        payload["model"] = self._model
        payload["effort"] = self._effort
        (self.logs_dir / "plak-termination.json").write_text(
            json.dumps(payload, indent=2) + "\n", encoding="utf-8"
        )

    async def _read_session_events(
        self, environment: BaseEnvironment, env: dict[str, str]
    ) -> tuple[list[dict[str, Any]], tuple[str, ...]]:
        listed = await self.exec_as_agent(
            environment,
            command=(
                f"ls {shlex.quote(REMOTE_HOME)}/state/*/sessions/*.events.jsonl "
                "2>/dev/null || true"
            ),
            env=env,
        )
        paths = [line.strip() for line in (listed.stdout or "").splitlines() if line.strip()]
        events: list[dict[str, Any]] = []
        ids: list[str] = []
        for path in paths:
            ids.append(Path(path).name.removesuffix(".events.jsonl"))
            events.extend(
                parse_session_events(
                    await self._read_agent_file(environment, env, path)
                )
            )
        return events, tuple(ids)

    async def _read_agent_file(
        self,
        environment: BaseEnvironment,
        env: dict[str, str],
        path: str,
    ) -> str:
        result = await self.exec_as_agent(
            environment,
            command=f"cat {shlex.quote(path)} 2>/dev/null || true",
            env=env,
        )
        return result.stdout or ""

    async def _capture_usage(
        self, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        stdout = await self.exec_as_agent(
            environment,
            command="cat /logs/agent/plak-stdout.txt 2>/dev/null || true",
            env=self._run_env(),
        )
        try:
            session_ids = parse_session_id_lines(stdout.stdout or "")
            totals: list[UsageTotals] = []
            for session_id in session_ids:
                query = telemetry_query(session_id)
                telemetry_cmd = (
                    f"{shlex.quote(REMOTE_BINARY)} telemetry sql {shlex.quote(query)} --json"
                )
                result = await self.exec_as_agent(
                    environment,
                    command=telemetry_cmd,
                    env=self._run_env(),
                )
                payload = result.stdout if result.return_code == 0 else "[]"
                totals.append(parse_telemetry_json(payload))
            self._last_usage = merge_usage(*totals) if totals else parse_telemetry_json("[]")
        except Exception:
            self._last_usage = parse_telemetry_json("[]")
        _apply_usage(
            context,
            self._last_usage,
            termination=self._termination,
            artifact=self._artifact,
        )
        evidence = self.logs_dir / "plak-usage.json"
        record = dict(self._last_usage.__dict__)
        record["artifact"] = self._artifact
        evidence.write_text(
            json.dumps(record, indent=2) + "\n", encoding="utf-8"
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        if self._last_usage is not None:
            _apply_usage(
                context,
                self._last_usage,
                termination=self._termination,
                artifact=self._artifact,
            )


def _plak_run_command(
    quoted: str,
    stdout_path: str,
    stderr_path: str,
    exit_path: str,
) -> str:
    return (
        f"mkdir -p /logs/agent && cd /app && "
        f": > {stderr_path} && "
        f"{quoted} > {stdout_path} 2>> {stderr_path}; "
        f"status=$?; echo $status > {exit_path}; "
        f"exit $status"
    )


def _apply_usage(
    context: AgentContext,
    usage: UsageTotals,
    *,
    termination: TerminationRecord | None = None,
    artifact: dict[str, Any] | None = None,
) -> None:
    context.n_input_tokens = usage.n_input_tokens
    context.n_cache_tokens = usage.n_cache_tokens
    context.n_output_tokens = usage.n_output_tokens
    if usage.resolved and usage.cost_usd is not None:
        context.cost_usd = usage.cost_usd
    else:
        context.cost_usd = None
    context.metadata = {
        "cost_status": usage.cost_status,
        "observed_turn_cost_usd": usage.observed_turn_cost_usd,
        "observed_cost_subtotal_usd": usage.observed_cost_usd,
        "compaction_rows": usage.compaction_rows,
        "missing_compaction_cost_rows": usage.missing_compaction_cost_rows,
        "missing_turn_cost_rows": usage.missing_turn_cost_rows,
        "endpoint_compact_workflows": usage.endpoint_compact_workflows,
        "usage_scope": "turns_plus_compactions_v1",
        "usage_resolved": usage.resolved,
        "usage_status": usage.usage_status,
        "missing_usage_buckets": usage.missing_buckets,
        "cache_write_tokens": usage.n_cache_write_tokens,
        "reasoning_tokens": usage.n_reasoning_tokens,
        "harness_termination": (
            termination.termination_class if termination else "not-run"
        ),
        "tool_rejections": (
            termination.rejection_count if termination else 0
        ),
        "artifact": artifact or {},
    }
