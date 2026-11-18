#!/usr/bin/env python3
"""Credential-gateway live matrix: the offline plan and the opt-in paid run.

`just live gateway` prints the plan and a `just live estimate` line. It does
not build and calls no provider.

`just live gateway --paid` drives the shared live route drivers (`live.py`:
routes, private homes, `plakd run`) through a host in two topologies:

1. in-process: a host home with no `[gateway] address` holds its copied API
   keys and the machine's subscription logins itself;
2. unix-socket: `plakd gateway serve` adopts that first home and the machine's
   subscription store in place, and a second host home, holding no key, sets
   `[gateway] address` to the service's socket.

Each topology runs one turn per route, restarts its gateway (a new in-process
holder, or a new service instance), reopens every session, and forces one
refresh of each OAuth kind. The unix-socket topology verifies the refreshed
logins through doctor and status, and records the gateway audit line count of
each turn and reopen. A binary without `plakd gateway serve`
fails as a harness error before any provider call. Artifacts land in
target/live/gateway-<stamp>/; homes are retained and listed in homes.json.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import re
import subprocess
import sys
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402
from live import HARNESS, MODEL, PASS, ScenarioFailure  # noqa: E402

# One gateway turn is one smoke scenario's share of the measured smoke budget
# (tests/live/estimate.py). Reopen is a second short turn. Refresh is not
# inference.
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
IN_PROCESS = "in-process"
UNIX_SOCKET = "unix-socket"
TOPOLOGIES = (
    (IN_PROCESS, "in-process: a host with no [gateway] address holds the logins and keys"),
    (UNIX_SOCKET, "unix-socket: plakd gateway serve on the same machine"),
)
PER_TOPOLOGY = (
    "one turn per route",
    "reopen every session across a gateway restart",
    "forced refresh of each OAuth kind",
)
NOT_DRIVEN = (
    "not driven: tailnet TCP from a second machine, remote login, the kill switch, "
    "hop=gateway faults"
)

REMEMBER = "Remember the word MAGENTA. Reply: ok"
RECALL = "What word did I ask you to remember? Reply with the word only."
TURN_TIMEOUT_S = 300.0
SERVE_READY_S = 30.0
# `drain` waits up to 30 seconds for in-flight work before the service exits.
SERVE_STOP_S = 35.0
_CHECK_LINE = re.compile(r"^gateway (\S+) protocol ", re.MULTILINE)


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
    for _, description in TOPOLOGIES:
        lines.append(f"  - {description}")
        for step in PER_TOPOLOGY:
            lines.append(f"      {step}")
    lines.append(
        "gateway: plakd gateway serve adopts the in-process home and the machine's "
        "subscription store in place; the second host sets [gateway] address to its socket"
    )
    lines.append("each unix-socket turn and reopen records the gateway audit line count")
    lines.append("unix-socket doctor and status must report every OAuth kind Ready")
    lines.append(NOT_DRIVEN)
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


@dataclasses.dataclass
class Step:
    topology: str
    step: str
    subject: str
    outcome: str
    detail: str
    seconds: float = 0.0
    audit_lines: int | None = None


def serve_refusal(binary: Path) -> str | None:
    """Why `binary` cannot run the unix-socket topology, before any provider call."""
    if not binary.is_file():
        return f"{binary} is missing"
    try:
        completed = subprocess.run(
            [str(binary), "gateway", "--help"],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"{binary} gateway --help could not run: {error}"
    if completed.returncode != 0 or "gateway serve" not in completed.stdout:
        return f"{binary} has no `plakd gateway serve`"
    return None


def cli_env(home: Path) -> dict[str, str]:
    """A gateway CLI child's environment: no provider key and no capture."""
    dropped = {"PLAK_TEST_FAULTS", "PLAK_HOME", "PLAK_TEST_EXCHANGE_CAPTURE_DIR"}
    dropped |= set(live.API_KEY_ENV.values())
    env = {key: value for key, value in live.plak_child_source().items() if key not in dropped}
    env["PLAK_HOME"] = str(home)
    return env


def gateway_cli(binary: Path, home: Path, args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(binary), *args],
        cwd=home,
        env=cli_env(home),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )


def stderr_of(log: Path) -> str:
    text = log.read_text() if log.exists() else ""
    return text.split("--- stderr\n", 1)[1] if "--- stderr\n" in text else text


class Serve:
    """`plakd gateway serve` over `home`, checked from the host home `host`."""

    def __init__(self, binary: Path, home: Path, host: Path, logs: Path):
        self.binary, self.home, self.host, self.logs = binary, home, host, logs
        self.process: subprocess.Popen | None = None
        self.starts = 0

    def start(self) -> str:
        """Starts the service; returns its instance once the host reaches it."""
        self.starts += 1
        log = self.logs / f"serve-{self.starts}.log"
        with log.open("w") as sink:
            self.process = subprocess.Popen(
                [str(self.binary), "gateway", "serve"],
                cwd=self.home,
                env=cli_env(self.home),
                stdin=subprocess.DEVNULL,
                stdout=sink,
                stderr=subprocess.STDOUT,
            )
        deadline = time.monotonic() + SERVE_READY_S
        while time.monotonic() < deadline:
            code = self.process.poll()
            if code is not None:
                raise ScenarioFailure(
                    HARNESS,
                    f"plakd gateway serve exited {code}: {live.failure_lines(log.read_text())}",
                )
            check = gateway_cli(self.binary, self.host, ["gateway", "check"])
            match = _CHECK_LINE.search(check.stdout) if check.returncode == 0 else None
            if match:
                return match[1]
            time.sleep(0.2)
        self.stop()
        raise ScenarioFailure(
            HARNESS, f"plakd gateway serve did not answer within {SERVE_READY_S:.0f}s"
        )

    def stop(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=SERVE_STOP_S)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


class GatewayRun:
    def __init__(self, env: live.Env, artifacts: Path):
        self.env, self.artifacts = env, artifacts
        self.steps: list[Step] = []

    def record(self, step: Step) -> None:
        self.steps.append(step)
        audit = "" if step.audit_lines is None else f" [audit +{step.audit_lines}]"
        print(
            f"{step.outcome:8} {step.topology} {step.step} {step.subject}: {step.detail}{audit}",
            flush=True,
        )

    def attempt(self, topology: str, step: str, subject: str, action, audit=None) -> Step:
        """Runs `action` as one step; `audit` counts gateway audit lines around it."""
        started = time.monotonic()
        before = audit() if audit else None
        try:
            outcome, detail = PASS, action()
        except ScenarioFailure as failure:
            outcome, detail = failure.outcome, str(failure)
        except Exception as error:  # a bug in this driver is a harness failure too
            outcome, detail = HARNESS, f"driver error: {error!r}"
            (self.artifacts / topology / f"{step}-{subject}.traceback.txt").write_text(
                traceback.format_exc()
            )
        result = Step(topology, step, subject, outcome, detail, time.monotonic() - started)
        if audit:
            after = audit()
            if before is None or after is None:
                result.outcome, result.detail = HARNESS, f"{detail}; gateway audit unreadable"
            else:
                result.audit_lines = after - before
        self.record(result)
        return result

    def turn(self, topology: str, home: Path, route: live.Route) -> str:
        """One remembered word on a fresh session; returns the session id."""
        work = self.artifacts / topology / route.name
        repo = live.git_repo(work / "repo", {"notes.txt": "credential gateway live turn\n"})
        log = work / "turn.log"
        code, out = live.run_cli(
            self.env,
            repo,
            [*route.flags(), "--print-session-id", "--", REMEMBER],
            log,
            timeout=TURN_TIMEOUT_S,
            home=home,
        )
        if code != 0:
            live.classify_cli_failure(code, stderr_of(log), f"{route.name} turn")
        session = next(
            (line.split("=", 1)[1] for line in out.splitlines() if line.startswith("session_id=")),
            None,
        )
        if not session:
            raise ScenarioFailure(HARNESS, f"{route.name} turn exited 0 without a session id")
        return session

    def reopen(self, topology: str, home: Path, route: live.Route, session: str) -> str:
        work = self.artifacts / topology / route.name
        log = work / "reopen.log"
        code, out = live.run_cli(
            self.env,
            work / "repo",
            ["--session", session, "--", RECALL],
            log,
            timeout=TURN_TIMEOUT_S,
            home=home,
        )
        if code != 0:
            live.classify_cli_failure(code, stderr_of(log), f"{route.name} reopen")
        if "MAGENTA" not in out.upper():
            raise ScenarioFailure(MODEL, f"{route.name} reopened without the remembered word")
        return "the reopened session kept the word"

    def refresh(self, topology: str, home: Path, kind: str) -> str:
        completed = gateway_cli(self.env.binary, home, ["refresh", kind])
        (self.artifacts / topology / f"refresh-{kind}.log").write_text(
            f"exit {completed.returncode}\n--- stdout\n{completed.stdout}"
            f"--- stderr\n{completed.stderr}"
        )
        if completed.returncode != 0:
            live.classify_cli_failure(completed.returncode, completed.stderr, f"refresh {kind}")
        if topology == UNIX_SOCKET and f"{kind}: credential=ready" not in completed.stdout:
            raise ScenarioFailure(HARNESS, f"the gateway's refresh of {kind} did not answer ready")
        return "forced refresh answered"

    def doctor_oauth(self, home: Path) -> str:
        completed = gateway_cli(self.env.binary, home, ["doctor"])
        (self.artifacts / UNIX_SOCKET / "doctor.log").write_text(
            f"exit {completed.returncode}\n--- stdout\n{completed.stdout}"
            f"--- stderr\n{completed.stderr}"
        )
        # Doctor also checks unrelated product setup. Require all three
        # gateway-backed OAuth rows regardless of those other results.
        for kind in OAUTH_KINDS:
            ready = re.search(
                rf"^\s*OAuth {kind}\s+OK\s+ready · record held by the gateway",
                completed.stdout,
                re.MULTILINE,
            )
            if ready is None:
                raise ScenarioFailure(HARNESS, f"doctor did not report {kind} Ready via gateway")
        return "all three OAuth rows OK (ready, held by gateway)"

    def credential_status(self, home: Path, kind: str) -> str:
        completed = gateway_cli(self.env.binary, home, ["status", kind])
        (self.artifacts / UNIX_SOCKET / f"status-{kind}.log").write_text(
            f"exit {completed.returncode}\n--- stdout\n{completed.stdout}"
            f"--- stderr\n{completed.stderr}"
        )
        if completed.returncode != 0:
            live.classify_cli_failure(completed.returncode, completed.stderr, f"status {kind}")
        if f"{kind}: credential=ready" not in completed.stdout.splitlines():
            raise ScenarioFailure(HARNESS, f"status did not report {kind} Ready via gateway")
        return "gateway status answered ready"

    def audit_lines(self, gateway_home: Path) -> int | None:
        completed = gateway_cli(self.env.binary, gateway_home, ["gateway", "audit"])
        if completed.returncode != 0:
            return None
        return sum(1 for line in completed.stdout.splitlines() if line.strip())

    def topology(self, topology: str, home: Path, restart, gateway_home: Path | None) -> None:
        (self.artifacts / topology).mkdir(parents=True, exist_ok=True)
        audit = (lambda: self.audit_lines(gateway_home)) if gateway_home else None
        sessions: dict[str, str] = {}
        for name in ROUTES:

            def turn(route=live.ROUTES[name]) -> str:
                sessions[route.name] = self.turn(topology, home, route)
                return f"session {sessions[route.name]}"

            self.attempt(topology, "turn", name, turn, audit)
        restarted = self.attempt(topology, "restart", "gateway", restart)
        for name in ROUTES:
            if name not in sessions or restarted.outcome != PASS:
                self.record(Step(topology, "reopen", name, HARNESS, "no session to reopen"))
                continue
            self.attempt(
                topology,
                "reopen",
                name,
                lambda route=live.ROUTES[name], session=sessions[name]: self.reopen(
                    topology, home, route, session
                ),
                audit,
            )
        for kind in OAUTH_KINDS:
            self.attempt(
                topology, "refresh", kind, lambda kind=kind: self.refresh(topology, home, kind)
            )
        if topology == UNIX_SOCKET:
            self.attempt(topology, "doctor", "oauth", lambda: self.doctor_oauth(home))
            for kind in OAUTH_KINDS:
                self.attempt(
                    topology,
                    "status",
                    kind,
                    lambda kind=kind: self.credential_status(home, kind),
                )


def write_homes(artifacts: Path, keyed: Path, keyless: Path, socket: Path) -> None:
    """The in-process home with every route's key, and the gateway host without one."""
    empty = artifacts / "no-keys"
    empty.mkdir(mode=0o700, exist_ok=True)
    for name in ROUTES:
        live.write_home(keyed, live.ROUTES[name])
        live.write_home(keyless, live.ROUTES[name], source_home=empty)
    # The service file stays in the home it adopts. The default socket under
    # XDG_RUNTIME_DIR is the machine's own gateway.
    (keyed / "gateway.toml").write_text(f'socket = "{socket}"\n')
    with (keyless / "config.toml").open("a") as config:
        config.write(f'\n[gateway]\naddress = "unix:{socket}"\n')


def run_paid(binary: Path, artifacts: Path) -> int:
    live.isolate_test_environment()
    refusal = serve_refusal(binary) or live.capture_root_mismatch(binary, live.ROOT)
    if refusal:
        sys.stderr.write(f"{HARNESS}: {refusal}; no provider call was made\n")
        return 2
    env = live.Env(binary, live.scratch_home(artifacts, IN_PROCESS), artifacts)
    host = live.scratch_home(artifacts, UNIX_SOCKET)
    write_homes(artifacts, env.home, host, env.home / "gateway.sock")
    binary_start = live.binary_sha256(binary)
    run = GatewayRun(env, artifacts)

    run.topology(IN_PROCESS, env.home, lambda: "each plakd run is a new in-process holder", None)

    (artifacts / UNIX_SOCKET).mkdir(parents=True, exist_ok=True)
    serve = Serve(binary, env.home, host, artifacts / UNIX_SOCKET)
    try:
        started = run.attempt(UNIX_SOCKET, "start", "gateway", lambda: f"instance {serve.start()}")
        if started.outcome == PASS:
            first = started.detail.removeprefix("instance ")

            def restart() -> str:
                serve.stop()
                second = serve.start()
                if second == first:
                    raise ScenarioFailure(HARNESS, "the restarted gateway kept its instance")
                return f"instance {first} -> {second}"

            run.topology(UNIX_SOCKET, host, restart, env.home)
    finally:
        serve.stop()

    binary_end = live.binary_sha256(binary)
    (artifacts / "report.json").write_text(
        json.dumps(
            {
                "homes": live.scratch_homes(artifacts),
                "binary": str(binary),
                "binary_sha256_start": binary_start,
                "binary_sha256_end": binary_end,
                "steps": [dataclasses.asdict(step) for step in run.steps],
            },
            indent=2,
        )
        + "\n"
    )
    print(f"\nartifacts: {artifacts}")
    passed = all(step.outcome == PASS for step in run.steps) and binary_start == binary_end
    return 0 if passed else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--paid",
        action="store_true",
        help="opt-in paid run through both topologies",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=live.ROOT / "target/release/plakd",
        help="test-hooks plakd built from this checkout, used only with --paid",
    )
    parser.add_argument("--artifacts", type=Path, help="fresh artifact directory, with --paid")
    args = parser.parse_args(argv)
    sys.stdout.write(plan_text())
    if not args.paid:
        return 0
    stamp = time.strftime("%Y%m%d-%H%M%S")
    artifacts = args.artifacts or live.ROOT / "target" / "live" / f"gateway-{stamp}"
    return run_paid(args.binary.resolve(), artifacts)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
