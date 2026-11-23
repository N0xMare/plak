"""The two client transports used by the offline host latency workload."""

from __future__ import annotations

import asyncio
import itertools
import json
import time
import uuid
from dataclasses import dataclass
from contextlib import suppress

WIRE = 12
LOGIN = "probe@example.invalid"
AUTHORITY = "probe.example.invalid"
ORIGIN = f"https://{AUTHORITY}"
FORWARDED = {
    "Host": "localhost",
    "X-Forwarded-Host": AUTHORITY,
    "X-Forwarded-Proto": "https",
    "X-Forwarded-For": "192.0.2.10",
    "Tailscale-User-Login": LOGIN,
}
DEADLINE = 60


class Refused(RuntimeError):
    def __init__(self, outcome):
        super().__init__(str(outcome)[:400])
        self.outcome = outcome


class Detached(ConnectionError):
    """This attachment needs a fresh baseline, but the transport may still work."""


@dataclass
class Attachment:
    first: float | None = None
    last: float | None = None
    pages: int = 0
    bytes: int = 0
    repair_bytes: int = 0
    seq: int = 0
    ended: bool = False
    deltas: int = 0
    last_delta: float = 0
    terminal: int = 0
    phase: str | None = None


async def browser_upgrade(socket, login=LOGIN):
    from websockets.asyncio.client import unix_connect

    reader, writer = await asyncio.open_unix_connection(socket)
    try:
        forward = {**FORWARDED, "Tailscale-User-Login": login}
        head = "".join(f"{k}: {v}\r\n" for k, v in forward.items())
        writer.write(f"GET / HTTP/1.1\r\n{head}Connection: close\r\n\r\n".encode())
        await writer.drain()
        raw = await reader.readuntil(b"\r\n\r\n")
        fields = raw.decode().splitlines()
        if fields[0].split()[1] != "200":
            raise RuntimeError(f"scratch page refused: {fields[0]}")
        cookie = next(
            line.split(": ", 1)[1].split(";", 1)[0]
            for line in fields
            if line.lower().startswith("set-cookie: ")
        )
    finally:
        writer.close()
        await writer.wait_closed()
    # websockets supplies Host itself. Override it using the URI authority.
    headers = {k: v for k, v in forward.items() if k != "Host"}
    return await unix_connect(
        str(socket),
        uri="ws://localhost/wire",
        origin=ORIGIN,
        additional_headers={**headers, "Cookie": cookie},
        compression=None,
        ping_interval=None,
        max_size=64 << 20,
        max_queue=4,
    )


class Client:
    def __init__(self, fixture, face, *, login=LOGIN, read_delay=0, observe=None):
        self.fixture = fixture
        self.face = face
        self.bridge = None
        self.ws = None
        self.reader = None
        self.failure = None
        self.pending = {}
        self.attachments = {}
        self.changed = asyncio.Event()
        self.ids = itertools.count(1)
        self.resyncs = 0
        self.sequence_gaps = 0
        self.active = None
        self.login = login
        self.read_delay = read_delay
        self.observe = observe
        self.welcome = None

    async def __aenter__(self):
        try:
            if self.face == "terminal":
                self.bridge = await self.fixture.spawn(
                    "host",
                    "connect",
                    "--stdio",
                    pipes=True,
                )
            else:
                self.ws = await asyncio.wait_for(
                    browser_upgrade(self.fixture.socket, self.login), DEADLINE
                )
            await self.send(
                {"frame": "hello", "proto": WIRE, "binary_id": "host-latency", "caps": []}
            )
            frame, _, _ = await asyncio.wait_for(self.receive(), DEADLINE)
            if frame.get("frame") != "welcome" or frame.get("proto") != WIRE:
                raise RuntimeError(f"wire {WIRE} handshake failed: {str(frame)[:400]}")
            self.welcome = frame
            self.reader = asyncio.create_task(self.drain())
            return self
        except BaseException:
            await self.close()
            raise

    async def __aexit__(self, *_):
        await self.close()

    async def close(self):
        if self.reader:
            self.reader.cancel()
            with suppress(asyncio.CancelledError):
                await self.reader
        if self.ws:
            await self.ws.close()
        if self.bridge:
            await self.fixture.stop(self.bridge)

    async def send(self, frame):
        data = json.dumps(frame, separators=(",", ":"))
        if self.ws:
            from websockets.exceptions import ConnectionClosed

            try:
                await self.ws.send(data)
            except ConnectionClosed as error:
                raise ConnectionError("websocket closed while sending") from error
        else:
            self.bridge.stdin.write(data.encode() + b"\n")
            await self.bridge.stdin.drain()

    async def receive(self):
        if self.ws:
            if self.read_delay:
                await asyncio.sleep(self.read_delay)
            raw = await self.ws.recv(decode=False)
        else:
            raw = await self.bridge.stdout.readline()
            if not raw:
                raise ConnectionError("stdio bridge EOF")
        return json.loads(raw), time.monotonic(), len(raw)

    async def drain(self):
        try:
            while True:
                frame, stamp, size = await self.receive()
                if self.observe:
                    self.observe(frame, stamp)
                kind = frame["frame"]
                if kind == "response":
                    future = self.pending.get(frame["id"])
                    if future is not None and not future.done():
                        future.set_result((frame["outcome"], stamp))
                elif kind in ("snapshot", "event", "resync_required", "moved"):
                    state = self.attachments.setdefault(frame["attachment"], Attachment())
                    if kind == "snapshot" and frame["page"] == 0:
                        # One snapshot observation consumes one sequence position.
                        state.seq += 1
                    if kind == "snapshot" and not state.last:
                        state.first = state.first or stamp
                        state.pages += 1
                        state.bytes += size
                        if frame["last"]:
                            state.last = stamp
                    elif kind == "snapshot":
                        state.repair_bytes += size
                    elif kind == "event":
                        if frame["seq"] != state.seq + 1:
                            self.sequence_gaps += 1
                        state.seq = frame["seq"]
                        observation = frame["observation"]
                        if observation["kind"] == "assistant_delta" or (
                            observation["kind"] == "transcript_stream"
                            and isinstance(observation["body"]["delta"], dict)
                            and "assistant" in observation["body"]["delta"]
                        ):
                            state.deltas += 1
                            state.last_delta = stamp
                        if observation.get("kind") == "turn_lifecycle":
                            phase = observation["body"]["phase"]
                            if phase != "admitted":
                                state.terminal += 1
                                state.phase = phase
                    else:
                        if kind == "resync_required":
                            self.resyncs += 1
                        state.ended = True
                    self.changed.set()
        except asyncio.CancelledError:
            pass
        except Exception as error:
            self.failure = ConnectionError(f"{self.face} receive: {str(error)[:300]}")
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(self.failure)
            self.changed.set()

    async def wait(self, predicate, timeout=DEADLINE):
        async with asyncio.timeout(timeout):
            while True:
                self.changed.clear()
                if self.failure:
                    raise self.failure
                if result := predicate():
                    return result
                await self.changed.wait()

    async def request(self, op, *, sent=None, deadline=DEADLINE):
        if self.failure:
            raise self.failure
        request_id = next(self.ids)
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        start = time.monotonic()
        try:
            async with asyncio.timeout(deadline):
                await self.send({"frame": "request", "id": request_id, "op": op})
                if sent:
                    sent.set()
                outcome, end = await future
        finally:
            self.pending.pop(request_id)
        if "ok" not in outcome:
            raise Refused(outcome)
        return outcome["ok"], start, end

    async def attach(self, node, *, sent=None):
        reply, start, _ = await self.request({"op": "attach", "node": node}, sent=sent)
        self.active = reply["attachment"]
        state = self.attachments.setdefault(self.active, Attachment())
        await self.wait(lambda: state.last or state.ended)
        if state.ended:
            raise Detached("baseline attachment ended")
        return state, start

    async def detach(self):
        if self.active is not None:
            state = self.attachments[self.active]
            if not state.ended:
                await self.request({"op": "detach", "attachment": self.active})
            self.attachments.pop(self.active)
            self.active = None

    async def intent(self, text, kind="prompt", *, client_id=None):
        return await self.request(
            {
                "op": "intent",
                "attachment": self.active,
                "client_id": client_id or str(uuid.uuid4()),
                "intent": {"kind": kind, "body": {"text": text, "images": []}},
                "blobs": [],
            }
        )
