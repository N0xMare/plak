#!/usr/bin/env python3
"""Run the same synthetic workload before and after a same-key restart upgrade."""

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
from collections import Counter
from pathlib import Path

from host_latency_fixture import Fixture, answer, create, tool, tree_script, turn
from host_latency_wire import LOGIN, Client
from staging_soak_evidence import (
    FAST_LATENCY_LIMITS_MS,
    METRICS,
    SUBSTITUTIONS,
    classify_cycle,
    compare,
    digest,
    harness_identity,
    known_defect_pairs,
    latency_reasons,
    pair,
    policy,
)
from staging_soak_measure import (
    ATTACH_PERIOD_S,
    MAX_LOAD_PER_CPU,
    ROLES,
    SLOW_ATTACH_PERIOD_S,
    WIRE_ERRORS,
    Browser,
    Drafts,
    LoadExcursion,
    check_load,
    cpu_facts,
    drive,
    durable_texts,
    login,
    timestamp,
)
from staging_soak_stream import CHUNKS, DELTA_MS, QUICK_CHUNKS, scripted_turn

PARKED = 2
WORK_ROOTS = 16
WEB_CLIENTS = 33
SLOW_CLIENTS = 3
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


def work_plan(fixture, args):
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
    rounds = (2 * (args.seconds + args.warmup)) // args.work_roots + 6
    plan(
        fixture,
        [part for turn in range(rounds) for part in scripted_turn(step, turn, args.chunks)],
    )


def park_plan(fixture, seconds):
    steps, _ = tree_script(0, PARKED + 1, 512)
    plan(
        fixture,
        steps[:PARKED]
        + [
            tool("wait_agent", agent_ids=["c1", "c2"], timeout_ms=1200_000),
            answer("synthetic children settled"),
        ],
        child_hold_ms=(seconds + 22) * 1000,
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


def exact_load(window, seconds, args):
    fast_rotating = args.clients - args.work_roots - args.slow - 1
    attaches = (
        fast_rotating * seconds // ATTACH_PERIOD_S + args.slow * seconds // SLOW_ATTACH_PERIOD_S
    )
    return (
        window["request_failed"] == window["fell_behind"] == 0
        and window["active_reader_closes"] == 0
        and window["steps"] == window["first_event_n"] == window["scheduled_steps"] == 2 * seconds
        and window["streamed_steps"] == window["steps"]
        and window["attach_n"] == window["scheduled_attaches"] == attaches
        and window["ping_n"] == (args.clients - args.slow - 1) * seconds // ATTACH_PERIOD_S
        and (args.quick or window["heavy_attach_while_streaming_n"] >= 2)
    )


def window_reasons(window, args, *, allow_known_defect=False):
    blocking = []
    if not exact_load(window, args.seconds, args):
        blocking.append("incomplete offered load")
    if not window["warmup_clean"]:
        blocking.append("incomplete warmup load")
    if not args.quick and not window["stalled_reader_backpressured"]:
        blocking.append("stalled reader never backpressured")
    if window["persisted_successful_steps"] != window["steps"]:
        blocking.append(
            f"durable steps {window['persisted_successful_steps']} != wire steps {window['steps']}"
        )
    if window["reference_mismatches"]:
        blocking.append(f"reference mismatches: {window['reference_mismatches']}")
    if window["roots_with_final_text"] != args.work_roots:
        blocking.append("missing durable text for a streaming root")
    for metric in METRICS:
        if window[metric] is None:
            blocking.append(f"{metric}: no samples")
    evidence, timing = latency_reasons(
        window, require_stamps=args.require_candidate_send_stamps and window["side"] == "B"
    )
    blocking.extend(evidence)
    pairs = 0
    if window["reconcile_failures"]:
        if allow_known_defect and not (blocking or timing):
            pairs = known_defect_pairs(window)
        if not pairs:
            blocking.append(f"reconcile mismatches: {window['reconcile_failures']}")
    return blocking, timing, pairs


async def phase(fixture, roles, args, heavy, work, parked, image, which, load_limit):
    processes = certify(fixture, roles, image)
    generation = await fresh(fixture, image["binary_id"])
    duration = args.seconds + args.warmup
    async with contextlib.AsyncExitStack() as stack:
        work_plan(fixture, args)
        workload = []
        for node in work:
            drafts = Drafts(args.chunks)
            client = await stack.enter_async_context(
                Client(fixture, "terminal", observe=drafts.observe)
            )
            state, _ = await client.attach(node)
            workload.append((client, state, node, drafts))
        park_plan(fixture, duration)
        parent = await stack.enter_async_context(Client(fixture, "terminal"))
        state, _ = await parent.attach(parked)
        # All reopened sessions see the common binding facts, not park's steps.
        work_plan(fixture, args)
        await parent.intent("park the synthetic children")
        await fixture.witness_wait(parked, PARKED)
        browsers = []
        for i in range(args.clients):
            fixed = work[i] if i < len(work) else None
            if i == args.clients - 1:
                cohort = "stalled"
            else:
                cohort = "slow" if i >= args.clients - args.slow - 1 else "fast"
            peer = Browser(fixture, i, cohort, args.chunks, fixed=fixed)
            await stack.enter_async_context(peer.client)
            if fixed is not None:
                await peer.attach(fixed)
            elif cohort == "stalled":
                await peer.stall(heavy[0])
            browsers.append(peer)
        targets = [*heavy, *work[2:6]]
        pids = {role: roles[role].pid for role in ROLES}
        print(json.dumps({"window": which, "event": "warmup_started"}), flush=True)
        warm = await drive(
            fixture,
            pids,
            args.warmup,
            browsers,
            targets,
            workload,
            load_limit,
            warmup=True,
        )
        # A well-isolated host may already have closed the warmup's stalled
        # socket. Re-offer the stalled attach at the measured boundary.
        browsers[-1].client.ws.transport.abort()
        await browsers[-1].client.close()
        stalled = Browser(fixture, args.clients - 1, "stalled", args.chunks)
        await stack.enter_async_context(stalled.client)
        await stalled.stall(heavy[0])
        browsers[-1] = stalled
        steps_before = completed_tools(fixture, work)
        print(json.dumps({"window": which, "event": "measurement_started"}), flush=True)
        result = await drive(fixture, pids, args.seconds, browsers, targets, workload, load_limit)
        if state.terminal or fixture.counts(parked)["active_children"] != PARKED:
            raise RuntimeError("parked children did not stay parked throughout the window")
        durable = durable_texts(fixture, work)

        async def verify_final(peer):
            root = peer.fixed["tree_id"]
            trace = peer.traces[peer.client.active]
            # The driver's terminal witness can arrive ahead of the viewer's
            # canonical patch. Drain that patch outside the timed interval.
            try:
                await peer.client.wait(
                    lambda: bool(durable[root]) and trace.settled == durable[root],
                    timeout=2,
                )
            except WIRE_ERRORS:
                trace.issue(
                    "final_root_text_mismatch",
                    wire=len(trace.settled),
                    durable=len(durable[root]),
                )
            if (
                not trace.counts.assistant_frames
                or not trace.counts.reasoning_frames
                or not trace.counts.tool_upserts
            ):
                trace.issue("missing_workload_witness")

        await asyncio.gather(*(verify_final(peer) for peer in browsers if peer.fixed is not None))
        references = {
            node["tree_id"]: draft.reference.checkpoints for _, _, node, draft in workload
        }
        attachments = [row for peer in browsers for row in peer.receipts(durable, references)]
        reconcile_reasons = Counter()
        for attachment in attachments:
            reconcile_reasons.update(attachment["reconcile_reasons"])
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
            warmup_clean=exact_load(warm, args.warmup, args),
            warmup_failures=warm["request_failed"] + warm["fell_behind"],
            warmup_load_1m_max=warm["load_1m_max"],
            warmup_load_samples=warm["load_samples"],
            attachments=attachments,
            reconcile_failures=sum(row["mismatches"] for row in attachments),
            reconcile_reasons=dict(reconcile_reasons),
            roots_with_final_text=sum(bool(texts) for texts in durable.values()),
            reference_mismatches=sum(
                draft.reference.counts.mismatches for _, _, _, draft in workload
            ),
        )
        result["stalled_reader_backpressured"] = browsers[-1].client.ws.recv_messages.paused
        certify(fixture, roles, image)
        return result


async def restart(fixture, roles, before, expected):
    """The restart upgrade: a planned stop of the source host, then the
    candidate image on the same home and stores."""
    await fixture.stop(roles["host"])
    roles["host"] = await start_host(fixture)
    identity(roles["host"].pid, fixture.binary, expected["sha256"])
    after = await fresh(fixture, expected["binary_id"])
    if after == before:
        raise RuntimeError("the restart did not advance the wire host generation")
    return {
        "kind": "restart",
        "pid": roles["host"].pid,
        "source_generation": before,
        "candidate_generation": after,
    }


def skip_sockets(directory, names):
    return [name for name in names if stat.S_ISSOCK((Path(directory) / name).lstat().st_mode)]


async def new_window(fixture, args, measured, side, load_limit):
    check_load(load_limit)
    fixture.binary = args.source
    roles = await start_auxiliaries(fixture)
    work_plan(fixture, args)
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
        transition = await restart(fixture, roles, before, measured[1])
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
    cpu = cpu_facts()
    bounds, load_limit, nightly = (
        policy(args.policy) if args.policy else (None, MAX_LOAD_PER_CPU * cpu["cpu_count"], {})
    )
    allow_known_defect = args.nightly and nightly.get("allow_known_late_prefix", False)
    timing_qualified = nightly.get("timing_qualified", True)
    check_load(load_limit)
    verdict.update(
        source=shipping[0],
        candidate=shipping[1],
        measured_source=measured[0],
        measured_candidate=measured[1],
        harness=harness_identity(args.harness_revision),
        cpu=cpu,
        load_1m_limit=load_limit,
        policy_sha256=digest(args.policy) if args.policy else None,
    )
    fixture = Fixture(args.scratch, args.source)
    fixture.host_log += ",plak::test_wire=info"
    fixture.soak_enqueue = True
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
            "stalled_clients": 1,
            "work_roots": args.work_roots,
            "stream_chunks": args.chunks,
            "stream_delta_ms": DELTA_MS,
            "seconds": args.seconds,
            "warmup_seconds": args.warmup,
            "process_output_bytes": PROCESS_OUTPUT_BYTES,
        }
        await fixture.close()
        golden = args.scratch.with_name("golden")
        shutil.copytree(fixture.root, golden, ignore=skip_sockets, symlinks=True)
        windows, blocking, timing = [], [], []
        known_pairs = 0
        verdict["windows"] = windows
        for index, side in enumerate(ORDER, 1):
            label = f"{side}{index}"
            verdict["active_window"] = label
            check_load(load_limit)
            shutil.rmtree(fixture.root)
            shutil.copytree(golden, fixture.root, symlinks=True)
            roles, image, transition = await new_window(fixture, args, measured, side, load_limit)
            result = await phase(
                fixture,
                roles,
                args,
                heavy,
                [*heavy, *other[: args.work_roots - 2]],
                other[args.work_roots - 2],
                image,
                label,
                load_limit,
            )
            result["handoff"] = transition
            windows.append(result)
            print(
                json.dumps({k: v for k, v in result.items() if k != "attachments"}),
                flush=True,
            )
            failures, regressions, pairs = window_reasons(
                result, args, allow_known_defect=allow_known_defect
            )
            blocking.extend(f"{label} {reason}" for reason in failures)
            timing.extend(f"{label} {reason}" for reason in regressions)
            known_pairs += pairs
            await fixture.close()
            saved = args.scratch.with_name(f"logs-{label}")
            saved.mkdir()
            for path in [*fixture.root.glob("*.log"), fixture.diagnostics]:
                shutil.copy2(path, saved / path.name)
        verdict.pop("active_window")
        if all(w[key] is not None for w in windows for key in METRICS):
            summary, deltas, regressions = compare(windows, None if args.aa else bounds)
            verdict.update(before=summary["A"], after=summary["B"], deltas=deltas)
            timing.extend(regressions)
        result, status = classify_cycle(
            blocking,
            timing,
            known_pairs,
            nightly=args.nightly,
            timing_qualified=timing_qualified,
            aa=args.aa,
        )
        reasons = blocking + timing
        if known_pairs:
            reasons.append(f"known late-attach prefix repairs: {known_pairs}")
        if args.quick:
            reasons.append("smoke_only")
            result = "fail"
        verdict.update(
            reasons=reasons,
            result=result,
            timing_reasons=timing,
            known_defect_count=known_pairs,
        )
        return status
    finally:
        await fixture.close()


def run_exercise(args, verdict):
    try:
        return asyncio.run(exercise(args, verdict))
    except* LoadExcursion as group:
        excursion = group.exceptions[0]
        while isinstance(excursion, BaseExceptionGroup):
            excursion = excursion.exceptions[0]
        verdict.update(
            result="inconclusive",
            reasons=[str(excursion)],
            load_excursion={"measured": excursion.measured, "limit": excursion.limit},
        )
    return 75


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
    parser.add_argument("--nightly", action="store_true", help="apply the separate nightly policy")
    parser.add_argument(
        "--require-candidate-send-stamps",
        action="store_true",
        help="refuse a candidate without enqueue timing for every sampled web text frame",
    )
    args = parser.parse_args()
    if (not args.aa or args.nightly) and not args.policy:
        parser.error("A/B and nightly runs require --policy")
    if args.aa and args.policy and not args.nightly:
        parser.error("standalone A/A omits --policy")
    if args.nightly and args.quick:
        parser.error("nightly evidence requires the full workload")
    if args.nightly:
        args.require_candidate_send_stamps = True
    args.seconds, args.clients, args.slow = (
        (8, 8, 1) if args.quick else (48, WEB_CLIENTS, SLOW_CLIENTS)
    )
    args.warmup = 4 if args.quick else 12
    args.work_roots, args.chunks = (4, QUICK_CHUNKS) if args.quick else (WORK_ROOTS, CHUNKS)
    args.roots, args.children = (8, 2) if args.quick else (74, 60)
    args.history_turns, args.prompt_bytes = (2, 4096) if args.quick else (48, 192 << 10)
    verdict = {
        "schema": 3,
        "mode": "test_support_same_revision",
        "calibration": args.aa,
        "nightly": args.nightly,
        "quick": args.quick,
        "result": "error",
        "started_at": timestamp(),
        "substitutions": SUBSTITUTIONS,
        "fast_latency_limits_ms": FAST_LATENCY_LIMITS_MS,
        "require_candidate_send_stamps": args.require_candidate_send_stamps,
    }
    os.umask(0o077)
    with args.verdict.open("x") as output:
        try:
            status = run_exercise(args, verdict)
            verdict["finished_at"] = timestamp()
            serialized = json.dumps(verdict, indent=2, sort_keys=True, allow_nan=False)
        except BaseException as error:
            status = 1
            verdict["result"] = "error"
            verdict["error"] = f"{type(error).__name__}: {error}"
            verdict["finished_at"] = timestamp()
            traceback.print_exception(error)
            # A serialization error must not leave a partial passing document.
            serialized = json.dumps(verdict, indent=2, sort_keys=True)
        output.write(serialized + "\n")
    return status


if __name__ == "__main__":
    sys.exit(main())
