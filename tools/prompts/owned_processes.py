"""Bounded subprocess execution and cleanup of owned process trees."""

import asyncio
from contextvars import ContextVar
from functools import wraps
import os
from pathlib import Path
import signal
import subprocess
import time

OUTPUT_LIMIT = 32 * 1024 * 1024


# Nested owners share records so setup and postprocessing exceptions retain every
# process already admitted by that operation. Records contain structural facts only.
_cleanup_records = ContextVar("qualification_cleanup_records", default=None)


def preserve_cleanup(function):
    @wraps(function)
    async def wrapped(*args, **kwargs):
        records = _cleanup_records.get()
        token = None
        if records is None:
            records = []
            token = _cleanup_records.set(records)
        try:
            return await function(*args, **kwargs)
        except BaseException as error:
            evidence = dict(getattr(error, "qualification_processes", {}))
            evidence["owned_commands"] = records.copy()
            error.qualification_processes = evidence
            raise
        finally:
            if token is not None:
                _cleanup_records.reset(token)

    return wrapped


def pending_cleanup(record=None):
    record = {} if record is None else record
    record.update(joined=False, observation="cleanup_not_completed")
    records = _cleanup_records.get()
    if records is not None:
        records.append({"cleanup": record})
    return record


async def shield_cleanup(operation):
    """Finish bounded cleanup and its receipt before propagating cancellation."""
    cancelled = asyncio.CancelledError() if asyncio.current_task().cancelling() else None
    task = asyncio.create_task(operation)
    try:
        while not task.done():
            try:
                await asyncio.shield(task)
            except asyncio.CancelledError as error:
                cancelled = cancelled or error
        result = task.result()
    except BaseException as error:
        if cancelled:
            raise cancelled from error
        raise
    if cancelled:
        raise cancelled
    return result


def processes():
    """Return PID -> (parent, group, birth identity, state) on Linux or macOS."""
    rows = {}
    if Path("/proc/self/stat").exists():
        for path in Path("/proc").iterdir():
            if not path.name.isdigit():
                continue
            try:
                parts = (path / "stat").read_text().rsplit(")", 1)[1].split()
                rows[int(path.name)] = (
                    int(parts[1]),
                    int(parts[2]),
                    parts[19],
                    parts[0],
                )
            except (OSError, ValueError, IndexError):
                pass
    else:
        output = subprocess.run(
            ["ps", "-axo", "pid=,ppid=,pgid=,lstart=,stat="],
            capture_output=True,
            text=True,
            timeout=5,
            check=True,
        ).stdout
        for line in output.splitlines():
            fields = line.split()
            if len(fields) >= 9:
                rows[int(fields[0])] = (
                    int(fields[1]),
                    int(fields[2]),
                    " ".join(fields[3:8]),
                    fields[8],
                )
    return rows


class PtyProcess:
    """Waitable child created by pty.fork; only this owner reaps its PID."""

    def __init__(self, pid):
        self.pid = pid
        self._returncode = None
        self._reaped_elsewhere = False

    @property
    def returncode(self):
        if self._returncode is None and not self._reaped_elsewhere:
            try:
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self._returncode = os.waitstatus_to_exitcode(status)
            except ChildProcessError:
                self._reaped_elsewhere = True
        return self._returncode

    async def wait(self):
        while self.returncode is None and not self._reaped_elsewhere:
            await asyncio.sleep(0.02)
        return self.returncode


class Owned:
    """Track only this newly spawned process, its group, and observed descendants."""

    def __init__(self, proc):
        self.proc = proc
        initial = processes().get(proc.pid)
        self.known = {proc.pid: initial[2]} if initial else {}
        self.running = True
        self.capture()
        self.task = asyncio.create_task(self.watch())

    def capture(self):
        rows = processes()
        parents = {
            pid
            for pid, birth in self.known.items()
            if pid in rows and rows[pid][2] == birth
        }
        group_owned = any(rows[pid][1] == self.proc.pid for pid in parents)
        while True:
            found = {
                pid
                for pid, row in rows.items()
                if (row[0] in parents or (group_owned and row[1] == self.proc.pid))
                and (pid not in self.known or self.known[pid] == row[2])
            }
            if found <= parents:
                break
            parents.update(found)
        for pid in parents:
            if pid in rows:
                self.known.setdefault(pid, rows[pid][2])
        return {
            pid
            for pid, birth in self.known.items()
            if pid in rows
            and rows[pid][2] == birth
            and not rows[pid][3].startswith("Z")
        }

    async def watch(self):
        while self.running:
            self.capture()
            await asyncio.sleep(0.1)

    async def close(self):
        forced = False
        try:
            if self.proc.returncode is not None:
                until = time.monotonic() + 0.5
                while self.capture() and time.monotonic() < until:
                    await asyncio.sleep(0.05)
            if self.capture():
                forced = True
                for sig, budget in [(signal.SIGTERM, 2), (signal.SIGKILL, 3)]:
                    until = time.monotonic() + budget
                    while self.capture() and time.monotonic() < until:
                        for pid in self.capture():
                            try:
                                os.kill(pid, sig)
                            except ProcessLookupError:
                                pass
                        await asyncio.sleep(0.05)
                    if not self.capture():
                        break
            try:
                await asyncio.wait_for(self.proc.wait(), 3)
            except TimeoutError:
                pass
            remaining = self.capture()
            return {
                "forced": forced,
                "joined": not remaining and self.proc.returncode is not None,
                "remaining_owned_processes": len(remaining),
            }
        finally:
            self.running = False
            await self.task


async def capture_stream(reader, path, exceeded, limit=OUTPUT_LIMIT):
    """Keep bounded private diagnostics, draining excess until the owner stops.

    Every chunk is flushed: the signal anchor in `signal_interrupt` polls the
    on-disk size for the turn's first byte, and an unflushed writer keeps the
    file empty until process close — the anchor then fires at its deadline into
    an idle or finished process instead of landing inside the turn.
    """
    written = 0
    with Path(path).open("xb") as output:
        os.chmod(path, 0o600)
        while chunk := await reader.read(65536):
            available = max(0, limit - written)
            output.write(chunk[:available])
            output.flush()
            written += len(chunk)
            if written > limit:
                exceeded.set()


@preserve_cleanup
async def run_command(
    argv, *, cwd, env, stdout_path, stderr_path, timeout, cleanup_record=None
):
    """Run with an absolute wall deadline; output never refreshes that deadline."""
    started = time.monotonic()
    proc = await asyncio.create_subprocess_exec(
        *argv,
        cwd=cwd,
        env=env,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )
    cleanup_record = pending_cleanup(cleanup_record)
    owned = Owned(proc)
    exceeded = asyncio.Event()
    pumps = [
        asyncio.create_task(capture_stream(reader, path, exceeded))
        for reader, path in [(proc.stdout, stdout_path), (proc.stderr, stderr_path)]
    ]
    waiter = asyncio.create_task(proc.wait())
    limit_waiter = asyncio.create_task(exceeded.wait())
    timed_out = False
    try:
        done, _ = await asyncio.wait(
            [waiter, limit_waiter], timeout=timeout, return_when=asyncio.FIRST_COMPLETED
        )
        timed_out = not done
    finally:
        async def finish_cleanup():
            cleanup = None
            try:
                cleanup = await owned.close()
            finally:
                limit_waiter.cancel()
                await asyncio.gather(limit_waiter, return_exceptions=True)
                try:
                    await asyncio.wait_for(asyncio.gather(*pumps), 3)
                except TimeoutError:
                    if cleanup is not None:
                        cleanup["joined"] = False
                finally:
                    if not waiter.done():
                        waiter.cancel()
                    await asyncio.gather(waiter, return_exceptions=True)
                if cleanup_record is not None and cleanup is not None:
                    cleanup_record.clear()
                    cleanup_record.update(cleanup)
            return cleanup

        def record_execution(record):
            # Stash the execution facts on the owned-command record too, so a
            # case preempted before it returns still carries them. Without this
            # a preempted row keeps only cleanup flags and loses whether the
            # process exited, how long it ran and whether it timed out.
            if record is not None:
                record["execution"] = dict(
                    returncode=proc.returncode,
                    timed_out=timed_out,
                    seconds=round(time.monotonic() - started, 3),
                )

        cleanup = await shield_cleanup(finish_cleanup())
        record_execution(cleanup_record)
    return {
        "returncode": proc.returncode,
        "timed_out": timed_out,
        "output_limit": exceeded.is_set(),
        "cleanup": cleanup,
        "seconds": round(time.monotonic() - started, 3),
    }
