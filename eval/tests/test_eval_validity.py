"""Zero-inference regression fixtures for the validity gates and estimand."""

from __future__ import annotations

import copy
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harbor_adapter.campaign import campaign_health, load_campaign, schedule, trial_gate, validate_campaign
from harbor_adapter.cell_prep import RG_PINS, prepare_cell
from harbor_adapter.credentials import (
    CredentialError, native_oauth_expiry, oauth_expiry, replace_codex_auth,
    require_native_oauth_lifetime, trial_oauth,
)
from harbor_adapter.hygiene import HARVEST_SCRIPT
from harbor_adapter.observed_config import extract_exchanges, extract_native, request_config, resolved_composition
from harbor_adapter.outcomes import failure_class
from harbor_adapter.paired import estimate
from harbor_adapter.preflight import PreflightError, task_budgets
from harbor_adapter.report import build_report, discover
from harbor_adapter.termination import classify_plak, classify_vendor


def campaign():
    observed = request_config({"model": "same-model", "reasoning": {"effort": "low"}, "tools": []})
    arm = {"account": "codex", "observed_config": observed,
           "binaries": {"binary": "b" * 64}, "calibration_sha256": "c" * 64,
           "policy": {"user": "root", "web_tools": False, "leftover_processes": "preserve",
                      "cell_prep": "common-v1"}}
    return {"schemaVersion": 1, "source_commit": "a" * 40, "arch": "arm64", "concurrency": 1,
            "blocks": 3, "tasks": {"task": {"image": "sha256:" + "d" * 64,
            "checksum": "checksum", "cell_manifest_sha256": "e" * 64,
            "agent_timeout": 1800, "verifier_timeout": 600}},
            "arms": {"a": copy.deepcopy(arm), "b": copy.deepcopy(arm)},
            "artifact_sha256": {".plak/installed/arm64/binary": "b" * 64}}


class CampaignTests(unittest.TestCase):
    def test_campaign_requires_explicit_parity_and_real_pins(self):
        self.assertEqual(validate_campaign(campaign()), [])
        for mutate, reason in (
            (lambda p: p["arms"]["a"]["observed_config"].update(effort="none"), "explicit_observed_config_required"),
            (lambda p: p["arms"]["a"]["policy"].update(user="pmux"), "policy_or_wire_parity"),
            (lambda p: p.update(concurrency=4), "platform_or_concurrency"),
            (lambda p: p["tasks"]["task"].update(image="moving:tag"), "task_binding_incomplete"),
            (lambda p: p.update(artifact_sha256={}), "artifact_pins_missing"),
        ):
            plan = campaign()
            mutate(plan)
            self.assertIn(reason, validate_campaign(plan))

    def test_campaign_file_is_digest_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "campaign.json"
            path.write_text(json.dumps(campaign()))
            with mock.patch.dict(os.environ, {"HARBOR_CAMPAIGN": str(path),
                                 "HARBOR_CAMPAIGN_SHA256": hashlib.sha256(path.read_bytes()).hexdigest()}):
                self.assertEqual(load_campaign()["blocks"], 3)
                path.write_text(path.read_text() + " ")
                with self.assertRaises(PreflightError):
                    load_campaign()

    def test_trial_requires_actual_observations_not_requested_kwargs(self):
        plan = campaign()
        evidence = dict(plan["tasks"]["task"], secrets_harvested=False,
                        artifact={"binaries": {"binary": {"sha256": "b" * 64}}},
                        observed_config={"wire_verified": True,
                            "first_request": plan["arms"]["a"]["observed_config"],
                            "requests": [{"response": {"served_model": "snapshot"}}]})
        self.assertEqual(trial_gate(plan, "a", "task", evidence), [])
        evidence["observed_config"] = extract_native("codex", [{"type": "turn_context", "payload": {
            "model": "same-model", "effort": "low"}}])
        self.assertIn("first_request_unverified_or_mismatched", trial_gate(plan, "a", "task", evidence))
        evidence["secrets_harvested"] = None
        self.assertIn("harvest_unverified", trial_gate(plan, "a", "task", evidence))

    def test_alternating_blocks_preserve_task_slots(self):
        slots = schedule(["a", "b"], ["x", "y"], 2)
        self.assertEqual([r["arm"] for r in slots], ["a", "a", "b", "b", "b", "b", "a", "a"])
        for slot in {r["slot"] for r in slots}:
            self.assertEqual({r["arm"] for r in slots if r["slot"] == slot}, {"a", "b"})

    def test_health_thresholds_are_blockers(self):
        rows = [{"arm": "a", "failure_class": "agent"} for _ in range(10)]
        self.assertEqual(campaign_health(rows), [])
        rows[0]["failure_class"] = "unknown"
        self.assertEqual(campaign_health(rows), ["a:unknown_threshold"])


class ObservedTests(unittest.TestCase):
    def test_wire_thinking_budget_and_chat_controls_are_not_lost(self):
        record = request_config({"model": "fixture", "thinking": {"type": "enabled", "budget_tokens": 2048},
                                 "reasoning_effort": "low", "max_completion_tokens": 8192})
        self.assertEqual(record["thinking"], "enabled")
        self.assertEqual(record["thinking_budget"], 2048)
        self.assertEqual(record["effort"], "low")
        self.assertEqual(record["max_output"], 8192)

    def test_wire_reduction_keeps_config_not_content_or_headers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = {"model": "same-model", "reasoning": {"effort": "low"},
                       "service_tier": "priority", "instructions": "synthetic private instruction",
                       "tools": [{"type": "function", "name": "exec", "description": "private"}]}
            (root / "exchange-0001.request").write_text(json.dumps(request))
            (root / "exchange-0001.body").write_text('data: ' + json.dumps({"type": "response.completed",
                "response": {"model": "snapshot-1", "service_tier": "default", "provider": "upstream"}}) + '\n')
            record = extract_exchanges(root)
            self.assertTrue(record["wire_verified"])
            self.assertEqual(record["first_request"]["effort"], "low")
            self.assertEqual(record["requests"][0]["response"]["served_service_tier"], "default")
            self.assertNotIn("private", json.dumps(record))

    def test_native_declarations_never_claim_wire_calibration(self):
        result = extract_native("pi", [{"type": "model_change", "modelId": "m", "provider": "p"},
            {"type": "thinking_level_change", "thinkingLevel": "low"},
            {"type": "message", "message": {"role": "user"}},
            {"type": "thinking_level_change", "thinkingLevel": "high"}])
        self.assertEqual(result["first_request"]["effort"], "low")
        self.assertFalse(result["wire_verified"])
        self.assertEqual(resolved_composition("plak: composition Direct: tools"), "direct")


class AttributionTests(unittest.TestCase):
    def test_tool_chatter_cannot_supply_provider_error(self):
        result = classify_vendor(harness="codex", log_text=json.dumps({"type": "item.completed", "item": {
            "type": "command_execution", "output": "HTTP 429 unauthorized"}}),
            exception_type="ApiRateLimitError", exception_message="HTTP 429")
        self.assertEqual(result.termination_class, "harness_exception:ApiRateLimitError")
        self.assertEqual(failure_class(result.termination_class, "ApiRateLimitError"), "unknown")
        result = classify_plak(exit_code=5, stderr="plak: tool bash: unauthorized\nplak: agent turn failed: round limit")
        self.assertEqual(result.termination_class, "turn_failed:round-limit")

    def test_pi_clean_exit_after_provider_error_is_scored_symmetrically(self):
        result = classify_vendor(harness="pi", exit_code=0, log_text=json.dumps({
            "type": "message_end", "message": {"role": "assistant", "stopReason": "error",
                                                   "errorMessage": "HTTP 429 Too Many Requests"}}))
        self.assertEqual(result.termination_class, "turn_failed:provider-rate-limit")
        self.assertEqual(failure_class(result.termination_class), "agent")
        self.assertEqual(failure_class("turn_failed:credential"), "infra")
        self.assertEqual(failure_class("harness_exception:AgentTimeoutError", "AgentTimeoutError"), "agent")

    def test_tool_error_is_not_a_claude_terminal_error(self):
        result = classify_vendor(harness="pmux", log_text=json.dumps({"type": "user", "is_error": True}))
        self.assertEqual(result.termination_class, "completed")
        result = classify_vendor(harness="pmux", log_text=json.dumps({"type": "result", "is_error": True,
                                                                 "result": "NeedsLogin"}))
        self.assertEqual(failure_class(result.termination_class), "infra")


class CredentialAndHarvestTests(unittest.TestCase):
    def test_native_expiry_metadata_uses_each_stores_units(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.json"
            claims = base64.urlsafe_b64encode(b'{"exp":9000}').decode().rstrip("=")
            for harness, provider, store in (
                ("codex", None, {"tokens": {"access_token": "fixture." + claims + ".signature"}}),
                ("pmux", None, {"claudeAiOauth": {"expiresAt": 9_000_000}}),
                ("pi", "openai-codex", {"openai-codex": {"type": "oauth", "expires": 9_000_000}}),
                ("opencode", "openai", {"openai": {"type": "oauth", "expires": 9_000_000}}),
            ):
                path.write_text(json.dumps(store))
                self.assertEqual(native_oauth_expiry(path, harness, provider), 9000)
            path.write_text('{"tokens":{"access_token":"synthetic-private"}}')
            with self.assertRaises(CredentialError) as found:
                native_oauth_expiry(path, "codex")
            self.assertNotIn("synthetic-private", str(found.exception))

    def test_native_dispatch_refuses_short_lifetime(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.json"
            path.write_text(json.dumps({"openai-codex": {"type": "oauth", "expires": 3_700_000}}))
            agent = SimpleNamespace(_native_harness="pi")
            with mock.patch("harbor_adapter.credentials.require_ready_pi_auth", return_value=path), \
                 mock.patch("harbor_adapter.credentials.time.time", return_value=1000):
                require_native_oauth_lifetime(agent, "codex", 1800, 600)
                with self.assertRaises(CredentialError):
                    require_native_oauth_lifetime(agent, "codex", 1801, 600)

    def test_expiry_checks_do_not_surface_private_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            credential = root / "plak/subscriptions/codex/credential.json"
            credential.parent.mkdir(parents=True)
            credential.write_text(json.dumps({"format_version": 1, "profile_id": "fixture",
                "slot_id": "fixture", "account_binding": "fixture", "generation": 1,
                "status": "active", "payload": base64.b64encode(json.dumps({
                    "access_expires_at": 9000, "access_token": "synthetic"}).encode()).decode()}))
            with mock.patch("harbor_adapter.credentials.time.time", return_value=1000), \
                 mock.patch("harbor_adapter.credentials.subprocess.run") as refresh:
                with trial_oauth("codex", agent_timeout=1800, verifier_timeout=600, xdg_state_home=root):
                    self.assertFalse((credential.parent / ".harbor-refresh.lock").exists())
                refresh.assert_not_called()
            credential.write_text('{"access_token":"synthetic"}')
            with self.assertRaises(CredentialError) as found:
                oauth_expiry(credential, "codex")
            self.assertNotIn("synthetic", str(found.exception))

    def test_codex_rotation_is_atomic_and_same_account(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            auth, candidate = root / "auth.json", root / "candidate.json"
            before = {"tokens": {"account_id": "fixture-account", "access_token": "old", "refresh_token": "old"}}
            auth.write_text(json.dumps(before))
            raw = auth.read_bytes()
            after = copy.deepcopy(before)
            after["tokens"].update(access_token="new", refresh_token="new")
            candidate.write_text(json.dumps(after))
            replace_codex_auth(auth, candidate, raw)
            self.assertEqual(json.loads(auth.read_text()), after)
            candidate.write_text(json.dumps(after))
            with self.assertRaises(CredentialError):
                replace_codex_auth(auth, candidate, raw)

    def test_harvest_excludes_secret_roots_and_raw_exchanges(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, logs = root / "home", root / "logs"
            (home / "state").mkdir(parents=True)
            (home / "secrets").mkdir()
            (home / "secrets/openrouter.key").write_text("synthetic-private")
            (home / "state/public.json").write_text('{}')
            logs.mkdir()
            program = HARVEST_SCRIPT.replace('/tmp/plak-cell/home', str(home)).replace('/logs', str(logs))
            for name in ('grok-secrets', 'pi-secrets', 'opencode-secrets', 'codex-secrets', 'codex-home', 'claude-secrets'):
                program = program.replace('/tmp/' + name, str(root / name))
            subprocess.run([sys.executable, "-c", program], check=True, capture_output=True)
            self.assertTrue((logs / "agent/plak-home/state/public.json").is_file())
            self.assertFalse(any(p.name == "openrouter.key" for p in logs.rglob('*')))
            self.assertNotIn("synthetic-private", ''.join(p.read_text() for p in logs.rglob('*') if p.is_file()))

    def test_pmux_uses_each_task_budget_not_a_fixed_900_seconds(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "task.toml").write_text('[agent]\ntimeout_sec=1800\n[verifier]\ntimeout_sec=600\n')
            self.assertEqual(task_budgets(SimpleNamespace(environment_dir=root / "environment")), (1800, 600))


class PinAndEstimatorTests(unittest.IsolatedAsyncioTestCase):
    async def test_common_rg_refuses_unpinned_bytes_before_upload(self):
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch("harbor_adapter.cell_prep.installed_root", return_value=Path(directory)), \
             mock.patch("harbor_adapter.cell_prep.load_campaign", return_value={"arch": "arm64"}):
            cell = SimpleNamespace(upload_file=mock.AsyncMock())
            with self.assertRaises(PreflightError):
                await prepare_cell(object(), cell, "arm64")
            cell.upload_file.assert_not_awaited()
        self.assertEqual(set(RG_PINS), {"amd64", "arm64"})

    async def test_estimator_is_task_weighted_and_drops_underpowered_tasks(self):
        result = estimate({"one": ([True] * 9, [False] * 9),
                           "two": ([False] * 3, [True] * 3),
                           "short": ([True] * 2, [False] * 3)}, replicates=200)
        self.assertEqual(result["difference"], 0)
        self.assertEqual(result["tasks"], 2)
        self.assertEqual(result["dropped_tasks"], ["short"])
        null = estimate({"task": ([True] * 3, [True] * 3)}, replicates=200)
        self.assertEqual(null["task_population_ci95"], [0, 0])
        self.assertEqual(null["fixed_tasks_ci95"], [0, 0])


class ReportValidityTests(unittest.TestCase):
    def test_only_bound_validity_evidence_enters_paired_primary_estimator(self):
        from test_eval_reporting import ReportTests
        fixture = ReportTests()
        plan = campaign()
        plan["arch"] = "amd64"
        plan["artifact_sha256"] = {".plak/installed/amd64/codex": "b" * 64}
        plan["tasks"]["bench/task-a"] = plan["tasks"].pop("task")
        plan["tasks"]["bench/task-a"]["checksum"] = "checksum-one"
        for arm in plan["arms"].values():
            arm["observed_config"]["model"] = "gpt-6-luna"
            arm["binaries"] = {"codex": "b" * 64}
        canonical = hashlib.sha256(json.dumps(plan, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for block in range(3):
                for name in ("a", "b"):
                    trial = fixture._trial(root / str(block), name, "task-a__one", reward=int(name == "a"))
                    agent = trial / "agent"
                    (agent / "vendor-artifact.json").write_text(json.dumps({
                        "schemaVersion": 1, "harness": "codex", "arch": "amd64", "version": "0.159.3",
                        "binaries": {"codex": {"sha256": "b" * 64, "bytes": 100}}}))
                    (agent / "cell-preflight.json").write_text(json.dumps(plan["tasks"]["bench/task-a"]))
                    (agent / "harvest.json").write_text('{"secrets_harvested":false}')
                    (agent / "observed-config.json").write_text(json.dumps({"schemaVersion": 1,
                        "wire_verified": True, "first_request": plan["arms"][name]["observed_config"],
                        "requests": [{"response": {"served_model": "snapshot"}}]}))
                    (trial.parent.parent / "comparison-context.json").write_text(json.dumps({
                        "arm": name, "slice": "ab", "account": "codex", "model": "gpt-6-luna", "effort": "low",
                        "cohort": "fixture", "repeat_block": str(block), "dataset": "bench/v1",
                        "eval_source_commit": "a" * 40, "eval_source_dirty": False, "arch": "amd64",
                        "concurrency": 1, "attempts_per_task": 1, "input_sha256": {"frozen": "x"},
                        "timeout_policy": "task-default", "adapter_task_retries": 0,
                        "native_internal_retries": "stock", "effort_semantics": "explicit",
                        "prompt_variant": "default", "campaign": plan, "campaign_sha256": "c" * 64,
                        "campaign_canonical_sha256": canonical}))
            report = build_report(discover([root]))
            self.assertTrue(all(p["comparison_eligible"] for p in report["pairs"]), report["pairs"])
            self.assertEqual(report["paired_estimates"][0]["primary"]["difference"], 1)
            self.assertEqual(report["paired_estimates"][0]["primary"]["tasks"], 1)
            for path in root.rglob("observed-config.json"):
                path.unlink()
            diagnostic = build_report(discover([root]))
            self.assertEqual(diagnostic["paired_estimates"], [])
            self.assertTrue(all(a["primary_pass_rate"] is None for a in diagnostic["arms"]))
