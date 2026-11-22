"""Measure the steady cost of a root stream with an already-held transcript."""

import asyncio
import contextlib
import json
import math
import os

from host_latency_fixture import Fixture, answer, create, turn
from host_latency_wire import WIRE, Client


async def stream_cost(args, emit, proc_sample):
    fixture = Fixture(args.scratch.resolve(), args.binary.resolve(strict=True))
    fixture.init()
    try:
        info = await fixture.spawn("version", "--build-info", pipes=True)
        raw, _ = await info.communicate()
        build = json.loads(raw)
        if info.returncode or not build["test_hooks"] or build["wire"]["number"] != WIRE:
            raise RuntimeError("expected a wire-12 plakd built with test-support")
        emit("build", revision=build["revision"], flavor=build["flavor"], wire=build["wire"])

        fixture.write_script([answer("seeded synthetic turn")] * args.history_turns)
        await fixture.serve()
        async with Client(fixture, "terminal") as seed:
            node = await create(seed, fixture.workspace)
            state, _ = await seed.attach(node)
            for i in range(args.history_turns):
                await turn(seed, state, f"synthetic history {i}: " + "x" * args.prompt_bytes)
        # Restart so the measured root opens from durable history, not from
        # the empty baseline held by the client that created it.
        await fixture.close()
        (fixture.root / "host.log").write_text("")
        count = math.ceil((args.seconds + 10) * 1000 / args.delta_ms)
        fixture.write_script(
            [
                {
                    "stream": [{"text": " synthetic stream delta"}, {"stall": args.delta_ms}]
                    * count,
                    "output": {"text": "synthetic stream done"},
                }
            ]
        )
        pid = await fixture.serve()
        async with contextlib.AsyncExitStack() as stack:
            leader = await stack.enter_async_context(Client(fixture, "terminal"))
            state, _ = await leader.attach(node)
            peers = [(leader, state)]
            for _ in range(args.clients - 1):
                web = await stack.enter_async_context(Client(fixture, "web"))
                other, _ = await web.attach(node)
                peers.append((web, other))
            transcript = fixture.counts(node)
            for client, held in peers:
                await client.wait(
                    lambda: held.bytes + held.repair_bytes >= transcript["transcript_row_bytes"]
                )
            emit(
                "stream_fixture",
                pid=pid,
                clients=args.clients,
                pages=[held.pages for _, held in peers],
                baseline_bytes=[held.bytes + held.repair_bytes for _, held in peers],
                **transcript,
            )

            def begin(phase):
                emit("stream_window_start", phase=phase, pid=pid)
                return (
                    proc_sample(pid),
                    [held.repair_bytes for _, held in peers],
                    [client.sequence_gaps for client, _ in peers],
                )

            def report(phase, window, deltas):
                before, repairs, gaps = window
                after = proc_sample(pid)
                if before[1] != after[1]:
                    raise RuntimeError("host PID changed during sampling")
                seconds = after[0] - before[0]
                cpu = (after[5] - before[5]) / os.sysconf("SC_CLK_TCK")
                emit(
                    "stream_cost",
                    phase=phase,
                    seconds=seconds,
                    host_cpu_ms=cpu * 1000,
                    cpu_percent=100 * cpu / seconds,
                    reads_per_s=(after[2] - before[2]) / seconds,
                    assistant_events=deltas[0],
                    assistant_deliveries=deltas,
                    cpu_us_per_event=1_000_000 * cpu / deltas[0] if phase == "streaming" else None,
                    repair_bytes=[
                        held.repair_bytes - base
                        for (_, held), base in zip(peers, repairs, strict=True)
                    ],
                    sequence_gaps=[
                        client.sequence_gaps - base
                        for (client, _), base in zip(peers, gaps, strict=True)
                    ],
                )

            await asyncio.sleep(2)
            idle = begin("idle")
            await asyncio.sleep(args.idle_seconds)
            report("idle", idle, [0] * len(peers))
            terminals = [held.terminal for _, held in peers]
            initial = [held.deltas for _, held in peers]
            admitted = begin("admission")
            reply, _, _ = await leader.intent("stream from the synthetic transcript")
            if reply["answer"] != "accepted":
                raise RuntimeError(f"stream intent was not accepted: {reply}")
            for (client, held), first in zip(peers, initial, strict=True):
                await client.wait(lambda: held.deltas > first or held.ended)
                if held.ended:
                    raise RuntimeError("an attachment ended before the first delta")
            report(
                "admission",
                admitted,
                [held.deltas - first for (_, held), first in zip(peers, initial, strict=True)],
            )
            initial = [held.deltas for _, held in peers]
            active = begin("streaming")
            await asyncio.sleep(args.seconds)
            if any(
                client.failure or held.ended or held.terminal != terminal
                for (client, held), terminal in zip(peers, terminals, strict=True)
            ):
                raise RuntimeError("an attachment or the turn ended during the streaming sample")
            deltas = [held.deltas - first for (_, held), first in zip(peers, initial, strict=True)]
            if not all(deltas):
                raise RuntimeError("not all viewers received streamed events")
            report("streaming", active, deltas)
    finally:
        await fixture.close()
