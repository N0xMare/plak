"""Wire observations and Linux process counters for one staging soak window."""

import asyncio
from contextlib import suppress
import itertools
import json
import math
import os
import platform
import re
import time
import uuid
from datetime import UTC, datetime
from pathlib import Path

from host_latency import proc_sample
from host_latency_wire import Client, Refused
from staging_soak_stream import MARK, RootTrace

WIRE_ERRORS = (ConnectionError, TimeoutError, Refused)
ROLES = ("host", "gateway", "runner")
DISPATCH_TOLERANCE_S = 0.100
MAX_LOAD_PER_CPU = 0.10
ATTACH_PERIOD_S = 2
SLOW_ATTACH_PERIOD_S = 4
READ_DELAY_S = 0.020
STAMP_BITS = 64
ENQUEUE_PONG = re.compile(r'^(\S+).*pong timing\{phase="enqueue" id=(\d+)\}:')


def timestamp():
    return datetime.now(UTC).isoformat(timespec="seconds")


def cpu_facts():
    fields = dict(
        line.split(":", 1) for line in Path("/proc/cpuinfo").read_text().splitlines() if ":" in line
    )
    model = next((v.strip() for k, v in fields.items() if k.strip() == "model name"), None)
    return {
        "model": model or platform.machine(),
        "cpu_count": len(os.sched_getaffinity(0)),
        "kernel_release": platform.release(),
    }


def check_load(limit):
    load = list(os.getloadavg())
    if load[0] > limit:
        raise LoadExcursion(load[0], limit)
    return load


class LoadExcursion(RuntimeError):
    def __init__(self, measured, limit):
        self.measured = measured
        self.limit = limit
        super().__init__(f"staging host too busy: one-minute load {measured:g} > {limit:g}")


def decode_stamp(raw):
    if len(raw) <= STAMP_BITS or raw[-STAMP_BITS - 1] != ord("}"):
        return None
    trailer = raw[-STAMP_BITS:]
    if trailer.rstrip(b" \t"):
        return None
    return int(trailer.replace(b" ", b"0").replace(b"\t", b"1"), 2)


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
        self.scheduled = {"attach": 0, "step": 0, "ping": 0}
        self.stream_spans = []
        self.attach_stamps = []
        self.failure_examples = []

    def fail(self, detail):
        self.failed += 1
        if len(self.failure_examples) < 8:
            self.failure_examples.append(detail)


class Browser:
    def __init__(self, fixture, index, cohort, chunks, *, fixed=None):
        self.index = index
        self.cohort = cohort
        self.fixed = fixed
        self.chunks = chunks
        self.route = None
        self.traces = {}
        self.archived = []
        self.arrivals = []
        self.client = Client(
            fixture,
            "web",
            login=login(index),
            read_delay=READ_DELAY_S if cohort == "slow" else 0,
            observe_wire=self.receive,
        )
        self.client.ids = itertools.count((index + 1) << 32)

    def receive(self, frame, raw, clock, wall):
        if self.cohort == "stalled":
            return
        stamp = decode_stamp(raw)
        pong = None
        if frame["frame"] == "response" and frame["outcome"].get("ok", {}).get("answer") == "pong":
            pong = frame["id"]
        self.arrivals.append((clock, wall, stamp, pong))
        if self.cohort == "fast" and self.route is not None and "attachment" in frame:
            attachment = frame["attachment"]
            trace = self.traces.get(attachment)
            if trace is None:
                trace = self.traces[attachment] = RootTrace(
                    self.chunks, continuous=self.fixed is not None
                )
            trace.observe(frame)

    async def attach(self, node):
        await self.client.detach()
        self.archive()
        self.route = node["tree_id"]
        return await self.client.attach(node)

    def archive(self):
        self.archived.extend(
            (attachment, self.route, trace) for attachment, trace in self.traces.items()
        )
        self.traces.clear()

    def receipts(self, durable, references):
        self.archive()
        return [
            trace.receipt(self.index, attachment, node, durable[node], references[node])
            for attachment, node, trace in self.archived
        ]

    async def stall(self, node):
        # Stop draining both application frames and the server heartbeat. The
        # WebSocket's bounded receive queue then applies real socket backpressure.
        self.client.reader.cancel()
        with suppress(asyncio.CancelledError):
            await self.client.reader
        await self.client.send(
            {
                "frame": "request",
                "id": next(self.client.ids),
                "op": {"op": "attach", "node": node},
            }
        )


def durable_texts(fixture, nodes):
    import contextlib
    import sqlite3

    uri = fixture.database("product").as_uri() + "?mode=ro"
    with contextlib.closing(sqlite3.connect(uri, uri=True)) as db:
        texts = {}
        for node in nodes:
            tree = node["tree_id"]
            texts[tree] = {}
            for (raw,) in db.execute(
                "select row from transcript_rows where session_id=? and json_extract(row,'$.kind')='assistant'",
                (tree,),
            ):
                text = json.loads(raw)["text"]
                if match := MARK.match(text):
                    texts[tree][int(match[1])] = text
    return texts


def frame_timings(browsers, lines):
    pong_times = {}
    for line in lines:
        if match := ENQUEUE_PONG.match(line):
            when, request_id = match.groups()
            seconds, _, fraction = when.removesuffix("Z").partition(".")
            sec_ns = int(datetime.fromisoformat(seconds).replace(tzinfo=UTC).timestamp()) * 10**9
            pong_times[int(request_id)] = sec_ns + int(fraction.ljust(9, "0"))
    results = {}
    for cohort in ("fast", "slow"):
        stamps, pongs, unstamped = [], [], 0
        for browser in (browser for browser in browsers if browser.cohort == cohort):
            for arrived, wall, enqueue, pong in browser.arrivals:
                if enqueue is not None:
                    stamps.append((arrived - enqueue) / 10**6)
                else:
                    unstamped += 1
                if pong in pong_times:
                    pongs.append((wall - pong_times[pong]) / 10**6)
        results.update(
            **distribution(f"send_frame_{cohort}", stamps),
            **distribution(f"send_pong_{cohort}", pongs),
            **{f"send_frame_{cohort}_unstamped": unstamped},
        )
        results[f"send_{cohort}_negative"] = sum(elapsed < 0 for elapsed in stamps + pongs)
    return results


class Drafts:
    def __init__(self, chunks):
        self.pending = {}
        self.first_delta = self.last_delta = None
        self.reference = RootTrace(chunks, continuous=True)

    def observe(self, frame, at):
        self.reference.observe(frame)
        if frame["frame"] != "event":
            return
        pending = self.pending.get(frame.get("client_id"))
        kind = frame["observation"]["kind"]
        if kind == "assistant_delta":
            self.first_delta = self.first_delta or at
            self.last_delta = at
        if pending and not pending.done() and kind in ("draft_accepted", "draft_rejected"):
            pending.set_result((kind, at))

    async def step(self, client, state, node, number, out):
        terminal = state.terminal
        draft = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending[draft] = future
        self.first_delta = self.last_delta = None
        began = time.monotonic()
        try:
            async with asyncio.timeout(30):
                reply, _, _ = await client.intent(f"synthetic tool step {number}", client_id=draft)
                kind, at = await future
                if reply["answer"] != "accepted" or kind != "draft_accepted":
                    out.fail(f"step {number}: draft {kind}, answer {reply['answer']}")
                    return False
                out.first.append((at - began) * 1000)
                await client.wait(lambda: state.terminal > terminal or state.ended)
                if state.ended or state.phase != "completed":
                    out.fail(
                        f"step {number}: terminal {state.phase}, attachment ended {state.ended}"
                    )
                    return False
                if self.first_delta is not None:
                    out.stream_spans.append((node["tree_id"], self.first_delta, self.last_delta))
                out.finished += 1
                return True
        except WIRE_ERRORS as error:
            out.fail(f"step {number}: {type(error).__name__}: {error}")
            return False
        finally:
            self.pending.pop(draft)


def login(index):
    return f"probe-{index}@example.invalid"


async def ping_slot(browser, out):
    result, sent, received = await browser.client.request({"op": "ping"})
    if result["answer"] != "pong":
        out.fail(f"client {browser.index}: unexpected ping answer {result['answer']}")
        return
    if browser.cohort == "fast":
        out.ping.append((received - sent) * 1000)


async def attach_slot(browser, node, out):
    state, began = await browser.attach(node)
    out.attach[browser.cohort].append((state.first - began) * 1000)
    out.pages += state.pages
    out.bytes += state.bytes
    out.attach_stamps.append((node["tree_id"], state.first))
    await ping_slot(browser, out)


async def sleep_until(at):
    await asyncio.sleep(max(0, at - time.monotonic()))


async def execute_slot(kind, index, deadline, operation, out):
    try:
        async with asyncio.timeout_at(deadline):
            await operation()
    except WIRE_ERRORS as error:
        out.fail(f"{kind} slot {index}: {type(error).__name__}: {error}")


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
                execute_slot(kind, index, at + period, lambda i=index: operation(i), out)
            )


def distribution(key, values):
    return {
        f"{key}_n": len(values),
        **{f"{key}_ms_p{p}": percentile(values, p / 100) for p in (50, 95, 99)},
    }


async def drive(fixture, pids, seconds, browsers, nodes, workload, load_limit, *, warmup=False):
    offset = fixture.diagnostics.stat().st_size
    old_resyncs = sum(browser.client.resyncs for browser in browsers if browser.cohort != "stalled")
    for browser in browsers:
        browser.arrivals.clear()
    out = Measurements()
    started_at = timestamp()
    start = time.monotonic() + 0.1
    end = start + seconds
    await sleep_until(start)
    a = {role: proc_sample(pids[role]) for role in ROLES} if not warmup else {}
    rss_start = {role: rss_mib(pids[role]) for role in ROLES} if not warmup else {}
    samples = [(a["host"][0], rss_start["host"])] if not warmup else []
    load_before = check_load(load_limit)
    loads = [load_before[0]]
    async with asyncio.TaskGroup() as tasks:
        active = [browser for browser in browsers if browser.cohort != "stalled"]
        for index, browser in enumerate(active):

            def operation(n, i=index, web=browser):
                if web.fixed is not None:
                    return ping_slot(web, out)
                node = nodes[(i + n) % len(nodes)]
                return attach_slot(web, node, out)

            period = SLOW_ATTACH_PERIOD_S if browser.cohort == "slow" else ATTACH_PERIOD_S
            times = [
                start + (index + 0.5) * 1.6 / len(active) + period * i
                for i in range(seconds // period)
            ]
            kind = "ping" if browser.fixed is not None else "attach"
            tasks.create_task(slots(tasks, kind, times, period, operation, out))
        for index, (client, state, node, drafts) in enumerate(workload):

            def step(n, i=index, c=client, s=state, root=node, d=drafts):
                return d.step(c, s, root, i + n * len(workload), out)

            period = len(workload) * 0.5
            times = [start + 0.5 * i for i in range(index, seconds * 2, len(workload))]
            tasks.create_task(slots(tasks, "step", times, period, step, out))
        for n in range(1, seconds):
            await sleep_until(start + n)
            loads.append(check_load(load_limit)[0])
            if not warmup:
                samples.append((time.monotonic(), rss_mib(pids["host"])))
        await sleep_until(end)
        b = {role: proc_sample(pids[role]) for role in ROLES} if not warmup else {}
        rss_end = {role: rss_mib(pids[role]) for role in ROLES} if not warmup else {}
        if not warmup:
            samples.append((b["host"][0], rss_end["host"]))
        load_after = check_load(load_limit)
        loads.append(load_after[0])
    drain_seconds = max(0, time.monotonic() - end)
    lag, failed = closes(fixture.diagnostics, offset)
    resyncs = sum(browser.client.resyncs for browser in active) - old_resyncs
    with fixture.diagnostics.open() as log:
        log.seek(offset)
        timing = frame_timings(active, log)
    steps = out.finished
    large = {node["tree_id"] for node in nodes[:2]}
    ongoing = sum(
        root in large
        and any(
            root == streaming and begin <= receipt <= finish
            for streaming, begin, finish in out.stream_spans
        )
        for root, receipt in out.attach_stamps
    )
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
        heavy_attach_while_streaming_n=ongoing,
        streamed_steps=len(out.stream_spans),
        **distribution("attach_fast", out.attach["fast"]),
        **distribution("attach_slow", out.attach["slow"]),
        **distribution("ping", out.ping),
        **timing,
        **distribution("first_event", out.first),
        steps=steps,
        fell_behind=resyncs,
        wire_resyncs=resyncs,
        web_lag_closes=lag,
        request_failed=failed + out.failed + out.missed + out.late,
        failure_examples=out.failure_examples,
        web_request_failed=failed,
        active_reader_closes=sum(browser.client.failure is not None for browser in active),
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
