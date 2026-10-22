#!/usr/bin/env python3
"""Opt-in interleaved Rung A campaign. Never invoked by harbor-test."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "eval"))

from harbor_adapter.campaign import campaign_gate, campaign_health, schedule, sha256
from harbor_adapter.report import build_report, discover
from harbor_adapter.campaign_builder import private_json
from harbor_adapter.leases import account_leases
from harbor_adapter.paired import estimate
from harbor_adapter.preflight import PreflightError
from harbor_adapter.observed_config import resolved_composition


def main() -> int:
    plan = campaign_gate(ROOT)
    mode = sys.argv[1]
    phase = plan.get("phase", "scored")
    if mode in ("calibration", "null", "codex"):
        expected = {"calibration": "calibration", "null": "null", "codex": "scored"}[mode]
        if phase != expected:
            raise PreflightError("recipe does not match reviewed campaign phase")
        if mode == "codex" and (set(plan["arms"]) != {"plak-codex", "codex"}
                                or plan["blocks"] != 5 or plan.get("slice") != "expanded"):
            raise PreflightError("Codex phase requires the expanded pair and five blocks")
        if mode == "codex":
            receipt_path = Path(plan.get("null_receipt_file", ""))
            if not receipt_path.is_file() or sha256(receipt_path) != plan.get("null_receipt_sha256"):
                raise PreflightError("Codex phase requires the reviewed successful A/A receipt")
            receipt = json.loads(receipt_path.read_text())
            if (receipt.get("passed") is not True or receipt.get("phase") != "null"
                    or receipt.get("source_commit") != plan["source_commit"]
                    or receipt.get("arch") != plan["arch"]
                    or receipt.get("plak_binaries") != plan["arms"]["plak-codex"]["binaries"]):
                raise PreflightError("A/A receipt does not validate this source, platform and Plak binary")
        slice_name = {"calibration": "plumbing", "null": "ab", "codex": "expanded"}[mode]
    else:
        slice_name = mode
    with account_leases(arm["account"] for arm in plan["arms"].values()):
        return run(plan, slice_name)


def run(plan, slice_name) -> int:
    phase = plan.get("phase", "scored")
    (ROOT / ".plak/harbor/jobs").mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="campaign-", dir=ROOT / ".plak/harbor/jobs"))
    print(output, flush=True)
    attempted, final = [], {}
    slots = schedule(list(plan["arms"]), list(plan["tasks"]), plan["blocks"])
    for index, slot in enumerate(slots):
        for retry in range(1 if phase == "calibration" else 3):
            invocation = output / f"slot-{index}-try-{retry}"
            env = dict(os.environ, HARBOR_INVOCATION_DIR=str(invocation),
                       HARBOR_REPEAT_BLOCK=str(slot["block"]), HARBOR_N_ATTEMPTS="1",
                       HARBOR_N_CONCURRENT="1", HARBOR_INCLUDE_TASK=slot["task"],
                       HARBOR_COMPARISON_ID=os.environ["HARBOR_CAMPAIGN_SHA256"])
            # Nonzero agent exits may still have a valid verifier result.
            subprocess.run([str(ROOT / "eval/scripts/rung-a.sh"), slice_name, slot["arm"]],
                           env=env, check=False)
            report = build_report(discover([invocation]), roots=[invocation])
            if len(report["trials"]) > 1:
                raise RuntimeError("campaign stopped: ambiguous multiple trials in one slot")
            row = report["trials"][0] if report["trials"] else {
                "arm": slot["arm"], "task": slot["task"], "failure_class": "infra",
                "eligible_pass_rate": False, "validity_failures": ["result_missing"], "pass": None,
            }
            attempted.append(row)
            final[(slot["slot"], slot["arm"])] = row
            if phase == "calibration":
                if not row.get("evidence", {}).get("result"):
                    raise PreflightError("calibration plumbing produced no readable trial result")
                result = Path(row["evidence"]["result"])
                capture = json.loads((result.parent / "agent/observed-config.json").read_text())
                observed = capture.get("first_request") or {}
                served = any(item.get("response", {}).get("served_model") or
                             item.get("response", {}).get("system_fingerprint")
                             for item in capture.get("requests", []))
                if (capture.get("source") != "exchange" or not capture.get("wire_verified")
                        or not observed.get("model") or observed.get("effort") in (None, "none", "omit")
                        or not served):
                    raise PreflightError("calibration has no actual first wire request")
                if slot["arm"].startswith("plak-"):
                    composition = resolved_composition((result.parent / "agent/plak-stderr.log").read_text())
                    if composition is None:
                        raise PreflightError("calibration has no observed shipped Plak composition")
                    capture["resolved_composition"] = composition
                capture["calibration_binding"] = {
                    "source_commit": plan["source_commit"], "arch": plan["arch"],
                    "binaries": plan["arms"][slot["arm"]]["binaries"],
                }
                destination = output / "calibrations"
                destination.mkdir(exist_ok=True)
                private_json(destination / (slot["arm"] + ".json"), capture)
                if row["failure_class"] != "agent" or row.get("pass") is not True:
                    raise PreflightError("wire capture saved, but the plumbing verifier did not pass")
                break
            if row["failure_class"] == "agent" and row["validity_failures"]:
                raise RuntimeError("campaign stopped: trial failed the post-run validity gate")
            if row["failure_class"] != "infra":
                break
        next_block = slots[index + 1]["block"] if index + 1 < len(slots) else None
        if phase != "calibration" and next_block != slot["block"]:
            blockers = campaign_health(attempted)
            if blockers:
                (output / "halt.json").write_text(json.dumps({"blockers": blockers}, indent=2) + "\n")
                return 2
    if phase == "calibration":
        private_json(output / "calibration-receipt.json", {"phase": phase, "scored": False,
                     "campaign_sha256": os.environ["HARBOR_CAMPAIGN_SHA256"], "trials": len(attempted)})
        return 0
    if phase == "null":
        arms = list(plan["arms"])
        tasks = {task: tuple([row["pass"] for (slot, arm), row in final.items()
                             if arm == name and row["task"] == task and row["eligible_pass_rate"]]
                            for name in arms) for task in plan["tasks"]}
        result = estimate(tasks)
        interval = result["task_population_ci95"]
        infra = {name: sum(r["failure_class"] == "infra" for r in attempted if r["arm"] == name) /
                 sum(r["arm"] == name for r in attempted) for name in arms}
        passed = (result["tasks"] == 8 and interval is not None and interval[0] <= 0 <= interval[1]
                  and all(rate < .05 for rate in infra.values()))
        private_json(output / "null-receipt.json", {"phase": "null", "passed": passed,
                     "campaign_sha256": os.environ["HARBOR_CAMPAIGN_SHA256"],
                     "source_commit": plan["source_commit"], "arch": plan["arch"],
                     "estimate": result, "infra": infra,
                     "plak_binaries": plan["arms"]["plak-codex"]["binaries"]})
        if not passed:
            print("A/A refused: require eight tasks, CI covering zero and <5% infra per arm", file=sys.stderr)
            return 2
    selected = []
    for slot in sorted({key[0] for key in final}):
        group = [final[(slot, arm)] for arm in plan["arms"]]
        # Exhausted infra slots are dropped symmetrically, never replaced by
        # a different repeat. Unknown and missing verifier slots block too.
        if not all(row["eligible_pass_rate"] for row in group):
            continue
        for row in group:
            selected.append({"arm": row["arm"], "slice": slice_name, "task_id": row["task"],
                             "slot": slot, "source_commit": plan["source_commit"],
                             "result_json_path": row["evidence"]["result"],
                             "result_json_sha256": row["result_sha256"]})
    (output / "selection.json").write_text(json.dumps({"schema": "paired-slots-v1",
        "record_count": len(selected), "records": selected}, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
