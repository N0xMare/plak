#!/usr/bin/env python3
"""Offline plan for the credential-gateway live matrix.

`just live gateway` prints the plan and a `just live estimate` line. It does
not build, and it does not call a provider. `just live gateway --paid` starts
`plakd gateway serve` when that command exists, under an isolated home, and
stops it again. It still makes no provider call: a host cannot dispatch
through the gateway yet.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

# One gateway turn is one smoke scenario's share of the measured smoke budget
# (tests/live/estimate.py). Reopen is a second short turn. Refresh, login, the
# kill switch and the hop=gateway fault run are not inference.
SMOKE_INPUT = 241_039
SMOKE_OUTPUT = 5_500
SMOKE_SCENARIOS = 11

ROUTES = (
    "codex",
    "claude-haiku55-low",
    "grok",
    "api-openai",
    "api-anthropic",
    "api-xai",
    "or-nemotron-free",
)
OAUTH_KINDS = ("codex", "claude", "grok")
API_ROUTES = ("api-openai", "api-anthropic", "api-xai", "or-nemotron-free")
TOPOLOGIES = (
    "in-process",
    "unix-socket on the same machine",
    "tailnet TCP on a second machine with a scoped key",
)
PER_TOPOLOGY = (
    "one turn per route",
    "reopen across a gateway restart",
    "forced refresh of each OAuth kind",
    "remote device-code login and paste login",
    "kill switch",
    "one hop=gateway fault run",
)


def plan_text() -> str:
    turn_in = SMOKE_INPUT // SMOKE_SCENARIOS
    turn_out = SMOKE_OUTPUT // SMOKE_SCENARIOS
    priced_turns = len(API_ROUTES) * len(TOPOLOGIES) * 2
    lines = [
        "credential gateway live plan",
        "routes: " + ", ".join(ROUTES),
        "oauth refresh: " + ", ".join(OAUTH_KINDS),
        "topologies:",
    ]
    for name in TOPOLOGIES:
        lines.append(f"  - {name}")
        for step in PER_TOPOLOGY:
            lines.append(f"      {step}")
    lines.append(
        "gateway: plakd gateway serve adopts the machine's existing test logins; "
        "the host sets [gateway] address"
    )
    lines.append("each route records the audit line count and certainty")
    lines.append("estimate: just live estimate --rate-card RATE_CARD --soak-row grok")
    lines.append(
        "gateway_api_turns "
        f"{priced_turns} "
        f"input {priced_turns * turn_in} output {priced_turns * turn_out}"
    )
    lines.append(
        "subscription quota, unpriced: codex, claude-haiku55-low, grok "
        f"({len(OAUTH_KINDS) * len(TOPOLOGIES) * 2} turns)"
    )
    lines.append("paid: just live gateway --paid")
    lines.append("no provider call in this plan")
    return "\n".join(lines) + "\n"


def paid_block(binary: Path) -> str:
    if not binary.is_file():
        return "plakd binary is missing; no provider call was made"
    try:
        completed = subprocess.run(
            [str(binary), "gateway", "--help"],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "plakd gateway could not be started; no provider call was made"
    if completed.returncode != 0 or "serve" not in completed.stdout:
        return "plakd gateway serve is not available; no provider call was made"
    return drive_serve(binary)


def drive_serve(binary: Path) -> str:
    """Start `gateway serve` in an isolated home and stop it.

    The child environment is not the caller's: no credential or provider key
    is copied. Provider turns wait until a host can dispatch through the
    gateway.
    """
    try:
        home = tempfile.TemporaryDirectory(prefix="plak-gateway-live-")
    except OSError:
        return "plakd gateway serve could not be started; no provider call was made"
    state = Path(home.name) / "state"
    try:
        state.mkdir(mode=0o700)
    except OSError:
        return "plakd gateway serve could not be started; no provider call was made"
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": home.name,
        "PLAK_HOME": home.name,
        "XDG_STATE_HOME": str(state),
        "PLAK_TEST_GATEWAY": "external",
    }
    try:
        process = subprocess.Popen(
            [str(binary), "gateway", "serve"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except OSError:
        return "plakd gateway serve could not be started; no provider call was made"
    try:
        try:
            process.wait(timeout=0.4)
        except subprocess.TimeoutExpired:
            return (
                "plakd gateway serve started and was stopped; "
                "provider turns wait until a host can dispatch through it; "
                "no provider call was made"
            )
        return "plakd gateway serve exited before listening; no provider call was made"
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--paid",
        action="store_true",
        help="opt-in paid run; refuses before any provider call",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path("target/release/plakd"),
        help="test-hooks plakd, used only with --paid",
    )
    args = parser.parse_args(argv)
    sys.stdout.write(plan_text())
    if not args.paid:
        return 0
    sys.stderr.write(paid_block(args.binary) + "\n")
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
