"""Small structural process/socket samples around a Harbor agent run."""

from __future__ import annotations

import asyncio
from datetime import datetime, timezone
from typing import Any


SAMPLE_COMMAND = (
    "printf 'PROCESSES\\n'; "
    "ps -eo pid=,ppid=,pgid=,stat= 2>/dev/null | head -n 256 || true; "
    "printf 'SOCKETS\\n'; "
    "awk 'NF > 9 && $4 == \"0A\" {split($2,a,\":\"); print a[2], $10}' "
    "/proc/net/tcp /proc/net/tcp6 2>/dev/null | head -n 128 || true"
)


def parse_sample(raw: str, phase: str) -> dict[str, Any]:
    """Keep identifiers and states only; never command lines or socket IPs."""
    result: dict[str, Any] = {
        "phase": phase,
        "observed_at_utc": datetime.now(timezone.utc).isoformat(),
        "processes": [],
        "listen_sockets": [],
    }
    section = ""
    for line in raw.splitlines():
        if line in ("PROCESSES", "SOCKETS"):
            section = line
            continue
        parts = line.split()
        if section == "PROCESSES" and len(parts) == 4:
            try:
                pid, ppid, pgid = (int(value) for value in parts[:3])
            except ValueError:
                continue
            if pid > 0 and ppid >= 0 and pgid >= 0 and parts[3][0].isalpha():
                result["processes"].append(
                    {"pid": pid, "ppid": ppid, "pgid": pgid, "state": parts[3][0]}
                )
        elif section == "SOCKETS" and len(parts) == 2:
            try:
                port = int(parts[0], 16)
                inode = int(parts[1])
            except ValueError:
                continue
            if 0 < port < 65536 and inode > 0:
                result["listen_sockets"].append({"port": port, "inode": inode})
    result["processes"] = result["processes"][:256]
    result["listen_sockets"] = result["listen_sockets"][:128]
    return result


async def sample_cell(environment: Any, phase: str) -> dict[str, Any]:
    try:
        output = await asyncio.wait_for(environment.exec(command=SAMPLE_COMMAND), 10)
        if output.return_code == 0:
            return parse_sample(output.stdout or "", phase)
    except Exception:
        pass
    return {"phase": phase, "observed_at_utc": datetime.now(timezone.utc).isoformat(),
            "unavailable": True}
