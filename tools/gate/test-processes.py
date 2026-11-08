#!/usr/bin/env python3
"""Fail a test command that leaves fixture hosts or runner daemons behind.

Only same-user processes whose cwd is inside this checkout's test TMPDIR are
eligible. Deleted cwd paths still count. Never inspect or print environments.
"""

from __future__ import annotations

import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


def fixture_path(path: str, root: Path) -> bool:
    return Path(path.removesuffix(" (deleted)")).is_relative_to(root)


def role(command: str) -> str | None:
    match = re.match(r"^(?:\S*/)?plak (runner serve|host)(?:\s|$)", command)
    return match[1] if match else None


def processes(root: Path) -> list[tuple[int, str]]:
    found = []
    if sys.platform.startswith("linux"):
        for entry in Path("/proc").iterdir():
            if not entry.name.isdecimal():
                continue
            try:
                if entry.stat().st_uid != os.getuid():
                    continue
                command = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode()
                kind = role(command)
                if kind and fixture_path(os.readlink(entry / "cwd"), root):
                    found.append((int(entry.name), kind))
            except (OSError, UnicodeError):
                continue
    else:
        rows = subprocess.check_output(
            ["ps", "-U", str(os.getuid()), "-o", "pid=,command="], text=True
        )
        for row in rows.splitlines():
            pid, command = row.strip().split(maxsplit=1)
            kind = role(command)
            if not kind:
                continue
            cwd = subprocess.run(
                ["lsof", "-a", "-p", pid, "-d", "cwd", "-Fn"],
                capture_output=True,
                text=True,
                check=False,
            )
            if any(
                fixture_path(line[1:], root)
                for line in cwd.stdout.splitlines()
                if line.startswith("n")
            ):
                found.append((int(pid), kind))
    return found


def reap_leaks(root: Path) -> bool:
    deadline = time.monotonic() + 2
    while leaks := processes(root):
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    if not leaks:
        return False
    cleanup_deadline = time.monotonic() + 5
    while leaks:
        # Stop launchers first, then re-list: a host losing its runner during
        # fail-fast may have launched a successor while cleanup was starting.
        for pid, kind in sorted(leaks, key=lambda row: row[1] != "host"):
            print(f"test process leak: pid {pid} ({kind})", file=sys.stderr, flush=True)
            # Recheck ownership and cwd before signalling. Only a process-group
            # leader authorizes a group signal.
            if (pid, kind) not in processes(root):
                continue
            try:
                if os.getpgid(pid) == pid:
                    os.killpg(pid, signal.SIGKILL)
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.05)
        leaks = processes(root)
        if time.monotonic() >= cleanup_deadline:
            if leaks:
                print("test process cleanup did not settle", file=sys.stderr, flush=True)
            break
    return True


def main(args: list[str]) -> int:
    root = Path(os.environ["TMPDIR"]).resolve()
    if reap_leaks(root):
        return 1
    if args == ["--check"]:
        return 0
    if not args or args[0] != "--":
        raise SystemExit("usage: test-processes.py --check | -- command...")
    child = subprocess.Popen(args[1:], start_new_session=True)
    interrupted = None

    def forward(signum, _frame):
        nonlocal interrupted
        interrupted = signum
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass

    signal.signal(signal.SIGTERM, forward)
    signal.signal(signal.SIGINT, forward)
    while child.poll() is None:
        try:
            child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            if interrupted:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
    leaked = reap_leaks(root)
    return (128 + interrupted) if interrupted else (child.returncode or int(leaked))


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
