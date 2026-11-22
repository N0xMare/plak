"""Measure the host while roots await real foreground processes."""

import asyncio
import contextlib
import os
from pathlib import Path

from host_latency_fixture import Fixture, answer, create, tool
from host_latency_wire import Client


def thread_switches(pid):
    """Voluntary scheduling-out counts, a proxy for blocking wakeups."""
    out = {}
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            counters = dict(
                line.split(":", 1) for line in (task / "status").read_text().splitlines()
            )
            out[task.name] = int(counters["voluntary_ctxt_switches"])
        except (FileNotFoundError, ProcessLookupError):
            continue
    return out


async def sample(pids, seconds, roots, mode, emit, proc_sample):
    starts = {role: (proc_sample(pid), thread_switches(pid)) for role, pid in pids.items()}
    emit("process_wait_start", pids=pids, roots=roots, mode=mode)
    await asyncio.sleep(seconds)
    for role, pid in pids.items():
        a, switches = starts[role]
        b = proc_sample(pid)
        remaining = thread_switches(pid)
        if a[1] != b[1]:
            raise RuntimeError("process PID changed while measuring")
        elapsed = b[0] - a[0]
        wakes = sum(value - switches.get(tid, 0) for tid, value in remaining.items()) / elapsed
        emit(
            "process_wait",
            seconds=elapsed,
            roots=roots,
            mode=mode,
            role=role,
            cpu_percent=100 * (b[5] - a[5]) / os.sysconf("SC_CLK_TCK") / elapsed,
            reads_per_s=(b[2] - a[2]) / elapsed,
            disk_kb_per_s=(b[4] - a[4]) / elapsed / 1000,
            voluntary_switches_per_s=wakes,
            voluntary_switches_per_root_per_s=wakes / roots if roots else None,
        )


def command(interval):
    ready = 'printf "%s\n" "$$" > ".process-wait-$$"; '
    if not interval:
        return ready + "exec sleep 900"
    return (
        ready
        + f'n=0; while [ "$n" -lt {900 // interval} ]; do '
        + f"printf 'process-wait output\n'; sleep {interval}; n=$((n + 1)); done"
    )


async def wait_until_running(fixture, held):
    async with asyncio.timeout(45):
        while True:
            if any(client.failure or state.ended or state.terminal for client, state in held):
                raise RuntimeError("a turn ended before the foreground process could be measured")
            active = 0
            for marker in fixture.workspace.glob(".process-wait-*"):
                pid = marker.name.removeprefix(".process-wait-")
                proc = Path("/proc") / pid
                if pid.isdecimal() and proc.exists() and marker.read_text().strip() == pid:
                    active += 1
            if active == len(held):
                return
            await asyncio.sleep(0.05)


async def run_mode(args, mode, interval, emit, proc_sample, report_build):
    fixture = Fixture(args.scratch / mode, args.binary)
    fixture.init()
    try:
        await report_build(fixture)
        fixture.write_script(
            [
                tool(
                    "process_session_start",
                    command=command(interval),
                    workdir=".",
                    login=False,
                    tty=False,
                    keep_stdin_open=False,
                    yield_time_ms=600_000,
                    max_output_bytes=8192,
                ),
                answer("synthetic foreground completed"),
            ],
            process_tools=True,
        )
        pid = await fixture.serve()
        pids = {"host": pid, "runner": fixture.runner_pid}
        async with contextlib.AsyncExitStack() as stack:
            control = await stack.enter_async_context(Client(fixture, "terminal"))
            resident = []
            for _ in range(8):
                node = await create(control, fixture.workspace)
                client = await stack.enter_async_context(Client(fixture, "terminal"))
                state, _ = await client.attach(node)
                resident.append((client, state))
            started = 0
            for roots in (0, 1, 4, 8):
                while started < roots:
                    client, _ = resident[started]
                    reply, _, _ = await client.intent("start the synthetic foreground command")
                    if reply["answer"] != "accepted":
                        raise RuntimeError("foreground probe intent was not accepted")
                    started += 1
                    await wait_until_running(fixture, resident[:started])
                await asyncio.sleep(1)
                await sample(pids, args.io_seconds, roots, mode, emit, proc_sample)
                await wait_until_running(fixture, resident[:started])
    finally:
        await fixture.close()


async def process_wait(args, emit, proc_sample, report_build):
    args.binary = args.binary.resolve(strict=True)
    args.scratch = args.scratch.resolve()
    args.scratch.mkdir(mode=0o700)
    for mode, interval in (("silent", 0), ("output", args.output_interval)):
        await run_mode(args, mode, interval, emit, proc_sample, report_build)
