"""Receipts for pure workspace policy checks.

The caller must supply every input explicitly. Discovery and checks that read
the filesystem or invoke subprocesses are not eligible. Only hashes of inputs
that produced no findings are stored, never source or diagnostics.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import tempfile
from collections.abc import Callable
from contextlib import suppress
from pathlib import Path

_HEX = re.compile(r"[0-9a-f]{64}")


def _digest(value: object) -> str:
    return hashlib.sha256(
        json.dumps(value, separators=(",", ":"), sort_keys=True).encode()
    ).hexdigest()


class Receipts:
    def __init__(self, path: Path, identity: str) -> None:
        self.path = path
        self.identity = identity
        self.previous: set[str] = set()
        self.current: set[str] = set()
        self.reused = False
        # Retain the original string, so an id cannot be reused during a scan.
        self._sources: dict[int, tuple[str | bytes, str]] = {}
        try:
            payload = json.loads(path.read_bytes())
            if not isinstance(payload, dict) or set(payload) != {"body", "checksum"}:
                return
            body = payload["body"]
            if not isinstance(body, dict) or set(body) != {"identity", "passed"}:
                return
            passed = body["passed"]
            if (
                body["identity"] == identity
                and isinstance(passed, list)
                and all(isinstance(item, str) and _HEX.fullmatch(item) for item in passed)
                and payload["checksum"] == _digest(body)
            ):
                self.previous = set(passed)
        except (OSError, ValueError, RecursionError):
            pass

    def source(self, data: str | bytes) -> str:
        key = id(data)
        if key not in self._sources:
            raw = data if isinstance(data, bytes) else data.encode("utf-8")
            self._sources[key] = (data, hashlib.sha256(raw).hexdigest())
        return self._sources[key][1]

    def check(self, check: Callable[[], list[str]], rule: str, *inputs: object) -> list[str]:
        try:
            key = _digest([rule, *inputs])
        except (TypeError, ValueError, RecursionError):
            # An unrecognised policy value cannot authorize a skip.
            return check()
        if key in self.previous:
            self.reused = True
            self.current.add(key)
            return []
        findings = check()
        if not findings:
            self.current.add(key)
        return findings

    def write(self) -> None:
        body = {"identity": self.identity, "passed": sorted(self.current)}
        payload = {"body": body, "checksum": _digest(body)}
        path = self.path
        temporary = None
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
                temporary = Path(stream.name)
                stream.write(json.dumps(payload, separators=(",", ":")).encode())
            os.replace(temporary, path)
        except OSError:
            if temporary is not None:
                with suppress(OSError):
                    temporary.unlink(missing_ok=True)
