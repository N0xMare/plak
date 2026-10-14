"""Conservative offline campaign report over harvested Harbor trial results."""

from __future__ import annotations

import hashlib
import json
import math
import re
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any

from harbor_adapter.pricing import RATE_CARD_PATH, from_harbor_context, resolve_model_key
from harbor_adapter.native_usage import extract_agent_usage
from harbor_adapter.usage import read_archived_plak_usage
from harbor_adapter.termination import (
    CLASS_COMPLETED, RESULT_OK, RESULT_REPAIRED, find_job_dirs, load_json,
    is_trial_dir, summarize_trial,
)
from harbor_adapter.timing import trial_phases
from harbor_adapter.verifier_evidence import inspect_verifier


def _number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value) if math.isfinite(value) else None


def _integer(value: object) -> int | None:
    value = _number(value)
    return int(value) if value is not None and value >= 0 and value.is_integer() else None


def _sha(path: Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def _quantile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = (len(ordered) - 1) * fraction
    lower = math.floor(index)
    upper = math.ceil(index)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (index - lower)


def _distribution(values: list[float]) -> dict[str, float | int | None]:
    return {
        "n": len(values), "median": _quantile(values, .5),
        "p10": _quantile(values, .1), "p90": _quantile(values, .9),
        "min": min(values) if values else None,
        "max": max(values) if values else None,
    }


def _wilson(successes: int, total: int) -> list[float] | None:
    """Wilson score 95% interval, appropriate even for one or zero passes."""
    if total == 0:
        return None
    z = 1.959963984540054
    p = successes / total
    denominator = 1 + z * z / total
    center = (p + z * z / (2 * total)) / denominator
    radius = z * math.sqrt(p * (1 - p) / total + z * z / (4 * total * total)) / denominator
    return [max(0.0, center - radius), min(1.0, center + radius)]


def _safe_part(value: object) -> str | None:
    return str(value) if isinstance(value, (str, int, float)) and value != "" else None


def _trial(job: Path, path: Path) -> dict[str, Any]:
    loaded = load_json(path / "result.json")
    result = loaded.data
    summary = summarize_trial(path)
    task_name = summary.task
    context_path = job.parent / "comparison-context.json"
    context = load_json(context_path).data if context_path.is_file() else {}
    config = result.get("config") if isinstance(result.get("config"), dict) else {}
    agent_config = config.get("agent") if isinstance(config.get("agent"), dict) else {}
    kwargs = agent_config.get("kwargs") if isinstance(agent_config.get("kwargs"), dict) else {}
    task_config = config.get("task") if isinstance(config.get("task"), dict) else {}
    source = _safe_part(result.get("source")) or _safe_part(task_config.get("source"))
    task_ref = _safe_part((result.get("task_id") or {}).get("ref")) if isinstance(result.get("task_id"), dict) else None
    task_ref = task_ref or _safe_part(task_config.get("ref"))
    task_checksum = _safe_part(result.get("task_checksum"))
    environment = config.get("environment") if isinstance(config.get("environment"), dict) else {}
    environment_kwargs = environment.get("kwargs") if isinstance(environment.get("kwargs"), dict) else {}
    image = _safe_part(environment_kwargs.get("image"))
    usage = result.get("agent_result") if isinstance(result.get("agent_result"), dict) else {}
    agent_info = result.get("agent_info") if isinstance(result.get("agent_info"), dict) else {}
    metadata = usage.get("metadata") if isinstance(usage.get("metadata"), dict) else {}
    model = _safe_part(agent_config.get("model_name"))
    cache_write = _integer(metadata.get("cache_write_tokens"))
    if cache_write is None:
        cache_write = _integer(metadata.get("n_cache_write_tokens"))
    if cache_write is None:
        cache_write = _integer(usage.get("n_cache_write_tokens"))
    raw_tokens = {
        "input": _integer(usage.get("n_input_tokens")),
        "cache_read": _integer(usage.get("n_cache_tokens")),
        "cache_write": cache_write,
        "output": _integer(usage.get("n_output_tokens")),
        "reasoning": _integer(metadata.get("reasoning_tokens"))
        if metadata.get("reasoning_tokens") is not None else _integer(usage.get("n_reasoning_tokens")),
    }
    harbor_tokens = {"input": raw_tokens["input"], "cache_read": raw_tokens["cache_read"],
                     "output": raw_tokens["output"]}
    native_usage = extract_agent_usage(path / "agent", summary.record.harness)
    native_counts = native_usage.as_dict()
    archived_plak = read_archived_plak_usage(path / "agent") if summary.record.harness == "plak" else None
    if native_usage.usage_records:
        raw_tokens = {
            "input": native_usage.input_tokens,
            "cache_read": native_usage.cache_read_tokens,
            "cache_write": native_usage.cache_write_tokens,
            "output": native_usage.output_tokens,
            "reasoning": native_usage.reasoning_tokens,
        }
    if archived_plak is not None and archived_plak.usage is not None:
        measured = archived_plak.usage
        raw_tokens = {
            "input": measured.n_input_tokens,
            "cache_read": measured.n_cache_tokens,
            "cache_write": measured.n_cache_write_tokens,
            "output": measured.n_output_tokens,
            "reasoning": measured.n_reasoning_tokens,
        }
    implied = from_harbor_context(
        model or "", n_input_tokens=raw_tokens["input"],
        n_cache_tokens=raw_tokens["cache_read"],
        n_cache_write_tokens=raw_tokens["cache_write"],
        n_output_tokens=raw_tokens["output"],
        n_reasoning_tokens=raw_tokens["reasoning"],
    )
    reported = _number(usage.get("cost_usd"))
    # Harbor's cost_usd can be an estimate. Its provenance is not billed money.
    status = _safe_part(metadata.get("cost_status")) or "unattributed"
    metadata_compactions = _integer(metadata.get("compaction_rows")) or 0
    scope_unverified = any(
        reason in metadata.get("missing_usage_buckets", ())
        for reason in ("compaction_coverage", "endpoint_compaction_scope_unverified")
    ) if isinstance(metadata.get("missing_usage_buckets"), (list, tuple)) else False
    if scope_unverified:
        reported = _number(metadata.get("observed_cost_subtotal_usd"))
        status = "observed_subtotal_unverified_compaction_scope"
    elif metadata_compactions and reported is None:
        reported = _number(metadata.get("observed_cost_subtotal_usd"))
        missing_compaction_cost = _integer(metadata.get("missing_compaction_cost_rows"))
        missing_turn_cost = _integer(metadata.get("missing_turn_cost_rows"))
        status = ("turn_subtotal_excludes_compactions"
                  if missing_compaction_cost == metadata_compactions and missing_turn_cost == 0
                  else "observed_subtotal_incomplete_reported_cost")
    if archived_plak is not None and archived_plak.usage is not None:
        archive_scope_unverified = any(
            reason in archived_plak.usage.missing_buckets
            for reason in ("compaction_coverage", "endpoint_compaction_scope_unverified")
        )
        if archive_scope_unverified:
            reported = archived_plak.usage.observed_cost_usd
            status = "observed_subtotal_unverified_compaction_scope"
        elif archived_plak.usage.cost_usd is not None:
            reported = archived_plak.usage.cost_usd
            status = archived_plak.usage.cost_status
        elif archived_plak.usage.compaction_rows:
            reported = archived_plak.usage.observed_cost_usd
            status = ("turn_subtotal_excludes_compactions"
                      if (archived_plak.usage.missing_compaction_cost_rows == archived_plak.usage.compaction_rows
                          and archived_plak.usage.missing_turn_cost_rows == 0)
                      else "observed_subtotal_incomplete_reported_cost")
    usage_status = native_usage.status if native_usage.usage_records else _safe_part(metadata.get("usage_status")) or "unknown"
    if summary.record.harness == "plak" and archived_plak is not None and archived_plak.usage is None \
            and metadata.get("usage_scope") != "turns_plus_compactions_v1":
        usage_status = "partial"
    if archived_plak is not None:
        if archived_plak.status == "complete":
            usage_status = "complete"
        elif archived_plak.status != "missing_db" or usage_status != "complete":
            usage_status = "partial"
    missing_buckets = metadata.get("missing_usage_buckets")
    if not isinstance(missing_buckets, list):
        missing_buckets = []
    if native_usage.usage_records:
        missing_buckets = list(native_usage.missing_buckets)
    if archived_plak is not None and archived_plak.usage is not None:
        missing_buckets = list(archived_plak.usage.missing_buckets)
    elif summary.record.harness == "plak" and metadata.get("usage_scope") != "turns_plus_compactions_v1":
        missing_buckets.append("compaction_scope_unverified")
    harbor_input = harbor_tokens["input"]
    harbor_cache = harbor_tokens["cache_read"]
    harbor_output = harbor_tokens["output"]
    expected_harbor_input = raw_tokens["input"]
    native_marker = metadata.get("native_usage")
    normalized_result = (
        isinstance(native_marker, dict) and native_marker.get("schemaVersion") == 1
        and native_marker.get("harness") == summary.record.harness
        and native_marker.get("source_sha256") == native_usage.source_sha256
        and native_usage.source_sha256 is not None
    )
    opencode_schemas = (
        "opencode.step_finish.part.tokens",
        "opencode.step_finish.part.tokens.text_plus_reasoning",
    )
    opencode_marker_valid = (
        normalized_result and isinstance(native_marker, dict)
        and native_marker.get("source_schema") in opencode_schemas
    )
    if (summary.record.harness == "opencode" and "native_usage" in metadata
            and not opencode_marker_valid):
        usage_status = "partial"
        missing_buckets.append("native_usage_contract_unverified")
    if (summary.record.harness == "pmux" and metadata.get("usage_status") == "complete"
            and _integer(metadata.get("cache_write_tokens")) is not None):
        normalized_result = True
    # Older pmux adapters existed on both sides of the prompt-total fix.
    # The structured result binds the full total, so the raw Harbor input
    # itself distinguishes the two archived conventions when metadata does not.
    if (summary.record.harness == "pmux" and not normalized_result
            and harbor_input is not None and raw_tokens["input"] is not None
            and harbor_input == raw_tokens["input"]):
        normalized_result = True
    if (summary.record.harness in ("pi", "opencode") and not normalized_result
            and expected_harbor_input is not None and raw_tokens["cache_write"] is not None):
        expected_harbor_input -= raw_tokens["cache_write"]
    if (summary.record.harness == "pmux" and not normalized_result
            and expected_harbor_input is not None and raw_tokens["cache_read"] is not None
            and raw_tokens["cache_write"] is not None):
        expected_harbor_input -= raw_tokens["cache_read"] + raw_tokens["cache_write"]
    expected_harbor_output = raw_tokens["output"]
    # OpenCode 1.18.30 stores visible output and reasoning separately. Older
    # Harbor rows (including our old normalized adapter rows) recorded only
    # visible output. The new bound adapter marker records their sum.
    canonical_opencode_output = (
        summary.record.harness == "opencode" and opencode_marker_valid
        and native_marker.get("source_schema") ==
        "opencode.step_finish.part.tokens.text_plus_reasoning"
    )
    if (summary.record.harness == "opencode" and not canonical_opencode_output
            and expected_harbor_output is not None and raw_tokens["reasoning"] is not None):
        expected_harbor_output -= raw_tokens["reasoning"]
    native_disagreement = native_usage.usage_records > 0 and any(
        actual is not None and expected is not None and actual != expected
        for actual, expected in ((harbor_input, expected_harbor_input),
                                 (harbor_cache, raw_tokens["cache_read"]),
                                 (harbor_output, expected_harbor_output))
    )
    archived_expected = None
    if archived_plak is not None:
        archived_expected = (archived_plak.usage if metadata.get("usage_scope") == "turns_plus_compactions_v1"
                             else archived_plak.turn_usage)
    archived_plak_disagreement = archived_expected is not None and any(
        actual is not None and expected is not None and actual != expected
        for actual, expected in ((harbor_input, archived_expected.n_input_tokens),
                                 (harbor_cache, archived_expected.n_cache_tokens),
                                 (harbor_output, archived_expected.n_output_tokens))
    )
    if native_disagreement or archived_plak_disagreement:
        usage_status = "partial"
    info = result.get("exception_info") if isinstance(result.get("exception_info"), dict) else {}
    exception = _safe_part(info.get("exception_type"))
    if summary.record.termination_class != CLASS_COMPLETED or exception is not None:
        usage_status = "partial"
        if "request_completion_unverified" not in missing_buckets:
            missing_buckets.append("request_completion_unverified")
    incomplete_usage = usage_status != "complete" or bool(missing_buckets)
    if incomplete_usage:
        implied = type(implied)(None, "unresolved_usage", implied.card_id, implied.model_key, implied.basis)
    phases = trial_phases(result)
    reward = summary.reward if loaded.ok and exception is None else None
    verifier_evidence = inspect_verifier(
        path, dataset=context.get("dataset"), slice_name=context.get("slice"),
        task=task_name, task_ref=task_ref, task_checksum=task_checksum,
    )
    assertions_verified = verifier_evidence["status"] in ("not_required", "assertions_recorded")
    termination = summary.record.termination_class
    setup = result.get("agent_setup")
    execution = result.get("agent_execution")
    verifier = result.get("verifier")
    if not loaded.ok:
        outcome = "unreadable_result"
    elif exception == "AgentTimeoutError":
        outcome = "agent_timeout"
    elif exception:
        lowered = exception.lower()
        if "verif" in lowered:
            outcome = "verifier_error"
        elif "setup" in lowered or "build" in lowered or "environment" in lowered:
            outcome = "setup_infrastructure_error"
        elif "provider" in lowered or "rate" in lowered or termination.startswith("turn_failed:provider"):
            outcome = "provider_error"
        else:
            outcome = "harness_error" if termination.startswith("harness_exception") else "agent_error"
    elif termination.startswith("turn_failed:provider"):
        outcome = "provider_error"
    elif termination != CLASS_COMPLETED:
        outcome = "harness_error" if termination.startswith("harness_exception") else "agent_error"
    elif reward is None:
        outcome = "verifier_missing"
    elif not assertions_verified:
        outcome = "verifier_unverified"
    else:
        outcome = "verifier_pass" if reward > 0 else "verifier_fail"
    # A repaired document is useful for diagnosis, but it is not immutable
    # evidence of the result Harbor wrote and must not enter a score.
    eligible = loaded.status == RESULT_OK and outcome in ("verifier_pass", "verifier_fail")
    context_bindings = {key: context.get(key) for key in (
        "arm", "slice", "account", "model", "effort", "arch", "cohort",
        "repeat_block", "eval_source_commit", "eval_source_dirty",
        "planned_trials", "attempts_per_task", "concurrency", "dataset",
        "timeout_policy", "adapter_task_retries", "native_internal_retries",
        "effort_semantics", "prompt_variant",
    )} if context else None
    if context_bindings is not None:
        context_bindings["input_sha256"] = context.get("input_sha256")
    context_mismatches = []
    if context:
        card_pin = (context.get("input_sha256") or {}).get("eval/harbor_adapter/rate_card.json")
        if card_pin and card_pin != _sha(RATE_CARD_PATH):
            context_mismatches.append("rate_card")
        if context.get("model"):
            if model is None or (
                resolve_model_key(context["model"]) or context["model"]
            ) != (resolve_model_key(model) or model):
                context_mismatches.append("model")
        result_effort = (_safe_part(kwargs.get("reasoning_effort"))
                         or _safe_part(kwargs.get("thinking")) or _safe_part(kwargs.get("effort")))
        expected_effort = context.get("effort")
        if expected_effort is not None:
            actual_effort = result_effort or "none"
            if expected_effort != actual_effort:
                context_mismatches.append("effort")
        expected_account = context.get("account")
        actual_account = _safe_part(kwargs.get("account")) or {
            "codex": "codex", "grok": "grok", "pi": (
                "codex" if model and model.startswith("openai/") else
                "openrouter" if model and model.startswith("openrouter/") else None),
            "opencode": "openrouter", "pmux": "claude",
        }.get(summary.record.harness)
        if expected_account is not None and actual_account != expected_account:
            context_mismatches.append("account_or_route")
        if summary.record.harness == "plak" and context.get("prompt_variant") != (
            _safe_part(kwargs.get("prompt_variant")) or "default"
        ):
            context_mismatches.append("prompt_variant")
    return {
        "arm": context.get("arm") or job.parent.name, "job": str(job), "trial": summary.trial,
        "slice": context.get("slice") if context else None,
        "cohort": context.get("cohort") if context else None,
        "repeat_block": context.get("repeat_block") if context else None,
        "comparison_context": context_bindings,
        "context_mismatches": context_mismatches,
        "comparison_context_sha256": _sha(context_path) if context else None,
        "task": task_name, "source": source, "task_ref": task_ref,
        "task_checksum": task_checksum,
        "repair_of": _safe_part(config.get("source_trial")),
        "harness": summary.record.harness, "model": model,
        "effort": (_safe_part(kwargs.get("reasoning_effort"))
                   or _safe_part(kwargs.get("thinking")) or _safe_part(kwargs.get("effort"))),
        "comparison_effort": context.get("effort") if context else None,
        "agent_version": _safe_part(kwargs.get("version")),
        "reported_agent_version": _safe_part(agent_info.get("version")),
        "pinned_agent_version": (_safe_part(metadata.get("claude_pin"))
                                 if summary.record.harness == "pmux" else None),
        "environment_image_digest": image if image and "@sha256:" in image else None,
        "artifact": summary.artifact,
        "result_status": loaded.status, "result_sha256": _sha(path / "result.json"),
        "outcome": outcome, "termination": termination,
        "eligible_pass_rate": eligible,
        "pass": (reward > 0 if loaded.status == RESULT_OK and reward is not None
                 and termination == CLASS_COMPLETED else None),
        "reward": reward if eligible else None, "raw_reward": summary.reward,
        "verifier_evidence": verifier_evidence,
        "exception_type": exception,
        "tokens": raw_tokens,
        "harbor_tokens": {"input": harbor_input, "cache_read": harbor_cache, "output": harbor_output},
        "native_usage": native_counts,
        "archived_plak_usage": ({"status": archived_plak.status,
                                 "source_sha256": archived_plak.source_sha256,
                                 "product_session_id": archived_plak.product_session_id,
                                 "runtime_session_id": archived_plak.runtime_session_id,
                                 "compaction_rows": archived_plak.usage.compaction_rows if archived_plak.usage else None}
                                if archived_plak is not None else None),
        "harbor_usage_normalized": normalized_result,
        "native_harbor_disagreement": native_disagreement,
        "archived_plak_harbor_disagreement": archived_plak_disagreement,
        "tokens_complete": not incomplete_usage and raw_tokens["input"] is not None and raw_tokens["output"] is not None,
        "usage_status": usage_status,
        "missing_usage_buckets": missing_buckets,
        "cost": {"implied_usd": implied.usd, "implied_status": implied.status,
                 "rate_card_id": implied.card_id, "model_key": implied.model_key,
                 "basis": implied.basis,
                 "reported_usd": reported, "reported_status": status},
        "phases_sec": {key: phases[key] for key in ("agent_setup_sec", "agent_execution_sec", "verifier_sec")},
        "evidence": {"result": str(path / "result.json"),
                     "agent": str(path / "agent") if (path / "agent").is_dir() else None},
    }


def _complete_total(rows: list[dict[str, Any]], getter, complete=lambda row: True) -> dict[str, Any]:
    values = [getter(row) for row in rows]
    observed = [value for value in values if value is not None]
    return {"observed_total": sum(observed) if observed else None,
            "complete_total": sum(observed) if len(observed) == len(rows) and all(complete(r) for r in rows) else None,
            "covered": len(observed), "attempts": len(rows)}


def _arm(name: str, rows: list[dict[str, Any]]) -> dict[str, Any]:
    eligible = [r for r in rows if r["eligible_pass_rate"]]
    eligible_passed = [r for r in eligible if r["pass"] is True]
    passed = [r for r in rows if r["pass"] is True]
    cost = _complete_total(rows, lambda r: r["cost"]["implied_usd"])
    reported_cost = _complete_total(rows, lambda r: r["cost"]["reported_usd"],
                                    lambda r: r["usage_status"] == "complete" and
                                    r["cost"]["reported_status"] in
                                    ("known", "reported", "mixed_reported"))
    input_tokens = _complete_total(rows, lambda r: r["tokens"]["input"], lambda r: r["tokens_complete"])
    output_tokens = _complete_total(rows, lambda r: r["tokens"]["output"], lambda r: r["tokens_complete"])
    times = [r["phases_sec"]["agent_execution_sec"] for r in rows]
    times = [x for x in times if x is not None]
    total_time = _complete_total(rows, lambda r: r["phases_sec"]["agent_execution_sec"])
    def per_attempt(total):
        return total["complete_total"] / len(rows) if rows and total["complete_total"] is not None else None
    successful_cost = _complete_total(passed, lambda r: r["cost"]["implied_usd"])
    successful_input = _complete_total(passed, lambda r: r["tokens"]["input"], lambda r: r["tokens_complete"])
    successful_output = _complete_total(passed, lambda r: r["tokens"]["output"], lambda r: r["tokens_complete"])
    def per_success(total):
        return total["complete_total"] / len(passed) if passed and total["complete_total"] is not None else None
    return {
        "arm": name, "attempts": len(rows), "eligible": len(eligible),
        "eligible_passes": len(eligible_passed),
        "passes": len(passed), "outcomes": dict(sorted(Counter(r["outcome"] for r in rows).items())),
        "pass_at_1": len(eligible_passed) / len(eligible) if eligible else None,
        "pass_at_1_ci95_wilson": _wilson(len(eligible_passed), len(eligible)),
        "operational_pass_per_attempt": len(passed) / len(rows) if rows else None,
        "operational_pass_per_attempt_ci95_wilson": _wilson(len(passed), len(rows)),
        "implied_cost_usd": cost, "implied_cost_per_attempt_usd": per_attempt(cost),
        "reported_cost_usd": reported_cost,
        "implied_cost_per_successful_attempt_usd": per_success(successful_cost),
        "total_attempt_cost_per_pass_usd": cost["complete_total"] / len(passed)
        if passed and cost["complete_total"] is not None else None,
        "input_tokens": input_tokens, "output_tokens": output_tokens,
        "input_tokens_per_attempt": per_attempt(input_tokens),
        "output_tokens_per_attempt": per_attempt(output_tokens),
        "input_tokens_per_successful_attempt": per_success(successful_input),
        "output_tokens_per_successful_attempt": per_success(successful_output),
        "total_attempt_input_tokens_per_pass": input_tokens["complete_total"] / len(passed)
        if passed and input_tokens["complete_total"] is not None else None,
        "total_attempt_output_tokens_per_pass": output_tokens["complete_total"] / len(passed)
        if passed and output_tokens["complete_total"] is not None else None,
        "agent_execution_sec": _distribution(times),
        "agent_execution_per_attempt_sec": per_attempt(total_time),
        "total_attempt_execution_per_pass_sec": total_time["complete_total"] / len(passed)
        if passed and total_time["complete_total"] is not None else None,
        "agent_execution_per_successful_attempt_sec": _distribution([
            r["phases_sec"]["agent_execution_sec"] for r in passed
            if r["phases_sec"]["agent_execution_sec"] is not None]),
        "agent_execution_coverage": {"covered": len(times), "attempts": len(rows)},
    }


def build_report(jobs: list[Path], selection: Path | None = None,
                 roots: list[Path] | None = None, *, prompt_ablation: bool = False) -> dict[str, Any]:
    rows = [_trial(job, trial) for job in sorted(set(jobs))
            for trial in sorted(job.iterdir()) if is_trial_dir(trial)]
    selection_binding = None
    if selection is not None:
        index = json.loads(selection.read_text(encoding="utf-8"))
        records = index.get("records")
        if not isinstance(records, list):
            raise ValueError("selection index has no records array")
        if index.get("record_count") is not None and index["record_count"] != len(records):
            raise ValueError("selection record_count does not match records")
        by_hash = defaultdict(list)
        for row in rows:
            by_hash[row["result_sha256"]].append(row)
        selected = []
        seen_digests = set()
        for record in records:
            if not isinstance(record, dict):
                raise ValueError("selection record is not an object")
            digest = record.get("result_json_sha256")
            if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
                raise ValueError("selection result hash is not sha256")
            if digest in seen_digests:
                raise ValueError(f"duplicate selection result hash: {digest}")
            seen_digests.add(digest)
            candidates = by_hash.get(digest, [])
            if len(candidates) != 1:
                raise ValueError(f"selection result hash is missing or ambiguous: {digest}")
            row = dict(candidates[0])
            if record.get("task_id") != row["task"]:
                raise ValueError(f"selection task does not match result: {digest}")
            row["arm"] = record.get("arm")
            row["slice"] = record.get("slice")
            row["source_commit"] = record.get("source_commit")
            slot = record.get("slot")
            if slot is not None and (isinstance(slot, bool) or not isinstance(slot, (str, int))):
                raise ValueError("selection slot must be a string or integer")
            row["selection_slot"] = slot
            row["selection_record_path"] = record.get("result_json_path")
            selected.append(row)
        rows = selected
        selection_binding = {"path": str(selection), "sha256": _sha(selection),
                             "records": len(records), "schema": index.get("schema")}
    arms = defaultdict(list)
    by_task = defaultdict(lambda: defaultdict(list))
    for row in rows:
        context = row["comparison_context"] or {}
        canonical_model = resolve_model_key(context.get("model") or row["model"] or "") or context.get("model") or row["model"]
        canonical_effort = context.get("effort") if context else row["effort"]
        arm_key = (row["arm"], row["slice"], canonical_model, canonical_effort, row["cohort"])
        arms[arm_key].append(row)
        family = (row["slice"], row["cohort"], row["repeat_block"],
                  context.get("account"), canonical_model,
                  canonical_effort, row["task"])
        by_task[family][row["arm"]].append(row)
    def artifact_signature(row: dict[str, Any]) -> str:
        artifact = row["artifact"]
        return json.dumps({"binaries": artifact.get("binaries"),
                           "arch": artifact.get("arch"),
                           "version": artifact.get("version"),
                           "source": artifact.get("source")}, sort_keys=True)

    arm_artifact_consistent = {}
    for key, entries in arms.items():
        originals = [row for row in entries if not row["repair_of"]]
        arm_artifact_consistent[key] = (bool(originals) and
            all(row["artifact"].get("binaries") for row in originals) and
            len({artifact_signature(row) for row in originals}) == 1)
    pairs = []
    for (slice_name, cohort, repeat_block, account, model_key, effort, task), groups in sorted(by_task.items(), key=lambda item: str(item[0])):
        if prompt_ablation:
            variant_arms = set(groups)
            if len(variant_arms) != 2 or not any(
                variant_arms == {f"plak-{family}-legacy", f"plak-{family}-parity"}
                for family in ("codex", "grok", "claude")
            ):
                raise ValueError("prompt ablation requires one legacy/parity Plak arm pair")
            if any(
                row["harness"] != "plak"
                or (row["comparison_context"] or {}).get("prompt_variant") != arm.rsplit("-", 1)[1]
                for arm, entries in groups.items() for row in entries
            ):
                raise ValueError("prompt ablation arm and executed variant differ")
        signatures = {
            arm: sorted({((r["comparison_context"] or {}).get("dataset") or r["source"],
                         r["task_ref"], r["task_checksum"]) for r in entries}, key=str)
            for arm, entries in groups.items()
        }
        uniform = len({str(value) for value in signatures.values()}) == 1
        provenance_complete = all(all(all(part is not None for part in signature)
                                      for signature in values)
                                  for values in signatures.values())
        complete = len(groups) >= 2
        originals = {arm: [r for r in entries if not r["repair_of"]] for arm, entries in groups.items()}
        repeats_match = len({len(v) for v in originals.values()}) == 1 and all(originals.values())
        slot_sets = {
            arm: [r.get("selection_slot") for r in entries]
            for arm, entries in originals.items()
        }
        # Harbor's trial suffix is random, not a repeat index. One attempt per
        # task has an unambiguous slot; repeated attempts require a selection
        # index with explicit shared slot identities.
        single_attempt = repeats_match and all(len(entries) == 1 for entries in originals.values())
        repeat_slots_verified = single_attempt or (repeats_match and
            all(all(slot is not None for slot in slots) for slots in slot_sets.values()))
        repeat_slots_match = (
            single_attempt or (repeat_slots_verified and
            all(len(slots) == len(set(slots)) for slots in slot_sets.values()) and
            len({json.dumps(sorted(slots, key=str)) for slots in slot_sets.values()}) == 1)
        )
        status = "matched" if uniform and provenance_complete and complete and repeat_slots_match else "mismatched"
        reasons = []
        if not complete: reasons.append("no_comparison_arm")
        if not uniform: reasons.append("source_or_task_mismatch")
        if not provenance_complete: reasons.append("source_or_task_unverified")
        if not repeats_match: reasons.append("repeat_count_mismatch")
        elif not repeat_slots_verified: reasons.append("repeat_slot_unverified")
        elif not repeat_slots_match: reasons.append("repeat_slot_mismatch")
        if any(len(v) != len(groups[arm]) for arm, v in originals.items()): reasons.append("repairs_present")
        controls = {arm: entries[0]["comparison_context"] for arm, entries in groups.items()}
        artifact_complete = all(
            arm_artifact_consistent.get((arm, slice_name, model_key, effort, cohort), False) and
            all(r["artifact"].get("binaries")
                and r["artifact"].get("harness") == r["harness"]
                and r["artifact"].get("arch") == (r["comparison_context"] or {}).get("arch")
                and (bool(r["pinned_agent_version"] and
                          r["reported_agent_version"] == f'{r["pinned_agent_version"]} (Claude Code)' and
                          r["artifact"].get("version") == r["pinned_agent_version"] and
                          {"pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook",
                           "claude-archive", "node-archive"}.issubset(r["artifact"]["binaries"]) and
                          (r["artifact"].get("source") or {}).get("commit") and
                          (r["artifact"].get("source") or {}).get("dirty") is False)
                     if r["harness"] == "pmux" else
                     (r["harness"] == "plak"
                      or not (r["agent_version"] or r["reported_agent_version"])
                      or r["artifact"].get("version") ==
                      (r["agent_version"] or r["reported_agent_version"])))
                for r in entries)
            for arm, entries in originals.items()
        )
        if prompt_ablation and len({artifact_signature(row) for entries in originals.values()
                                    for row in entries}) != 1:
            artifact_complete = False
        known_controls = all(control is not None for control in controls.values())
        commits = {control["eval_source_commit"] for control in controls.values() if control}
        clean = known_controls and all(not control["eval_source_dirty"] for control in controls.values())
        context_consistent = not any(r["context_mismatches"] for entries in groups.values() for r in entries)
        equal_control_keys = ("arch", "dataset", "concurrency", "attempts_per_task", "input_sha256",
                              "timeout_policy", "adapter_task_retries",
                              "native_internal_retries", "effort_semantics")
        if not prompt_ablation:
            equal_control_keys += ("prompt_variant",)
        control_mismatches = [key for key in equal_control_keys if known_controls and
                              (any(control.get(key) is None for control in controls.values()) or
                               len({json.dumps(control.get(key), sort_keys=True) for control in controls.values()}) != 1)]
        if known_controls and any(
            len(originals[arm]) != control.get("attempts_per_task")
            for arm, control in controls.items()
        ):
            control_mismatches.append("planned_original_attempts")
        comparison_eligible = (status == "matched" and known_controls and clean and
                               len(commits) == 1 and context_consistent and not control_mismatches
                               and artifact_complete)
        if not known_controls: reasons.append("comparison_context_missing")
        elif not clean: reasons.append("dirty_eval_source")
        elif len(commits) != 1: reasons.append("eval_source_mismatch")
        if not context_consistent: reasons.append("context_result_mismatch")
        if not artifact_complete: reasons.append("artifact_missing_or_varied_within_arm")
        reasons.extend(f"control_unverified_or_mismatched:{key}" for key in control_mismatches)
        if status == "matched" and not comparison_eligible:
            status = "unverified"
        latency = {}
        for arm, entries in originals.items():
            values = [v["phases_sec"]["agent_execution_sec"] for v in entries
                      if v["phases_sec"]["agent_execution_sec"] is not None]
            if values:
                latency[arm] = statistics.mean(values)
        diff = None
        complete_timing = all(
            sum(v["phases_sec"]["agent_execution_sec"] is not None for v in entries) == len(entries)
            for entries in originals.values()
        )
        if status in ("matched", "unverified") and complete_timing and len(latency) >= 2:
            labels = sorted(latency)
            diff = [{"arms": [left, right], "seconds": latency[left] - latency[right]}
                    for index, left in enumerate(labels) for right in labels[index + 1:]]
        pairs.append({"task": task, "slice": slice_name, "cohort": cohort,
                      "repeat_block": repeat_block, "account": account,
                      "model_key": model_key, "effort": effort,
                      "status": status, "comparison_eligible": comparison_eligible,
                      "reasons": reasons, "controls": controls,
                      "signatures": {arm: [list(v) for v in values] for arm, values in signatures.items()},
                      "original_attempts": {arm: len(v) for arm, v in originals.items()},
                      "repair_attempts": {arm: len(groups[arm]) - len(v) for arm, v in originals.items()},
                      "paired_agent_execution_difference": diff})
    original_arms = {name: [r for r in group if not r["repair_of"]]
                     for name, group in arms.items()}
    repair_arms = {name: [r for r in group if r["repair_of"]]
                   for name, group in arms.items()}
    def arm_label(key):
        name, slice_name, model, effort, cohort = key
        same_name = sum(other[0] == name for other in arms) > 1
        return "/".join(str(part or "unknown") for part in key) if same_name else name
    contexts = []
    for root in sorted(set(roots or [])):
        context_path = root / "comparison-context.json"
        if context_path.is_file():
            loaded = load_json(context_path)
            contexts.append({"path": str(context_path), "sha256": _sha(context_path),
                             "status": loaded.status, "binding": loaded.data})
    planned = sum(binding["binding"].get("planned_trials", 0)
                  for binding in contexts if isinstance(binding["binding"].get("planned_trials"), int))
    method = "offline observed original attempts; repairs shown separately; selected Rung A A/B conditional verifier rates require asserted CTRF tests; unverified raw scores remain in operational counts; Wilson 95% intervals assume independent attempts and do not account for repeated tasks; no broad coding population or causal ranking claim"
    if prompt_ablation:
        method += "; prompt ablation admits only an executed legacy/parity variant delta on the same Plak artifact"
    return {"schemaVersion": 1, "method": method,
            "selection": selection_binding,
            "invocations": contexts, "planned_trials": planned if contexts else None,
            "planned_unattempted": max(0, planned - len(rows)) if contexts else None,
            "rate_card_sha256": _sha(RATE_CARD_PATH), "trials": rows,
            "arms": [_arm(arm_label(name), group) for name, group in sorted(original_arms.items(), key=lambda item: str(item[0]))],
            "repair_arms": [_arm(arm_label(name), group) for name, group in sorted(repair_arms.items(), key=lambda item: str(item[0])) if group],
            "pairs": pairs,
            "warnings": (["Task/source/repeat mismatches make paired comparisons incomplete."]
                         if any(p["status"] != "matched" for p in pairs) else [])}


def discover(roots: list[Path]) -> list[Path]:
    return sorted({job for root in roots for job in find_job_dirs(root)})


def render_markdown(report: dict[str, Any], output_dir: Path) -> str:
    lines = ["# Offline eval report", "", report["method"], "",
             "| Arm | Attempts | Eligible | Eligible passes | Operational passes | Operational pass/attempt | Conditional pass@1 (95% Wilson CI) | Cost coverage | Implied $/attempt | Total attempt $/pass | Token coverage | Input tokens/attempt | Execution coverage | Execution median s |",
             "| --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: | ---: | --- | ---: | --- | ---: |"]
    if report["planned_trials"] is not None:
        lines[3:3] = [f"Planned trials: {report['planned_trials']}; observed attempts: {len(report['trials'])}; not started or not archived: {report['planned_unattempted']}.", ""]
    def shown(value: Any, digits: int = 3) -> str:
        return "—" if value is None else f"{value:.{digits}f}" if isinstance(value, float) else str(value)
    for arm in report["arms"]:
        ci = arm["pass_at_1_ci95_wilson"]
        rate = shown(arm["pass_at_1"]) + (f" [{ci[0]:.3f}, {ci[1]:.3f}]" if ci else "")
        cost_coverage = arm["implied_cost_usd"]
        token_coverage = arm["input_tokens"]
        time_coverage = arm["agent_execution_coverage"]
        lines.append("| " + " | ".join((arm["arm"], str(arm["attempts"]), str(arm["eligible"]),
            str(arm["eligible_passes"]), str(arm["passes"]),
            shown(arm["operational_pass_per_attempt"]), rate,
            f"{cost_coverage['covered']}/{cost_coverage['attempts']}",
            shown(arm["implied_cost_per_attempt_usd"], 6),
            shown(arm["total_attempt_cost_per_pass_usd"], 6),
            f"{token_coverage['covered']}/{token_coverage['attempts']}",
            shown(arm["input_tokens_per_attempt"]),
            f"{time_coverage['covered']}/{time_coverage['attempts']}",
            shown(arm["agent_execution_sec"]["median"]))) + " |")
    lines += ["", "Missing cost or usage leaves full-attempt totals unresolved. Reported Harbor dollars are recorded separately from frozen-card implied dollars.",
              "", "## Pair coverage", "", "| Task | Status | Reasons | Execution difference s |", "| --- | --- | --- | ---: |"]
    for pair in report["pairs"]:
        diff = pair["paired_agent_execution_difference"]
        differences = ", ".join(f"{item['arms'][0]}−{item['arms'][1]}: {shown(item['seconds'])}"
                                for item in diff) if diff else "—"
        lines.append(f"| {pair['task']} | {pair['status']} | {', '.join(pair['reasons']) or '—'} | {differences} |")
    lines += ["", "## Trial drilldown", "", "| Arm | Task | Trial | Outcome | Verifier evidence | Termination | Raw reward | Qualified reward | Implied USD | Reported USD | Agent s | Result |",
              "| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- |"]
    for trial in report["trials"]:
        result_path = Path(trial["evidence"]["result"])
        try: link = result_path.relative_to(output_dir)
        except ValueError: link = Path(__import__("os").path.relpath(result_path, output_dir))
        lines.append("| " + " | ".join((trial["arm"], trial["task"], trial["trial"],
            trial["outcome"], trial["verifier_evidence"]["status"], trial["termination"],
            shown(trial["raw_reward"]), shown(trial["reward"]),
            shown(trial["cost"]["implied_usd"], 6), shown(trial["cost"]["reported_usd"], 6),
            shown(trial["phases_sec"]["agent_execution_sec"]), f"[result]({link.as_posix()})")) + " |")
    return "\n".join(lines) + "\n"
