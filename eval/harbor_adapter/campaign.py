"""Fail-closed, content-free campaign and trial validity gates."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any

from harbor_adapter.preflight import PreflightError


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def validate_campaign(plan: dict[str, Any]) -> list[str]:
    if not isinstance(plan, dict):
        return ["campaign_schema"]
    failures = []
    if plan.get("schemaVersion") != 1:
        failures.append("campaign_schema")
    if not re.fullmatch(r"[0-9a-f]{40}", str(plan.get("source_commit", ""))):
        failures.append("source_commit")
    if (plan.get("arch") not in ("amd64", "arm64") or type(plan.get("concurrency")) is not int
            or plan.get("concurrency") != 1):
        failures.append("platform_or_concurrency")
    tasks, arms = plan.get("tasks"), plan.get("arms")
    if not isinstance(tasks, dict) or not tasks:
        failures.append("tasks_missing")
    else:
        for task in tasks.values():
            if (not isinstance(task, dict) or not re.fullmatch(r"sha256:[0-9a-f]{64}", str(task.get("image", "")))
                    or not task.get("checksum") or not task.get("cell_manifest_sha256")
                    or not all(type(task.get(k)) in (int, float) and task[k] > 0
                               for k in ("agent_timeout", "verifier_timeout"))):
                failures.append("task_binding_incomplete")
    controls = []
    if not isinstance(arms, dict) or len(arms) < 2:
        failures.append("arms_missing")
    else:
        for arm in arms.values():
            if not isinstance(arm, dict):
                failures.append("arm_binding_incomplete")
                continue
            observed = arm.get("observed_config")
            observed = observed if isinstance(observed, dict) else {}
            if (not observed.get("model") or observed.get("effort") in (None, "none", "omit")
                    or not all(k in observed for k in ("thinking", "max_output", "temperature",
                                                       "requested_service_tier", "tool_names",
                                                       "tool_schema_sha256", "instructions_sha256"))):
                failures.append("explicit_observed_config_required")
            if not re.fullmatch(r"[0-9a-f]{64}", str(arm.get("calibration_sha256", ""))):
                failures.append("wire_calibration_required")
            binaries = arm.get("binaries")
            if not isinstance(binaries, dict) or not binaries or any(
                not re.fullmatch(r"[0-9a-f]{64}", str(digest)) for digest in binaries.values()
            ):
                failures.append("executed_binary_pins_required")
            policy = arm.get("policy")
            policy = policy if isinstance(policy, dict) else {}
            if not all(k in policy for k in ("user", "web_tools", "leftover_processes", "cell_prep")):
                failures.append("policy_unverified")
            if policy.get("web_tools") is False and any(
                str(name).lower().replace("_", "").startswith(("websearch", "webfetch"))
                for name in observed.get("tool_names", [])
            ):
                failures.append("web_tool_policy_mismatch")
            controls.append(json.dumps({"model": observed.get("model"),
                                        "effort": observed.get("effort"), "thinking": observed.get("thinking"),
                                        "tier": observed.get("requested_service_tier"), "policy": policy}, sort_keys=True))
        if len(set(controls)) != 1:
            failures.append("policy_or_wire_parity")
    pins = plan.get("artifact_sha256")
    if not isinstance(pins, dict) or not pins or any(
        not re.fullmatch(r"[0-9a-f]{64}", str(value)) for value in pins.values()
    ):
        failures.append("artifact_pins_missing")
    if type(plan.get("blocks")) is not int or plan["blocks"] < 3:
        failures.append("repeat_blocks")
    return sorted(set(failures))


def load_campaign() -> dict[str, Any]:
    path = os.environ.get("HARBOR_CAMPAIGN")
    pin = os.environ.get("HARBOR_CAMPAIGN_SHA256")
    if not path or not pin:
        raise PreflightError("HARBOR_CAMPAIGN and its reviewed HARBOR_CAMPAIGN_SHA256 are required")
    try:
        if sha256(Path(path)) != pin:
            raise ValueError
        plan = json.loads(Path(path).read_text())
        failures = validate_campaign(plan)
    except (OSError, ValueError, TypeError, AttributeError):
        raise PreflightError("campaign unreadable or digest mismatched") from None
    if failures:
        raise PreflightError("campaign refused: " + ",".join(failures))
    return plan


def campaign_gate(root: Path) -> dict[str, Any]:
    plan = load_campaign()
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal",
                                     "--", "eval", "Justfile"], cwd=root)
    if dirty or commit != plan["source_commit"]:
        raise PreflightError("campaign requires clean recorded eval sources")
    if os.environ.get("HARBOR_ARCH") != plan["arch"]:
        raise PreflightError("campaign architecture mismatch")
    if os.environ.get("HARBOR_N_CONCURRENT", "1") != "1" or os.environ.get("HARBOR_N_ATTEMPTS", "1") != "1":
        raise PreflightError("campaign uses serial single-attempt slots; increase blocks, not concurrency or attempts")
    for relative, expected in plan["artifact_sha256"].items():
        path = (root / relative).resolve()
        if not path.is_relative_to((root / ".plak/installed" / plan["arch"]).resolve()):
            raise PreflightError("campaign artifact is outside the selected frozen prefix")
        if not path.is_file() or sha256(path) != expected:
            raise PreflightError("campaign artifact missing or digest mismatched")
    for arm in plan["arms"].values():
        calibration = Path(arm.get("calibration_file", ""))
        if not calibration.is_file() or sha256(calibration) != arm["calibration_sha256"]:
            raise PreflightError("campaign wire calibration missing or mismatched")
        reduced = json.loads(calibration.read_text())
        if (reduced.get("source") != "exchange" or not reduced.get("wire_verified")
                or reduced.get("first_request") != arm["observed_config"]):
            raise PreflightError("campaign calibration is not the pinned first wire request")
    for task in plan["tasks"].values():
        try:
            info = json.loads(subprocess.check_output(
                ["docker", "image", "inspect", task["image"]], text=True, stderr=subprocess.DEVNULL,
                timeout=15))[0]
        except (OSError, ValueError, IndexError, subprocess.SubprocessError):
            raise PreflightError("campaign image is not frozen in the local daemon") from None
        if info.get("Id") != task["image"] or info.get("Architecture") != plan["arch"]:
            raise PreflightError("campaign image identity or architecture mismatch")
    return plan


def trial_gate(plan: dict[str, Any], arm: str, task: str, evidence: dict[str, Any]) -> list[str]:
    failures = []
    expected = plan.get("arms", {}).get(arm)
    target = plan.get("tasks", {}).get(task)
    if not expected or not target:
        return ["unplanned_arm_or_task"]
    for key, value in (("image", target["image"]), ("checksum", target["checksum"]),
                       ("cell_manifest_sha256", target["cell_manifest_sha256"]),
                       ("agent_timeout", target["agent_timeout"]),
                       ("verifier_timeout", target["verifier_timeout"])):
        if evidence.get(key) != value:
            failures.append("trial_binding:" + key)
    if evidence.get("secrets_harvested") is not False:
        failures.append("harvest_unverified")
    observed = evidence.get("observed_config") or {}
    if not observed.get("wire_verified") or observed.get("first_request") != expected["observed_config"]:
        failures.append("first_request_unverified_or_mismatched")
    requests = observed.get("requests") or []
    if not requests or any(not (r.get("response", {}).get("served_model") or
                               r.get("response", {}).get("system_fingerprint")) for r in requests):
        failures.append("served_identity_missing")
    if expected.get("account") == "openrouter" and any(
        not r.get("response", {}).get("upstream_provider") for r in requests
    ):
        failures.append("upstream_missing")
    if arm.startswith("plak-") and (not evidence.get("resolved_composition") or
                                  evidence.get("resolved_composition") != expected.get("resolved_composition")):
        failures.append("shipped_composition_unverified")
    binaries = evidence.get("artifact", {}).get("binaries") or {}
    pins = expected.get("binaries") or {}
    if not pins or set(pins) != set(binaries) or any(
        binaries.get(name, {}).get("sha256") != digest for name, digest in pins.items()
    ):
        failures.append("executed_artifact_mismatch")
    return failures


def schedule(arms: list[str], tasks: list[str], blocks: int) -> list[dict[str, Any]]:
    """AB, BA, AB, BA blocks, with one explicit shared slot per task/block."""
    if blocks < 1 or len(arms) < 2 or not tasks or len(set(arms)) != len(arms):
        raise ValueError("invalid paired schedule")
    return [{"block": block, "slot": f"{block}:{task}", "task": task, "arm": arm}
            for block in range(blocks)
            for arm in (arms if block % 2 == 0 else list(reversed(arms))) for task in tasks]


def campaign_health(rows: list[dict[str, Any]]) -> list[str]:
    failures = []
    for arm in sorted({r["arm"] for r in rows}):
        group = [r for r in rows if r["arm"] == arm]
        for kind, limit in (("infra", .10), ("unknown", .02)):
            if sum(r.get("failure_class") == kind for r in group) / len(group) > limit:
                failures.append(arm + ":" + kind + "_threshold")
    return failures


if __name__ == "__main__":
    import sys
    try:
        campaign_gate(Path(__file__).resolve().parents[2])
    except PreflightError as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(2)
