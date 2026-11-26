#!/usr/bin/env python3
"""Test-only socket driver for host protocol contracts."""

from __future__ import annotations

import json
import socket
import time

# Exact match, not a negotiated range. Update alongside each wire bump.
WIRE_VERSION = 13
MAX_LINE_BYTES = 16 * 1024 * 1024


class ProtocolError(Exception):
    pass


class RequestRefused(ProtocolError):
    def __init__(self, refusal: dict):
        super().__init__(f"request refused: {refusal}")
        self.refusal = refusal["refusal"]


def row_key(row: dict) -> tuple[int, str, int]:
    return row["anchor"], row["lane"], row["ordinal"]


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
        # JSON integers decode exactly, and decimal strings (e.g. usage) stay strings.
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
                if "refused" in outcome:
                    raise RequestRefused(outcome["refused"])
                return outcome["ok"]

    def attach_first_page(self, request_id: int, node: dict) -> dict:
        while True:
            try:
                attached = self.request(request_id, {"op": "attach", "node": node})
                break
            except RequestRefused as error:
                remaining = self.deadline - time.monotonic()
                if error.refusal != "backpressure" or remaining <= 0:
                    raise
                time.sleep(min(0.05, remaining))
                if time.monotonic() >= self.deadline:
                    raise
                request_id += 1
        if attached.get("answer") != "attached" or attached["transcript"]["mode"] != "first_page":
            raise ProtocolError("expected a first-page attachment")
        return self.read_first_page(attached["attachment"])

    def read_first_page(self, attachment: int) -> dict:
        page = None
        part = 0
        while True:
            frame = self.receive()
            if frame.get("attachment") != attachment:
                continue
            if frame["frame"] == "resync_required":
                raise ProtocolError("attachment requires a fresh baseline")
            if frame["frame"] != "snapshot":
                continue
            metadata = {
                field: frame[field]
                for field in ("scope", "revision", "has_older", "older_rows", "older_bytes")
            }
            metadata["older_cursor"] = frame.get("older_cursor")
            if frame["part"] != part or (page is not None and page["metadata"] != metadata):
                raise ProtocolError("inconsistent first-page snapshot parts")
            if metadata["has_older"] != (metadata["older_cursor"] is not None):
                raise ProtocolError("inconsistent older history cursor")
            if page is None:
                page = {"metadata": metadata, "items": [], "live_rows": {}}
            page["items"].extend(frame["items"])
            for live in frame["live_rows"]:
                page["live_rows"][row_key(live["row"])] = live["len"]
            if frame["last"]:
                return page
            part += 1


def inspect_session(path: str, session: str) -> tuple[list[dict], dict]:
    with Client(path) as client:
        client.hello()
        directory = client.request(1, {"op": "list_sessions", "include_archived": False})
        if directory.get("answer") != "sessions":
            raise ProtocolError("expected the session directory")
        node = next((row["node"] for row in directory["rows"] if row["session"] == session), None)
        if node is None:
            raise ProtocolError("session is not in the host directory")
        page = client.attach_first_page(2, node)
        return client.frames, page
