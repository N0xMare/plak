"""Fixture-owned framed bridge; withhold one answered model key before Ack."""

import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import threading
import time


def packet(stream):
    prefix = stream.read(4)
    if not prefix:
        return None
    if len(prefix) != 4:
        raise EOFError("partial frame prefix")
    size = struct.unpack(">I", prefix)[0]
    if size > 64 * 1024 * 1024:
        raise ValueError("oversized fixture frame")
    chunks = []
    remaining = size
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            raise EOFError("partial fixture frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    body = b"".join(chunks)
    return prefix + body, json.loads(body)


def main():
    directory = Path(sys.argv[1])
    child = subprocess.Popen(sys.argv[2:], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    requests = {}
    mutex = threading.Lock()

    def cut(name):
        if not (directory / f"{name}.armed").is_file():
            return
        try:
            marker = (directory / f"{name}.entered").open("x")
        except FileExistsError:
            return
        marker.close()
        deadline = time.monotonic() + 30
        while not (directory / f"{name}.release").is_file():
            if time.monotonic() >= deadline:
                raise TimeoutError(f"{name} release not supplied")
            time.sleep(0.005)

    def forward():
        try:
            while item := packet(sys.stdin.buffer):
                raw, frame = item
                if frame["kind"] == "request":
                    evidence = {
                        key: frame.get(key)
                        for key in ("request_id", "class", "digest", "replay", "age_ms")
                    }
                    evidence["verb"] = frame["operation"]["verb"]
                    with mutex:
                        requests[frame["request_id"]] = evidence
                        with (directory / "wire.jsonl").open("a") as log:
                            log.write(json.dumps(evidence) + "\n")
                            log.flush()
                            os.fsync(log.fileno())
                    if evidence["verb"] == "process.poll":
                        cut("before-poll")
                child.stdin.write(raw)
                child.stdin.flush()
        finally:
            child.stdin.close()

    threading.Thread(target=forward, daemon=True).start()
    try:
        while item := packet(child.stdout):
            raw, frame = item
            with mutex:
                request = requests.get(frame.get("request_id"))
            if (
                frame["kind"] == "result"
                and request
                and request["class"]["class"] == "effect_call"
                and (directory / "before-answer.armed").is_file()
            ):
                try:
                    marker = (directory / "before-answer.entered").open("x")
                except FileExistsError:
                    pass
                else:
                    with marker:
                        json.dump(request, marker)
                    deadline = time.monotonic() + 30
                    while not (directory / "before-answer.release").is_file():
                        if time.monotonic() >= deadline:
                            raise TimeoutError("answered-key release not supplied")
                        time.sleep(0.005)
            sys.stdout.buffer.write(raw)
            sys.stdout.buffer.flush()
    finally:
        if child.poll() is None:
            child.terminate()
        child.wait(timeout=5)


if __name__ == "__main__":
    main()
