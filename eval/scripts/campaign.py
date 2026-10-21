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

from harbor_adapter.campaign import campaign_gate, campaign_health, schedule
from harbor_adapter.report import build_report, discover


def main() -> int:
    plan = campaign_gate(ROOT)
    slice_name = sys.argv[1]
    (ROOT / ".plak/harbor/jobs").mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="campaign-", dir=ROOT / ".plak/harbor/jobs"))
    attempted, final = [], {}
    slots = schedule(list(plan["arms"]), list(plan["tasks"]), plan["blocks"])
    for index, slot in enumerate(slots):
        for retry in range(3):
            invocation = output / f"slot-{index}-try-{retry}"
            env = dict(os.environ, HARBOR_INVOCATION_DIR=str(invocation),
                       HARBOR_REPEAT_BLOCK=str(slot["block"]), HARBOR_N_ATTEMPTS="1",
                       HARBOR_N_CONCURRENT="1", HARBOR_INCLUDE_TASK=slot["task"],
                       HARBOR_COMPARISON_ID=os.environ["HARBOR_CAMPAIGN_SHA256"])
            # Nonzero agent exits may still have a valid verifier result.
            subprocess.run([str(ROOT / "eval/scripts/rung-a.sh"), slice_name, slot["arm"]],
                           env=env, check=False)
            report = build_report(discover([invocation]), roots=[invocation])
            if len(report["trials"]) != 1:
                raise RuntimeError("campaign stopped: expected one archived trial in slot")
            row = report["trials"][0]
            attempted.append(row)
            final[(slot["slot"], slot["arm"])] = row
            if row["failure_class"] == "agent" and row["validity_failures"]:
                raise RuntimeError("campaign stopped: trial failed the post-run validity gate")
            if row["failure_class"] != "infra":
                break
        next_block = slots[index + 1]["block"] if index + 1 < len(slots) else None
        if next_block != slot["block"]:
            blockers = campaign_health(attempted)
            if blockers:
                (output / "halt.json").write_text(json.dumps({"blockers": blockers}, indent=2) + "\n")
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
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
