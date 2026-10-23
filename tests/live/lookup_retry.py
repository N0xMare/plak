#!/usr/bin/env python3
"""Opt-in real-turn proof that a first model lookup fault is absorbed.

Usage: lookup_retry.py ROUTE --fault connect|503 [--binary PATH]
A fresh private product home avoids product list caches. Subscription auth is
loaded by the product's normal external credential store, never copied here.
"""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import live

CATALOGS = {
    "codex": ("chatgpt.com", "/backend-api/codex/models"),
    "grok": ("cli-chat-proxy.grok.com", "/v1/models"),
    "openrouter": ("openrouter.ai", "/api/v1/models"),
    "openai": ("api.openai.com", "/v1/models"),
    "anthropic": ("api.anthropic.com", "/v1/models"),
    "xai-api": ("api.x.ai", "/v1/models"),
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("route", choices=sorted(live.ROUTES))
    parser.add_argument("--fault", choices=("connect", "503"), required=True)
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    args = parser.parse_args()
    route = live.ROUTES[args.route]
    if route.account not in CATALOGS:
        parser.error("this route has no network model catalog")
    binary = args.binary.resolve()
    mismatch = live.capture_root_mismatch(binary)
    if mismatch:
        parser.error(mismatch)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    artifacts = live.ROOT / "target/live" / f"lookup-{args.route}-{args.fault}-{stamp}"
    env = live.Env(binary, artifacts / "home", artifacts)
    env.route = route
    live.write_home(env.home, route)
    repo = live.git_repo(artifacts / "repo", {"README.md": "Lookup retry proof.\n"})
    host, path = CATALOGS[route.account]
    faults = f"host={host},path={path},nth=1,fault={args.fault}"
    log = artifacts / "run.log"
    before = live.binary_sha256(binary)
    code, out = live.run_cli(env, repo, [*route.flags(), "--print-session-id", "--",
        "Write lookup-proof.txt containing exactly LOOKUP_OK, then reply DONE."], log, faults=faults)
    text = log.read_text(errors="replace")
    if code:
        live.classify_cli_failure(code, text, "lookup retry", segments=env.capture_segments, lookup_faults=faults)
    if "retrying model lookup (2/3):" not in text:
        raise live.ScenarioFailure(live.HARNESS, "fault did not exercise the model lookup retry")
    if route.account in {"openai", "anthropic", "xai-api"} and not any((env.home / "cache/api-models").glob("*.json")):
        raise live.ScenarioFailure(live.HARNESS, "API model list fell back instead of caching successful lookup evidence")
    session = next((line.split("=", 1)[1] for line in out.splitlines()
                    if line.startswith("session_id=")), None)
    if session is None:
        raise live.ScenarioFailure(live.HARNESS, "turn completed without session id")
    records = live.session_records(env.home, session)
    terminals = [row for row in records if row.get("kind") == "turn.lifecycle"
                 and row.get("phase") != "admitted"]
    if not terminals or terminals[-1].get("phase") != "completed":
        raise live.ScenarioFailure(live.HARNESS, "turn has no completed durable terminal")
    proof = repo / "lookup-proof.txt"
    if not proof.is_file() or proof.read_text().strip() != "LOOKUP_OK":
        raise live.ScenarioFailure(live.MODEL, "turn completed without the requested file")
    if live.binary_sha256(binary) != before:
        raise live.ScenarioFailure(live.HARNESS, "executable changed during the run")
    report = {"route": args.route, "fault": faults, "outcome": live.PASS,
              "session_id": session, "binary_sha256": before}
    (artifacts / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
