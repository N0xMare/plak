#!/usr/bin/env python3
"""Run the same synthetic workload before and after a native same-key handoff."""

import argparse
import asyncio
import contextlib
import json
import os
import shutil
import socket
import stat
import sys
import traceback
from pathlib import Path

from host_latency_fixture import Fixture, answer, create, tool, tree_script, turn
from host_latency_wire import LOGIN, Client
from staging_soak_evidence import SUBSTITUTIONS, compare, digest, harness_identity, pair, policy
from staging_soak_measure import (
    ROLES,
    Drafts,
    browser,
    check_load,
    cpu_facts,
    drive,
    login,
    timestamp,
)

PARKED = 2
WORK_ROOTS = 4
PROCESS_OUTPUT_BYTES = 6144
ORDER = "ABBA"


def setup(fixture, clients):
    fixture.init()
    config = fixture.home / "config.toml"
    names = json.dumps([LOGIN, *(login(i) for i in range(clients))])
    config.write_text(config.read_text().replace(json.dumps([LOGIN]), names))


def plan(fixture, steps, **dials):
    fixture.write_script(
        steps,
        tree=True,
        durable_tree=True,
        tree_process_tools=True,
        **dials,
    )


async def start_host(fixture):
    log = fixture.root / "host.log"
    offset = log.stat().st_size if log.exists() else 0
    host = await fixture.spawn(
        "host", "--web", f"unix:{fixture.socket}", "--ui", str(fixture.root / "ui")
    )

    def ready():
        with log.open() as lines:
            lines.seek(offset)
            return "plakd host: serving on " in lines.read()

    await fixture.ready(host, ready)
    return host


async def start_auxiliaries(fixture):
    gateway = await fixture.spawn("gateway", "serve")
    await fixture.ready(gateway, fixture.gateway.exists)
    runner = await fixture.spawn("runner", "serve", "--local")
    fixture.runner_pid = runner.pid
    await fixture.ready(runner, lambda: any(fixture.home.glob("host/runner/*/run/r.sock")))
    return {"gateway": gateway, "runner": runner}


async def seed(fixture, args, count):
    steps, batches = tree_script(args.history_turns, args.children + 1, 32 << 10)
    plan(fixture, steps)
    async with Client(fixture, "terminal") as terminal:
        heavy = []
        for root_index in range(2):
            node = await create(terminal, fixture.workspace)
            attached, _ = await terminal.attach(node)
            for ordinal in range(args.history_turns):
                await turn(
                    terminal,
                    attached,
                    f"synthetic history {root_index}:{ordinal}: " + "x" * args.prompt_bytes,
                )
            if root_index == 0:
                for batch in range(batches):
                    await turn(terminal, attached, f"retain synthetic children batch {batch}")
            await terminal.detach()
            heavy.append(node)
        extra = [await create(terminal, fixture.workspace) for _ in range(count - 2)]
    shape = [fixture.counts(node) for node in heavy]
    if shape[0]["retained_children"] != args.children or shape[0]["active_children"]:
        raise RuntimeError("the retained child fixture did not settle")
    if not args.quick and any(row["transcript_row_bytes"] < 8 << 20 for row in shape):
        raise RuntimeError("the large session transcript fixture was not retained")
    return heavy, extra, shape


def work_plan(fixture, seconds):
    step = tool(
        "process_session_start",
        command="printf '%06143d\\n' 0",
        workdir=".",
        login=False,
        tty=False,
        keep_stdin_open=False,
        yield_time_ms=1000,
        max_output_bytes=8192,
    )
    # The same session-independent script is used on each side, including reopen.
    plan(fixture, [step, answer("synthetic step completed")] * (2 * seconds + 20))


def park_plan(fixture, seconds):
    steps, _ = tree_script(0, PARKED + 1, 512)
    plan(
        fixture,
        steps[:PARKED]
        + [
            tool("wait_agent", agent_ids=["c1", "c2"], timeout_ms=1200_000),
            answer("synthetic children settled"),
        ],
        child_hold_ms=(seconds + 15) * 1000,
    )


def completed_tools(fixture, nodes):
    return sum(
        fixture.query(
            "product",
            "select count(*) from transcript_rows where session_id=? "
            "and json_extract(row,'$.name')='process_session_start' "
            "and json_extract(row,'$.state')='succeeded'",
            node["tree_id"],
        )[0]
        for node in nodes
    )


def identity(pid, binary, expected):
    exe = Path("/proc") / str(pid) / "exe"
    if exe.resolve(strict=True) != binary:
        raise RuntimeError("serving process image differs from expected binary")
    if digest(exe) != expected:
        raise RuntimeError("serving process image digest differs from expected binary")


async def fresh(fixture, binary_id):
    async with Client(fixture, "terminal") as client:
        # An uninstalled image can advertise before its background identity
        # cache is populated. The caller verified the mapped executable.
        if client.welcome["binary_id"] not in (binary_id, "unknown"):
            raise RuntimeError("wire Welcome named another serving image")
        return client.welcome["host_generation"]


def certify(fixture, roles, image):
    processes = {}
    for role, child in roles.items():
        identity(child.pid, fixture.binary, image["sha256"])
        processes[role] = {
            "pid": child.pid,
            "binary_id": image["binary_id"],
            "sha256": image["sha256"],
        }
    return processes


def exact_load(window, seconds, clients):
    return (
        window["request_failed"] == window["fell_behind"] == 0
        and window["steps"] == window["first_event_n"] == window["scheduled_steps"] == 2 * seconds
        and window["attach_n"] == window["scheduled_attaches"] == clients * seconds // 2
    )


async def phase(fixture, roles, args, heavy, work, parked, image, which, cpu):
    processes = certify(fixture, roles, image)
    generation = await fresh(fixture, image["binary_id"])
    duration = args.seconds + args.warmup
    async with contextlib.AsyncExitStack() as stack:
        primary = await stack.enter_async_context(Client(fixture, "terminal"))
        await primary.attach(heavy[0])
        second = await stack.enter_async_context(Client(fixture, "terminal"))
        await second.attach(heavy[1])
        work_plan(fixture, duration)
        workload = []
        for node in work:
            drafts = Drafts()
            client = await stack.enter_async_context(
                Client(fixture, "terminal", observe=drafts.observe)
            )
            state, _ = await client.attach(node)
            workload.append((client, state, drafts))
        park_plan(fixture, duration)
        parent = await stack.enter_async_context(Client(fixture, "terminal"))
        state, _ = await parent.attach(parked)
        # All reopened sessions see the common binding facts, not park's steps.
        work_plan(fixture, duration)
        await parent.intent("park the synthetic children")
        await fixture.witness_wait(parked, PARKED)
        browsers = [
            await stack.enter_async_context(browser(fixture, i, args.slow))
            for i in range(args.clients)
        ]
        pids = {role: roles[role].pid for role in ROLES}
        print(json.dumps({"window": which, "event": "warmup_started"}), flush=True)
        warm = await drive(
            fixture, pids, args.warmup, browsers, args.slow, heavy, workload, cpu, warmup=True
        )
        steps_before = completed_tools(fixture, work)
        print(json.dumps({"window": which, "event": "measurement_started"}), flush=True)
        result = await drive(fixture, pids, args.seconds, browsers, args.slow, heavy, workload, cpu)
        if state.terminal or fixture.counts(parked)["active_children"] != PARKED:
            raise RuntimeError("parked children did not stay parked throughout the window")
        await parent.wait(lambda: state.terminal, timeout=45)
        # End the load before upgrading; retention of actively executing jobs
        # across a changed runner triple is not part of this same-key proof.
        async with asyncio.timeout(30):
            while fixture.counts(parked)["active_children"]:
                await asyncio.sleep(0.1)
        result.update(
            side=which[0],
            label=which,
            parked_children=PARKED,
            persisted_successful_steps=completed_tools(fixture, work) - steps_before,
            generation=generation,
            processes=processes,
            warmup_seconds=args.warmup,
            warmup_clean=exact_load(warm, args.warmup, args.clients),
            warmup_failures=warm["request_failed"] + warm["fell_behind"],
            warmup_load_1m_max=warm["load_1m_max"],
            warmup_load_samples=warm["load_samples"],
        )
        if result["persisted_successful_steps"] != result["steps"]:
            raise RuntimeError(f"wire turns and durable successful tool steps disagree: {result}")
        certify(fixture, roles, image)
        return result


async def handoff(fixture, host, before, expected):
    requester = await fixture.spawn(
        "host", "upgrade", "--expect-binary", expected["binary_id"], "--drain", "30s", pipes=True
    )
    reply, _ = await asyncio.wait_for(requester.communicate(), timeout=180)
    await fixture.stop(requester)
    if requester.returncode or reply.strip():
        raise RuntimeError("native same-key upgrade did not succeed; inspect the scratch host log")
    identity(host.pid, fixture.binary, expected["sha256"])
    after = await fresh(fixture, expected["binary_id"])
    if after == before:
        raise RuntimeError("native upgrade did not advance the wire host generation")
    return {
        "kind": "native_same_key",
        "pid": host.pid,
        "source_generation": before,
        "candidate_generation": after,
    }


def skip_sockets(directory, names):
    return [name for name in names if stat.S_ISSOCK((Path(directory) / name).lstat().st_mode)]


async def new_window(fixture, args, measured, side, cpu):
    check_load(cpu["cpu_count"])
    fixture.binary = args.source
    roles = await start_auxiliaries(fixture)
    work_plan(fixture, args.seconds + args.warmup)
    roles["host"] = await start_host(fixture)
    if side == "A":
        return roles, measured[0], None
    certify(fixture, roles, measured[0])
    before = await fresh(fixture, measured[0]["binary_id"])
    # No workload has opened a turn in this freshly restored home. Advance
    # auxiliaries before the host, without interrupting live runner jobs.
    await fixture.stop(roles["gateway"])
    await fixture.stop(roles["runner"])
    fixture.binary = args.candidate
    roles.update(await start_auxiliaries(fixture))
    if args.aa:
        # Identical binary has no native replacement to prove; emulate the
        # allocator/process restart but never admit this receipt to a cut.
        await fixture.stop(roles["host"])
        roles["host"] = await start_host(fixture)
        transition = {"kind": "same_image_restart"}
    else:
        transition = await handoff(fixture, roles["host"], before, measured[1])
    return roles, measured[1], transition


async def exercise(args, verdict):
    if set(socket.if_nameindex()) != {(1, "lo")}:
        raise ValueError("run the soak in an isolated Linux network namespace")
    shipping, measured = pair(
        (args.source_release, args.candidate_release),
        (args.source, args.candidate),
        args.build_profile,
        args.aa,
    )
    bounds = policy(args.policy) if args.policy else None
    cpu = cpu_facts()
    check_load(cpu["cpu_count"])
    verdict.update(
        source=shipping[0],
        candidate=shipping[1],
        measured_source=measured[0],
        measured_candidate=measured[1],
        harness=harness_identity(args.harness_revision),
        cpu=cpu,
        policy_sha256=digest(args.policy) if args.policy else None,
    )
    fixture = Fixture(args.scratch, args.source)
    setup(fixture, args.clients)
    try:
        plan(fixture, [answer("seed")])
        await fixture.serve()
        heavy, other, shape = await seed(fixture, args, args.roots)
        verdict["fixture"] = {
            "roots": args.roots,
            "heavy": shape,
            "web_clients": args.clients,
            "slow_clients": args.slow,
            "seconds": args.seconds,
            "warmup_seconds": args.warmup,
            "process_output_bytes": PROCESS_OUTPUT_BYTES,
        }
        await fixture.close()
        golden = args.scratch.with_name("golden")
        shutil.copytree(fixture.root, golden, ignore=skip_sockets, symlinks=True)
        windows, reasons = [], []
        verdict["windows"] = windows
        for index, side in enumerate(ORDER, 1):
            label = f"{side}{index}"
            shutil.rmtree(fixture.root)
            shutil.copytree(golden, fixture.root, symlinks=True)
            roles, image, transition = await new_window(fixture, args, measured, side, cpu)
            result = await phase(
                fixture,
                roles,
                args,
                heavy,
                other[:WORK_ROOTS],
                other[WORK_ROOTS],
                image,
                label,
                cpu,
            )
            result["handoff"] = transition
            windows.append(result)
            print(json.dumps(result), flush=True)
            if not exact_load(result, args.seconds, args.clients):
                reasons.append(f"{label}_incomplete_load")
            if not result["warmup_clean"]:
                reasons.append(f"{label}_incomplete_warmup")
            await fixture.close()
            saved = args.scratch.with_name(f"logs-{label}")
            saved.mkdir()
            for path in [*fixture.root.glob("*.log"), fixture.diagnostics]:
                shutil.copy2(path, saved / path.name)
        if not reasons:
            summary, deltas, regressions = compare(windows, bounds)
            verdict.update(before=summary["A"], after=summary["B"], deltas=deltas)
            reasons.extend(regressions)
        if args.quick:
            reasons.append("smoke_only")
        verdict.update(
            reasons=reasons,
            result="fail" if reasons else ("calibration" if args.aa else "pass"),
        )
    finally:
        await fixture.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in (
        "source_release",
        "candidate_release",
        "source",
        "candidate",
        "scratch",
        "verdict",
    ):
        parser.add_argument(name, metavar=name.replace("_", "-"), type=lambda p: Path(p).resolve())
    parser.add_argument(
        "--quick", action="store_true", help="short wiring smoke, cannot pass the gate"
    )
    parser.add_argument(
        "--aa", action="store_true", help="measure same-image noise, never qualifies"
    )
    parser.add_argument("--harness-revision", required=True)
    parser.add_argument("--build-profile", choices=["maintainer"], required=True)
    parser.add_argument("--policy", type=Path)
    args = parser.parse_args()
    if args.aa == bool(args.policy):
        parser.error("A/A omits --policy; A/B requires --policy")
    args.seconds, args.clients, args.slow = (8, 4, 1) if args.quick else (180, 12, 3)
    args.warmup = 4 if args.quick else 40
    args.roots, args.children = (8, 2) if args.quick else (74, 60)
    args.history_turns, args.prompt_bytes = (2, 4096) if args.quick else (48, 192 << 10)
    verdict = {
        "schema": 2,
        "mode": "test_support_same_revision",
        "calibration": args.aa,
        "quick": args.quick,
        "result": "error",
        "started_at": timestamp(),
        "substitutions": SUBSTITUTIONS,
    }
    os.umask(0o077)
    with args.verdict.open("x") as output:
        try:
            asyncio.run(exercise(args, verdict))
            verdict["finished_at"] = timestamp()
            serialized = json.dumps(verdict, indent=2, sort_keys=True, allow_nan=False)
        except BaseException as error:
            verdict["result"] = "error"
            verdict["error"] = f"{type(error).__name__}: {error}"
            verdict["finished_at"] = timestamp()
            traceback.print_exception(error)
            # A serialization error must not leave a partial passing document.
            serialized = json.dumps(verdict, indent=2, sort_keys=True)
        output.write(serialized + "\n")
    ok = verdict["result"] in ("pass", "calibration") or (
        args.quick and verdict.get("reasons") == ["smoke_only"]
    )
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
