#!/usr/bin/env python3
"""Read a session's initial snapshot from an explicitly trusted local host socket."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import socket
import sys
import time

# Exact match, not a negotiated range. Update alongside each wire bump.
WIRE_VERSION = 10
MAX_LINE_BYTES = 16 * 1024 * 1024


class ProtocolError(Exception):
    pass


class Client:
    def __init__(self, path: str, timeout: float = 10):
        self.deadline = time.monotonic() + timeout
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(timeout)
        try:
            self.socket.connect(path)
            self.reader = self.socket.makefile("rb")
        except BaseException:
            self.socket.close()
            raise
        self.frames: list[dict] = []

    def __enter__(self):
        return self

    def __exit__(self, *_):
        try:
            self.send({"frame": "bye"})
        except OSError:
            pass
        finally:
            self.reader.close()
            self.socket.close()

    def send(self, frame: dict) -> None:
        self.socket.settimeout(max(0.001, self.deadline - time.monotonic()))
        self.socket.sendall(json.dumps(frame).encode() + b"\n")

    def receive(self) -> dict:
        if time.monotonic() >= self.deadline:
            raise ProtocolError("host deadline exceeded")
        if len(self.frames) >= 128:
            raise ProtocolError("example frame budget exceeded")
        self.socket.settimeout(self.deadline - time.monotonic())
        line = self.reader.readline(MAX_LINE_BYTES + 1)
        if not line.endswith(b"\n") or len(line) > MAX_LINE_BYTES:
            raise ProtocolError("host closed or exceeded the frame limit")
        frame = json.loads(line)
        if not isinstance(frame, dict):
            raise ProtocolError("expected a host frame")
        self.frames.append(frame)
        return frame

    def hello(self) -> dict:
        self.send(
            {"frame": "hello", "proto": WIRE_VERSION, "binary_id": "python-example", "caps": []}
        )
        welcome = self.receive()
        if welcome.get("frame") != "welcome" or welcome.get("proto") != WIRE_VERSION:
            raise ProtocolError("host refused the exact wire version")
        return welcome

    def request(self, request_id: int, op: dict) -> dict:
        self.send({"frame": "request", "id": request_id, "op": op})
        while True:
            frame = self.receive()
            if frame.get("frame") == "response" and frame.get("id") == request_id:
                outcome = frame["outcome"]
                if "ok" not in outcome:
                    raise ProtocolError(f"request refused: {outcome}")
                return outcome["ok"]


def inspect_session(path: str, session: str) -> list[dict]:
    with Client(path) as client:
        client.hello()
        directory = client.request(1, {"op": "list_sessions", "include_archived": False})
        node = next((row["node"] for row in directory["rows"] if row["session"] == session), None)
        if node is None:
            raise ProtocolError("session is not in the host directory")
        attached = client.request(2, {"op": "attach", "node": node})
        if attached.get("answer") != "attached":
            raise ProtocolError("expected an attachment")
        while True:
            frame = client.receive()
            if (
                frame.get("frame") == "snapshot"
                and frame.get("attachment") == attached["attachment"]
                and frame.get("last")
            ):
                return client.frames


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True, type=Path)
    parser.add_argument("--session", required=True, help="full session id from the host directory")
    args = parser.parse_args()
    try:
        for frame in inspect_session(str(args.socket), args.session):
            print(json.dumps(frame))
    except (OSError, ValueError, KeyError, ProtocolError) as error:
        print(f"host client: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
