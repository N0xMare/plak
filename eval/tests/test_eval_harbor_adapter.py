"""Zero-inference contracts for the Harbor bakeoff adapter and comparison plans."""

from __future__ import annotations

import contextlib
import asyncio
import importlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "eval"))


def _import_harbor_module(name: str):
    """Import an adapter module that needs Harbor, or skip. Harbor is opt-in."""
    try:
        return importlib.import_module(name)
    except ImportError as exc:  # pragma: no cover - depends on eval extras
        raise unittest.SkipTest(f"{name} needs Harbor installed: {exc}") from exc

@contextlib.contextmanager
def _isolated_adapter_probes(module):
    """Run-loop unit fixtures isolate the separately tested campaign probes."""
    with contextlib.ExitStack() as stack:
        for name in ("prepare_cell", "record_cell"):
            if hasattr(module, name):
                stack.enter_context(mock.patch.object(module, name, new_callable=mock.AsyncMock))
        if hasattr(module, "task_budgets"):
            stack.enter_context(mock.patch.object(module, "task_budgets", return_value=(900, 600)))
        stack.enter_context(mock.patch("harbor_adapter.cell_prep.record_cell", new_callable=mock.AsyncMock))
        yield


@contextlib.contextmanager
def _installed_agent_with_harbor_stub(module_name="pmux_agent"):
    """Yield an installed-agent module, standing in for absent Harbor.

    The stubs carry no behavior: they only satisfy the imports so the adapter's
    own termination recording can be driven without the opt-in Harbor extras.
    Every module this installs is removed again, so Harbor stays opt-in for the
    tests that legitimately skip without it.
    """
    if "harbor" in sys.modules:
        module = importlib.import_module(f"harbor_adapter.{module_name}")
        with _isolated_adapter_probes(module):
            yield module
        return

    class _BaseInstalledAgent:
        def __init__(self, logs_dir, **_kwargs):
            self.logs_dir = logs_dir
            self.extra_env = _kwargs.get("extra_env", {})

    class _NonZeroAgentExitCodeError(Exception):
        pass

    children = {
        "harbor": ["agents", "environments", "models"],
        "harbor.agents": ["installed"],
        "harbor.agents.installed": ["base", "codex", "grok_build", "opencode", "pi"],
        "harbor.agents.installed.base": [],
        "harbor.agents.installed.codex": [],
        "harbor.agents.installed.grok_build": [],
        "harbor.agents.installed.opencode": [],
        "harbor.agents.installed.pi": [],
        "harbor.environments": ["base"],
        "harbor.environments.base": [],
        "harbor.models": ["agent", "trial"],
        "harbor.models.agent": ["context"],
        "harbor.models.agent.context": [],
        "harbor.models.trial": ["paths"],
        "harbor.models.trial.paths": [],
    }
    for name in children:
        sys.modules[name] = types.ModuleType(name)
    adapter_module = f"harbor_adapter.{module_name}"
    had_agent = adapter_module in sys.modules
    try:
        for name, nested in children.items():
            for child in nested:
                setattr(sys.modules[name], child, sys.modules[f"{name}.{child}"])
        base = sys.modules["harbor.agents.installed.base"]
        base.BaseInstalledAgent = _BaseInstalledAgent
        base.NonZeroAgentExitCodeError = _NonZeroAgentExitCodeError
        base.with_prompt_template = lambda method: method
        for name, class_name in (("codex", "Codex"), ("grok_build", "GrokBuild"),
                                 ("opencode", "OpenCode"), ("pi", "Pi")):
            setattr(sys.modules[f"harbor.agents.installed.{name}"], class_name,
                    type(class_name, (_BaseInstalledAgent,), {}))
        sys.modules["harbor.models.trial.paths"].EnvironmentPaths = types.SimpleNamespace(
            agent_dir=pathlib.Path("/logs/agent")
        )
        sys.modules["harbor.environments.base"].BaseEnvironment = object
        sys.modules["harbor.models.agent.context"].AgentContext = object
        try:
            module = importlib.import_module(adapter_module)
        except ImportError as exc:  # pragma: no cover - needs more extras
            raise unittest.SkipTest(
                f"{module_name} needs Harbor installed: {exc}"
            ) from exc
        with _isolated_adapter_probes(module):
            yield module
    finally:
        if not had_agent:
            sys.modules.pop(adapter_module, None)
        for name in children:
            sys.modules.pop(name, None)


from harbor_adapter.arguments import (  # noqa: E402
    ArgumentError,
    require_js_host,
    run_arguments,
    run_env,
    sanitize_extra_env,
)
from harbor_adapter.credentials import (  # noqa: E402
    CredentialError,
    opencode_auth_provider,
    pi_cli_model_name,
    remote_claude_securestorage_acl_command,
    remote_grok_auth_acl_command,
    refuse_stripped_env,
    remote_state_acl_command,
    require_local_docker,
    replace_claude_code_store,
    replace_grok_auth,
    require_ready_claude_code_store,
    require_ready_grok_auth,
    require_ready_oauth,
    require_ready_opencode_auth,
    require_ready_openrouter_key,
    require_ready_pi_auth,
    require_watched_openrouter_key,
    validate_grok_auth,
)
from harbor_adapter.plans import (  # noqa: E402
    PlanError,
    draft_plan,
    plan_digest,
    validate_plan,
)
from harbor_adapter.timing import trial_phases  # noqa: E402
from harbor_adapter.overload import (  # noqa: E402
    should_retry_compose,
    should_score_through_rosetta,
)
from harbor_adapter.tuples import (  # noqa: E402
    DATASET,
    DATASET_PLUMBING,
    DATASET_TB21,
    OPENROUTER_EFFORT,
    OPENROUTER_MODEL,
    REMOTE_BINARY,
    REMOTE_OPENROUTER_KEY,
    RUNG_A_AB_TASKS,
    RUNG_A_EXPANDED_TASKS,
    RUNG_A_PLUMBING_TASKS,
    SMOKE_TASKS,
    STRIP_ENV,
)
from harbor_adapter.usage import (  # noqa: E402
    parse_session_id_line,
    parse_session_id_lines,
    parse_telemetry_json,
    parse_pmux_json,
    parse_pmux_jsonl,
    merge_usage,
    telemetry_query,
)
from harbor_adapter.controls import (  # noqa: E402
    ControlError,
    NON_STOCK_KWARGS,
    SMOKE_EFFORT,
    harbor_ak_flags,
    vendor_effort_ak,
)
from harbor_adapter.pricing import (  # noqa: E402
    CanonicalUsage,
    from_harbor_context,
    implied_usd,
    load_rate_card,
)
from harbor_adapter.vendor_install import (  # noqa: E402
    grok_install_command,
    node_cli_install_command,
)
from harbor_adapter.vendor_pins import (  # noqa: E402
    CLI_PINS,
    FORBIDDEN_INSTALL_MARKERS,
    NODE_SHA256,
    NODE_VERSION,
    VendorArtifactError,
    docker_build_args,
    node_sha256,
    node_tarball_name,
    require_frozen_artifacts,
)
from harbor_adapter.arch import (  # noqa: E402
    ARCHES,
    DOCKER_PLATFORMS,
    ELF_MACHINES,
    NODE_ARCH_SUFFIXES,
    ArchError,
    installed_root,
    platform_record_command,
    read_elf_machine,
    require_arch,
    require_elf_arch,
    resolve_arch,
)
from harbor_adapter.pmux_pins import (  # noqa: E402
    PmuxArtifactError,
    require_frozen_pmux,
)
from harbor_adapter.artifact_identity import (  # noqa: E402
    SCHEMA_VERSION as ARTIFACT_SCHEMA_VERSION,
    ArtifactDigest,
    artifact_record,
    digest_all,
    digest_file,
    plak_artifact_record,
    pmux_artifact_record,
    record_source_commit,
    short_digest,
)
from harbor_adapter.termination import trial_artifact  # noqa: E402
from harbor_adapter.pmux_provenance import (  # noqa: E402
    DIRTY_OVERRIDE_ENV,
    MANIFEST_NAME,
    PSEUDOMUX_PIN,
    PSEUDOMUX_PIN_SHORT,
    UNPINNED_OVERRIDE_ENV,
    PmuxProvenanceError,
    SourceCommit,
    build_manifest,
    manifest_path,
    read_manifest,
    record_provenance,
    require_pinned_provenance,
    resolve_source_commit,
)
from harbor_adapter.termination import (  # noqa: E402
    CLASS_AGENT_EXIT_NONZERO,
    CLASS_AGENT_INIT_FAILED,
    INIT_FAILURE_MARKER,
    agent_init_class,
    initialization_kind,
    CLASS_COMPLETED,
    CLASS_RESULT_UNPARSEABLE,
    CLASS_UNKNOWN,
    RESULT_OK,
    RESULT_REPAIRED,
    RESULT_UNPARSEABLE,
    load_json,
    repair_scrubbed_booleans,
    summarize_trial,
    KIND_CONTEXT_CAPACITY,
    KIND_CREDENTIAL,
    KIND_OTHER,
    KIND_PROVIDER_RATE_LIMIT,
    KIND_PROVIDER_REJECTION,
    KIND_ROUND_LIMIT,
    KIND_TOOL_FAILURE,
    TERMINATION_CLASSES,
    classify_failure_kind,
    classify_plak,
    classify_trial_dir,
    classify_vendor,
    find_job_dirs,
    parse_exit_code,
    parse_session_events,
    render_table,
    summarize_job,
    tool_rejections,
)
from harbor_adapter.arguments import (  # noqa: E402
    require_prompt_variant,
    run_env,
)
from harbor_adapter.tuples import (  # noqa: E402
    PROMPT_VARIANT_ENV,
    PROMPT_VARIANTS,
)


class ArgumentContractTests(unittest.TestCase):
    def test_run_uses_plak_headless_mode(self) -> None:
        argv = run_arguments(
            account="codex",
            model="gpt-5.6-luna",
            effort="low",
            prompt="fix the tests",
        )
        self.assertEqual(argv[0], REMOTE_BINARY)
        self.assertEqual(argv[1], "run")
        self.assertIn("--print-session-id", argv)
        self.assertEqual(argv[-2], "--")
        self.assertEqual(argv[-1], "fix the tests")
        self.assertNotIn("--composition", argv)

    def test_grok_follows_direct_composition(self) -> None:
        argv = run_arguments(
            account="grok",
            model="grok-4.6",
            effort="low",
            prompt="hello",
        )
        self.assertNotIn("--composition", argv)

    def test_claude_follows_direct_composition(self) -> None:
        argv = run_arguments(
            account="claude",
            model="claude-sonnet-5",
            effort="low",
            prompt="hello",
        )
        self.assertNotIn("--composition", argv)
        self.assertIn("--account", argv)
        self.assertIn("claude", argv)

    def test_unqualified_tuple_is_refused(self) -> None:
        with self.assertRaises(ArgumentError):
            run_arguments(
                account="codex",
                model="gpt-4o",
                effort="low",
                prompt="x",
            )
        with self.assertRaises(ArgumentError):
            run_arguments(
                account="openrouter",
                model="gpt-5.6-luna",
                effort="low",
                prompt="x",
            )

    def test_empty_prompt_is_refused(self) -> None:
        with self.assertRaises(ArgumentError):
            run_arguments(
                account="codex",
                model="gpt-5.6-luna",
                effort="low",
                prompt="  ",
            )

    def test_openrouter_effort_none_omits_effort_flag(self) -> None:
        with self.assertRaises(ArgumentError):
            run_arguments(account="openrouter", model=OPENROUTER_MODEL,
                          effort=OPENROUTER_EFFORT, prompt="wire check")

    def test_api_keys_are_stripped_from_extra_env(self) -> None:
        env = sanitize_extra_env(
            {
                "OPENAI_API_KEY": "sk-secret",
                "ANTHROPIC_API_KEY": "sk-ant",
                "OPENROUTER_API_KEY": "sk-or",
                "KEEP": "ok",
            }
        )
        self.assertEqual(env, {"KEEP": "ok"})
        self.assertNotIn("OPENAI_API_KEY", run_env(extra_env={"OPENAI_API_KEY": "x"}))
        self.assertNotIn(
            "OPENROUTER_API_KEY",
            run_env(extra_env={"OPENROUTER_API_KEY": "sk-or"}),
        )
        self.assertEqual(run_env()["PLAK_HOME"], "/tmp/plak-cell/home")


class UsageContractTests(unittest.TestCase):
    def test_session_id_line(self) -> None:
        self.assertEqual(
            parse_session_id_line("session_id=0f2cabc\nhello\n"),
            "0f2cabc",
        )
        self.assertEqual(
            parse_session_id_lines(
                "session_id=aaa\nhello\nsession_id=bbb\n"
            ),
            ("aaa", "bbb"),
        )
        with self.assertRaises(ValueError):
            parse_session_id_line("nope")

    def test_missing_telemetry_is_unresolved_not_zero(self) -> None:
        totals = parse_telemetry_json("[]")
        self.assertFalse(totals.resolved)
        self.assertIsNone(totals.n_input_tokens)
        self.assertIsNone(totals.cost_usd)

    def test_cli_json_arrays_of_strings(self) -> None:
        totals = parse_telemetry_json(
            '[["10","2","5","1","2500000","reported"]]\n'
        )
        self.assertFalse(totals.resolved)
        self.assertEqual(totals.n_input_tokens, 10)
        self.assertEqual(totals.n_cache_tokens, 2)
        self.assertIsNone(totals.cost_usd)
        self.assertEqual(totals.cost_status, "unverified_aggregate")

    def test_merge_usage_sums_resolved_sessions(self) -> None:
        first = parse_telemetry_json(
            '[["10","2","5","1","0","unknown","1","0","0","0","1","0","0","0","0","0","0","1","0"]]\n'
        )
        second = parse_telemetry_json(
            '[["4","1","3","0","0","unknown","1","0","0","0","1","0","0","0","0","0","0","1","0"]]\n'
        )
        merged = merge_usage(first, second)
        self.assertTrue(merged.resolved)
        self.assertEqual(merged.n_input_tokens, 14)
        self.assertEqual(merged.n_cache_tokens, 3)
        self.assertEqual(merged.n_output_tokens, 8)

    def test_merge_usage_preserves_missing_session(self) -> None:
        resolved = parse_telemetry_json('[["10","0","1","0","0","unknown"]]\n')
        merged = merge_usage(parse_telemetry_json("[]"), resolved)
        self.assertFalse(merged.resolved)
        self.assertEqual(merged.n_input_tokens, 10)


class InstalledAgentFirstAttemptTests(unittest.IsolatedAsyncioTestCase):
    async def test_plak_usage_capture_preserves_compaction_fields_through_merge(self) -> None:
        with _installed_agent_with_harbor_stub("agent") as module:
            def payload(*, cost_micros: int, compaction_cost_missing: bool) -> str:
                return json.dumps([{
                    "input_tokens": 107, "cached_input_tokens": 22,
                    "cache_write_tokens": 30, "output_tokens": 13,
                    "reasoning_tokens": 3, "cost_micros": cost_micros,
                    "cost_status": "partial" if compaction_cost_missing else "reported",
                    "row_count": 2, "missing_input_rows": 0, "missing_cache_rows": 0,
                    "missing_output_rows": 0, "missing_cost_rows": int(compaction_cost_missing),
                    "missing_cache_write_rows": 0, "compaction_rows": 1,
                    "observed_turn_cost_micros": 1000,
                    "missing_compaction_cost_rows": int(compaction_cost_missing),
                    "compaction_coverage_mismatches": 0, "missing_turn_cost_rows": 0,
                    "endpoint_compact_workflows": 0,
                }])

            for sessions in (1, 2):
                with self.subTest(sessions=sessions), tempfile.TemporaryDirectory() as tmp:
                    agent = module.PlakAgent(
                        pathlib.Path(tmp), account="grok",
                        model="grok-4.6", effort="low",
                    )
                    responses = [types.SimpleNamespace(
                        stdout="".join(f"session_id=s{i}\n" for i in range(sessions)),
                        return_code=0,
                    )]
                    responses.append(types.SimpleNamespace(
                        stdout=payload(cost_micros=1200, compaction_cost_missing=False),
                        return_code=0,
                    ))
                    if sessions == 2:
                        responses.append(types.SimpleNamespace(
                            stdout=payload(cost_micros=1000, compaction_cost_missing=True),
                            return_code=0,
                        ))
                    agent.exec_as_agent = mock.AsyncMock(side_effect=responses)
                    context = types.SimpleNamespace()
                    await agent._capture_usage(object(), context)
                    self.assertEqual(agent.exec_as_agent.await_count, sessions + 1)
                    self.assertEqual(context.metadata["compaction_rows"], sessions)
                    self.assertEqual(context.metadata["usage_scope"], "turns_plus_compactions_v1")
                    self.assertEqual(context.n_input_tokens, 107 * sessions)
                    self.assertEqual(context.metadata["observed_turn_cost_usd"], .001 * sessions)
                    self.assertEqual(context.metadata["missing_compaction_cost_rows"], sessions - 1)
                    self.assertEqual(context.cost_usd, .0012 if sessions == 1 else None)
                    evidence = json.loads((pathlib.Path(tmp) / "plak-usage.json").read_text())
                    self.assertEqual(evidence["compaction_rows"], sessions)

    async def test_plak_overload_preserves_failure_after_one_task_run(self) -> None:
        with _installed_agent_with_harbor_stub("agent") as module:
            class ProviderFailure(Exception):
                pass

            failure = ProviderFailure("server_is_overloaded")
            with tempfile.TemporaryDirectory() as tmp:
                agent = module.PlakAgent(
                    pathlib.Path(tmp), account="grok",
                    model="grok-4.6", effort="low",
                )
                agent.exec_as_agent = mock.AsyncMock(side_effect=failure)
                agent._read_session_events = mock.AsyncMock(return_value=([], ()))
                async def read_agent_file(_environment, _env, path):
                    if path.endswith("plak-exit.txt"):
                        return "5\n"
                    return "plak: agent turn failed: provider code: server_is_overloaded"

                agent._read_agent_file = read_agent_file
                agent._capture_usage = mock.AsyncMock()
                environment, context = object(), object()
                with mock.patch.object(module, "NonZeroAgentExitCodeError", ProviderFailure):
                    with self.assertRaises(ProviderFailure) as caught:
                        await agent.run("Perform the task once.", environment, context)
                self.assertIs(caught.exception, failure)
                self.assertEqual(agent.exec_as_agent.await_count, 2)
                agent._capture_usage.assert_awaited_once_with(environment, context)
                record = json.loads((pathlib.Path(tmp) / "plak-termination.json").read_text())
                self.assertEqual(record["class"], "turn_failed:provider-rejection")
                self.assertEqual(record["exit_code"], 5)
                self.assertIn("server_is_overloaded", record["detail"])

    async def test_opencode_failure_preserves_one_underlying_task_and_attempt(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            class ProviderFailure(Exception):
                pass

            for detail in ("HTTP 429 Too Many Requests", "provider refused the request"):
                with self.subTest(detail=detail), tempfile.TemporaryDirectory() as tmp:
                    agent = module.FrozenOpenCode.__new__(module.FrozenOpenCode)
                    agent.logs_dir = pathlib.Path(tmp)
                    agent.model_name = "openrouter/deepseek/deepseek-v4-flash"
                    agent._get_env = mock.Mock(return_value=None)
                    agent.exec_as_root = mock.AsyncMock()
                    agent.exec_as_agent = mock.AsyncMock()
                    agent._error_messages = mock.Mock(return_value=[detail])
                    environment = types.SimpleNamespace(default_user="root", upload_file=mock.AsyncMock())
                    failure = ProviderFailure(detail)
                    run = mock.AsyncMock(side_effect=failure)
                    with mock.patch.object(module, "NonZeroAgentExitCodeError", ProviderFailure), \
                         mock.patch.object(module.OpenCode, "run", run, create=True), \
                         mock.patch.object(module, "require_ready_opencode_auth", return_value=pathlib.Path("/private/test-auth.json")):
                        with self.assertRaises(ProviderFailure) as caught:
                            await agent.run("Perform the task once.", environment, object())
                    self.assertIs(caught.exception, failure)
                    run.assert_awaited_once()
                    rows = [json.loads(line) for line in (pathlib.Path(tmp) / "opencode-attempts.jsonl").read_text().splitlines()]
                    self.assertEqual(len(rows), 1)
                    self.assertEqual(rows[0]["attempt"], 1)
                    self.assertEqual(rows[0]["outcome"], "failed")
                    self.assertEqual(rows[0]["error"], detail)
                    self.assertGreaterEqual(rows[0]["duration_s"], 0)


class ComposeRetryContractTests(unittest.TestCase):
    def test_retry_only_on_hub_timeout(self) -> None:
        self.assertTrue(
            should_retry_compose(
                "Docker compose command failed: context deadline exceeded"
            )
        )
        self.assertFalse(should_retry_compose("NonZeroAgentExitCodeError: exit 1"))
        source = (ROOT / "eval" / "harbor_adapter" / "docker_retry.py").read_text()
        self.assertIn("class RetryingDocker", source)
        self.assertIn("down", source)
        self.assertIn("should_retry_compose", source)


class RosettaScoreThroughTests(unittest.TestCase):
    def test_score_through_only_with_transcripts_and_rosetta_crash(self) -> None:
        stderr = (
            "assertion failed [!result.is_error]: Failed to create temporary file\n"
            "(ThreadContextFcntl.cpp:85 create_tempfile)\n"
            'pmux: pmuxd error code=DaemonLost message="private rmux session '
            'control plane was lost" retryable=true\n'
        )
        self.assertTrue(
            should_score_through_rosetta(
                stderr,
                has_transcripts=True,
                cell_arch="amd64",
                host_system="Darwin",
            )
        )
        self.assertFalse(
            should_score_through_rosetta(
                stderr,
                has_transcripts=False,
                cell_arch="amd64",
                host_system="Darwin",
            )
        )
        self.assertFalse(
            should_score_through_rosetta(
                "exit 1",
                has_transcripts=True,
                cell_arch="amd64",
                host_system="Darwin",
            )
        )
        agent = (ROOT / "eval" / "harbor_adapter" / "pmux_agent.py").read_text()
        self.assertIn("should_score_through_rosetta", agent)
        self.assertIn("NonZeroAgentExitCodeError", agent)

    def test_bare_daemon_loss_is_not_scored_through(self) -> None:
        for stderr in (
            'pmux: pmuxd error code=DaemonLost message="lost" retryable=true',
            "pmux: RmuxUnavailable",
        ):
            with self.subTest(stderr=stderr):
                self.assertFalse(
                    should_score_through_rosetta(
                        stderr,
                        has_transcripts=True,
                        cell_arch="amd64",
                        host_system="Darwin",
                    )
                )

    def test_rosetta_evidence_alone_is_not_scored_through(self) -> None:
        self.assertFalse(
            should_score_through_rosetta(
                "Failed to create temporary file (create_tempfile)",
                has_transcripts=True,
                cell_arch="amd64",
                host_system="Darwin",
            )
        )

    def test_daemon_loss_with_rosetta_trace_is_scored_through(self) -> None:
        stderr = (
            "rosetta error: Failed to create temporary file\n"
            "pmux: pmuxd error code=DaemonLost retryable=true\n"
        )
        self.assertTrue(
            should_score_through_rosetta(
                stderr,
                has_transcripts=True,
                cell_arch="amd64",
                host_system="Darwin",
            )
        )
        self.assertFalse(
            should_score_through_rosetta(
                stderr,
                has_transcripts=False,
                cell_arch="amd64",
                host_system="Darwin",
            )
        )

    def test_native_cells_never_score_through_a_daemon_loss(self) -> None:
        # Rosetta only exists for an emulated amd64 cell on a Darwin host.
        # A native arm64 cell, or an amd64 cell on Linux, has no such excuse:
        # a lost daemon there is an ordinary harness failure and must be
        # reported as one even if the stderr happens to carry the words.
        stderr = (
            "rosetta error: Failed to create temporary file\n"
            "pmux: pmuxd error code=DaemonLost retryable=true\n"
        )
        self.assertFalse(
            should_score_through_rosetta(
                stderr,
                has_transcripts=True,
                cell_arch="arm64",
                host_system="Darwin",
            )
        )
        self.assertFalse(
            should_score_through_rosetta(
                stderr,
                has_transcripts=True,
                cell_arch="amd64",
                host_system="Linux",
            )
        )
        agent = (ROOT / "eval" / "harbor_adapter" / "pmux_agent.py").read_text()
        self.assertIn("cell_arch=self._arch", agent)
        self.assertIn("host_system=platform.system()", agent)

    def test_harness_termination_is_always_recorded(self) -> None:
        agent = (ROOT / "eval" / "harbor_adapter" / "pmux_agent.py").read_text()
        self.assertIn('"harness_termination": harness_termination', agent)
        self.assertIn('TERMINATION_CRASH = "harness-crash"', agent)
        self.assertIn(
            'TERMINATION_CRASH_SCORED_THROUGH = "harness-crash-scored-through"',
            agent,
        )
        self.assertIn('record["harness_termination"]', agent)

        pmux_agent_module = _import_harbor_module("harbor_adapter.pmux_agent")
        from harbor_adapter.usage import UsageTotals

        class _Context:
            pass

        context = _Context()
        usage = UsageTotals(
            n_input_tokens=1,
            n_cache_tokens=None,
            n_output_tokens=2,
            n_reasoning_tokens=None,
            cost_usd=None,
            cost_status="unknown",
            resolved=True,
        )
        pmux_agent_module._apply_usage(
            context,
            usage,
            harness_termination=pmux_agent_module.TERMINATION_CRASH,
        )
        self.assertEqual(context.metadata["harness_termination"], "harness-crash")
        pmux_agent_module._apply_usage(context, usage)
        self.assertEqual(context.metadata["harness_termination"], "not-run")


class TelemetryRemainderTests(unittest.TestCase):
    def test_aggregate_zero_unknown_is_unresolved(self) -> None:
        totals = parse_telemetry_json('[["0","0","0","0","0",""]]')
        self.assertFalse(totals.resolved)
        self.assertIsNone(totals.cost_usd)
        self.assertIsNone(totals.n_input_tokens)

    def test_known_cost_converts_micros(self) -> None:
        payload = json.dumps(
            [
                {
                    "input_tokens": 10,
                    "cached_input_tokens": 2,
                    "output_tokens": 5,
                    "reasoning_tokens": 1,
                    "cost_micros": 2500000,
                    "cost_status": "known",
                }
            ]
        )
        totals = parse_telemetry_json(payload)
        self.assertFalse(totals.resolved)
        self.assertEqual(totals.n_input_tokens, 10)
        self.assertEqual(totals.n_cache_tokens, 2)
        self.assertIsNone(totals.cost_usd)
        self.assertEqual(totals.cost_status, "unverified_aggregate")

    def test_unknown_cost_with_tokens_is_resolved_without_dollars(self) -> None:
        totals = parse_telemetry_json(
            '[["10","0","4","1","","unknown"]]'
        )
        self.assertFalse(totals.resolved)
        self.assertEqual(totals.n_input_tokens, 10)
        self.assertIsNone(totals.cost_usd)
        self.assertEqual(totals.cost_status, "unknown")

    def test_telemetry_query_rejects_injection(self) -> None:
        with self.assertRaises(ValueError):
            telemetry_query("abc'; drop table turns; --")
        sql = telemetry_query("0f2c-1")
        self.assertIn("0f2c-1", sql)
        self.assertIn("product.sessions", sql)
        self.assertIn("HAVING COUNT(*) > 0", sql)
        self.assertNotIn("COALESCE(SUM(", sql)


class CredentialContractTests(unittest.TestCase):
    @staticmethod
    def _grok_auth(*, key: str = "access-a", refresh: str = "refresh-a",
                   principal: str = "principal-a", client: str = "client-a") -> bytes:
        return json.dumps({f"https://auth.x.ai::{client}": {
            "auth_mode": "oidc", "key": key, "refresh_token": refresh,
            "expires_at": "2027-01-01T00:00:00Z",
            "create_time": "2026-01-01T00:00:00Z", "principal_id": principal,
            "user_id": "user-a", "oidc_issuer": "https://auth.x.ai",
            "oidc_client_id": client,
        }}).encode()

    def test_grok_auth_requires_one_complete_oidc_account(self) -> None:
        validate_grok_auth(self._grok_auth())
        invalid = (
            b"{}", b"not-json",
            self._grok_auth(refresh=""),
            self._grok_auth(client="client-a").replace(
                b"auth.x.ai::client-a", b"auth.x.ai::client-b"
            ),
            self._grok_auth().replace(b'"oidc"', b'"api_key"'),
            self._grok_auth().replace(
                b'"oidc_issuer": "https://auth.x.ai"',
                b'"oidc_issuer": "https://other.example"',
            ),
        )
        for raw in invalid:
            with self.subTest(raw=raw[:5]), self.assertRaises(CredentialError):
                validate_grok_auth(raw)

    def test_grok_refresh_replaces_only_same_account_and_unchanged_host(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            host, candidate = root / "auth.json", root / "candidate.json"
            original = self._grok_auth()
            host.write_bytes(original)
            candidate.write_bytes(self._grok_auth(key="access-b", refresh="refresh-b"))
            candidate.chmod(0o644)
            self.assertTrue(replace_grok_auth(host, candidate, original))
            self.assertEqual(host.stat().st_mode & 0o777, 0o600)
            self.assertFalse(candidate.exists())
            self.assertEqual(
                validate_grok_auth(host.read_bytes())[1]["refresh_token"],
                "refresh-b",
            )
            candidate.write_bytes(refreshed := host.read_bytes())
            self.assertFalse(replace_grok_auth(host, candidate, refreshed))
            self.assertEqual(host.read_bytes(), refreshed)

    def test_grok_refresh_preserves_host_on_bad_or_competing_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            host, candidate = root / "auth.json", root / "candidate.json"
            original = self._grok_auth()
            for invalid in (
                self._grok_auth(refresh=""),
                self._grok_auth(principal="different"),
                self._grok_auth(client="different"),
                self._grok_auth().replace(
                    b"2027-01-01T00:00:00Z", b"2025-01-01T00:00:00Z"
                ),
            ):
                host.write_bytes(original)
                candidate.write_bytes(invalid)
                with self.assertRaises(CredentialError):
                    replace_grok_auth(host, candidate, original)
                self.assertEqual(host.read_bytes(), original)
            host.write_bytes(self._grok_auth(key="concurrent"))
            candidate.write_bytes(self._grok_auth(key="cell-new"))
            with self.assertRaises(CredentialError):
                replace_grok_auth(host, candidate, original)
            self.assertEqual(host.read_bytes(), self._grok_auth(key="concurrent"))
            host.write_bytes(original)
            candidate.unlink()
            candidate.symlink_to(host)
            with self.assertRaises(CredentialError):
                replace_grok_auth(host, candidate, original)
            self.assertEqual(host.read_bytes(), original)

    def test_grok_preflight_rejects_malformed_and_symlinked_store(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            host = root / "auth.json"
            host.write_bytes(b"{}")
            with self.assertRaises(CredentialError):
                require_ready_grok_auth(grok_home=root)
            real = root / "real.json"
            real.write_bytes(self._grok_auth())
            host.unlink()
            host.symlink_to(real)
            with self.assertRaises(CredentialError):
                require_ready_grok_auth(grok_home=root)

    def test_grok_cell_refresh_reads_effective_home_file_and_persists_it(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            with tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                host = root / "auth.json"
                original = self._grok_auth()
                refreshed = self._grok_auth(key="access-b", refresh="refresh-b")
                host.write_bytes(original)
                agent = module.FrozenGrokBuild.__new__(module.FrozenGrokBuild)
                agent.exec_as_agent = mock.AsyncMock()

                async def download(_remote: str, local: str) -> None:
                    pathlib.Path(local).write_bytes(refreshed)

                environment = types.SimpleNamespace(
                    download_file=mock.AsyncMock(side_effect=download)
                )
                asyncio.run(agent._persist_refreshed_auth(environment, host, original))
                command = agent.exec_as_agent.await_args.kwargs["command"]
                self.assertIn('"$HOME/.grok/auth.json"', command)
                self.assertEqual(host.read_bytes(), refreshed)
                self.assertEqual(host.stat().st_mode & 0o777, 0o600)
                self.assertFalse(list(root.glob(".auth.json.cell-*")))

    def test_grok_cell_refresh_failure_cleans_candidate_and_keeps_host(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            with tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                host = root / "auth.json"
                original = self._grok_auth()
                host.write_bytes(original)
                agent = module.FrozenGrokBuild.__new__(module.FrozenGrokBuild)
                agent.exec_as_agent = mock.AsyncMock()

                async def malformed(_remote: str, local: str) -> None:
                    pathlib.Path(local).write_bytes(b"{}")

                async def interrupted(_remote: str, _local: str) -> None:
                    raise OSError("synthetic download failure")

                for failure, expected in (
                    (malformed, CredentialError),
                    (interrupted, OSError),
                ):
                    environment = types.SimpleNamespace(
                        download_file=mock.AsyncMock(side_effect=failure)
                    )
                    with self.subTest(expected=expected), self.assertRaises(expected):
                        asyncio.run(agent._persist_refreshed_auth(environment, host, original))
                    self.assertEqual(host.read_bytes(), original)
                    self.assertFalse(list(root.glob(".auth.json.cell-*")))

    def test_grok_run_attempts_refresh_writeback_on_agent_failure(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            with tempfile.TemporaryDirectory() as tmp:
                host = pathlib.Path(tmp) / "auth.json"
                host.write_bytes(self._grok_auth())
                agent = module.FrozenGrokBuild.__new__(module.FrozenGrokBuild)
                agent._provider = None
                agent._CLI_LOG_FILENAME = "grok.log"
                agent.logger = mock.Mock()
                agent._get_env = lambda name: "trace" if name == "RUST_LOG" else ""
                agent._build_write_config_command = lambda: "CONFIG"
                agent._build_run_script = lambda _instruction: "RUN"
                agent.exec_as_root = mock.AsyncMock()
                agent._persist_refreshed_auth = mock.AsyncMock()

                async def execute(_environment, *, command, **_kwargs):
                    if command == "id -u":
                        return types.SimpleNamespace(stdout="0\n")
                    if command == "RUN":
                        raise RuntimeError("synthetic agent failure")

                agent.exec_as_agent = mock.AsyncMock(side_effect=execute)
                environment = types.SimpleNamespace(
                    default_user="agent", upload_file=mock.AsyncMock()
                )
                with mock.patch.object(module, "require_ready_grok_auth", return_value=host):
                    with self.assertRaisesRegex(RuntimeError, "synthetic agent failure"):
                        asyncio.run(agent.run("task", environment, object()))
                agent._persist_refreshed_auth.assert_awaited_once()
                run_call = next(
                    call for call in agent.exec_as_agent.await_args_list
                    if call.kwargs["command"] == "RUN"
                )
                self.assertEqual(run_call.kwargs["env"]["RUST_LOG"], "error")

    def test_grok_run_preserves_agent_error_when_writeback_also_fails(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            with tempfile.TemporaryDirectory() as tmp:
                host = pathlib.Path(tmp) / "auth.json"
                host.write_bytes(self._grok_auth())
                agent = module.FrozenGrokBuild.__new__(module.FrozenGrokBuild)
                agent._provider = None
                agent._CLI_LOG_FILENAME = "grok.log"
                agent.logger = mock.Mock()
                agent._get_env = lambda _name: ""
                agent._build_write_config_command = lambda: "CONFIG"
                agent._build_run_script = lambda _instruction: "RUN"
                agent.exec_as_root = mock.AsyncMock()
                agent._persist_refreshed_auth = mock.AsyncMock(
                    side_effect=CredentialError("synthetic writeback failure")
                )

                async def execute(_environment, *, command, **_kwargs):
                    if command == "id -u":
                        return types.SimpleNamespace(stdout="0\n")
                    if command == "RUN":
                        raise RuntimeError("primary agent failure")

                agent.exec_as_agent = mock.AsyncMock(side_effect=execute)
                environment = types.SimpleNamespace(
                    default_user="agent", upload_file=mock.AsyncMock()
                )
                with mock.patch.object(module, "require_ready_grok_auth", return_value=host):
                    with self.assertRaisesRegex(RuntimeError, "primary agent failure"):
                        asyncio.run(agent.run("task", environment, object()))
                agent._persist_refreshed_auth.assert_awaited_once()
                commands = [
                    call.kwargs["command"] for call in agent.exec_as_agent.await_args_list
                ]
                self.assertTrue(any(".grok/sessions" in command for command in commands))
                self.assertEqual(host.read_bytes(), self._grok_auth())

    def test_grok_run_uses_actual_agent_uid_and_refuses_invalid_identity(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            with tempfile.TemporaryDirectory() as tmp:
                host = pathlib.Path(tmp) / "auth.json"
                host.write_bytes(self._grok_auth())
                for uid, expected in (("0\n", 0), ("1001\n", 1001), ("bad", None)):
                    agent = module.FrozenGrokBuild.__new__(module.FrozenGrokBuild)
                    agent.logs_dir = pathlib.Path(tmp)
                    agent._provider = None
                    agent._CLI_LOG_FILENAME = "grok.log"
                    agent.logger = mock.Mock()
                    agent._get_env = lambda _name: ""
                    agent._build_write_config_command = lambda: "CONFIG"
                    agent._build_run_script = lambda _instruction: "RUN"
                    agent.exec_as_root = mock.AsyncMock()
                    agent._persist_refreshed_auth = mock.AsyncMock()

                    async def execute(_environment, *, command, **_kwargs):
                        if command == "id -u":
                            return types.SimpleNamespace(stdout=uid)

                    agent.exec_as_agent = mock.AsyncMock(side_effect=execute)
                    environment = types.SimpleNamespace(
                        default_user=None, upload_file=mock.AsyncMock()
                    )
                    with mock.patch.object(module, "require_ready_grok_auth", return_value=host):
                        if expected is None:
                            with self.assertRaises(CredentialError):
                                asyncio.run(agent.run("task", environment, object()))
                            environment.upload_file.assert_not_awaited()
                            agent.exec_as_root.assert_awaited_once()
                        else:
                            asyncio.run(agent.run("task", environment, object()))
                            acl = [
                                call.kwargs["command"]
                                for call in agent.exec_as_root.await_args_list
                                if "grok-secrets/auth.json" in call.kwargs["command"]
                            ]
                            self.assertEqual(len(acl), 1)
                            self.assertIn(f"chown {expected} ", acl[0])
                            self.assertIn("chmod 0700 /tmp/grok-secrets", acl[0])

    def test_paid_vendor_key_must_match_shared_watch_without_disclosure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            key, auth = root / "key", root / "auth.json"
            key.write_text("watched-secret\n")
            with mock.patch.dict(os.environ, {
                "DOCKER_HOST": "", "PLAK_OPENROUTER_KEY_PATH": str(key),
                "PI_AUTH_JSON_PATH": str(auth), "OPENCODE_AUTH_JSON_PATH": str(auth),
            }):
                for arm, kind in (("pi-openrouter", "api_key"),
                                  ("opencode-openrouter", "api")):
                    auth.write_text(json.dumps({"openrouter": {
                        "type": kind, "key": "watched-secret"}}))
                    self.assertEqual(require_watched_openrouter_key(arm), key)
                    auth.write_text(json.dumps({"openrouter": {
                        "type": kind, "key": "unwatched-secret"}}))
                    with self.assertRaises(CredentialError) as failure:
                        require_watched_openrouter_key(arm)
                    self.assertNotIn("secret", str(failure.exception))
                    self.assertIn("does not match", str(failure.exception))

    def test_remote_docker_is_refused(self) -> None:
        previous = os.environ.get("DOCKER_HOST")
        os.environ["DOCKER_HOST"] = "tcp://127.0.0.1:2375"
        try:
            with self.assertRaises(CredentialError):
                require_local_docker()
        finally:
            if previous is None:
                os.environ.pop("DOCKER_HOST", None)
            else:
                os.environ["DOCKER_HOST"] = previous

    def test_missing_oauth_file_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(CredentialError):
                require_ready_oauth("codex", xdg_state_home=pathlib.Path(tmp))

    def test_remote_state_acl_chowns_and_forces_private_home(self) -> None:
        command = remote_state_acl_command("agent", "/logs/agent/plak-home")
        self.assertIn("chown -R agent", command)
        self.assertIn("chmod 0700 /tmp/plak-cell/home /tmp/plak-cell/xdg", command)
        self.assertIn("find /tmp/plak-cell/xdg/plak -type d -exec chmod 0700", command)
        self.assertIn("find /tmp/plak-cell/xdg/plak -type f -exec chmod 0600", command)

    def test_remote_state_acl_chowns_when_user_is_unspecified(self) -> None:
        command = remote_state_acl_command(None, "/logs/agent/plak-home")
        self.assertIn('chown -R "$(id -u)"', command)
        self.assertNotIn("chown -R None", command)

    def test_missing_grok_auth_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            grok_home = pathlib.Path(tmp) / ".grok"
            grok_home.mkdir()
            previous = os.environ.get("GROK_HOME")
            os.environ["GROK_HOME"] = str(grok_home)
            try:
                with self.assertRaises(CredentialError):
                    require_ready_grok_auth()
            finally:
                if previous is None:
                    os.environ.pop("GROK_HOME", None)
                else:
                    os.environ["GROK_HOME"] = previous

    def test_remote_grok_auth_acl_tightens_modes(self) -> None:
        command = remote_grok_auth_acl_command(
            "agent",
            "/tmp/grok-secrets/auth.json",
        )
        self.assertIn("chown agent", command)
        self.assertIn("chmod 0700 /tmp/grok-secrets", command)
        self.assertIn("chmod 0600 /tmp/grok-secrets/auth.json", command)
        self.assertNotIn("$HOME", command)
        with self.assertRaises(CredentialError):
            remote_grok_auth_acl_command(None, "/tmp/grok-secrets/auth.json")

    def test_claude_securestorage_acl_is_owner_only(self) -> None:
        command = remote_claude_securestorage_acl_command(
            "agent",
            "/installed-agent/claude-securestorage",
        )
        self.assertIn("chown -R agent /installed-agent/claude-securestorage", command)
        self.assertIn("chmod 0700 /installed-agent/claude-securestorage", command)
        self.assertIn(
            "chmod 0600 /installed-agent/claude-securestorage/.credentials.json",
            command,
        )

    def test_missing_pi_auth_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            previous = os.environ.get("PI_AUTH_JSON_PATH")
            os.environ["PI_AUTH_JSON_PATH"] = str(pathlib.Path(tmp) / "missing.json")
            try:
                with self.assertRaises(CredentialError):
                    require_ready_pi_auth("openai-codex")
            finally:
                if previous is None:
                    os.environ.pop("PI_AUTH_JSON_PATH", None)
                else:
                    os.environ["PI_AUTH_JSON_PATH"] = previous

    def test_pi_auth_requires_named_provider(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "auth.json"
            path.write_text('{"xai": {"type": "oauth"}}\n', encoding="utf-8")
            previous = os.environ.get("PI_AUTH_JSON_PATH")
            os.environ["PI_AUTH_JSON_PATH"] = str(path)
            try:
                with self.assertRaises(CredentialError):
                    require_ready_pi_auth("openai-codex")
                self.assertEqual(require_ready_pi_auth("xai"), path)
            finally:
                if previous is None:
                    os.environ.pop("PI_AUTH_JSON_PATH", None)
                else:
                    os.environ["PI_AUTH_JSON_PATH"] = previous

    def test_pi_cli_model_rewrites_openai_to_openai_codex(self) -> None:
        self.assertEqual(
            pi_cli_model_name("openai/gpt-5.6-luna"),
            "openai-codex/gpt-5.6-luna",
        )
        self.assertEqual(pi_cli_model_name("xai/grok-4.6"), "xai/grok-4.6")

    def test_missing_opencode_auth_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            previous = os.environ.get("OPENCODE_AUTH_JSON_PATH")
            os.environ["OPENCODE_AUTH_JSON_PATH"] = str(
                pathlib.Path(tmp) / "missing.json"
            )
            try:
                with self.assertRaises(CredentialError):
                    require_ready_opencode_auth("openai")
            finally:
                if previous is None:
                    os.environ.pop("OPENCODE_AUTH_JSON_PATH", None)
                else:
                    os.environ["OPENCODE_AUTH_JSON_PATH"] = previous

    def test_opencode_auth_provider_is_harbor_openai_or_xai(self) -> None:
        self.assertEqual(opencode_auth_provider("openai/gpt-5.6-luna"), "openai")
        self.assertEqual(opencode_auth_provider("xai/grok-4.6"), "xai")
        self.assertEqual(
            opencode_auth_provider(f"openrouter/{OPENROUTER_MODEL}"),
            "openrouter",
        )

    def test_pi_cli_model_passes_openrouter_through(self) -> None:
        harbor_model = f"openrouter/{OPENROUTER_MODEL}"
        self.assertEqual(pi_cli_model_name(harbor_model), harbor_model)

    def test_missing_openrouter_key_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            key_path = pathlib.Path(tmp) / "openrouter.key"
            previous = os.environ.get("PLAK_OPENROUTER_KEY_PATH")
            os.environ["PLAK_OPENROUTER_KEY_PATH"] = str(key_path)
            try:
                with self.assertRaises(CredentialError):
                    require_ready_openrouter_key()
            finally:
                if previous is None:
                    os.environ.pop("PLAK_OPENROUTER_KEY_PATH", None)
                else:
                    os.environ["PLAK_OPENROUTER_KEY_PATH"] = previous

    def test_empty_openrouter_key_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            key_path = pathlib.Path(tmp) / "openrouter.key"
            key_path.write_text("", encoding="utf-8")
            previous = os.environ.get("PLAK_OPENROUTER_KEY_PATH")
            os.environ["PLAK_OPENROUTER_KEY_PATH"] = str(key_path)
            try:
                with self.assertRaises(CredentialError):
                    require_ready_openrouter_key()
            finally:
                if previous is None:
                    os.environ.pop("PLAK_OPENROUTER_KEY_PATH", None)
                else:
                    os.environ["PLAK_OPENROUTER_KEY_PATH"] = previous

    def test_native_ptc_requires_js_host(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            missing = pathlib.Path(tmp) / "plakous-js-host"
            with self.assertRaises(ArgumentError):
                require_js_host(missing)


class PlanContractTests(unittest.TestCase):
    def test_smoke_draft_validates_without_jobs(self) -> None:
        plan = draft_plan(
            plan_id="tb4-smoke-codex-luna-low",
            account="codex",
            model="gpt-5.6-luna",
            effort="low",
            arms=("plak", "codex"),
        )
        self.assertEqual(plan["dataset"], DATASET)
        self.assertEqual([task["name"] for task in plan["tasks"]], list(SMOKE_TASKS))
        validate_plan(plan, require_jobs=False)
        digest = plan_digest(plan)
        self.assertTrue(digest.startswith("sha256:"))
        with self.assertRaises(PlanError):
            validate_plan(plan, require_jobs=True)

    def test_committed_plans_validate(self) -> None:
        for path in (ROOT / "eval" / "comparisons").glob("tb4-smoke-*.json"):
            plan = json.loads(path.read_text())
            validate_plan(plan, require_jobs=False)
            self.assertEqual(plan["dataset"], DATASET)
        plan = draft_plan(
            plan_id="x",
            account="grok",
            model="grok-4.6",
            effort="low",
            arms=("plak", "grok-build"),
        )
        plan["dataset"] = "terminal-bench/terminal-bench@3.0.0"
        with self.assertRaises(PlanError):
            validate_plan(plan)

    def test_require_jobs_checks_digest_shape(self) -> None:
        good = "sha256:" + "ab" * 32

        def locked() -> dict:
            plan = draft_plan(
                plan_id="digest-shape",
                account="codex",
                model="gpt-5.6-luna",
                effort="low",
                arms=("plak", "codex"),
            )
            for task in plan["tasks"]:
                task["digest"] = good
            for arm in plan["arms"].values():
                arm["job"] = {"id": "job-1", "lockDigest": good}
                arm["candidateProvenanceDigest"] = good
            return plan

        validate_plan(locked(), require_jobs=True)

        malformed = (
            None,
            "",
            "ab" * 32,
            "sha256:" + "ab" * 31,
            "sha256:" + "ab" * 33,
            "sha256:" + "AB" * 32,
            "sha256:" + "zz" * 32,
            "sha1:" + "ab" * 32,
            123,
        )
        for value in malformed:
            with self.subTest(field="task.digest", value=value):
                plan = locked()
                plan["tasks"][0]["digest"] = value
                with self.assertRaises(PlanError):
                    validate_plan(plan, require_jobs=True)
            with self.subTest(field="job.lockDigest", value=value):
                plan = locked()
                plan["arms"]["plak"]["job"]["lockDigest"] = value
                with self.assertRaises(PlanError):
                    validate_plan(plan, require_jobs=True)
            with self.subTest(field="candidateProvenanceDigest", value=value):
                plan = locked()
                plan["arms"]["plak"]["candidateProvenanceDigest"] = value
                with self.assertRaises(PlanError):
                    validate_plan(plan, require_jobs=True)

        missing_job = locked()
        missing_job["arms"]["codex"]["job"] = {"id": None, "lockDigest": good}
        with self.assertRaises(PlanError):
            validate_plan(missing_job, require_jobs=True)

    def test_drafts_stay_null_valued_and_round_trip(self) -> None:
        plan = draft_plan(
            plan_id="draft-null",
            account="codex",
            model="gpt-5.6-luna",
            effort="low",
            arms=("plak", "codex"),
        )
        for task in plan["tasks"]:
            self.assertIsNone(task["digest"])
        for arm in plan["arms"].values():
            self.assertIsNone(arm["job"]["id"])
            self.assertIsNone(arm["job"]["lockDigest"])
            self.assertIsNone(arm["candidateProvenanceDigest"])
        validate_plan(plan, require_jobs=False)
        with self.assertRaises(PlanError):
            validate_plan(plan, require_jobs=True)
        round_tripped = json.loads(json.dumps(plan))
        validate_plan(round_tripped, require_jobs=False)
        self.assertEqual(plan_digest(round_tripped), plan_digest(plan))

    def test_draft_rung_a_plans_validate(self) -> None:
        plumbing = draft_plan(
            plan_id="rung-a-plumbing-codex-luna-low",
            account="codex",
            model="gpt-5.6-luna",
            effort="low",
            arms=("plak", "codex"),
            dataset=DATASET_PLUMBING,
            tasks=RUNG_A_PLUMBING_TASKS,
        )
        self.assertEqual(plumbing["dataset"], DATASET_PLUMBING)
        self.assertEqual(
            [task["name"] for task in plumbing["tasks"]],
            list(RUNG_A_PLUMBING_TASKS),
        )
        validate_plan(plumbing, require_jobs=False)

        ab = draft_plan(
            plan_id="rung-a-ab-codex-luna-low",
            account="codex",
            model="gpt-5.6-luna",
            effort="low",
            arms=("plak", "codex"),
            dataset=DATASET_TB21,
            tasks=RUNG_A_AB_TASKS,
        )
        self.assertEqual(ab["dataset"], DATASET_TB21)
        self.assertEqual(len(ab["tasks"]), 8)
        validate_plan(ab, require_jobs=False)

        openrouter = draft_plan(
            plan_id="rung-a-openrouter-deepseek-v4-flash-none",
            account="openrouter",
            model=OPENROUTER_MODEL,
            effort=OPENROUTER_EFFORT,
            arms=("plak", "pi", "opencode"),
            dataset=DATASET_TB21,
            tasks=RUNG_A_AB_TASKS,
        )
        validate_plan(openrouter, require_jobs=False)

    def test_rung_a_committed_json_or_draft_fallback(self) -> None:
        paths = sorted((ROOT / "eval" / "comparisons").glob("rung-a-*.json"))
        if paths:
            for path in paths:
                plan = json.loads(path.read_text())
                validate_plan(plan, require_jobs=False)
                for arm in plan["arms"].values():
                    job = arm.get("job") or {}
                    self.assertIsNone(job.get("id"), path.name)
                    self.assertIsNone(job.get("lockDigest"), path.name)
                    self.assertIsNone(arm.get("candidateProvenanceDigest"))
                with self.assertRaises(PlanError):
                    validate_plan(plan, require_jobs=True)
            return
        for plan_id, dataset, tasks in (
            (
                "rung-a-plumbing-codex-luna-low",
                DATASET_PLUMBING,
                RUNG_A_PLUMBING_TASKS,
            ),
            ("rung-a-ab-codex-luna-low", DATASET_TB21, RUNG_A_AB_TASKS),
        ):
            plan = draft_plan(
                plan_id=plan_id,
                account="codex",
                model="gpt-5.6-luna",
                effort="low",
                arms=("plak", "codex"),
                dataset=dataset,
                tasks=tasks,
            )
            self.assertEqual(plan["id"], plan_id)
            validate_plan(plan, require_jobs=False)


class ConfigurationContractTests(unittest.TestCase):
    def test_eval_python_project_is_self_contained(self) -> None:
        self.assertTrue((ROOT / "eval" / "pyproject.toml").is_file())
        self.assertFalse((ROOT / "pyproject.toml").is_file())

    def test_terminal_bench_pin(self) -> None:
        text = (ROOT / "eval" / "terminal-bench.yaml").read_text()
        self.assertIn("terminal-bench/terminal-bench@4.0.0", text)
        self.assertIn("harbor_adapter.agent:PlakAgent", text)
        self.assertIn("environment:\n  import_path: harbor_adapter.docker_retry:RetryingDocker", text)
        self.assertIn("n_concurrent_trials: 1", text)
        self.assertIn("model_name: gpt-5.6-luna", text)

    def test_rung_eval_yaml_pins(self) -> None:
        plumbing = (ROOT / "eval" / "rung-a-plumbing.yaml").read_text()
        self.assertIn("path: eval/datasets/rung-a-plumbing", plumbing)
        self.assertIn("n_concurrent_trials: 1", plumbing)

        ab = (ROOT / "eval" / "rung-a-ab.yaml").read_text()
        self.assertIn("name: terminal-bench/terminal-bench-2-1", ab)
        self.assertIn('ref: "6"', ab)
        self.assertIn("n_concurrent_trials: 1", ab)
        for short_name in (
            "openssl-selfsigned-cert",
            "fix-git",
            "nginx-request-logging",
            "sqlite-db-truncate",
            "cancel-async-tasks",
            "log-summary-date-ranges",
            "regex-log",
            "configure-git-webserver",
        ):
            self.assertIn(short_name, ab)
        for prefixed in RUNG_A_AB_TASKS:
            self.assertIn(prefixed.removeprefix("terminal-bench/"), ab)

        expanded = (ROOT / "eval" / "rung-a-expanded.yaml").read_text()
        self.assertIn('ref: "6"', expanded)
        self.assertEqual(len(RUNG_A_EXPANDED_TASKS), 16)
        for prefixed in RUNG_A_EXPANDED_TASKS:
            self.assertIn(prefixed, expanded)

        wire = (ROOT / "eval" / "rung-b-wire.yaml").read_text()
        self.assertIn("terminal-bench/terminal-bench", wire)
        self.assertIn('ref: "4.0.0"', wire)
        self.assertIn("n_concurrent_trials: 1", wire)

    def test_plumbing_task_names_and_verifiers_exist(self) -> None:
        dataset_root = ROOT / "eval" / "datasets" / "rung-a-plumbing"
        for slug, harbor_name in (
            ("write-hello", "plak/write-hello"),
            ("sum-file", "plak/sum-file"),
            ("fix-typo", "plak/fix-typo"),
        ):
            task_dir = dataset_root / slug
            toml = task_dir / "task.toml"
            self.assertTrue(toml.is_file(), slug)
            self.assertIn(f'name = "{harbor_name}"', toml.read_text())
            self.assertTrue((task_dir / "tests" / "test.sh").is_file(), slug)
            dockerfile = (task_dir / "environment" / "Dockerfile").read_text()
            # Harbor builds these on the daemon platform; a FROM pin would
            # force an emulated amd64 cell on an arm64 lane.
            self.assertIn("FROM ubuntu:24.04", dockerfile)
            self.assertNotIn("--platform", dockerfile)
            verifier = (task_dir / "tests" / "test_outputs.py").read_text()
            self.assertNotIn(".strip()", verifier)
            if slug == "write-hello":
                self.assertIn('== "hello\\n"', verifier)
            if slug == "sum-file":
                self.assertIn('== "20\\n"', verifier)
            if slug == "fix-typo":
                self.assertIn('== "Hello\\n"', verifier)

    def test_every_run_script_pins_the_cell_platform(self) -> None:
        # The plumbing tasks declare no docker_image, so Harbor always builds
        # them with `docker compose build`, and neither Harbor's build compose
        # template nor the task Dockerfiles name a platform. Without
        # DOCKER_DEFAULT_PLATFORM an `HARBOR_ARCH=amd64` lane on an arm64
        # daemon would build arm64 cells and label them linux/amd64.
        for name in ("rung-a.sh", "rung-b.sh", "smoke.sh", "harbor.sh"):
            with self.subTest(script=name):
                script = (ROOT / "eval" / "scripts" / name).read_text()
                self.assertIn('platform="$(harbor_platform "$arch")"', script)
                self.assertIn('export DOCKER_DEFAULT_PLATFORM="$platform"', script)

    def test_harbor_eval_runs_on_the_arch_switch(self) -> None:
        script = (ROOT / "eval/scripts/harbor.sh").read_text()
        branch = script.split("\n  eval)\n", 1)[1].split("\n  view)", 1)[0]
        self.assertIn("select_arch", branch)
        self.assertIn('export HARBOR_ARCH="$arch"', branch)
        self.assertIn('export DOCKER_DEFAULT_PLATFORM="$platform"', branch)
        self.assertIn("harbor_adapter.campaign", branch)
        self.assertNotIn("force_build=(--force-build)", branch)

    def test_arm_jobs_are_named_inside_the_jobs_tree(self) -> None:
        # rung-a and rung-b put the -arm64 suffix on the job name under jobs/;
        # smoke and harbor.sh eval spell it the same way. amd64 keeps its bare
        # jobs/ path so archived runs stay valid.
        smoke = (ROOT / "eval" / "scripts" / "smoke.sh").read_text()
        self.assertIn(
            'jobs_dir="$root/.plak/harbor/jobs${job_suffix:+/smoke$job_suffix}"',
            smoke,
        )
        self.assertNotIn('.plak/harbor/jobs${job_suffix}"', smoke)
        harbor = (ROOT / "eval" / "scripts" / "harbor.sh").read_text()
        self.assertNotIn('.plak/harbor/jobs${job_suffix}"', harbor)

    def test_rung_a_sweeps_preserve_failed_arm_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name in ("docker", "uv"):
                executable = root / name
                executable.write_text("#!/bin/sh\n" + ("exit 0\n" if name == "docker" else
                    'touch "$STUB_DISPATCH"\nexit 37\n'))
                executable.chmod(0o700)
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", HARBOR_ARCH="arm64",
                       STUB_DISPATCH=str(root / "dispatch"))
            env.pop("HARBOR_CAMPAIGN", None)
            env.pop("HARBOR_CAMPAIGN_SHA256", None)
            for sweep in ("all", "ablation"):
                completed = subprocess.run(["bash", str(ROOT / "eval/scripts/rung-a.sh"), "plumbing", sweep],
                                           env=env, capture_output=True, text=True, timeout=30)
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn("HARBOR_CAMPAIGN", completed.stderr)
                self.assertFalse((root / "dispatch").exists())

    def test_rung_a_requires_summary_after_successful_harbor_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            scripts = root / "eval" / "scripts"
            scripts.mkdir(parents=True)
            for name in ("rung-a.sh", "arch.sh", "summarize.py"):
                shutil.copy2(ROOT / "eval" / "scripts" / name, scripts / name)
            for child in (ROOT / "eval").iterdir():
                if child.name != "scripts":
                    (root / "eval" / child.name).symlink_to(child)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(
                ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "--allow-empty", "-qm", "Fixture source"],
                cwd=root, check=True,
            )
            stub_bin = root / "stub-bin"
            stub_bin.mkdir()
            for name in ("docker", "uv"):
                executable = stub_bin / name
                executable.write_text("#!/bin/sh\nexit 0\n")
                executable.chmod(0o700)
            result = subprocess.run(
                ["bash", str(scripts / "rung-a.sh"), "plumbing", "plak-codex"],
                env=dict(os.environ, PATH=f"{stub_bin}:{os.environ['PATH']}",
                         HARBOR_ARCH="arm64"),
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("no job directories found", result.stderr)
            receipts = list((root / ".plak/harbor/jobs").glob("*/termination-invocation.json"))
            self.assertEqual(len(receipts), 1)
            self.assertEqual(json.loads(receipts[0].read_text())["found_trials"], 0)

    def test_paid_vendor_arms_keep_full_arm_for_watched_key(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            docker = root / "docker"
            docker.write_text("#!/bin/sh\nexit 0\n")
            docker.chmod(0o700)
            for arm in ("plak-openrouter", "pi-openrouter", "opencode-openrouter"):
                result = subprocess.run(["bash", str(ROOT / "eval/scripts/rung-a.sh"), "plumbing", arm],
                    env=dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", HARBOR_ARCH="arm64"),
                    capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 2)
                self.assertIn("OpenRouter comparisons disabled", result.stderr)

    def test_rung_a_defaults_to_serial_paired_trials(self) -> None:
        rung_a = (ROOT / "eval" / "scripts" / "rung-a.sh").read_text()
        self.assertIn("concurrent=1", rung_a)
        self.assertIn('--n-concurrent "$concurrent"', rung_a)

    def test_just_recipes_forward_to_eval_script(self) -> None:
        justfile = (ROOT / "justfile").read_text()
        for command in ("rung-a.sh", "rung-b.sh", "smoke.sh", "harbor.sh"):
            self.assertIn("eval/scripts/" + command, justfile)
        self.assertIn("fetch-harbor-tools:", justfile)
        self.assertIn("harbor-test:", justfile)
        for name in ("rung-a.sh", "rung-b.sh", "smoke.sh", "harbor.sh"):
            script = (ROOT / "eval/scripts" / name).read_text()
            self.assertIn("harbor_adapter.campaign", script)
            self.assertIn('uv run --project "$root/eval" harbor', script)
            self.assertIn("</dev/null", script)

    def test_harbor_pin_and_linux_packaging(self) -> None:
        pyproject = (ROOT / "eval" / "pyproject.toml").read_text()
        self.assertIn("harbor==0.23.0", pyproject)
        self.assertTrue((ROOT / "eval" / "uv.lock").is_file())
        dockerfile = (ROOT / "eval" / "harbor_adapter" / "Dockerfile").read_text()
        self.assertIn("FROM rust:1.99.0-bookworm", dockerfile)
        self.assertNotIn("FROM --platform=", dockerfile)
        self.assertIn("clang mold", dockerfile)
        self.assertIn("-p plakous-js-runtime --bin plakous-js-host", dockerfile)
        self.assertNotIn("plakous-js-eval --features embedded", dockerfile)
        agent = (ROOT / "eval" / "harbor_adapter" / "agent.py").read_text()
        self.assertNotIn("bwrap", agent)
        self.assertIn("subscriptions/compatibility", agent)
        self.assertIn("ca-certificates", agent)
        self.assertNotIn('REMOTE_BINARY, "refresh", self._account', agent)
        self.assertNotIn("quoted_refresh", agent)
        self.assertNotIn("|| true; fi", agent)
        self.assertIn("REMOTE_OPENROUTER_KEY", agent)
        self.assertIn("require_ready_openrouter_key", agent)
        self.assertIn('self._account == "openrouter"', agent)
        self.assertEqual(
            REMOTE_OPENROUTER_KEY,
            "/tmp/plak-cell/home/secrets/openrouter.key",
        )
        gitignore = (ROOT / ".gitignore").read_text()
        self.assertIn(".plak/", gitignore)

    def test_committed_plan_digests_are_stable(self) -> None:
        pinned = {
            "tb4-smoke-codex-luna-low.json": (
                "sha256:88c7a1a5e376fc62de882826abe51d94f18c37ac9f3b3c921bb51487921915ee"
            ),
            "tb4-smoke-grok-46-low.json": (
                "sha256:bfdc3de516cb2769217c44170d8950387e1d0473403f270db26f67c3e64c3b74"
            ),
            "tb4-smoke-pi-opencode-codex-luna-low.json": (
                "sha256:63b5317ceb32056eb6e3d6888e64de0dfb51196a84944627806bb356ece455a2"
            ),
            "tb4-smoke-pi-opencode-grok-46-low.json": (
                "sha256:997f7d2aff9a1710652a92f8d36c3eaf10271cc8f2d999f75befb80894910e4a"
            ),
        }
        for name, digest in pinned.items():
            plan = json.loads((ROOT / "eval" / "comparisons" / name).read_text())
            self.assertEqual(plan_digest(plan), digest)


class VendorCliContractTests(unittest.TestCase):
    def test_missing_prefix_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(VendorArtifactError):
                require_frozen_artifacts("codex", pathlib.Path(tmp), "amd64")

    def test_manifest_version_mismatch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "node-v22.20.0-linux-x64.tar.gz").write_bytes(b"node")
            (root / "codex.tar.gz").write_bytes(b"codex")
            (root / "manifest.json").write_text(
                json.dumps(
                    {
                        "arch": "amd64",
                        "node": {
                            "version": NODE_VERSION,
                            "file": f"node-v{NODE_VERSION}-linux-x64.tar.gz",
                            "sha256": NODE_SHA256["amd64"],
                        },
                        "clis": {
                            "codex": {"version": "9.9.9", "file": "codex.tar.gz"},
                        },
                    }
                )
                + "\n"
            )
            with self.assertRaises(VendorArtifactError):
                require_frozen_artifacts("codex", root, "amd64")

    def test_cell_install_does_not_fetch_or_float(self) -> None:
        command = node_cli_install_command("codex")
        grok = grok_install_command()
        agents = (ROOT / "eval" / "harbor_adapter" / "vendor_agents.py").read_text()
        install = (ROOT / "eval" / "harbor_adapter" / "vendor_install.py").read_text()
        for blob in (command, grok, agents, install):
            for marker in FORBIDDEN_INSTALL_MARKERS:
                self.assertNotIn(marker, blob)
        self.assertIn("tar -xzf", command)
        self.assertIn(".nvm/nvm.sh", command)
        self.assertIn("$HOME/.grok/bin", grok)
        self.assertEqual(CLI_PINS["codex"], "0.159.3")
        self.assertNotIn("@latest", CLI_PINS.values())

    def test_stock_cells_reject_host_agent_overlays(self) -> None:
        agents = (ROOT / "eval" / "harbor_adapter" / "vendor_agents.py").read_text()
        self.assertIn("_reject_host_overlays", agents)
        self.assertEqual(
            NON_STOCK_KWARGS,
            {
                "codex": ("config",),
                "grok": ("grok_config",),
                "pi": ("model_api",),
                "opencode": ("opencode_config",),
            },
        )
        self.assertNotIn("settings.json", agents)
        self.assertIn("auth.json", agents)
        self.assertIn("require_ready_grok_auth", agents)
        self.assertIn("require_ready_pi_auth", agents)
        self.assertIn("require_ready_opencode_auth", agents)
        self.assertIn("grok-secrets", agents)
        self.assertIn("pi-secrets", agents)
        self.assertIn("opencode-secrets", agents)
        self.assertIn("pi_cli_model_name", agents)
        self.assertIn("openai-codex", (ROOT / "eval" / "harbor_adapter" / "credentials.py").read_text())
        frozen_grok = agents.split("class FrozenGrokBuild", 1)[1].split(
            "class FrozenPi", 1
        )[0]
        self.assertNotIn("grok_config", frozen_grok)
        self.assertNotIn("config.toml", frozen_grok)
        frozen_pi = agents.split("class FrozenPi", 1)[1].split(
            "class FrozenOpenCode", 1
        )[0]
        self.assertNotIn("enabledModels", frozen_pi)
        self.assertNotIn("upload_dir", frozen_pi)
        frozen_opencode = agents.split("class FrozenOpenCode", 1)[1]
        self.assertNotIn("opencode.jsonc", frozen_opencode)
        self.assertIn("XAI_API_KEY", agents)
        self.assertIn("OPENROUTER_API_KEY", agents)
        self.assertIn("not XAI_API_KEY", agents)
        self.assertIn("not provider API keys", agents)
        self.assertIn("OPENROUTER_API_KEY", frozen_pi)
        self.assertIn("OPENROUTER_API_KEY", frozen_opencode)

    def test_fetch_dockerfile_pins_node_and_refuses_latest(self) -> None:
        dockerfile = (ROOT / "eval" / "harbor_adapter" / "vendor.Dockerfile").read_text()
        self.assertIn("ARG NODE_SHA256", dockerfile)
        self.assertIn('echo "$NODE_SHA256  /out/$NODE_TARBALL" | sha256sum -c', dockerfile)
        self.assertIn("${CODEX_PACKAGE}@${CODEX_VERSION}", dockerfile)
        self.assertIn("${CLAUDE_PACKAGE}@${CLAUDE_VERSION}", dockerfile)
        self.assertNotIn("@latest", dockerfile)
        self.assertIn("x.ai/cli/install.sh", dockerfile)
        # The manifest describes its own build: buildx sets TARGETARCH from
        # --platform, so no caller-passed arch can disagree with the image.
        self.assertIn("ARG TARGETARCH", dockerfile)
        self.assertIn('\\"arch\\": \\"$TARGETARCH\\"', dockerfile)
        self.assertNotIn("VENDOR_ARCH", dockerfile)
        self.assertNotIn("FROM --platform=", dockerfile)
        for arch in ARCHES:
            args = docker_build_args(arch)
            self.assertNotIn("VENDOR_ARCH", args)
            self.assertNotIn("TARGETARCH", args)
            self.assertEqual(args["NODE_TARBALL"], node_tarball_name(arch))
        self.assertEqual(
            NODE_SHA256["amd64"],
            "eeaccb0378b79406f2208e8b37a62479c70595e20be6b659125eb77dd1ab2a29",
        )
        self.assertEqual(
            NODE_SHA256["arm64"],
            "4181609e03dcb9880e7e5bf956061ecc0503c77a480c6631d868cb1f65a2c7dd",
        )

    def test_verifier_timeout_is_not_a_zero_reward(self) -> None:
        phases = trial_phases(
            {
                "task_name": "terminal-bench/bun-sourcemap-leak",
                "agent_setup": {
                    "started_at": "2026-09-15T02:42:12.689723Z",
                    "finished_at": "2026-09-15T02:46:12.689723Z",
                },
                "agent_execution": {
                    "started_at": "2026-09-15T02:46:12.689723Z",
                    "finished_at": "2026-09-15T02:50:12.689723Z",
                },
                "verifier_result": {"rewards": {"reward": 0.0}},
                "exception_info": {"exception_type": "VerifierTimeoutError"},
            }
        )
        self.assertEqual(phases["agent_setup_sec"], 240.0)
        self.assertEqual(phases["agent_execution_sec"], 240.0)
        self.assertIsNone(phases["reward"])
        self.assertEqual(phases["exception_type"], "VerifierTimeoutError")


class CodexCaPrerequisiteTests(unittest.IsolatedAsyncioTestCase):
    async def test_missing_bundle_is_installed_and_recorded_before_dispatch(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            missing = types.SimpleNamespace(return_code=1, stdout="")
            present = types.SimpleNamespace(
                return_code=0,
                stdout=f"{'a' * 64}  /etc/ssl/certs/ca-certificates.crt\n12345\n",
            )
            environment = types.SimpleNamespace(
                exec=mock.AsyncMock(side_effect=(missing, present))
            )
            agent = types.SimpleNamespace(
                ensure_system_dependencies=mock.AsyncMock()
            )
            record = await module._ensure_codex_ca_bundle(agent, environment)
            agent.ensure_system_dependencies.assert_awaited_once_with(
                environment, ("ca_certificates",)
            )
            self.assertEqual(record["bytes"], 12345)
            self.assertEqual(record["sha256"], "a" * 64)
            self.assertEqual(environment.exec.await_count, 2)
            command = environment.exec.await_args_list[0].kwargs["command"]
            self.assertIn("test -s /etc/ssl/certs/ca-certificates.crt", command)
            self.assertIn("test -r /etc/ssl/certs/ca-certificates.crt", command)

    async def test_existing_bundle_skips_install_and_invalid_bundle_fails(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            present = types.SimpleNamespace(
                return_code=0,
                stdout=f"{'b' * 64}  /etc/ssl/certs/ca-certificates.crt\n42\n",
            )
            environment = types.SimpleNamespace(exec=mock.AsyncMock(return_value=present))
            agent = types.SimpleNamespace(ensure_system_dependencies=mock.AsyncMock())
            self.assertEqual(
                (await module._ensure_codex_ca_bundle(agent, environment))["bytes"],
                42,
            )
            agent.ensure_system_dependencies.assert_not_awaited()

            environment.exec.return_value = types.SimpleNamespace(
                return_code=0, stdout=f"{'b' * 64}  bundle\n0\n"
            )
            with self.assertRaises(module.VendorArtifactError):
                await module._ensure_codex_ca_bundle(agent, environment)
            agent.ensure_system_dependencies.assert_awaited_once_with(
                environment, ("ca_certificates",)
            )

    async def test_frozen_codex_install_binds_ca_to_existing_artifact(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            agent = module.FrozenCodex.__new__(module.FrozenCodex)
            agent._arch = "amd64"
            agent._vendor_root = pathlib.Path("/frozen")
            agent.ensure_system_dependencies = mock.AsyncMock()
            agent.exec_as_root = mock.AsyncMock()
            environment = object()
            artifacts = types.SimpleNamespace(version="0.155.1")
            ca = {"path": module._CA_BUNDLE, "sha256": "c" * 64, "bytes": 7}
            with (
                mock.patch.object(module, "require_frozen_artifacts", return_value=artifacts),
                mock.patch.object(module, "_install_node_cli", new_callable=mock.AsyncMock),
                mock.patch.object(module, "_ensure_codex_ca_bundle", new_callable=mock.AsyncMock, return_value=ca),
                mock.patch.object(module, "_assert_version", new_callable=mock.AsyncMock),
                mock.patch.object(module, "_record_executable_identity", new_callable=mock.AsyncMock) as identity,
            ):
                await agent.install(environment)
            agent.ensure_system_dependencies.assert_not_awaited()
            identity.assert_awaited_once_with(
                agent, environment, "codex", "amd64",
                prerequisites={"ca_bundle": ca},
            )

    async def test_vendor_artifact_retains_ca_identity(self) -> None:
        with _installed_agent_with_harbor_stub("vendor_agents") as module:
            with tempfile.TemporaryDirectory() as directory:
                agent = types.SimpleNamespace(logs_dir=pathlib.Path(directory))
                environment = types.SimpleNamespace(
                    exec=mock.AsyncMock(return_value=types.SimpleNamespace(
                        return_code=0, stdout=f"{'d' * 64}  binary\n99\n"
                    ))
                )
                ca = {"path": module._CA_BUNDLE, "sha256": "e" * 64, "bytes": 42}
                await module._record_executable_identity(
                    agent, environment, "codex", "amd64",
                    prerequisites={"ca_bundle": ca},
                )
                artifact = json.loads(
                    (agent.logs_dir / "vendor-artifact.json").read_text()
                )
                self.assertEqual(artifact["prerequisites"]["ca_bundle"], ca)
                self.assertEqual(artifact["harness"], "codex")


class EffortControlTests(unittest.TestCase):
    def test_smoke_effort_maps_on_every_vendor(self) -> None:
        self.assertEqual(SMOKE_EFFORT, "low")
        self.assertEqual(vendor_effort_ak("codex", "low"), ("reasoning_effort=low",))
        self.assertEqual(
            vendor_effort_ak("grok-build", "low"), ("reasoning_effort=low",)
        )
        self.assertEqual(vendor_effort_ak("pi", "low"), ("thinking=low",))
        self.assertEqual(vendor_effort_ak("opencode", "low"), ("variant=low",))

    def test_missing_vendor_rung_fails_closed(self) -> None:
        with self.assertRaises(ControlError):
            vendor_effort_ak("pi", "ultra")
        with self.assertRaises(ControlError):
            vendor_effort_ak("opencode", "max")
        with self.assertRaises(ControlError):
            vendor_effort_ak("codex", "ultra")

    def test_frozen_ak_includes_cli_pin(self) -> None:
        flags = harbor_ak_flags("codex", effort="low", version=CLI_PINS["codex"])
        self.assertEqual(flags[0], f"version={CLI_PINS['codex']}")
        self.assertIn("reasoning_effort=low", flags)
        with self.assertRaises(ControlError):
            harbor_ak_flags("codex", effort="low", version="9.9.9")

    def test_effort_none_omits_vendor_effort_ak(self) -> None:
        for agent in ("codex", "grok-build", "pi", "opencode"):
            with self.subTest(agent=agent), self.assertRaises(ControlError):
                vendor_effort_ak(agent, "none")


class ImpliedCostTests(unittest.TestCase):
    def test_rate_card_covers_smoke_models(self) -> None:
        card = load_rate_card()
        self.assertEqual(card["id"], "rung-a-2026-09-22")
        self.assertIn("gpt-5.6-luna", card["models"])
        self.assertIn("gpt-6-luna", card["models"])
        self.assertIn("claude-opus-5-5", card["models"])
        self.assertIn("grok-4.6", card["models"])

    def test_new_rung_a_models_use_public_list_rates(self) -> None:
        usage = CanonicalUsage(input_tokens=1_000_000, output_tokens=1_000_000, cache_read=0, cache_write=0)
        self.assertAlmostEqual(implied_usd("openai/gpt-6-luna", usage).usd, 0.60)
        self.assertAlmostEqual(implied_usd("anthropic/claude-opus-5-5", usage).usd, 24.0)
        self.assertAlmostEqual(
            implied_usd("openrouter/deepseek/deepseek-v4-flash", usage).usd,
            0.088606 + 0.177212,
        )

    def test_uncached_and_cache_are_not_double_counted(self) -> None:
        cost = implied_usd(
            "openai/gpt-5.6-luna",
            CanonicalUsage(input_tokens=1_000_000, cache_read=500_000, cache_write=0, output_tokens=0),
        )
        self.assertEqual(cost.status, "implied")
        # 0.5M uncached at $0.20 + 0.5M cache-read at $0.02
        self.assertAlmostEqual(cost.usd, 0.10 + 0.01)

    def test_unpriced_cache_write_is_unresolved(self) -> None:
        cost = implied_usd(
            "grok-4.6",
            CanonicalUsage(input_tokens=10, cache_read=0, cache_write=4, output_tokens=1),
        )
        self.assertIsNone(cost.usd)
        self.assertEqual(cost.status, "unpriced_cache_write")

    def test_missing_tokens_are_not_zero_dollars(self) -> None:
        cost = from_harbor_context(
            "gpt-5.6-luna",
            n_input_tokens=None,
            n_cache_tokens=0,
            n_output_tokens=None,
        )
        self.assertFalse(cost.usd == 0)
        self.assertIsNone(cost.usd)
        self.assertEqual(cost.status, "unresolved_usage")

    def test_unknown_model_is_unpriced(self) -> None:
        cost = implied_usd("mystery-model", CanonicalUsage(input_tokens=10, output_tokens=1, cache_read=0, cache_write=0))
        self.assertIsNone(cost.usd)
        self.assertEqual(cost.status, "unpriced_model")


class FrozenPmuxContractTests(unittest.TestCase):
    def test_pmux_run_is_full_cell_not_ask(self) -> None:
        from harbor_adapter.pmux_args import pmux_run_arguments

        argv = pmux_run_arguments(
            model="claude-sonnet-5",
            effort="low",
            prompt="Fix the tests.",
        )
        self.assertEqual(argv[0], "/installed-agent/pmux")
        self.assertEqual(argv[1:3], ["--output", "json"])
        self.assertEqual(argv[3], "run")
        self.assertNotIn("ask", argv)
        self.assertIn("--cwd", argv)
        self.assertIn("/app", argv)
        self.assertIn("dangerously-skip-permissions", argv)
        self.assertNotIn("--print", argv)
        self.assertEqual(argv[-2], "--")
        self.assertEqual(argv[-1], "Fix the tests.")

    def test_pmux_harbor_uses_prompt_file_not_nested_su_c(self) -> None:
        import shlex

        from harbor_adapter.pmux_args import PmuxArgumentError, pmux_run_arguments
        from harbor_adapter.pmux_trial import (
            PMUX_CELL_USER,
            pmux_prompt_write_command,
            pmux_trial_command,
        )
        from harbor_adapter.tuples import REMOTE_PMUX_PROMPT, REMOTE_PMUX_TRIAL

        instruction = "Create a certificate whose subject's CN is example.com"
        argv = pmux_run_arguments(
            model="claude-sonnet-5",
            effort="low",
            prompt_file=REMOTE_PMUX_PROMPT,
        )
        self.assertIn("--prompt-file", argv)
        self.assertEqual(argv[-2], "--prompt-file")
        self.assertEqual(argv[-1], REMOTE_PMUX_PROMPT)
        self.assertNotIn("--", argv)
        self.assertNotIn(instruction, argv)

        write = pmux_prompt_write_command(instruction)
        self.assertNotIn("certificate", write)
        self.assertNotIn("subject's", write)
        self.assertIn("base64 -d", write)

        quoted = " ".join(shlex.quote(part) for part in argv)
        command = pmux_trial_command(
            quoted, "/logs/agent/pmux-stdout.txt", "/logs/agent/pmux-stderr.log"
        )
        self.assertNotIn("certificate", command)
        self.assertIn(REMOTE_PMUX_TRIAL, command)
        self.assertIn(f"exec su -s /bin/bash {PMUX_CELL_USER} --", command)
        self.assertNotIn(" -c ", command)
        self.assertIn("base64 -d", command)
        import base64
        import re

        match = re.search(r"printf '%s' ([A-Za-z0-9+/=]+) \| base64 -d >", command)
        self.assertIsNotNone(match)
        decoded = base64.b64decode(match.group(1)).decode("utf-8")
        self.assertIn("--prompt-file", decoded)
        self.assertIn(REMOTE_PMUX_PROMPT, decoded)
        self.assertIn("--pool-turn-timeout-ms 900000", decoded)
        self.assertIn("/transcripts/", decoded)
        self.assertIn("pmux_harvest", decoded)
        self.assertIn("trap 'pmux_harvest' EXIT", decoded)
        self.assertIn("sudo -n true", decoded)
        self.assertIn("TMPDIR=/tmp/pmux-tmp", decoded)
        self.assertNotIn("certificate", decoded)

        with self.assertRaises(PmuxArgumentError):
            pmux_run_arguments(
                model="claude-sonnet-5",
                effort="low",
                prompt=instruction,
                prompt_file=REMOTE_PMUX_PROMPT,
            )
        with self.assertRaises(PmuxArgumentError):
            pmux_run_arguments(
                model="claude-sonnet-5",
                effort="low",
                prompt_file="prompt.txt",
            )

    def test_pmux_etc_acl_excludes_sudo_control_files(self) -> None:
        from harbor_adapter.pmux_trial import pmux_trial_command

        command = pmux_trial_command(
            "true", "/logs/agent/pmux-stdout.txt", "/logs/agent/pmux-stderr.log"
        )
        etc_find = [
            line.strip()
            for line in command.splitlines()
            if line.strip().startswith("find /etc ")
        ]
        self.assertEqual(len(etc_find), 3)
        for line in etc_find:
            self.assertIn("-path '/etc/sudo*' -prune -o", line)
        self.assertEqual(sum("-exec setfacl " in line for line in etc_find), 2)
        self.assertEqual(sum("-exec chown " in line for line in etc_find), 1)

    def test_pmux_refuses_relative_cwd_and_empty_prompt(self) -> None:
        from harbor_adapter.pmux_args import PmuxArgumentError, pmux_run_arguments
        from harbor_adapter.tuples import REMOTE_PMUX_RUNTIME

        with self.assertRaises(PmuxArgumentError):
            pmux_run_arguments(model="claude-sonnet-5", effort="low", prompt="x", cwd="app")
        with self.assertRaises(PmuxArgumentError):
            pmux_run_arguments(model="claude-sonnet-5", effort="low", prompt="  ")
        with self.assertRaises(PmuxArgumentError):
            pmux_run_arguments(model="claude-sonnet-5", effort="high", prompt="x")
        with self.assertRaises(PmuxArgumentError):
            pmux_run_arguments(
                model="claude-sonnet-5",
                effort="low",
                prompt="x",
                cwd=f"{REMOTE_PMUX_RUNTIME}/pool/task",
            )

    def test_pmux_json_usage_maps_cache_read(self) -> None:
        payload = json.dumps(
            {
                "usage": {
                    "main": {
                        "input_tokens": 10,
                        "output_tokens": 3,
                        "cache_read_input_tokens": 8,
                        "cache_creation_input_tokens": 0,
                    }
                }
            }
        )
        totals = parse_pmux_json(payload)
        self.assertTrue(totals.resolved)
        self.assertEqual(totals.n_input_tokens, 18)
        self.assertEqual(totals.n_cache_tokens, 8)
        self.assertEqual(totals.n_cache_write_tokens, 0)
        self.assertEqual(totals.n_output_tokens, 3)
        self.assertIsNone(totals.cost_usd)

    def test_pmux_json_prefers_combined_usage(self) -> None:
        payload = json.dumps(
            {
                "usage": {
                    "main": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                    },
                    "combined": {
                        "input_tokens": 40,
                        "output_tokens": 9,
                        "cache_read_input_tokens": 12,
                    },
                }
            }
        )
        totals = parse_pmux_json(payload)
        self.assertFalse(totals.resolved)
        self.assertIsNone(totals.n_input_tokens)
        self.assertEqual(totals.n_cache_tokens, 12)
        self.assertEqual(totals.n_output_tokens, 9)

    def test_pmux_json_prompt_is_remainder_plus_cache_buckets(self) -> None:
        payload = json.dumps(
            {
                "usage": {
                    "combined": {
                        "input_tokens": 12,
                        "output_tokens": 1741,
                        "cache_creation_input_tokens": 14489,
                        "cache_read_input_tokens": 239480,
                    }
                }
            }
        )
        totals = parse_pmux_json(payload)
        self.assertEqual(totals.n_input_tokens, 253981)
        self.assertEqual(totals.n_cache_tokens, 239480)
        self.assertEqual(totals.n_cache_write_tokens, 14489)
        self.assertEqual(totals.n_output_tokens, 1741)

    @staticmethod
    def _assistant(
        message_id: str | None,
        usage: dict[str, int],
        *,
        request_id: str | None = None,
    ) -> dict[str, object]:
        message: dict[str, object] = {"role": "assistant", "usage": usage}
        if message_id is not None:
            message["id"] = message_id
        row: dict[str, object] = {"type": "assistant", "message": message}
        if request_id is not None:
            row["requestId"] = request_id
        return row

    def test_pmux_jsonl_collapses_snapshots_of_one_message_id(self) -> None:
        first = {
            "input_tokens": 2,
            "output_tokens": 376,
            "cache_read_input_tokens": 29159,
            "cache_creation_input_tokens": 11924,
        }
        second = {
            "input_tokens": 10,
            "output_tokens": 1365,
            "cache_read_input_tokens": 210321,
            "cache_creation_input_tokens": 2565,
        }
        payload = "\n".join(
            json.dumps(row)
            for row in (
                self._assistant("msg_a", first),
                self._assistant("msg_a", first),
                self._assistant("msg_b", second),
                {"type": "user"},
            )
        )
        totals = parse_pmux_jsonl(payload)
        self.assertTrue(totals.resolved)
        self.assertEqual(totals.n_input_tokens, 2 + 29159 + 11924 + 10 + 210321 + 2565)
        self.assertEqual(totals.n_cache_tokens, 29159 + 210321)
        self.assertEqual(totals.n_cache_write_tokens, 11924 + 2565)
        self.assertEqual(totals.n_output_tokens, 376 + 1365)

    def test_pmux_jsonl_sums_distinct_ids_with_equal_usage(self) -> None:
        usage = {
            "input_tokens": 5,
            "output_tokens": 7,
            "cache_read_input_tokens": 11,
            "cache_creation_input_tokens": 13,
        }
        payload = "\n".join(
            json.dumps(row)
            for row in (
                self._assistant("msg_a", usage),
                self._assistant("msg_b", dict(usage)),
            )
        )
        totals = parse_pmux_jsonl(payload)
        self.assertTrue(totals.resolved)
        self.assertEqual(totals.n_input_tokens, 2 * (5 + 11 + 13))
        self.assertEqual(totals.n_output_tokens, 2 * 7)
        self.assertEqual(totals.n_cache_tokens, 2 * 11)
        self.assertEqual(totals.n_cache_write_tokens, 2 * 13)

    def test_pmux_jsonl_keeps_the_last_snapshot_of_one_id(self) -> None:
        payload = "\n".join(
            json.dumps(row)
            for row in (
                self._assistant(
                    "msg_a",
                    {
                        "input_tokens": 3,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                ),
                self._assistant(
                    "msg_a",
                    {
                        "input_tokens": 3,
                        "output_tokens": 40,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                ),
                self._assistant(
                    "msg_a",
                    {
                        "input_tokens": 3,
                        "output_tokens": 900,
                        "cache_read_input_tokens": 12,
                        "cache_creation_input_tokens": 4,
                    },
                ),
            )
        )
        totals = parse_pmux_jsonl(payload)
        self.assertTrue(totals.resolved)
        self.assertEqual(totals.n_output_tokens, 900)
        self.assertEqual(totals.n_input_tokens, 3 + 12 + 4)

    def test_pmux_jsonl_separates_request_ids_under_one_message_id(self) -> None:
        usage = {
            "input_tokens": 1,
            "output_tokens": 2,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        }
        payload = "\n".join(
            json.dumps(row)
            for row in (
                self._assistant("msg_a", usage, request_id="req_1"),
                self._assistant("msg_a", dict(usage), request_id="req_2"),
            )
        )
        totals = parse_pmux_jsonl(payload)
        self.assertEqual(totals.n_output_tokens, 4)
        self.assertEqual(totals.n_input_tokens, 2)

    def test_pmux_jsonl_without_message_id_is_unresolved(self) -> None:
        usage = {
            "input_tokens": 2,
            "output_tokens": 376,
            "cache_read_input_tokens": 1,
            "cache_creation_input_tokens": 1,
        }
        payload = "\n".join(
            json.dumps(row)
            for row in (
                self._assistant("msg_a", usage),
                self._assistant(None, dict(usage)),
            )
        )
        totals = parse_pmux_jsonl(payload)
        self.assertFalse(totals.resolved)
        self.assertIsNone(totals.n_input_tokens)
        self.assertIsNone(totals.n_output_tokens)

    def test_darwin_pmux_is_refused(self) -> None:
        from harbor_adapter.pmux_pins import PmuxArtifactError, require_frozen_pmux

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "pmux").write_bytes(b"\xcf\xfa\xed\xfe" + b"\x00" * 16)
            for name in ("pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook"):
                (root / name).write_bytes(b"\x7fELF" + b"\x00" * 16)
            with self.assertRaises(PmuxArtifactError):
                require_frozen_pmux(root)

    def test_linux_elf_pmux_is_admitted(self) -> None:
        from harbor_adapter.pmux_pins import require_frozen_pmux

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for name in (
                "pmux",
                "pmuxd",
                "pmux-rmuxd",
                "pmux-launcher",
                "pmux-hook",
            ):
                (root / name).write_bytes(_elf_header(0x3E) + b"\x00" * 64)
            _pin_pmux_manifest(root, "amd64")
            bins = require_frozen_pmux(root, "amd64")
            self.assertEqual(bins.pmux.name, "pmux")
            self.assertEqual(bins.rmuxd.name, "pmux-rmuxd")
            self.assertEqual(bins.arch, "amd64")
            self.assertEqual(bins.commit, PSEUDOMUX_PIN)
            self.assertFalse(bins.dirty)

    def test_pmux_without_provenance_is_refused(self) -> None:
        # Five correct ELF files are not an identity. Binaries built before
        # the manifest existed must be refetched, not waved through.
        from harbor_adapter.pmux_pins import require_frozen_pmux

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for name in (
                "pmux",
                "pmuxd",
                "pmux-rmuxd",
                "pmux-launcher",
                "pmux-hook",
            ):
                (root / name).write_bytes(_elf_header(0x3E) + b"\x00" * 64)
            with self.assertRaises(PmuxArtifactError) as caught:
                require_frozen_pmux(root, "amd64")
            self.assertIn("just fetch-harbor-pmux", str(caught.exception))

    def test_claude_file_store_fails_closed_without_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            os.environ["PLAK_CLAUDE_CODE_CREDENTIALS_DIR"] = tmp
            os.environ.pop("DOCKER_HOST", None)
            try:
                with self.assertRaises(CredentialError) as ctx:
                    require_ready_claude_code_store()
                self.assertIn("keychain", str(ctx.exception).lower())
            finally:
                os.environ.pop("PLAK_CLAUDE_CODE_CREDENTIALS_DIR", None)

    def test_replace_claude_code_store_writes_owner_only_pin(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = pathlib.Path(tmp)
            credential = directory / ".credentials.json"
            credential.write_text(
                '{"claudeAiOauth": {"accessToken": "old-a", "refreshToken": "old-r"}}\n',
                encoding="utf-8",
            )
            os.chmod(credential, 0o644)
            candidate = directory / ".credentials.json.cell"
            candidate.write_text(
                '{"claudeAiOauth": {"accessToken": "new-a", "refreshToken": "new-r"}}\n',
                encoding="utf-8",
            )
            replace_claude_code_store(directory, candidate)
            self.assertFalse(candidate.exists())
            self.assertEqual(
                json.loads(credential.read_text(encoding="utf-8")),
                {"claudeAiOauth": {"accessToken": "new-a", "refreshToken": "new-r"}},
            )
            self.assertEqual(credential.stat().st_mode & 0o777, 0o600)
            for bad in (
                {},
                {"claudeAiOauth": {"accessToken": "", "refreshToken": ""}},
                {"claudeAiOauth": {"accessToken": "new-a", "refreshToken": " "}},
                {"claudeAiOauth": {"accessToken": "new-a"}},
            ):
                candidate.write_text(json.dumps(bad), encoding="utf-8")
                with self.assertRaises(CredentialError):
                    replace_claude_code_store(directory, candidate)
                self.assertTrue(candidate.exists())
                self.assertEqual(
                    json.loads(credential.read_text(encoding="utf-8"))["claudeAiOauth"],
                    {"accessToken": "new-a", "refreshToken": "new-r"},
                )

    def test_claude_store_requires_nonempty_oauth_pair_but_allows_refresh(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            credential = pathlib.Path(tmp, ".credentials.json")
            with mock.patch.dict(
                os.environ,
                {"PLAK_CLAUDE_CODE_CREDENTIALS_DIR": tmp, "DOCKER_HOST": ""},
            ):
                for bad in (
                    {"unrelated": "account"},
                    {"claudeAiOauth": {}},
                    {"claudeAiOauth": {"accessToken": "", "refreshToken": ""}},
                    {"claudeAiOauth": {"accessToken": "a", "refreshToken": " "}},
                    {"claudeAiOauth": {"accessToken": 1, "refreshToken": "r"}},
                ):
                    credential.write_text(json.dumps(bad), encoding="utf-8")
                    with self.assertRaises(CredentialError) as ctx:
                        require_ready_claude_code_store()
                    self.assertNotIn("accessToken", str(ctx.exception))
                credential.write_text(
                    json.dumps(
                        {
                            "claudeAiOauth": {
                                "accessToken": "synthetic-a",
                                "refreshToken": "synthetic-r",
                                "expiresAt": 0,
                            }
                        }
                    ),
                    encoding="utf-8",
                )
                self.assertEqual(require_ready_claude_code_store(), pathlib.Path(tmp))

    def test_empty_claude_credentials_file_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            pathlib.Path(tmp, ".credentials.json").write_text("{}\n", encoding="utf-8")
            os.environ["PLAK_CLAUDE_CODE_CREDENTIALS_DIR"] = tmp
            os.environ.pop("DOCKER_HOST", None)
            try:
                with self.assertRaises(CredentialError) as ctx:
                    require_ready_claude_code_store()
                self.assertIn("empty", str(ctx.exception).lower())
            finally:
                os.environ.pop("PLAK_CLAUDE_CODE_CREDENTIALS_DIR", None)

    def test_darwin_default_claude_store_requires_override(self) -> None:
        import sys

        if sys.platform != "darwin":
            self.skipTest("Darwin default store refuse")
        previous = os.environ.pop("PLAK_CLAUDE_CODE_CREDENTIALS_DIR", None)
        os.environ.pop("DOCKER_HOST", None)
        try:
            with self.assertRaises(CredentialError) as ctx:
                require_ready_claude_code_store()
            self.assertIn("PLAK_CLAUDE_CODE_CREDENTIALS_DIR", str(ctx.exception))
        finally:
            if previous is not None:
                os.environ["PLAK_CLAUDE_CODE_CREDENTIALS_DIR"] = previous

    def test_source_never_scores_ask_or_print(self) -> None:
        agent = (ROOT / "eval" / "harbor_adapter" / "pmux_agent.py").read_text()
        trial = (ROOT / "eval" / "harbor_adapter" / "pmux_trial.py").read_text()
        self.assertIn("pmux run", agent)
        self.assertIn("--stateful", trial)
        self.assertIn("chmod 700", trial)
        self.assertIn("REMOTE_CLAUDE_SECURESTORAGE", trial)
        self.assertIn("chmod 700 {runtime} {pool} {tmpdir} {pin}", trial)
        self.assertIn("useradd --system --create-home", trial)
        self.assertIn("NOPASSWD:ALL", trial)
        self.assertIn("!requiretty", trial)
        self.assertIn("setfacl", trial)
        self.assertIn("/etc/sudo*", trial)
        self.assertIn("chown root:root /etc/sudoers", trial)
        self.assertIn("pmux_harvest", trial)
        self.assertIn("sudo -n true", trial)
        self.assertIn("parse_pmux_jsonl", agent)
        self.assertIn("exec su -s /bin/bash", trial)
        self.assertIn('PMUX_CELL_USER = "pmux"', trial)
        self.assertIn('if [ "$(id -u)" -eq 0 ]', trial)
        self.assertIn("/bin/ps", trial)
        self.assertIn("procps", trial)
        self.assertIn("claude-transcripts", trial)
        self.assertIn("{pool}/transcripts/", trial)
        self.assertIn("projects/*/*.jsonl", trial)
        self.assertIn("--pool-turn-timeout-ms", trial)
        self.assertIn("HARBOR_PMUX_TURN_TIMEOUT_MS = 900_000", trial)
        self.assertNotIn("harvester=$!", trial)
        self.assertNotIn("while true; do", trial)
        self.assertNotIn(") &\n) &", trial)
        self.assertIn("TMPDIR={tmpdir}", trial)
        self.assertIn("REMOTE_PMUX_TMPDIR", trial)
        self.assertIn("download_file", agent)
        self.assertIn("replace_claude_code_store", agent)
        self.assertIn("ulimit -n 8192", trial)
        self.assertIn("prompt_file=", agent)
        self.assertIn("--prompt-file", (ROOT / "eval" / "harbor_adapter" / "pmux_args.py").read_text())
        self.assertNotIn("pmux ask", agent)
        self.assertNotIn("claude --print", agent)
        self.assertIn("unset ANTHROPIC_API_KEY", trial)
        self.assertIn("/installed-agent/vendor/node/bin", trial)
        dockerfile = (ROOT / "eval" / "harbor_adapter" / "pmux.Dockerfile").read_text()
        self.assertIn("FROM rust:1.88.0-bookworm", dockerfile)
        self.assertNotIn("FROM --platform=", dockerfile)
        self.assertIn(
            "-p pmux -p pmuxd -p pmux-rmuxd -p pmux-launcher -p pmux-hook",
            dockerfile,
        )
        self.assertNotIn("-p pmux-mcp", dockerfile)
        self.assertIn("2.1.280", CLI_PINS["claude"])
        tuples = (ROOT / "eval" / "harbor_adapter" / "tuples.py").read_text()
        self.assertIn(
            'REMOTE_CLAUDE_SECURESTORAGE = "/tmp/claude-secrets"',
            tuples,
        )
        self.assertIn('REMOTE_PMUX_RUNTIME = "/tmp/pmux"', tuples)
        self.assertIn('REMOTE_PMUX_TMPDIR = "/tmp/pmux-tmp"', tuples)
        self.assertIn('REMOTE_PMUX_PROMPT = f"{REMOTE_PMUX_RUNTIME}/prompt.txt"', tuples)
        self.assertNotIn('REMOTE_PMUX_RUNTIME = "/logs/agent/pmux"', tuples)

    def test_stripped_anthropic_env_is_refused(self) -> None:
        # A cleared environment is the only way this is deterministic: the
        # refusal loops over all of STRIP_ENV, so an ambient OPENROUTER_API_KEY
        # on the operator's shell would otherwise decide the outcome.
        with mock.patch.dict(os.environ, {}, clear=True):
            refuse_stripped_env(None)
            refuse_stripped_env({})
            for key in STRIP_ENV:
                with self.subTest(key=key, source="process"):
                    with mock.patch.dict(os.environ, {key: "refused"}, clear=True):
                        with self.assertRaises(CredentialError):
                            refuse_stripped_env(None)
                        with self.assertRaises(CredentialError):
                            refuse_stripped_env({})
                with self.subTest(key=key, source="extra_env"):
                    with self.assertRaises(CredentialError):
                        refuse_stripped_env({key: "refused"})
            refuse_stripped_env({"PATH": "/usr/bin"})

    def test_claude_pin_is_in_vendor_dockerfile(self) -> None:
        self.assertEqual(CLI_PINS["claude"], "2.1.280")
        self.assertIn("claude", (ROOT / "eval" / "harbor_adapter" / "vendor_pins.py").read_text())


class HarnessTerminationRecordTests(unittest.TestCase):
    """Every harness death names itself; none is recorded as never having run."""

    def test_harness_error_state_is_declared(self) -> None:
        agent = (ROOT / "eval" / "harbor_adapter" / "pmux_agent.py").read_text()
        self.assertIn('TERMINATION_ERROR = "harness-error"', agent)
        self.assertIn("except BaseException as error:", agent)
        self.assertIn('f"{TERMINATION_ERROR}:{type(error).__name__}"', agent)

    def test_any_harness_death_records_harness_error_and_reraises(self) -> None:
        with _installed_agent_with_harbor_stub() as pmux_agent:
            self._assert_harness_error_is_recorded(pmux_agent)

    def _assert_harness_error_is_recorded(self, pmux_agent) -> None:
        import asyncio

        class _HarnessGone(RuntimeError):
            pass

        class _Environment:
            default_user = "agent"

            async def upload_file(self, *_args: object, **_kwargs: object) -> None:
                return None

        class _Context:
            pass

        async def _ok(*_args: object, **_kwargs: object) -> None:
            return None

        async def _boom(*_args: object, **_kwargs: object) -> None:
            raise _HarnessGone("pmux disappeared before it could exit")

        with tempfile.TemporaryDirectory() as tmp:
            store = pathlib.Path(tmp)
            (store / ".credentials.json").write_text("{}")
            agent = pmux_agent.FrozenPmux.__new__(pmux_agent.FrozenPmux)
            agent.extra_env = {}
            agent._model = "claude-sonnet-5"
            agent._effort = "low"
            agent._turn_timeout_ms = 900_000
            agent._harness_termination = pmux_agent.TERMINATION_NOT_RUN
            agent.exec_as_root = _ok
            agent.exec_as_agent = _boom
            agent._persist_refreshed_pin = _ok
            agent._capture_usage = _ok
            with (
                mock.patch.object(
                    pmux_agent, "refuse_stripped_env", lambda _env: None
                ),
                mock.patch.object(
                    pmux_agent, "require_ready_claude_code_store", lambda: store
                ),
            ):
                with self.assertRaises(_HarnessGone):
                    asyncio.run(agent.run("fix the tests", _Environment(), _Context()))
        self.assertEqual(agent._harness_termination, "harness-error:_HarnessGone")


def _pin_pmux_manifest(root: pathlib.Path, arch: str) -> None:
    """The pin check now needs provenance too, not only ELF headers."""
    record_provenance(
        root,
        arch=arch,
        source=SourceCommit(commit=PSEUDOMUX_PIN, dirty=False),
        bins=("pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook"),
    )


def _elf_header(machine: int, *, little_endian: bool = True) -> bytes:
    """A 20-byte ELF prologue: e_ident, e_type, then e_machine."""
    data = 1 if little_endian else 2
    ident = b"\x7fELF" + bytes([2, data, 1, 0]) + b"\x00" * 8
    return ident + (2).to_bytes(2, "little") + machine.to_bytes(2, "little")


class ArchSwitchTests(unittest.TestCase):
    """HARBOR_ARCH is the single switch the whole eval plane agrees on."""

    def test_explicit_value_wins_and_aliases_are_refused(self) -> None:
        self.assertEqual(resolve_arch("amd64"), "amd64")
        self.assertEqual(resolve_arch("arm64"), "arm64")
        for bad in ("x86_64", "aarch64", "x64", "AMD64", "", "i386", "linux/amd64"):
            with self.subTest(value=bad):
                with self.assertRaises(ArchError):
                    require_arch(bad)

    def test_env_selects_the_arch_and_rejects_anything_else(self) -> None:
        self.assertEqual(resolve_arch(env={"HARBOR_ARCH": "arm64"}), "arm64")
        self.assertEqual(resolve_arch(env={"HARBOR_ARCH": " amd64 "}), "amd64")
        with self.assertRaises(ArchError):
            resolve_arch(env={"HARBOR_ARCH": "x86_64"})

    def test_each_arch_maps_to_one_platform_tarball_and_machine(self) -> None:
        self.assertEqual(ARCHES, ("amd64", "arm64"))
        self.assertEqual(DOCKER_PLATFORMS["amd64"], "linux/amd64")
        self.assertEqual(DOCKER_PLATFORMS["arm64"], "linux/arm64")
        self.assertEqual(NODE_ARCH_SUFFIXES["amd64"], "linux-x64")
        self.assertEqual(NODE_ARCH_SUFFIXES["arm64"], "linux-arm64")
        self.assertEqual(ELF_MACHINES["amd64"], 0x3E)
        self.assertEqual(ELF_MACHINES["arm64"], 0xB7)
        self.assertEqual(
            node_tarball_name("arm64"), f"node-v{NODE_VERSION}-linux-arm64.tar.gz"
        )
        self.assertEqual(node_sha256("arm64"), NODE_SHA256["arm64"])

    def test_artifact_roots_are_per_arch_with_no_flat_fallback(self) -> None:
        self.assertEqual(
            installed_root("amd64"), pathlib.Path(".plak/installed/amd64")
        )
        self.assertEqual(
            installed_root("arm64"), pathlib.Path(".plak/installed/arm64")
        )
        for name in ("rung-a.sh", "rung-b.sh", "smoke.sh", "harbor.sh"):
            script = (ROOT / "eval" / "scripts" / name).read_text()
            with self.subTest(script=name):
                self.assertNotIn(".plak/installed/plak", script)
                self.assertNotIn(".plak/installed/vendor", script)
                self.assertNotIn(".plak/installed/pmux", script)
        for name in ("terminal-bench.yaml", "rung-b-wire.yaml"):
            config = (ROOT / "eval" / name).read_text()
            with self.subTest(config=name):
                self.assertNotIn(".plak/installed/plak", config)

    def test_platform_record_lands_beside_the_other_agent_logs(self) -> None:
        command = platform_record_command("arm64")
        self.assertIn("/logs/agent/platform.txt", command)
        self.assertIn("linux/arm64", command)
        with self.assertRaises(ArchError):
            platform_record_command("x86_64")
        for module in ("agent.py", "pmux_agent.py", "vendor_agents.py"):
            source = (ROOT / "eval" / "harbor_adapter" / module).read_text()
            with self.subTest(module=module):
                self.assertIn("platform_record_command(self._arch)", source)


class ElfArchCheckTests(unittest.TestCase):
    """Architecture is part of an artifact's identity, so it fails closed."""

    def test_reads_e_machine_from_a_synthetic_header(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            amd = root / "amd"
            amd.write_bytes(_elf_header(0x3E) + b"\x00" * 64)
            arm = root / "arm"
            arm.write_bytes(_elf_header(0xB7) + b"\x00" * 64)
            self.assertEqual(read_elf_machine(amd), 0x3E)
            self.assertEqual(read_elf_machine(arm), 0xB7)
            require_elf_arch(amd, "amd64", "plak")
            require_elf_arch(arm, "arm64", "plak")

    def test_cross_built_and_non_elf_artifacts_are_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            arm = root / "arm"
            arm.write_bytes(_elf_header(0xB7) + b"\x00" * 64)
            with self.assertRaises(ArchError) as caught:
                require_elf_arch(arm, "amd64", "frozen plak")
            self.assertIn("linux/arm64", str(caught.exception))
            self.assertIn("linux/amd64", str(caught.exception))

            macho = root / "macho"
            macho.write_bytes(b"\xcf\xfa\xed\xfe" + b"\x00" * 64)
            with self.assertRaises(ArchError):
                require_elf_arch(macho, "arm64", "frozen plak")

            stub = root / "stub"
            stub.write_bytes(b"\x7fELF")
            with self.assertRaises(ArchError):
                require_elf_arch(stub, "arm64", "frozen plak")

    def test_pmux_companions_must_all_match_the_selected_arch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for name in ("pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook"):
                (root / name).write_bytes(_elf_header(0xB7) + b"\x00" * 64)
            _pin_pmux_manifest(root, "arm64")
            bins = require_frozen_pmux(root, "arm64")
            self.assertEqual(bins.arch, "arm64")
            with self.assertRaises(PmuxArtifactError):
                require_frozen_pmux(root, "amd64")
            (root / "pmux-hook").write_bytes(_elf_header(0x3E) + b"\x00" * 64)
            with self.assertRaises(PmuxArtifactError):
                require_frozen_pmux(root, "arm64")


class VendorManifestArchTests(unittest.TestCase):
    def _write(self, root: pathlib.Path, arch: str, *, node_arch: str | None = None) -> None:
        suffix = NODE_ARCH_SUFFIXES[node_arch or arch]
        tarball = f"node-v{NODE_VERSION}-{suffix}.tar.gz"
        (root / tarball).write_bytes(b"node")
        (root / "codex.tar.gz").write_bytes(b"codex")
        (root / "grok").write_bytes(_elf_header(ELF_MACHINES[arch]) + b"\x00" * 64)
        (root / "manifest.json").write_text(
            json.dumps(
                {
                    "arch": arch,
                    "node": {
                        "version": NODE_VERSION,
                        "file": tarball,
                        "sha256": NODE_SHA256[node_arch or arch],
                    },
                    "clis": {
                        "codex": {
                            "version": CLI_PINS["codex"],
                            "file": "codex.tar.gz",
                        },
                        "grok": {"version": CLI_PINS["grok"], "file": "grok"},
                    },
                }
            )
            + "\n"
        )

    def test_matching_manifest_is_accepted_for_both_arches(self) -> None:
        for arch in ARCHES:
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                self._write(root, arch)
                artifacts = require_frozen_artifacts("codex", root, arch)
                self.assertEqual(artifacts.arch, arch)
                self.assertEqual(
                    artifacts.node_tarball.name, node_tarball_name(arch)
                )
                self.assertEqual(
                    require_frozen_artifacts("grok", root, arch).arch, arch
                )

    def test_manifest_from_the_other_arch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._write(root, "amd64")
            with self.assertRaises(VendorArtifactError) as caught:
                require_frozen_artifacts("codex", root, "arm64")
            self.assertIn("arm64", str(caught.exception))

    def test_node_tarball_must_carry_the_matching_suffix(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            # An arm64 manifest that still names the x64 tarball is a mix-up,
            # not a pin: the cell would unpack an x86 Node on an ARM machine.
            self._write(root, "arm64", node_arch="amd64")
            with self.assertRaises(VendorArtifactError):
                require_frozen_artifacts("codex", root, "arm64")

    def test_grok_binary_machine_is_verified_against_the_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._write(root, "arm64")
            (root / "grok").write_bytes(_elf_header(0x3E) + b"\x00" * 64)
            with self.assertRaises(VendorArtifactError):
                require_frozen_artifacts("grok", root, "arm64")


class ArchShellHelperTests(unittest.TestCase):
    def _helper(self, call: str, env: dict[str, str] | None = None) -> str:
        script = ROOT / "eval" / "scripts" / "arch.sh"
        environ = dict(os.environ)
        environ.pop("HARBOR_ARCH", None)
        environ.update(env or {})
        result = subprocess.run(
            ["bash", "-c", f'source "{script}"; {call}'],
            capture_output=True,
            text=True,
            env=environ,
        )
        return result.stdout.strip(), result.returncode

    def test_env_value_is_honoured_and_anything_else_is_refused(self) -> None:
        self.assertEqual(
            self._helper("harbor_arch", {"HARBOR_ARCH": "arm64"}), ("arm64", 0)
        )
        self.assertEqual(
            self._helper("harbor_arch", {"HARBOR_ARCH": "amd64"}), ("amd64", 0)
        )
        _, code = self._helper("harbor_arch", {"HARBOR_ARCH": "x86_64"})
        self.assertEqual(code, 2)

    def test_only_arm_job_names_take_a_suffix(self) -> None:
        # amd64 job names keep their existing spelling so archived runs and
        # anything that reads them stay valid; ARM is the separate lane.
        self.assertEqual(self._helper("harbor_job_suffix amd64"), ("", 0))
        self.assertEqual(self._helper("harbor_job_suffix arm64"), ("-arm64", 0))
        self.assertEqual(
            self._helper("harbor_installed_root /repo arm64"),
            ("/repo/.plak/installed/arm64", 0),
        )
        self.assertEqual(
            self._helper("harbor_platform arm64"), ("linux/arm64", 0)
        )


JOBS = ROOT / ".plak" / "harbor" / "jobs"


def _events(*records: dict) -> list[dict]:
    return parse_session_events("\n".join(json.dumps(record) for record in records))


class FailureKindTests(unittest.TestCase):
    """The kind comes from the diagnostic plak actually prints."""

    def test_each_kind_is_named_from_a_real_diagnostic(self) -> None:
        cases = (
            (
                "plak: agent turn failed: context admission denied: protected "
                "model input and output reservation exceed context capacity",
                KIND_CONTEXT_CAPACITY,
            ),
            ("plak: native subscription credential is unavailable", KIND_CREDENTIAL),
            ("logical turn exceeded its model-operation limit", KIND_ROUND_LIMIT),
            (
                "OpenRouter Chat HTTP 429 (rejected-before-acceptance): "
                "Rate limit exceeded: free-models-per-day-high-balance.",
                KIND_PROVIDER_RATE_LIMIT,
            ),
            (
                "Responses provider returned a typed terminal error: Our servers "
                "are currently overloaded. \u00b7 provider code: server_is_overloaded",
                KIND_PROVIDER_REJECTION,
            ),
            (
                "agent turn requires recovery: mutating tool outcome requires "
                "exact journal reconciliation: joined process cleanup remains "
                "unresolved",
                KIND_TOOL_FAILURE,
            ),
            ("the sky turned an unusual colour", KIND_OTHER),
        )
        for text, kind in cases:
            with self.subTest(kind=kind):
                self.assertEqual(classify_failure_kind(text), kind)

    def test_a_rate_limit_is_not_reported_as_a_bare_rejection(self) -> None:
        # The 429 body also says "rejected-before-acceptance"; the rate limit
        # is the more specific answer and must win.
        text = "provider returned a rate-limit response; HTTP 429 rejected-before-acceptance"
        self.assertEqual(classify_failure_kind(text), KIND_PROVIDER_RATE_LIMIT)

    def test_declared_classes_cover_every_kind(self) -> None:
        self.assertIn(CLASS_COMPLETED, TERMINATION_CLASSES)
        self.assertIn(CLASS_AGENT_EXIT_NONZERO, TERMINATION_CLASSES)
        for kind in (
            KIND_CONTEXT_CAPACITY,
            KIND_CREDENTIAL,
            KIND_ROUND_LIMIT,
            KIND_PROVIDER_RATE_LIMIT,
            KIND_PROVIDER_REJECTION,
            KIND_TOOL_FAILURE,
            KIND_OTHER,
        ):
            self.assertIn(f"turn_failed:{kind}", TERMINATION_CLASSES)


class PlakTerminationTests(unittest.TestCase):
    def test_exit_zero_is_completed_even_with_rejected_tools(self) -> None:
        events = _events(
            {"kind": "tool.started", "seq": 1},
            {"kind": "tool.rejected", "name": "list_dir", "reason": "InvalidArguments"},
            {"kind": "turn.completed", "seq": 3},
        )
        record = classify_plak(exit_code=0, stderr="", events=events)
        self.assertEqual(record.termination_class, CLASS_COMPLETED)
        self.assertFalse(record.is_harness_termination)
        # A rejected tool is still counted; it just did not end the trial.
        self.assertEqual(record.rejection_count, 1)

    def test_context_admission_refusal_reads_from_stderr(self) -> None:
        # This death prints to stderr and writes no terminal session record,
        # so a JSONL-only classifier would call it a bare nonzero exit.
        stderr = (
            "plak: tool running: process_session_start\n"
            "plak: tool done: process_session_start\n"
            "plak: tool-grown context overflow contains only protected material "
            "and cannot be compacted\n"
            "plak: agent turn failed: context admission denied: protected model "
            "input and output reservation exceed context capacity\n"
        )
        record = classify_plak(exit_code=5, stderr=stderr, events=_events())
        self.assertEqual(
            record.termination_class, f"turn_failed:{KIND_CONTEXT_CAPACITY}"
        )
        self.assertIn("context admission denied", record.detail)

    def test_session_turn_failure_beats_stderr(self) -> None:
        events = _events(
            {
                "kind": "turn.failed",
                "disposition": "definite",
                "msg": "the provider returned a rate-limit response; HTTP 429",
            }
        )
        record = classify_plak(
            exit_code=5, stderr="plak: turn failed definitively", events=events
        )
        self.assertEqual(
            record.termination_class, f"turn_failed:{KIND_PROVIDER_RATE_LIMIT}"
        )
        self.assertEqual(record.disposition, "definite")

    def test_recovery_required_is_a_tool_failure(self) -> None:
        events = _events(
            {
                "kind": "turn.recovery_required",
                "disposition": "recovery_required",
                "msg": (
                    "agent turn requires recovery: local composition host or cell "
                    "outcome requires exact recovery"
                ),
            }
        )
        record = classify_plak(exit_code=5, stderr="", events=events)
        self.assertEqual(record.termination_class, f"turn_failed:{KIND_TOOL_FAILURE}")

    def test_unexplained_nonzero_exit_is_not_dressed_up_as_a_turn_failure(self) -> None:
        record = classify_plak(exit_code=2, stderr="plak: usage: plak run", events=[])
        self.assertEqual(record.termination_class, CLASS_AGENT_EXIT_NONZERO)

    def test_harness_exception_names_itself(self) -> None:
        record = classify_plak(
            exit_code=None,
            stderr="",
            exception_type="RuntimeError",
            exception_message="Docker compose command failed for environment fix-git",
        )
        self.assertEqual(record.termination_class, "harness_exception:RuntimeError")

    def test_typed_provider_exception_is_a_turn_failure(self) -> None:
        record = classify_plak(
            exit_code=None,
            stderr="",
            exception_type="ApiRateLimitError",
            exception_message="rate limit exceeded",
        )
        self.assertEqual(
            record.termination_class, "harness_exception:ApiRateLimitError"
        )

    def test_no_evidence_at_all_is_unknown_not_completed(self) -> None:
        record = classify_plak(exit_code=None, stderr="", events=[])
        self.assertEqual(record.termination_class, CLASS_UNKNOWN)

    def test_tool_rejections_group_by_tool_and_reason(self) -> None:
        events = _events(
            {"kind": "tool.rejected", "name": "list_dir", "reason": "InvalidArguments"},
            {"kind": "tool.rejected", "name": "list_dir", "reason": "InvalidArguments"},
            {"kind": "tool.rejected", "name": "exec_command", "reason": "Denied"},
        )
        rejections = tool_rejections(events)
        self.assertEqual(
            [(item.tool, item.reason, item.count) for item in rejections],
            [("exec_command", "Denied", 1), ("list_dir", "InvalidArguments", 2)],
        )

    def test_exit_code_is_read_from_the_recorded_file(self) -> None:
        self.assertEqual(parse_exit_code("5\n"), 5)
        self.assertIsNone(parse_exit_code(""))
        self.assertIsNone(parse_exit_code("not a number\n"))


class VendorTerminationTests(unittest.TestCase):
    def test_clean_vendor_run_is_completed(self) -> None:
        log = '{"type":"turn.completed","usage":{"input_tokens":10}}\n'
        record = classify_vendor(harness="codex", log_text=log)
        self.assertEqual(record.termination_class, CLASS_COMPLETED)

    def test_transcript_ending_in_a_typed_error_is_a_turn_failure(self) -> None:
        # Codex exited 0 here, but its last typed record is a reconnect error,
        # so the turn never finished however the process ended.
        log = (
            '{"type":"item.completed","item":{"type":"agent_message"}}\n'
            '{"type":"error","message":"Reconnecting... waiting for network '
            '(Connection failed: error sending request)"}\n'
        )
        record = classify_vendor(harness="codex", log_text=log)
        self.assertEqual(
            record.termination_class, f"turn_failed:{KIND_PROVIDER_REJECTION}"
        )

    def test_nonzero_exit_without_typed_evidence_stays_agent_exit_nonzero(self) -> None:
        record = classify_vendor(
            harness="pmux",
            log_text="pmux: the daemon went away",
            exception_type="NonZeroAgentExitCodeError",
            exception_message="Command failed (exit 1): set -euo pipefail",
        )
        self.assertEqual(record.termination_class, CLASS_AGENT_EXIT_NONZERO)

    def test_agent_timeout_is_a_harness_exception_not_a_provider_refusal(self) -> None:
        record = classify_vendor(
            harness="opencode",
            log_text="",
            exception_type="AgentTimeoutError",
            exception_message="Agent execution timed out after 900.0 seconds",
        )
        self.assertEqual(
            record.termination_class, "harness_exception:AgentTimeoutError"
        )


class ArchivedJobTreeTests(unittest.TestCase):
    """The classifier reads job trees recorded before it existed."""

    def _job(self, relative: str) -> pathlib.Path:
        path = JOBS / relative
        if not path.is_dir():
            raise unittest.SkipTest(f"no archived job tree at {path}")
        return path

    def test_openrouter_sqlite_trial_is_a_context_capacity_death(self) -> None:
        job = self._job("ab-plak-openrouter/2026-09-15__18-39-18")
        summary = summarize_job(job)
        by_task = {trial.task: trial for trial in summary.trials}
        trial = by_task["terminal-bench/sqlite-db-truncate"]
        self.assertEqual(trial.reward, 0.0)
        self.assertEqual(
            trial.record.termination_class, f"turn_failed:{KIND_CONTEXT_CAPACITY}"
        )
        self.assertEqual(summary.harness_terminated, 1)
        self.assertEqual(len(summary.trials), 8)

    def test_grok_credential_run_names_six_harness_deaths(self) -> None:
        job = self._job("ab-plak-grok/2026-09-15__23-22-54")
        summary = summarize_job(job)
        self.assertEqual(
            summary.class_counts.get(f"turn_failed:{KIND_CREDENTIAL}"), 6
        )

    def test_clean_grok_plumbing_run_has_no_harness_deaths(self) -> None:
        job = self._job("plumbing-grok-build/2026-09-15__18-24-18")
        summary = summarize_job(job)
        self.assertEqual(summary.harness_terminated, 0)
        self.assertEqual(summary.class_counts, {CLASS_COMPLETED: 3})

    def test_openrouter_arm64_rate_limit_run(self) -> None:
        job = self._job("plumbing-plak-openrouter-arm64/2026-09-17__13-16-04")
        summary = summarize_job(job)
        self.assertEqual(
            summary.class_counts, {f"turn_failed:{KIND_PROVIDER_RATE_LIMIT}": 3}
        )
        self.assertEqual(summary.rewarded, 0)

    def test_a_job_parent_is_not_mistaken_for_a_one_trial_job(self) -> None:
        # Job directories carry their own result.json, so the parent of a
        # timestamped tree must not read as a single trial named by date.
        root = JOBS / "ab-plak-openrouter"
        if not root.is_dir():
            raise unittest.SkipTest("no archived job trees")
        jobs = find_job_dirs(root)
        self.assertEqual([path.parent.name for path in jobs], ["ab-plak-openrouter"])

    def test_table_names_every_column_the_maintainer_reads(self) -> None:
        job = self._job("ab-plak-openrouter/2026-09-15__18-39-18")
        table = render_table(summarize_job(job))
        for header in ("task", "reward", "termination", "rejections", "dur(s)"):
            self.assertIn(header, table)
        self.assertIn(f"turn_failed:{KIND_CONTEXT_CAPACITY}", table)
        self.assertIn("harness-terminated=1", table)

    def test_every_archived_trial_classifies(self) -> None:
        if not JOBS.is_dir():
            raise unittest.SkipTest("no archived job trees")
        jobs = find_job_dirs(JOBS)
        if not jobs:
            raise unittest.SkipTest("no archived job trees")
        for job in jobs:
            for result in sorted(job.glob("*/result.json")):
                record = classify_trial_dir(result.parent)
                with self.subTest(trial=str(result.parent)):
                    self.assertNotEqual(record.termination_class, CLASS_UNKNOWN)
                    self.assertIn(record.harness, ("plak", "codex", "grok-build", "pi", "opencode", "pmux"))


class PromptVariantTests(unittest.TestCase):
    def test_variant_env_is_named_once(self) -> None:
        self.assertEqual(PROMPT_VARIANT_ENV, "PLAK_PROMPT_VARIANT")
        self.assertEqual(PROMPT_VARIANTS, ("legacy", "parity"))

    def test_python_and_rust_name_the_same_switch(self) -> None:
        # The eval plane sets this variable; the Rust side reads it. A rename
        # on either side must fail here rather than in a paid Harbor run.
        switch = ROOT / "crates" / "harness" / "prompts" / "src" / "bases" / "variant.rs"
        if not switch.is_file():
            raise unittest.SkipTest("prompt variant switch not in tree yet")
        text = switch.read_text()
        self.assertIn(f'PLAK_PROMPT_VARIANT_ENV: &str = "{PROMPT_VARIANT_ENV}"', text)
        for variant in PROMPT_VARIANTS:
            with self.subTest(variant=variant):
                self.assertIn(f'&str = "{variant}"', text)

    def test_no_variant_leaves_the_env_untouched(self) -> None:
        self.assertNotIn(PROMPT_VARIANT_ENV, run_env())

    def test_each_variant_reaches_the_plak_binary(self) -> None:
        for variant in PROMPT_VARIANTS:
            with self.subTest(variant=variant):
                self.assertEqual(run_env(prompt_variant=variant)[PROMPT_VARIANT_ENV], variant)

    def test_unknown_variant_fails_closed(self) -> None:
        with self.assertRaises(ArgumentError):
            require_prompt_variant("parity-v2")
        with self.assertRaises(ArgumentError):
            run_env(prompt_variant="")

    def test_ablation_arms_exist_and_carry_the_variant(self) -> None:
        rung_a = (ROOT / "eval" / "scripts" / "rung-a.sh").read_text()
        for arm in (
            "plak-codex-legacy",
            "plak-codex-parity",
            "plak-grok-legacy",
            "plak-grok-parity",
            "plak-claude-legacy",
            "plak-claude-parity",
        ):
            with self.subTest(arm=arm):
                self.assertIn(f"{arm})", rung_a)
        self.assertIn('prompt_variant=$variant', rung_a)
        self.assertIn("prompt_variant_kwargs", rung_a)
        # The job name carries the variant, so the arms never share a tree.
        self.assertIn('${slice}-${named}${job_suffix}', rung_a)
        # Vendor arms must not learn about the switch.
        vendor = rung_a[rung_a.index("run_vendor() {"):]
        vendor = vendor[: vendor.index("\n}\n")]
        self.assertNotIn("prompt_variant", vendor)

    def test_ablation_comparison_plans_are_digest_shaped(self) -> None:
        expected = {
            "rung-a-ab-codex-luna-low-prompt-ablation.json": (
                "plak-codex-legacy",
                "plak-codex-parity",
            ),
            "rung-a-ab-grok-46-low-prompt-ablation.json": (
                "plak-grok-legacy",
                "plak-grok-parity",
            ),
            "rung-a-ab-claude-opus55-low-prompt-ablation.json": (
                "plak-claude-legacy",
                "plak-claude-parity",
            ),
        }
        for name, arms in expected.items():
            with self.subTest(plan=name):
                path = ROOT / "eval" / "comparisons" / name
                plan = json.loads(path.read_text())
                validate_plan(plan, require_jobs=False)
                self.assertEqual(tuple(sorted(plan["arms"])), tuple(sorted(arms)))
                self.assertEqual(plan["dataset"], "terminal-bench/terminal-bench-2-1@6")
                self.assertEqual(
                    [task["name"] for task in plan["tasks"]], list(RUNG_A_AB_TASKS)
                )
                with self.assertRaises(PlanError):
                    validate_plan(plan, require_jobs=True)


class TerminationSurfaceTests(unittest.TestCase):
    def test_adapter_writes_a_record_next_to_the_agent_logs(self) -> None:
        agent = (ROOT / "eval" / "harbor_adapter" / "agent.py").read_text()
        self.assertIn('"plak-termination.json"', agent)
        self.assertIn("_record_termination", agent)
        self.assertIn("classify_plak", agent)
        # Written on every path, including the one that raises.
        self.assertIn("finally:", agent)
        self.assertIn("await self._record_termination(environment, env, exception_type)", agent)
        self.assertIn('"harness_termination"', agent)

    def test_summary_script_and_recipe_exist(self) -> None:
        script = ROOT / "eval" / "scripts" / "summarize.py"
        self.assertTrue(script.is_file())
        self.assertTrue(os.access(script, os.X_OK))
        text = script.read_text()
        self.assertIn("--write", text)
        self.assertIn("--json", text)
        self.assertIn("termination-summary.json", text)
        justfile = (ROOT / "Justfile").read_text()
        self.assertIn("harbor-summarize", justfile)
        self.assertIn("eval/scripts/summarize.py", justfile)
        rung_a = (ROOT / "eval" / "scripts" / "rung-a.sh").read_text()
        self.assertIn("summarize_jobs", rung_a)

    def test_readme_documents_the_classes_and_the_variant(self) -> None:
        readme = (ROOT / "eval" / "README.md").read_text()
        self.assertIn("PLAK_PROMPT_VARIANT", readme)
        self.assertIn("plak-codex-parity", readme)
        self.assertIn("turn_failed:", readme)
        self.assertIn("eval/scripts/summarize.py", readme)
        for kind in (
            KIND_CONTEXT_CAPACITY,
            KIND_CREDENTIAL,
            KIND_ROUND_LIMIT,
            KIND_PROVIDER_RATE_LIMIT,
            KIND_PROVIDER_REJECTION,
            KIND_TOOL_FAILURE,
        ):
            with self.subTest(kind=kind):
                self.assertIn(kind, readme)

    def test_summary_script_counts_current_layout_tool_rejections(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            job = pathlib.Path(directory) / "job"
            agent_dir = job / "trial__one" / "agent"
            events_dir = agent_dir / "plak-home" / "state" / "current" / "sessions"
            events_dir.mkdir(parents=True)
            (agent_dir / "plak-exit.txt").write_text("5\n")
            (agent_dir / "plak-stderr.log").write_text(
                "plak: native subscription credential is unavailable\n"
            )
            (events_dir / "session.events.jsonl").write_text(
                json.dumps({"kind": "tool.rejected", "name": "list_dir", "reason": "InvalidArguments"}) + "\n"
            )
            (job / "trial__one" / "result.json").write_text(json.dumps({
                "task_name": "trial", "verifier_result": {"rewards": {"reward": 0}},
            }))
            summary = summarize_job(job)
            self.assertEqual(summary.class_counts, {f"turn_failed:{KIND_CREDENTIAL}": 1})
            self.assertEqual(summary.rejection_counts, {"list_dir:InvalidArguments": 1})
            result = subprocess.run(
                [sys.executable, str(ROOT / "eval" / "scripts" / "summarize.py"), str(job)],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("turn_failed:credential", result.stdout)
            self.assertIn("list_dir:InvalidArguments", result.stdout)

    def test_summary_gate_rejects_unparseable_and_incomplete_trials(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            job = pathlib.Path(directory) / "job"
            trial = job / "task__one"
            (trial / "agent").mkdir(parents=True)
            result_path = trial / "result.json"
            result_path.write_text("{invalid")
            command = [
                sys.executable, str(ROOT / "eval" / "scripts" / "summarize.py"),
                "--require-readable", "--write", "--expect-trials", "1", str(job),
            ]
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("unreadable trial results", result.stderr)
            result_path.write_text("{}")
            result = subprocess.run(
                command[:4] + ["--expect-trials", "2", str(job)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("expected 2 trials, found 1", result.stderr)
            self.assertTrue((job / "termination-summary.json").is_file())
            invocation = json.loads((job / "termination-invocation.json").read_text())
            self.assertEqual(invocation["status"], "incomplete")
            self.assertEqual((invocation["expected_trials"], invocation["found_trials"]), (2, 1))


class ScrubbedResultTests(unittest.TestCase):
    """Harbor's `--ae` scrub corrupts result.json; the reader must not fail open."""

    _SCRUBBED = """{
    "task_name": "plak/fix-typo",
    "config": {"environment": {"force_build": [REDACTED], "delete": [REDACTED]}},
    "agent_result": {"n_input_tokens": 52339, "n_output_tokens": 321},
    "verifier_result": {"rewards": {"reward": 1.0}},
    "exception_info": null,
    "started_at": "2026-09-15T18:21:46Z",
    "finished_at": "2026-09-15T18:23:26Z"
}
"""

    def _trial(self, tmp: str, result_text: str) -> pathlib.Path:
        trial = pathlib.Path(tmp) / "fix-typo__abc"
        (trial / "agent").mkdir(parents=True)
        (trial / "agent" / "codex.txt").write_text(
            '{"type":"turn.completed","usage":{}}\n'
        )
        (trial / "result.json").write_text(result_text)
        return trial

    def test_true_in_value_position_is_repaired(self) -> None:
        repaired = repair_scrubbed_booleans(self._SCRUBBED)
        loaded = json.loads(repaired)
        self.assertIs(loaded["config"]["environment"]["force_build"], True)
        self.assertIs(loaded["config"]["environment"]["delete"], True)

    def test_a_redacted_token_inside_a_string_is_left_alone(self) -> None:
        # Only a bare token is a scrubbed boolean. A quoted one is a real
        # redaction of a real secret and parses fine already.
        text = '{"msg": "[REDACTED] inside", "ok": [REDACTED]}'
        self.assertEqual(
            json.loads(repair_scrubbed_booleans(text)),
            {"msg": "[REDACTED] inside", "ok": True},
        )

    def test_reader_reports_the_repair_rather_than_hiding_it(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "result.json"
            path.write_text(self._SCRUBBED)
            loaded = load_json(path)
            self.assertEqual(loaded.status, RESULT_REPAIRED)
            self.assertTrue(loaded.ok)
            self.assertEqual(loaded.data["task_name"], "plak/fix-typo")

    def test_a_clean_file_is_not_marked_repaired(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "result.json"
            path.write_text(json.dumps({"task_name": "plak/sum-file"}))
            loaded = load_json(path)
            self.assertEqual(loaded.status, RESULT_OK)

    def test_an_unrepairable_file_never_fails_open(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "result.json"
            path.write_text("{ this is not json at all")
            loaded = load_json(path)
            self.assertEqual(loaded.status, RESULT_UNPARSEABLE)
            self.assertFalse(loaded.ok)
            self.assertEqual(loaded.data, {})

    def test_an_unreadable_trial_is_not_reported_as_completed(self) -> None:
        # This is the defect: a dead vendor trial whose record would not parse
        # inherited `completed` from an empty dict and read like a pass.
        with tempfile.TemporaryDirectory() as tmp:
            trial = self._trial(tmp, "{ not json")
            record = classify_trial_dir(trial)
            self.assertEqual(record.termination_class, CLASS_RESULT_UNPARSEABLE)
            self.assertEqual(record.result_status, RESULT_UNPARSEABLE)
            self.assertTrue(record.is_harness_termination)

    def test_a_repaired_trial_keeps_its_reward_and_tokens(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            trial = self._trial(tmp, self._SCRUBBED)
            summary = summarize_trial(trial)
            self.assertEqual(summary.task, "plak/fix-typo")
            self.assertEqual(summary.reward, 1.0)
            self.assertEqual(summary.input_tokens, 52339)
            self.assertEqual(summary.output_tokens, 321)
            # Old archives with only whole-trial timestamps cannot prove agent time.
            self.assertIsNone(summary.duration_s)
            self.assertEqual(summary.record.result_status, RESULT_REPAIRED)
            self.assertEqual(summary.record.termination_class, CLASS_COMPLETED)
            # The repair is visible wherever the class is printed.
            self.assertEqual(summary.record.label, "completed [repaired]")

    def test_archived_vendor_jobs_recover_their_rewards(self) -> None:
        job = JOBS / "plumbing-codex" / "2026-09-15__18-21-46"
        if not job.is_dir():
            raise unittest.SkipTest("no archived codex plumbing job")
        summary = summarize_job(job)
        # Harbor's own console recorded Mean 1.000 for this job; the reader
        # used to print three reward `-` rows with 0/0 tokens.
        self.assertEqual(summary.rewarded, 3)
        self.assertEqual(summary.unreadable_results, 3)
        self.assertEqual(summary.result_status_counts, {RESULT_REPAIRED: 3})
        for trial in summary.trials:
            with self.subTest(trial=trial.trial):
                self.assertEqual(trial.reward, 1.0)
                self.assertGreater(trial.input_tokens, 0)
        self.assertIn("result.json not clean", render_table(summary))


class ScrubSafeVendorEnvTests(unittest.TestCase):
    """No eval script may make a JSON literal a Harbor secret."""

    # `--ae NAME=<value>` makes <value> a secret that Harbor string-replaces
    # out of the whole trial directory. These values also occur as bare JSON
    # literals in result.json, so passing one destroys the trial record.
    _UNSAFE = ("true", "false", "null", "0", "1")

    def _scripts(self) -> list[pathlib.Path]:
        return sorted((ROOT / "eval" / "scripts").glob("*.sh"))

    def test_every_eval_script_is_covered(self) -> None:
        # No script is exempt from the rule; a new one joins automatically.
        names = {path.name for path in self._scripts()}
        for required in ("rung-a.sh", "rung-b.sh", "smoke.sh", "harbor.sh"):
            with self.subTest(script=required):
                self.assertIn(required, names)

    def test_no_script_passes_a_json_literal_as_an_agent_env_secret(self) -> None:
        self.assertTrue(self._scripts())
        for script in self._scripts():
            text = script.read_text()
            for line in text.splitlines():
                stripped = line.strip()
                if stripped.startswith("#") or "--ae " not in stripped:
                    continue
                for value in self._UNSAFE:
                    with self.subTest(script=script.name, value=value):
                        self.assertNotIn(f"--ae {value}", stripped)
                        self.assertFalse(
                            stripped.split("--ae ", 1)[1].split()[0].endswith(f"={value}"),
                            f"{script.name}: --ae with the JSON literal {value!r} "
                            "corrupts every result.json in the job",
                        )

    def test_vendor_arms_still_export_the_flag(self) -> None:
        # Dropping --ae is only safe because Harbor's env fallback reads
        # os.environ, which the exported value reaches.
        for name, flag in (
            ("smoke.sh", "CODEX_FORCE_AUTH_JSON"),
            ("smoke.sh", "GROK_FORCE_AUTH_JSON"),
            ("rung-b.sh", "CODEX_FORCE_AUTH_JSON"),
            ("rung-a.sh", "CODEX_FORCE_AUTH_JSON"),
            ("rung-a.sh", "GROK_FORCE_AUTH_JSON"),
        ):
            with self.subTest(script=name, flag=flag):
                text = (ROOT / "eval" / "scripts" / name).read_text()
                self.assertIn(f"export {flag}=true", text)


class PmuxConcurrencyTests(unittest.TestCase):
    """One FrozenPmux trial at a time: the Claude refresh token is single-use."""

    def test_claude_code_arm_pins_concurrency_to_one(self) -> None:
        rung_a = (ROOT / "eval" / "scripts" / "rung-a.sh").read_text()
        self.assertIn("concurrent=1", rung_a)
        # run_pmux must not inherit the slice default.
        body = rung_a[rung_a.index("run_pmux() {"):]
        body = body[: body.index("\n}\n")]
        self.assertNotIn('concurrent="$(default_concurrent)"', body)
        self.assertIn("--n-concurrent 1", body)

    def test_no_other_script_fans_frozen_pmux_out(self) -> None:
        for name in ("smoke.sh", "rung-b.sh"):
            with self.subTest(script=name):
                text = (ROOT / "eval" / "scripts" / name).read_text()
                self.assertNotIn("FrozenPmux", text)

    def test_readme_states_the_rule(self) -> None:
        readme = (ROOT / "eval" / "README.md").read_text()
        self.assertIn("single-use", readme)
        self.assertIn("NeedsLogin", readme)
        self.assertIn("result_unparseable", readme)
        self.assertIn("[REDACTED]", readme)


def _scratch_git_repo(root: pathlib.Path) -> str:
    """A throwaway checkout in TMPDIR. Never the repo under test."""
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@example.invalid",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@example.invalid",
        "GIT_CONFIG_GLOBAL": str(root / "nonexistent-gitconfig"),
        "GIT_CONFIG_SYSTEM": str(root / "nonexistent-gitconfig"),
    }

    def run(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["git", "-C", str(root), *args],
            capture_output=True,
            text=True,
            env=env,
            check=True,
        )

    run("init", "-q")
    (root / "Cargo.toml").write_text("[workspace]\n")
    run("add", "-A")
    run("commit", "-qm", "seed")
    return run("rev-parse", "HEAD").stdout.strip()


class PmuxProvenanceTests(unittest.TestCase):
    """Frozen pmux binaries must name the pseudomux commit they came from."""

    BINS = ("pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook")

    def setUp(self) -> None:
        for name in (DIRTY_OVERRIDE_ENV, UNPINNED_OVERRIDE_ENV):
            self.addCleanup(os.environ.pop, name, None)
            os.environ.pop(name, None)

    def _pinned(self, root: pathlib.Path, **overrides: object) -> pathlib.Path:
        source = SourceCommit(
            commit=str(overrides.get("commit", PSEUDOMUX_PIN)),
            dirty=bool(overrides.get("dirty", False)),
        )
        record_provenance(
            root,
            arch=str(overrides.get("arch", "arm64")),
            source=source,
            bins=self.BINS,
        )
        return root

    def test_the_pin_is_written_once_and_the_short_form_derived(self) -> None:
        self.assertEqual(len(PSEUDOMUX_PIN), 40)
        self.assertEqual(PSEUDOMUX_PIN_SHORT, PSEUDOMUX_PIN[:7])
        # The full pin appears in exactly one source file.
        hits = [
            path
            for path in (ROOT / "eval").rglob("*.py")
            if ".venv" not in path.parts and PSEUDOMUX_PIN in path.read_text()
        ]
        self.assertEqual([path.name for path in hits], ["pmux_provenance.py"])

    def test_manifest_records_arch_commit_and_claude_pin(self) -> None:
        manifest = build_manifest(
            arch="arm64",
            source=SourceCommit(commit=PSEUDOMUX_PIN, dirty=False),
            bins=self.BINS,
        )
        self.assertEqual(manifest["arch"], "arm64")
        self.assertEqual(manifest["source"]["commit"], PSEUDOMUX_PIN)
        self.assertEqual(manifest["source"]["repo"], "pseudomux")
        self.assertIs(manifest["source"]["dirty"], False)
        self.assertEqual(manifest["claude_code"], CLI_PINS["claude"])
        self.assertEqual(manifest["bins"], list(self.BINS))

    def test_missing_manifest_names_the_fetch_recipe(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            with self.assertRaises(PmuxProvenanceError) as caught:
                read_manifest(root)
            self.assertIn("just fetch-harbor-pmux", str(caught.exception))
            self.assertIn(MANIFEST_NAME, str(manifest_path(root)))

    def test_pinned_provenance_round_trips(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self._pinned(pathlib.Path(tmp))
            source = require_pinned_provenance(root, arch="arm64")
            self.assertEqual(source.commit, PSEUDOMUX_PIN)
            self.assertFalse(source.dirty)

    def test_a_different_commit_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self._pinned(pathlib.Path(tmp), commit="b" * 40)
            with self.assertRaises(PmuxProvenanceError) as caught:
                require_pinned_provenance(root, arch="arm64")
            message = str(caught.exception)
            self.assertIn(PSEUDOMUX_PIN_SHORT, message)
            self.assertIn("bbbbbbb", message)

    def test_a_dirty_build_never_passes_as_its_commit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self._pinned(pathlib.Path(tmp), dirty=True)
            with self.assertRaises(PmuxProvenanceError) as caught:
                require_pinned_provenance(root, arch="arm64")
            self.assertIn("dirty", str(caught.exception))

    def test_the_other_architecture_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self._pinned(pathlib.Path(tmp), arch="amd64")
            with self.assertRaises(PmuxProvenanceError):
                require_pinned_provenance(root, arch="arm64")

    def test_off_pin_needs_a_named_override(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self._pinned(pathlib.Path(tmp), commit="c" * 40, dirty=True)
            with self.assertRaises(PmuxProvenanceError):
                require_pinned_provenance(root, arch="arm64")
            os.environ[UNPINNED_OVERRIDE_ENV] = "1"
            source = require_pinned_provenance(root, arch="arm64")
            self.assertTrue(source.dirty)

    def test_head_is_read_from_a_real_checkout(self) -> None:
        if not shutil.which("git"):
            raise unittest.SkipTest("git is not available")
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp) / "pseudomux"
            root.mkdir()
            head = _scratch_git_repo(root)
            source = resolve_source_commit(root)
            self.assertEqual(source.commit, head)
            self.assertFalse(source.dirty)

    def test_a_dirty_checkout_refuses_to_build(self) -> None:
        if not shutil.which("git"):
            raise unittest.SkipTest("git is not available")
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp) / "pseudomux"
            root.mkdir()
            _scratch_git_repo(root)
            (root / "Cargo.toml").write_text("[workspace]\n# edited\n")
            with self.assertRaises(PmuxProvenanceError) as caught:
                resolve_source_commit(root)
            self.assertIn(DIRTY_OVERRIDE_ENV, str(caught.exception))
            # The override is explicit, and the result says it was used.
            os.environ[DIRTY_OVERRIDE_ENV] = "1"
            source = resolve_source_commit(root)
            self.assertTrue(source.dirty)
            self.assertEqual(source.override, "1")

    def test_an_untracked_file_counts_as_dirty(self) -> None:
        if not shutil.which("git"):
            raise unittest.SkipTest("git is not available")
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp) / "pseudomux"
            root.mkdir()
            _scratch_git_repo(root)
            (root / "stray.rs").write_text("fn main() {}\n")
            with self.assertRaises(PmuxProvenanceError):
                resolve_source_commit(root)

    def test_a_non_checkout_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(PmuxProvenanceError) as caught:
                resolve_source_commit(pathlib.Path(tmp))
            self.assertIn("git checkout", str(caught.exception))

    def test_readme_quotes_the_pin_and_the_overrides(self) -> None:
        readme = (ROOT / "eval" / "README.md").read_text()
        self.assertIn(PSEUDOMUX_PIN_SHORT, readme)
        self.assertIn(DIRTY_OVERRIDE_ENV, readme)
        self.assertIn(UNPINNED_OVERRIDE_ENV, readme)
        self.assertIn("pmux/manifest.json", readme)


class AgentInitFailureTests(unittest.TestCase):
    """A pre-dispatch refusal is neither a failed turn nor an unexplained exit."""

    # The stderr shape of a session-creation refusal, recovered only through
    # the stderr fallback.
    STDERR = (
        "plak: agent could not be initialized (Revalidation): "
        "durable runtime revalidation rejected the rehydrated checkpoint\n"
    )

    def test_the_live_stderr_is_classified_as_an_init_failure(self) -> None:
        record = classify_plak(exit_code=5, stderr=self.STDERR, events=[])
        self.assertEqual(record.termination_class, "agent_init_failed:Revalidation")
        self.assertTrue(record.is_harness_termination)
        self.assertIn(INIT_FAILURE_MARKER, record.detail)

    def test_it_is_not_reported_as_a_bare_nonzero_exit(self) -> None:
        record = classify_plak(exit_code=5, stderr=self.STDERR, events=[])
        self.assertNotEqual(record.termination_class, CLASS_AGENT_EXIT_NONZERO)

    def test_it_is_not_reported_as_a_turn_failure(self) -> None:
        # No session was opened, so calling this a failed turn would be wrong.
        record = classify_plak(exit_code=5, stderr=self.STDERR, events=[])
        self.assertFalse(record.termination_class.startswith("turn_failed"))

    def test_an_empty_session_jsonl_does_not_change_the_answer(self) -> None:
        # The live trial's session JSONL was zero bytes.
        record = classify_plak(
            exit_code=5,
            stderr=self.STDERR,
            events=parse_session_events(""),
            session_ids=("6e796112-63bb-4490-8690-c9a5fb89d38f",),
        )
        self.assertEqual(record.termination_class, "agent_init_failed:Revalidation")

    def test_other_kinds_keep_their_own_name(self) -> None:
        for kind in ("Revalidation", "Rehydration", "Storage"):
            with self.subTest(kind=kind):
                stderr = f"plak: agent could not be initialized ({kind}): boom\n"
                self.assertEqual(initialization_kind(stderr), kind)
                record = classify_plak(exit_code=5, stderr=stderr, events=[])
                self.assertEqual(record.termination_class, f"agent_init_failed:{kind}")

    def test_an_unparenthesised_refusal_still_classifies(self) -> None:
        stderr = "plak: agent could not be initialized: no detail\n"
        self.assertEqual(initialization_kind(stderr), "")
        record = classify_plak(exit_code=5, stderr=stderr, events=[])
        self.assertEqual(record.termination_class, CLASS_AGENT_INIT_FAILED)

    def test_a_clean_exit_is_never_an_init_failure(self) -> None:
        record = classify_plak(exit_code=0, stderr=self.STDERR, events=[])
        self.assertEqual(record.termination_class, CLASS_COMPLETED)

    def test_unrelated_stderr_is_not_matched(self) -> None:
        self.assertIsNone(initialization_kind("plak: tool running: exec_command"))
        self.assertIsNone(initialization_kind(""))

    def test_the_residual_still_exists(self) -> None:
        # agent_exit_nonzero must stay reachable, just narrower.
        record = classify_plak(exit_code=2, stderr="plak: usage: plak run", events=[])
        self.assertEqual(record.termination_class, CLASS_AGENT_EXIT_NONZERO)

    def test_class_helper_round_trips(self) -> None:
        self.assertEqual(agent_init_class("Revalidation"), "agent_init_failed:Revalidation")
        self.assertEqual(agent_init_class(""), CLASS_AGENT_INIT_FAILED)
        self.assertEqual(agent_init_class(None), CLASS_AGENT_INIT_FAILED)

    def test_summarize_renders_and_counts_the_class(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            trial = pathlib.Path(tmp) / "job" / "regex-log__abc"
            (trial / "agent").mkdir(parents=True)
            (trial / "agent" / "plak-exit.txt").write_text("5\n")
            (trial / "agent" / "plak-stderr.log").write_text(self.STDERR)
            (trial / "result.json").write_text(
                json.dumps(
                    {
                        "task_name": "terminal-bench/regex-log",
                        "verifier_result": {"rewards": {"reward": 0.0}},
                        "agent_result": {},
                    }
                )
            )
            summary = summarize_job(trial.parent)
            self.assertEqual(
                summary.class_counts, {"agent_init_failed:Revalidation": 1}
            )
            self.assertEqual(summary.harness_terminated, 1)
            self.assertIn("agent_init_failed:Revalidation", render_table(summary))

    def test_readme_documents_the_class(self) -> None:
        readme = (ROOT / "eval" / "README.md").read_text()
        self.assertIn("agent_init_failed", readme)
        self.assertIn(INIT_FAILURE_MARKER, readme)


class _Bins:
    """Stand-in for FrozenPmuxBins with the five paths and provenance."""

    def __init__(self, root: pathlib.Path, commit: str, dirty: bool = False) -> None:
        self.pmux = root / "pmux"
        self.pmuxd = root / "pmuxd"
        self.rmuxd = root / "pmux-rmuxd"
        self.launcher = root / "pmux-launcher"
        self.hook = root / "pmux-hook"
        self.arch = "arm64"
        self.commit = commit
        self.dirty = dirty


class ArtifactIdentityTests(unittest.TestCase):
    """A trial must be able to say which bytes it ran, after the fact."""

    def _bins(self, root: pathlib.Path, body: bytes = b"pmux-bytes") -> _Bins:
        for name in ("pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook"):
            (root / name).write_bytes(body + name.encode())
        return _Bins(root, PSEUDOMUX_PIN)

    def test_digest_is_sha256_and_size(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "pmuxd"
            path.write_bytes(b"abc")
            found = digest_file(path)
            self.assertEqual(
                found.sha256,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            )
            self.assertEqual(found.bytes, 3)
            self.assertEqual(found.name, "pmuxd")

    def test_a_missing_artifact_is_skipped_not_fatal(self) -> None:
        # Collecting evidence must never be what fails a trial.
        with tempfile.TemporaryDirectory() as tmp:
            missing = pathlib.Path(tmp) / "nope"
            self.assertIsNone(digest_file(missing))
            self.assertEqual(digest_all([("nope", missing)]), {})

    def test_pmux_record_has_the_exact_shape(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            record = pmux_artifact_record(self._bins(root))
            self.assertEqual(record["schemaVersion"], ARTIFACT_SCHEMA_VERSION)
            self.assertEqual(record["harness"], "pmux")
            self.assertEqual(record["arch"], "arm64")
            self.assertEqual(
                sorted(record["binaries"]),
                ["pmux", "pmux-hook", "pmux-launcher", "pmux-rmuxd", "pmuxd"],
            )
            for name, entry in record["binaries"].items():
                with self.subTest(binary=name):
                    self.assertEqual(sorted(entry), ["bytes", "sha256"])
                    self.assertEqual(len(entry["sha256"]), 64)
                    self.assertGreater(entry["bytes"], 0)
            self.assertEqual(
                record["source"],
                {"repo": "pseudomux", "commit": PSEUDOMUX_PIN, "dirty": False},
            )

    def test_a_dirty_source_is_carried_into_the_record(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            bins = self._bins(root)
            bins.dirty = True
            record = pmux_artifact_record(bins)
            self.assertIs(record["source"]["dirty"], True)

    def test_plak_record_includes_the_js_host_when_present(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "plak").write_bytes(b"plak")
            (root / "js").write_bytes(b"js")
            record = plak_artifact_record(root / "plak", root / "js", arch="amd64")
            self.assertEqual(record["harness"], "plak")
            self.assertEqual(sorted(record["binaries"]), ["plak", "plakous-js-host"])
            self.assertNotIn("source", record)
            bare = plak_artifact_record(root / "plak", None, arch="amd64")
            self.assertEqual(sorted(bare["binaries"]), ["plak"])

    def test_helpers_read_the_record_back(self) -> None:
        record = artifact_record(
            harness="pmux",
            binaries={"pmuxd": {"sha256": "a" * 64, "bytes": 10}},
            source_commit=PSEUDOMUX_PIN,
        )
        self.assertEqual(short_digest(record, "pmuxd"), "a" * 12)
        self.assertEqual(short_digest(record, "absent"), "")
        self.assertEqual(record_source_commit(record), PSEUDOMUX_PIN)
        self.assertEqual(record_source_commit({}), "")

    def test_both_agents_record_identity_before_upload(self) -> None:
        pmux_agent = (ROOT / "eval" / "harbor_adapter" / "pmux_agent.py").read_text()
        self.assertIn("pmux_artifact_record", pmux_agent)
        self.assertIn('record["artifact"] = self._artifact', pmux_agent)
        self.assertIn('"artifact": artifact or {}', pmux_agent)
        agent = (ROOT / "eval" / "harbor_adapter" / "agent.py").read_text()
        self.assertIn("plak_artifact_record", agent)
        self.assertIn('record["artifact"] = self._artifact', agent)
        self.assertIn('"artifact": artifact or {}', agent)


class ArtifactInSummaryTests(unittest.TestCase):
    """summarize.py prints the pmuxd digest, and flags a job that mixed them."""

    SHA_A = "6edcb86cf622ebdcc01427e8f399efd55b7a164153925f3e3ff7cf683c331bad"
    SHA_B = "5e5d95c76ac2112207c340b3514474af9ff8a5b496618393f0f35efb8786610d"

    def _trial(self, job: pathlib.Path, name: str, sha: str) -> None:
        trial = job / name
        (trial / "agent").mkdir(parents=True)
        (trial / "agent" / "pmux-stdout.txt").write_text("ok\n")
        (trial / "agent" / "pmux-usage.json").write_text(
            json.dumps(
                {
                    "harness_termination": "ok",
                    "artifact": {
                        "schemaVersion": ARTIFACT_SCHEMA_VERSION,
                        "harness": "pmux",
                        "arch": "arm64",
                        "binaries": {
                            "pmuxd": {"sha256": sha, "bytes": 13760264},
                            "pmux": {"sha256": "c" * 64, "bytes": 3459864},
                        },
                        "source": {
                            "repo": "pseudomux",
                            "commit": PSEUDOMUX_PIN,
                            "dirty": False,
                        },
                    },
                }
            )
        )
        (trial / "result.json").write_text(
            json.dumps(
                {
                    "task_name": f"plak/{name.split('__')[0]}",
                    "verifier_result": {"rewards": {"reward": 1.0}},
                    "agent_result": {},
                }
            )
        )

    def test_the_digest_is_read_back_from_the_usage_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            job = pathlib.Path(tmp) / "job"
            self._trial(job, "write-hello__aaa", self.SHA_A)
            found = trial_artifact(job / "write-hello__aaa")
            self.assertEqual(found["binaries"]["pmuxd"]["sha256"], self.SHA_A)
            self.assertEqual(record_source_commit(found), PSEUDOMUX_PIN)

    def test_a_single_artifact_job_reports_one_digest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            job = pathlib.Path(tmp) / "job"
            for name in ("write-hello__aaa", "sum-file__bbb", "fix-typo__ccc"):
                self._trial(job, name, self.SHA_A)
            summary = summarize_job(job)
            self.assertEqual(summary.artifact_digests("pmuxd"), {self.SHA_A: 3})
            self.assertEqual(summary.source_commits, {PSEUDOMUX_PIN: 3})
            table = render_table(summary)
            self.assertIn("pmuxd: 6edcb86cf622x3", table)
            self.assertIn(f"pseudomux: {PSEUDOMUX_PIN[:7]}x3", table)
            self.assertNotIn("MORE THAN ONE", table)

    def test_a_job_that_mixed_two_artifacts_is_flagged(self) -> None:
        # This is the case a pass-then-fail investigation must see at once.
        with tempfile.TemporaryDirectory() as tmp:
            job = pathlib.Path(tmp) / "job"
            self._trial(job, "write-hello__aaa", self.SHA_A)
            self._trial(job, "sum-file__bbb", self.SHA_B)
            summary = summarize_job(job)
            self.assertEqual(
                summary.artifact_digests("pmuxd"), {self.SHA_B: 1, self.SHA_A: 1}
            )
            self.assertIn("MORE THAN ONE", render_table(summary))

    def test_a_job_with_no_artifact_record_prints_nothing_extra(self) -> None:
        # Trials archived before this existed must still summarize cleanly.
        with tempfile.TemporaryDirectory() as tmp:
            job = pathlib.Path(tmp) / "job"
            trial = job / "write-hello__aaa"
            (trial / "agent").mkdir(parents=True)
            (trial / "result.json").write_text(
                json.dumps(
                    {
                        "task_name": "plak/write-hello",
                        "verifier_result": {"rewards": {"reward": 1.0}},
                        "agent_result": {},
                    }
                )
            )
            summary = summarize_job(job)
            self.assertEqual(summary.artifact_digests("pmuxd"), {})
            self.assertNotIn("pmuxd:", render_table(summary))

    def test_the_json_carries_the_artifact_roll_up(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            job = pathlib.Path(tmp) / "job"
            self._trial(job, "write-hello__aaa", self.SHA_A)
            payload = summarize_job(job).as_dict()
            self.assertEqual(payload["artifacts"]["pmuxd"], {self.SHA_A: 1})
            self.assertEqual(payload["source_commits"], {PSEUDOMUX_PIN: 1})
            self.assertEqual(
                payload["rows"][0]["artifact"]["binaries"]["pmuxd"]["sha256"],
                self.SHA_A,
            )

if __name__ == "__main__":
    unittest.main()
