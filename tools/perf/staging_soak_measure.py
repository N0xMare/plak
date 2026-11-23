"""Wire observations and Linux process counters for one staging soak window."""

import asyncio
import math
import os
import platform
import time
import uuid
from datetime import UTC, datetime
from pathlib import Path

from host_latency import proc_sample
from host_latency_wire import Client, Refused

WIRE_ERRORS = (ConnectionError, TimeoutError, Refused)
ROLES = ("host", "gateway", "runner")
DISPATCH_TOLERANCE_S = 0.100
MAX_LOAD_PER_CPU = 0.10


def timestamp():
    return datetime.now(UTC).isoformat(timespec="seconds")


def cpu_facts():
    fields = dict(
        line.split(":", 1) for line in Path("/proc/cpuinfo").read_text().splitlines() if ":" in line
    )
    model = next((v.strip() for k, v in fields.items() if k.strip() == "model name"), None)
    return {"model": model or platform.machine(), "cpu_count": len(os.sched_getaffinity(0))}


def check_load(cpu_count):
    load = list(os.getloadavg())
    if load[0] > cpu_count * MAX_LOAD_PER_CPU:
        raise RuntimeError(
            f"staging host too busy: one-minute load {load[0]:g} on {cpu_count} CPUs"
        )
    return load


def percentile(values, q):
    return sorted(values)[math.ceil(q * len(values)) - 1] if values else None


def rss_mib(pid):
    stat = (Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()
    return int(stat[21]) * os.sysconf("SC_PAGE_SIZE") / (1 << 20)


def slope(points):
    x = sum(t for t, _ in points) / len(points)
    y = sum(r for _, r in points) / len(points)
    return 60 * sum((t - x) * (r - y) for t, r in points) / sum((t - x) ** 2 for t, _ in points)


def closes(path, offset):
    with path.open() as log:
        log.seek(offset)
        lines = [line for line in log if "web connection closed" in line]
    return tuple(
        sum(f'ended="{reason}"' in line for line in lines)
        for reason in ("client fell behind", "request failed")
    )


class Measurements:
    def __init__(self):
        self.attach = {"fast": [], "slow": []}
        self.ping = []
        self.first = []
        self.failed = 0
        self.finished = 0
        self.pages = 0
        self.bytes = 0
        self.missed = 0
        self.late = 0
        self.scheduled = {"attach": 0, "step": 0}


class Drafts:
    def __init__(self):
        self.pending = {}

    def observe(self, frame, at):
        if frame["frame"] != "event":
            return
        pending = self.pending.get(frame.get("client_id"))
        kind = frame["observation"]["kind"]
        if pending and not pending.done() and kind in ("draft_accepted", "draft_rejected"):
            pending.set_result((kind, at))

    async def step(self, client, state, number, out):
        terminal = state.terminal
        draft = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending[draft] = future
        began = time.monotonic()
        try:
            async with asyncio.timeout(30):
                reply, _, _ = await client.intent(f"synthetic tool step {number}", client_id=draft)
                kind, at = await future
                if reply["answer"] != "accepted" or kind != "draft_accepted":
                    out.failed += 1
                    return False
                out.first.append((at - began) * 1000)
                await client.wait(lambda: state.terminal > terminal or state.ended)
                if state.ended or state.phase != "completed":
                    out.failed += 1
                    return False
                out.finished += 1
                return True
        except WIRE_ERRORS:
            out.failed += 1
            return False
        finally:
            self.pending.pop(draft)


def login(index):
    return f"probe-{index}@example.invalid"


async def attach_slot(client, node, cohort, out):
    await client.detach()
    state, began = await client.attach(node)
    result, sent, received = await client.request({"op": "ping"})
    if result["answer"] != "pong":
        out.failed += 1
        return
    out.attach[cohort].append((state.first - began) * 1000)
    out.ping.append((received - sent) * 1000)
    out.pages += state.pages
    out.bytes += state.bytes


async def sleep_until(at):
    await asyncio.sleep(max(0, at - time.monotonic()))


async def execute_slot(deadline, operation, out):
    try:
        async with asyncio.timeout_at(deadline):
            await operation()
    except WIRE_ERRORS:
        out.failed += 1


async def slots(tasks, kind, times, period, operation, out):
    # One in-flight request chain per connection. A slow chain never shifts the
    # offered schedule, and a missed operation is not retried in a later slot.
    pending = None
    for index, at in enumerate(times):
        out.scheduled[kind] += 1
        await sleep_until(at)
        if time.monotonic() > at + DISPATCH_TOLERANCE_S:
            out.late += 1
        elif pending and not pending.done():
            out.missed += 1
        else:
            pending = tasks.create_task(
                execute_slot(at + period, lambda i=index: operation(i), out)
            )


def distribution(key, values):
    return {
        f"{key}_n": len(values),
        **{f"{key}_ms_p{p}": percentile(values, p / 100) for p in (50, 95, 99)},
    }


async def drive(fixture, pids, seconds, browsers, slow, nodes, workload, cpu, *, warmup=False):
    offset = fixture.diagnostics.stat().st_size
    old_resyncs = sum(client.resyncs for client in browsers)
    out = Measurements()
    started_at = timestamp()
    start = time.monotonic() + 0.1
    end = start + seconds
    await sleep_until(start)
    a = {role: proc_sample(pids[role]) for role in ROLES} if not warmup else {}
    rss_start = {role: rss_mib(pids[role]) for role in ROLES} if not warmup else {}
    samples = [(a["host"][0], rss_start["host"])] if not warmup else []
    load_before = check_load(cpu["cpu_count"])
    loads = [load_before[0]]
    async with asyncio.TaskGroup() as tasks:
        for index, client in enumerate(browsers):

            def attach(n, i=index, web=client):
                return attach_slot(
                    web, nodes[(i + n) % len(nodes)], "slow" if i < slow else "fast", out
                )

            times = [start + index * 0.13 + 2 * i for i in range(seconds // 2)]
            tasks.create_task(slots(tasks, "attach", times, 2, attach, out))
        for index, (client, state, drafts) in enumerate(workload):

            def step(n, i=index, c=client, s=state, d=drafts):
                return d.step(c, s, i + n * len(workload), out)

            period = len(workload) * 0.5
            times = [start + 0.5 * i for i in range(index, seconds * 2, len(workload))]
            tasks.create_task(slots(tasks, "step", times, period, step, out))
        for n in range(1, seconds):
            await sleep_until(start + n)
            loads.append(check_load(cpu["cpu_count"])[0])
            if not warmup:
                samples.append((time.monotonic(), rss_mib(pids["host"])))
        await sleep_until(end)
        b = {role: proc_sample(pids[role]) for role in ROLES} if not warmup else {}
        rss_end = {role: rss_mib(pids[role]) for role in ROLES} if not warmup else {}
        if not warmup:
            samples.append((b["host"][0], rss_end["host"]))
        load_after = check_load(cpu["cpu_count"])
        loads.append(load_after[0])
    drain_seconds = max(0, time.monotonic() - end)
    lag, failed = closes(fixture.diagnostics, offset)
    resyncs = sum(client.resyncs for client in browsers) - old_resyncs
    steps = out.finished
    result = dict(
        started_at=started_at,
        load_before=load_before,
        load_after=load_after,
        load_1m_max=max(loads),
        load_samples=len(loads),
        drain_seconds=drain_seconds,
        scheduled_attaches=out.scheduled["attach"],
        scheduled_steps=out.scheduled["step"],
        missed_slots=out.missed,
        late_slots=out.late,
        attach_n=sum(map(len, out.attach.values())),
        snapshot_pages=out.pages,
        snapshot_bytes=out.bytes,
        **distribution("attach_fast", out.attach["fast"]),
        **distribution("attach_slow", out.attach["slow"]),
        ping_ms_p99=percentile(out.ping, 0.99),
        **distribution("first_event", out.first),
        steps=steps,
        fell_behind=lag + resyncs,
        wire_resyncs=resyncs,
        web_lag_closes=lag,
        request_failed=failed + out.failed + out.missed + out.late,
        web_request_failed=failed,
    )
    if warmup:
        return result
    elapsed = b["host"][0] - a["host"][0]
    rate = {}
    for role in ROLES:
        if a[role][1] != b[role][1]:
            raise RuntimeError(f"{role} PID was reused during a measurement window")
        rate[role] = 1000 * (b[role][5] - a[role][5]) / os.sysconf("SC_CLK_TCK") / elapsed
    result.update(
        seconds=elapsed,
        cpu_ms_per_s=rate,
        cpu_total_ms_per_s=sum(rate.values()),
        reads_per_step=(b["host"][2] - a["host"][2]) / steps if steps else None,
        rss_start_mib=samples[0][1],
        rss_end_mib=samples[-1][1],
        process_rss_mib={role: {"start": rss_start[role], "end": rss_end[role]} for role in ROLES},
        rss_mib_per_min=slope(samples),
        rss_samples=len(samples),
    )
    return result


def browser(fixture, index, slow):
    return Client(fixture, "web", login=login(index), read_delay=0.02 if index < slow else 0)
