"""Small structural process/socket samples around a Harbor agent run."""

from __future__ import annotations

import asyncio
import functools
import json
import sys
import time
from datetime import datetime, timezone
from typing import Any

from harbor_adapter.hygiene import harvest_command, native_state_harvest


def observe_run(run):
    """Identical non-mutating process samples, even on timeout or failure."""
    @functools.wraps(run)
    async def observed(self, instruction, environment, context):
        from harbor_adapter.cell_prep import record_cell
        from harbor_adapter.preflight import task_budgets
        self._eval_instruction = instruction
        self._eval_deadline = time.monotonic() + task_budgets(environment)[0] - 10
        await record_cell(self, environment)
        from harbor_adapter.wire import start_recording, harvest_recording
        samples = []
        recording_error = None
        try:
            await start_recording(self, environment)
            samples.append(await sample_cell(environment, "before_agent_run"))
            return await run(self, instruction, environment, context)
        finally:
            primary_error = sys.exc_info()[0]
            try:
                await harvest_recording(self, environment)
            except Exception as error:
                recording_error = error
            samples.append(await sample_cell(environment, "after_agent_run"))
            try:
                (self.logs_dir / "lifecycle.json").write_text(
                    json.dumps({"schema_version": 1, "samples": samples}, indent=2) + "\n"
                )
            except Exception:
                if primary_error is None:
                    raise
            finally:
                try:
                    if getattr(self, "_native_harness", None) in ("pi", "opencode"):
                        await self.exec_as_root(environment, command=native_state_harvest())
                    await self.exec_as_root(environment, command=harvest_command())
                except Exception:
                    if primary_error is None:
                        raise
            if primary_error is None and recording_error is not None:
                raise recording_error
    return observed


SAMPLE_COMMAND = (
    "printf 'PROCESSES\\n'; "
    "ps -eo pid=,ppid=,pgid=,stat= 2>/dev/null | head -n 256 || true; "
    "printf 'SOCKETS\\n'; "
    "awk 'NF > 9 && $4 == \"0A\" {split($2,a,\":\"); print a[2], $10}' "
    "/proc/net/tcp /proc/net/tcp6 2>/dev/null | head -n 128 || true; "
    "printf 'RESOURCES\\n'; "
    "for metric in memory.peak memory.events cpu.stat; do "
    "printf '%s\\n' \"$metric\"; cat /sys/fs/cgroup/$metric 2>/dev/null || true; done"
)


def parse_sample(raw: str, phase: str) -> dict[str, Any]:
    """Keep identifiers and states only; never command lines or socket IPs."""
    result: dict[str, Any] = {
        "phase": phase,
        "observed_at_utc": datetime.now(timezone.utc).isoformat(),
        "processes": [],
        "listen_sockets": [],
        "resources": {},
    }
    section = ""
    metric = ""
    for line in raw.splitlines():
        if line in ("PROCESSES", "SOCKETS", "RESOURCES"):
            section = line
            continue
        parts = line.split()
        if section == "RESOURCES":
            if line in ("memory.peak", "memory.events", "cpu.stat"):
                metric = line
            elif len(parts) == 1 and parts[0].isdigit() and metric == "memory.peak":
                result["resources"]["memory_peak_bytes"] = int(parts[0])
            elif len(parts) == 2 and parts[1].isdigit():
                key = (metric, parts[0])
                name = {("memory.events", "oom_kill"): "oom_kill",
                        ("memory.events", "oom"): "oom",
                        ("cpu.stat", "usage_usec"): "cpu_usage_usec"}.get(key)
                if name:
                    result["resources"][name] = int(parts[1])
            continue
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
