"""Fixture-owned framed bridge; withhold one answered model key before Ack."""

import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
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


def claim_marker(path, contents=""):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent) as marker:
        marker.write(contents)
        marker.flush()
        # A complete marker and the one-shot claim must become visible together.
        # Linking, unlike replacement, preserves the exclusive-create winner.
        try:
            os.link(marker.name, path)
        except FileExistsError:
            return False
    return True


def main():
    directory = Path(sys.argv[1])
    if (directory / "redial.held").is_file():
        deadline = time.monotonic() + 9
        while not (directory / "redial.release").is_file():
            if time.monotonic() >= deadline:
                raise TimeoutError("fixture redial was not released")
            time.sleep(0.005)
    child = subprocess.Popen(sys.argv[2:], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    requests = {}
    held_probes = set()
    mutex = threading.Lock()

    def cut(name):
        if not (directory / f"{name}.armed").is_file():
            return
        if not claim_marker(directory / f"{name}.entered"):
            return
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
                    if evidence["verb"].startswith("vcs."):
                        evidence["vcs_op"] = frame["operation"]["payload"]["op"]
                    with mutex:
                        requests[frame["request_id"]] = evidence
                        marker = directory / "final-probe.next"
                        if evidence["verb"] == "probe.workspace" and marker.is_file():
                            marker.unlink()
                            held_probes.add(frame["request_id"])
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
                withheld = frame.get("request_id") in held_probes
            if frame["kind"] == "result" and withheld:
                count = len(held_probes)
                claim_marker(directory / f"final-probe.{count}")
                continue
            if frame["kind"] == "result" and request:
                operation = request.get("vcs_op")
                if operation in {"mint", "bundle_apply"}:
                    cut(f"after-{operation.replace('_', '-')}")
            if (
                frame["kind"] == "result"
                and request
                and request["class"]["class"] == "effect_call"
                and (directory / "before-answer.armed").is_file()
            ):
                if claim_marker(directory / "before-answer.entered", json.dumps(request)):
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
    # The stdin reader is a daemon thread that can still hold the stdin buffer
    # lock, and interpreter shutdown then aborts with a non-zero exit. Exit
    # without finalization; an exception from main() still exits non-zero.
    sys.stdout.flush()
    os._exit(0)
