#!/usr/bin/env python3
"""Informational cold and warm cell round trips, one nested call per cell."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import select
import statistics
import struct
import subprocess
import time

REVISION = "plakous-cell-protocol.v2"
MAX_FRAME = 8 * 1024 * 1024


def write_frame(child, frame):
    body = json.dumps(frame).encode()
    if len(body) > MAX_FRAME:
        raise ValueError("over-bound command")
    child.stdin.write(struct.pack(">I", len(body)) + body)
    child.stdin.flush()


def read_frame(child):
    deadline = time.monotonic() + 30

    def exact(length):
        chunks = bytearray()
        while len(chunks) < length:
            if not select.select([child.stdout], [], [], max(0, deadline - time.monotonic()))[0]:
                raise TimeoutError("cell worker did not reach a boundary")
            chunk = os.read(child.stdout.fileno(), length - len(chunks))
            if not chunk:
                raise RuntimeError("cell worker closed before a terminal")
            chunks.extend(chunk)
        return chunks

    (length,) = struct.unpack(">I", exact(4))
    if length > MAX_FRAME:
        raise ValueError("over-bound worker frame")
    return json.loads(exact(length))


def run_cell(child, engine, cell):
    if engine == "quickjs":
        source = "text(await tools.noop({}))"
        prelude = {
            "engine": "quickjs",
            "stored": {},
            "tools": [
                {"name": "noop", "tool_name": "noop", "kind": "function", "description": "noop"}
            ],
        }
        window = 10_000
    else:
        source, window = "await noop({})", None
        bootstrap = (
            {
                "source": "",
                "bindings": [{"function": "noop", "tool": "noop", "input": "dictionary"}],
            }
            if cell == 1
            else None
        )
        prelude = {"engine": "monty", "bootstrap": bootstrap}
    write_frame(
        child,
        {
            "frame": "start_cell",
            "cell": cell,
            "source": source,
            "prelude": prelude,
            "window": window,
        },
    )
    calls = accepted = 0
    while True:
        event = read_frame(child)
        kind = event["frame"]
        if kind == "nested_call":
            calls += 1
            write_frame(
                child,
                {
                    "frame": "nested_result",
                    "cell": cell,
                    "call": event["call"],
                    "outcome": {"Ok": {"type": "int", "value": 7}},
                },
            )
        elif kind == "delivery":
            if event["verdict"]["verdict"] != "accepted":
                raise RuntimeError(f"unaccepted nested result: {event}")
            accepted += 1
        elif kind == "cell_done":
            if calls != 1 or accepted != 1 or event["outcome"]["outcome"] != "completed":
                raise RuntimeError(f"incomplete cell round trip: {event}")
            return
        elif kind == "host_lost":
            raise RuntimeError(f"worker lost: {event}")


def measure(worker, engine):
    cold, warm = [], []
    config = {"engine": engine}
    config.update(
        {"heap_bytes": 64 * 1024 * 1024, "stack_bytes": 512 * 1024}
        if engine == "quickjs"
        else {"memory_bytes": 256 * 1024 * 1024}
    )
    for _ in range(5):
        began = time.monotonic()
        child = subprocess.Popen(
            [str(worker)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env={"PATH": "/usr/bin:/bin"},
            bufsize=0,
        )
        try:
            hello = read_frame(child)
            if hello.get("revision") != REVISION or hello.get("engine") != engine:
                raise RuntimeError(f"wrong worker: {hello}")
            write_frame(child, {"frame": "configure", "revision": REVISION, "engine": config})
            run_cell(child, engine, 1)
            cold.append((time.monotonic() - began) * 1000)
            for cell in range(2, 18):
                began = time.monotonic()
                run_cell(child, engine, cell)
                warm.append((time.monotonic() - began) * 1000)
            write_frame(child, {"frame": "shutdown"})
            if read_frame(child)["frame"] != "resource_report" or child.wait(timeout=10) != 0:
                raise RuntimeError("worker did not join cleanly")
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=10)
            child.stdin.close()
            child.stdout.close()
    return statistics.median(cold), statistics.median(warm)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("machine")
    parser.add_argument("profile")
    args = parser.parse_args()
    for engine, binary in [("quickjs", "plakous-js-host"), ("monty", "plakous-py-host")]:
        cold, warm = measure((args.directory / binary).resolve(strict=True), engine)
        for phase, value in [("spawn_round_trip", cold), ("per_cell", warm)]:
            print(
                json.dumps(
                    {
                        "metric": f"cell_{engine}_{phase}_ms",
                        "value": value,
                        "unit": "ms",
                        "machine": args.machine,
                        "profile": args.profile,
                    },
                    sort_keys=True,
                )
            )


if __name__ == "__main__":
    main()
