#!/usr/bin/env python3
"""Run the wire-12 host latency workload or sample one Linux process."""

import argparse
import asyncio
import importlib.util
import json
import math
import os
import sys
import time
from pathlib import Path

from host_latency_fixture import Fixture, answer, populate, start_fixture, turn
from host_latency_wire import Client, Detached, Refused

CONSOLE = ("profiles_list", "templates_list", "accounts_status")
CONSOLE_DEADLINE = 10


def emit(metric, **values):
    print(json.dumps({"metric": metric, **values}, sort_keys=True), flush=True)


def proc_sample(pid):
    root = Path(f"/proc/{pid}")
    io = dict(line.split(": ") for line in (root / "io").read_text().splitlines())
    # comm can contain spaces and parentheses; the fields begin after its final ')'.
    stat = (root / "stat").read_text().rsplit(")", 1)[1].split()
    return (
        time.monotonic(),
        int(stat[19]),
        int(io["syscr"]),
        int(io["rchar"]),
        int(io["read_bytes"]),
        int(stat[11]) + int(stat[12]),
    )


async def rates(pid, seconds, metric):
    start = proc_sample(pid)
    await asyncio.sleep(seconds)
    end = proc_sample(pid)
    if start[1] != end[1]:
        raise RuntimeError("PID was reused during sampling")
    elapsed = end[0] - start[0]
    _, _, reads, chars, disk, ticks = (b - a for a, b in zip(start, end, strict=True))
    emit(
        metric,
        seconds=elapsed,
        reads_per_s=reads / elapsed,
        kb_per_s=chars / elapsed / 1000,
        disk_kb_per_s=disk / elapsed / 1000,
        cpu_percent=100 * ticks / os.sysconf("SC_CLK_TCK") / elapsed,
    )


class Distribution:
    def __init__(self):
        self.ms = []
        self.deadlines = 0
        self.refused = 0
        self.lost = 0

    def record(self, start, end):
        self.ms.append((end - start) * 1000)

    def report(self, metric, *percentiles, **extra):
        ordered = sorted(self.ms)
        values = {
            f"p{p}_ms": ordered[math.ceil(len(ordered) * p / 100) - 1] if ordered else None
            for p in percentiles
        }
        emit(
            metric,
            samples=len(ordered),
            deadlines=self.deadlines,
            refused=self.refused,
            lost=self.lost,
            **values,
            **extra,
        )


async def probe(distribution, request, *, deadline=None, answer_kind=None, defer=False):
    try:
        answer, start, end = await request
        if answer_kind and answer.get("answer") != answer_kind:
            raise RuntimeError(f"wire reply was not {answer_kind}: {str(answer)[:400]}")
        if deadline is not None and end - start >= deadline:
            distribution.deadlines += 1
        elif not defer:
            distribution.record(start, end)
        else:
            return start, end
    except TimeoutError:
        distribution.deadlines += 1
    except Refused as error:
        if "deadline" in str(error):
            distribution.deadlines += 1
        else:
            distribution.refused += 1
    except ConnectionError:
        distribution.lost += 1


async def switch(fixture, args, browser, observer, live, terminal, waiting, heavy, stream):
    first_pages, pings, intent = (Distribution() for _ in range(3))
    console = {kind: Distribution() for kind in CONSOLE}
    gone = replacements = resyncs = gaps = completed = pages = baseline_bytes = 0
    multi_page = pings_outside_baseline = pings_without_stream = 0
    stop_at = time.monotonic() + args.switch_seconds
    stream_before = live.deltas
    stream_terminals = live.terminal
    attempt = 0
    start = time.monotonic()
    offset = fixture.diagnostics.stat().st_size if fixture.diagnostics.exists() else 0
    current = browser
    try:
        while time.monotonic() < stop_at:
            began = time.monotonic()
            big = attempt % 2 == 0
            if (
                terminal.failure
                or observer.failure
                or waiting.ended
                or live.ended
                or (live.terminal != stream_terminals and live.phase != "completed")
            ):
                raise RuntimeError(
                    "a pinned attachment or the scripted stream ended during switching"
                )
            try:
                if current is None:
                    current = await Client(fixture, "web").__aenter__()
                    replacements += 1
                await current.detach()
                sent = asyncio.Event()
                opening = asyncio.create_task(current.attach(heavy if big else stream, sent=sent))
                send_notice = asyncio.create_task(sent.wait())
                await asyncio.wait((opening, send_notice), return_when=asyncio.FIRST_COMPLETED)
                send_notice.cancel()
                if opening.done():
                    opening.result()

                chores = []
                last_delta_before_ping = live.last_delta
                if big:
                    chores.append(
                        asyncio.create_task(
                            probe(
                                pings,
                                current.request({"op": "ping"}),
                                answer_kind="pong",
                                defer=True,
                            )
                        )
                    )
                kind = CONSOLE[attempt % len(CONSOLE)]
                chores.append(
                    asyncio.create_task(
                        probe(
                            console[kind],
                            current.request(
                                {"op": "host", "request": {"request": kind}},
                                deadline=CONSOLE_DEADLINE,
                            ),
                            deadline=CONSOLE_DEADLINE,
                        )
                    )
                )
                # Keep the stream open. A real follow-up receives a mailbox
                # acknowledgement but will not replace the running generation.
                if attempt < 200:
                    chores.append(
                        asyncio.create_task(
                            probe(
                                intent,
                                observer.intent(f"synthetic queued input {attempt}", "follow_up"),
                                answer_kind="accepted",
                            )
                        )
                    )
                outcomes = await asyncio.gather(opening, *chores, return_exceptions=True)
                for outcome in outcomes:
                    if isinstance(outcome, BaseException):
                        raise outcome
                state, sent_at = outcomes[0]
                if big:
                    first_pages.record(sent_at, state.first)
                    pages += state.pages
                    baseline_bytes += state.bytes
                    multi_page += state.pages > 1
                    if ping := outcomes[1]:
                        if ping[0] > state.last:
                            pings_outside_baseline += 1
                        elif ping[0] - last_delta_before_ping <= 0.25 or live.last_delta >= ping[0]:
                            pings.record(*ping)
                        else:
                            pings_without_stream += 1
                completed += 1
            except Detached:
                # The next iteration requests a new attachment without
                # conflating an attachment resync and a transport replacement.
                first_pages.lost += big
            except ConnectionError:
                first_pages.lost += big
                gone += 1
                if current:
                    resyncs += current.resyncs
                    gaps += current.sequence_gaps
                    await current.close()
                    current = None
            except TimeoutError:
                first_pages.deadlines += big
            attempt += 1
            await asyncio.sleep(
                max(0, min(stop_at, began + args.switch_interval) - time.monotonic())
            )
        elapsed = time.monotonic() - start
    finally:
        if current:
            resyncs += current.resyncs
            gaps += current.sequence_gaps
            if current is not browser:
                await current.close()
    stream_deltas = live.deltas - stream_before
    if not stream_deltas:
        raise RuntimeError("no live assistant output observed during switching")
    pings.report(
        "stream_and_baseline_ping",
        50,
        99,
        stream_deltas=stream_deltas,
        sent_after_baseline=pings_outside_baseline,
        no_recent_stream_delta=pings_without_stream,
        completed_stream_turns=live.terminal - stream_terminals,
    )
    intent.report("intent_accepted", 99)
    for kind, distribution in console.items():
        distribution.report(kind, 95)
    first_pages.report(
        "resident_attach_first_page",
        50,
        95,
        99,
        100,
        baseline_pages=pages,
        baseline_bytes=baseline_bytes,
        multi_page_attaches=multi_page,
    )
    socket_lag = 0
    if fixture.diagnostics.exists():
        with fixture.diagnostics.open() as log:
            log.seek(offset)
            socket_lag = sum(
                "web connection closed" in line and "fell behind" in line for line in log
            )
    emit(
        "switching",
        seconds=elapsed,
        attempted=attempt,
        completed=completed,
        fell_behind=resyncs + terminal.resyncs + observer.resyncs + socket_lag,
        wire_resyncs=resyncs + terminal.resyncs + observer.resyncs,
        web_lag_closes=socket_lag,
        event_sequence_gaps=gaps + terminal.sequence_gaps + observer.sequence_gaps,
        disconnected=gone,
        replaced=replacements,
    )
    if args.history_turns * args.prompt_bytes > 8 << 20 and not multi_page:
        raise RuntimeError("large fixture did not deliver a multi-page baseline")


async def report_build(fixture):
    info = await fixture.spawn("version", "--build-info", pipes=True)
    raw, _ = await info.communicate()
    await fixture.stop(info)
    build = json.loads(raw)
    if info.returncode or not build["test_hooks"] or build["wire"]["number"] != 12:
        raise RuntimeError("expected a wire-12 plakd built with test-support")
    emit("build", revision=build["revision"], flavor=build["flavor"], wire=build["wire"])


async def run(args):
    fixture = Fixture(args.scratch.resolve(), args.binary.resolve(strict=True))
    fixture.init()
    await report_build(fixture)
    try:
        pid, batches = await start_fixture(fixture, args)
        # The normal model operation deadline is shorter than a full soak.
        # Queue follow-ups to keep successive bounded streams running.
        stream_time = min(args.switch_seconds / 3, 120)
        events = [{"text": " synthetic stream delta"}, {"stall": 50}] * math.ceil(stream_time * 20)
        # Each worker reads its script when it opens. Keep the seeded heavy
        # worker resident while installing the separate stream worker's plan.
        async with Client(fixture, "terminal") as terminal:
            heavy, waiting, stream = await populate(fixture, args, terminal, batches)
            fixture.write_script(
                [
                    answer("synthetic warm stream root"),
                    {"stream": events, "output": {"text": "stream done"}},
                ],
                repeat_last_step=True,
            )
            async with Client(fixture, "web") as observer, Client(fixture, "web") as browser:
                live, _ = await observer.attach(stream)
                await turn(observer, live, "initialize the stream root")
                await browser.attach(stream)
                idle = fixture.counts(heavy)
                if idle["retained_children"] != args.children - 1 or idle["active_children"] != 0:
                    raise RuntimeError(f"idle child shape did not settle: {idle}")
                emit("fixture_idle", **idle)
                await rates(pid, args.io_seconds, "idle_clients")
                await terminal.intent("wait for the final synthetic child")
                emit("fixture_waiting", **await fixture.witness_wait(heavy, args.children))
                await rates(pid, args.io_seconds, "waiting_clients")
                deltas_before, terminals_before = live.deltas, live.terminal
                await observer.intent("begin the synthetic stream")
                await observer.wait(
                    lambda: live.deltas > deltas_before or live.terminal > terminals_before
                )
                if live.terminal != terminals_before:
                    raise RuntimeError(f"scripted stream did not start: {live.phase}")
                await switch(
                    fixture, args, browser, observer, live, terminal, waiting, heavy, stream
                )
                await fixture.witness_wait(heavy, args.children)
    finally:
        await fixture.close()


def parser():
    commands = argparse.ArgumentParser(description=__doc__)
    sub = commands.add_subparsers(dest="command", required=True)
    work = sub.add_parser("run", help="generate and measure a new scratch host")
    work.add_argument("binary", type=Path, help="path to the test-support binary and its sidecars")
    work.add_argument("scratch", type=Path, help="short, nonexistent scratch directory")
    work.add_argument("--roots", type=int, default=74)
    work.add_argument("--children", type=int, default=60)
    work.add_argument("--history-turns", type=int, default=56)
    work.add_argument("--prompt-bytes", type=int, default=240 << 10)
    work.add_argument("--child-bytes", type=int, default=48 << 10)
    work.add_argument("--switch-seconds", type=float, default=600)
    work.add_argument("--switch-interval", type=float, default=2)
    work.add_argument("--io-seconds", type=float, default=60)
    process = sub.add_parser("process-wait", help="measure roots parked on foreground tools")
    process.add_argument("binary", type=Path)
    process.add_argument("scratch", type=Path)
    process.add_argument("--io-seconds", type=float, default=30)
    process.add_argument("--output-interval", type=int, default=2)
    stream = sub.add_parser("stream-cost", help="measure one root streamed to attached viewers")
    stream.add_argument("binary", type=Path)
    stream.add_argument("scratch", type=Path)
    stream.add_argument("--history-turns", type=int, default=56)
    stream.add_argument("--prompt-bytes", type=int, default=240 << 10)
    stream.add_argument("--clients", type=int, default=3)
    stream.add_argument("--seconds", type=float, default=30)
    stream.add_argument("--idle-seconds", type=float, default=10)
    stream.add_argument("--delta-ms", type=int, default=12)
    io = sub.add_parser("io", help="measure Linux /proc without connecting to the process")
    io.add_argument("pid", type=int)
    io.add_argument("--seconds", type=float, default=30)
    return commands


def main():
    cli = parser()
    args = cli.parse_args()
    if args.command == "io":
        if args.pid <= 0 or not 0 < args.seconds <= 3600:
            cli.error("positive PID and window up to an hour required")
        asyncio.run(rates(args.pid, args.seconds, "process_io"))
        return
    if not sys.platform.startswith("linux"):
        raise SystemExit("the workload requires Linux /proc")
    if args.command == "process-wait":
        from host_latency_process_wait import process_wait

        if not (0 < args.io_seconds <= 120 and 1 <= args.output_interval <= 30):
            cli.error(
                "process samples require a window up to 120 seconds and output every 1–30 seconds"
            )
        asyncio.run(process_wait(args, emit, proc_sample, report_build))
    elif args.command == "stream-cost":
        from host_latency_stream_cost import stream_cost

        if not (
            1 <= args.clients <= 3
            and 1 <= args.history_turns <= 400
            and 1 <= args.prompt_bytes < (1 << 20) - 1024
            and 0 < args.seconds <= 60
            and 0 < args.idle_seconds <= 60
            and 5 <= args.delta_ms <= 100
        ):
            cli.error("invalid stream-cost workload size or timing")
        if args.clients > 1 and importlib.util.find_spec("websockets") is None:
            cli.error("install websockets==15.0.1 to attach web viewers")
        asyncio.run(stream_cost(args, emit, proc_sample))
    else:
        if importlib.util.find_spec("websockets") is None:
            raise SystemExit("install websockets==15.0.1 in the probe's Python environment")
        if not (
            2 <= args.roots
            and 1 <= args.children <= 64
            and 1 <= args.history_turns
            and 1 <= args.prompt_bytes < (1 << 20) - 100
            and 1 <= args.child_bytes < (64 << 10) - 1024
            and 0 < args.switch_seconds <= 1800
            and 0 < args.switch_interval
            and 0 < args.io_seconds <= 600
        ):
            cli.error("invalid scratch workload size or duration")
        asyncio.run(run(args))


if __name__ == "__main__":
    main()
