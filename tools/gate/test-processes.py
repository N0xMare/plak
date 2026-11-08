#!/usr/bin/env python3
"""Own one isolated test invocation and reap only its fixture processes."""

from __future__ import annotations

from dataclasses import dataclass
import fcntl
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import secrets
import time


@dataclass(frozen=True)
class Process:
    pid: int
    parent: int
    identity: str
    kind: str | None


def role(command: str) -> str | None:
    match = re.match(r"^(?:\S*/)?plak (runner serve|host)(?:\s|$)", command)
    return match[1] if match else None


class ProcessTable:
    def __init__(self):
        self.cwd: dict[tuple[int, str], str | None] = {}
        self.warned = False

    def scan(self) -> dict[int, Process]:
        found = {}
        if sys.platform.startswith("linux"):
            for entry in Path("/proc").iterdir():
                try:
                    if not entry.name.isdecimal() or entry.stat().st_uid != os.getuid():
                        continue
                    fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
                    if fields[0] == "Z":
                        continue
                    argv = (entry / "cmdline").read_bytes().split(b"\0")
                    command = " ".join(os.fsdecode(word) for word in argv)
                    found[int(entry.name)] = Process(
                        int(entry.name), int(fields[1]), fields[19], role(command)
                    )
                except (OSError, UnicodeError, ValueError, IndexError):
                    continue
        else:
            rows = subprocess.check_output(
                ["ps", "-axww", "-o", "uid=,pid=,ppid=,lstart=,stat=,command="], text=True
            )
            for row in rows.splitlines():
                fields = row.split(maxsplit=9)
                if len(fields) != 10 or fields[0] != str(os.getuid()) or fields[8].startswith("Z"):
                    continue
                pid = int(fields[1])
                found[pid] = Process(pid, int(fields[2]), " ".join(fields[3:8]), role(fields[9]))
        live = {(p.pid, p.identity) for p in found.values()}
        self.cwd = {key: value for key, value in self.cwd.items() if key in live}
        return found

    def inside(self, process: Process, root: Path) -> bool:
        key = (process.pid, process.identity)
        if key not in self.cwd:
            try:
                if sys.platform.startswith("linux"):
                    cwd = os.readlink(f"/proc/{process.pid}/cwd")
                else:
                    result = subprocess.run(
                        ["lsof", "-a", "-p", str(process.pid), "-d", "cwd", "-Fn"],
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    cwd = next(
                        (line[1:] for line in result.stdout.splitlines() if line.startswith("n")),
                        None,
                    )
                self.cwd[key] = cwd
            except FileNotFoundError:
                if not sys.platform.startswith("linux"):
                    raise RuntimeError(
                        "test process guard requires lsof on this platform"
                    ) from None
                return False
            except OSError as error:
                if not self.warned:
                    print(f"test process inspection unavailable: {error}", file=sys.stderr)
                    self.warned = True
                return False
        cwd = self.cwd.get(key)
        return cwd is not None and Path(cwd.removesuffix(" (deleted)")).is_relative_to(root)

    def candidates(self, root: Path) -> list[Process]:
        return [p for p in self.scan().values() if p.kind and self.inside(p, root)]

    def kill(self, process: Process) -> None:
        current = self.scan().get(process.pid)
        if current is None or current.identity != process.identity:
            return
        try:
            if os.getpgid(process.pid) == process.pid:
                os.killpg(process.pid, signal.SIGKILL)
            else:
                os.kill(process.pid, signal.SIGKILL)
        except OSError:
            pass

    def descendants(self, parent: int) -> list[Process]:
        rows = self.scan()
        ordered = []

        def visit(pid):
            for child in rows.values():
                if child.parent == pid:
                    visit(child.pid)
                    ordered.append(child)

        visit(parent)
        return ordered


def reap_leaks(root: Path, table: ProcessTable, *, settle: bool) -> bool:
    deadline = time.monotonic() + (2 if settle else 0)
    while leaks := table.candidates(root):
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    if not leaks:
        return False
    reported = set()
    deadline = time.monotonic() + 5
    while leaks:
        # Stop launchers before daemons, then re-list successors launched as
        # the host dies. Descendants may own separate process groups.
        for process in sorted(leaks, key=lambda p: p.kind != "host"):
            key = (process.pid, process.identity)
            if key not in reported:
                print(
                    f"test process leak: pid {process.pid} ({process.kind})",
                    file=sys.stderr,
                    flush=True,
                )
                reported.add(key)
            children = table.descendants(process.pid)
            if process.kind == "host":
                table.kill(process)
            for child in children:
                table.kill(child)
            if process.kind != "host":
                table.kill(process)
        time.sleep(0.05)
        leaks = table.candidates(root)
        if time.monotonic() >= deadline:
            if leaks:
                print("test process cleanup did not settle", file=sys.stderr, flush=True)
            break
    return True


def reap_previous(base: Path, table: ProcessTable, *, remove: bool = False) -> None:
    for root in base.glob("run-*"):
        try:
            if root.is_symlink() or not root.is_dir() or root.stat().st_uid != os.getuid():
                continue
            with (root / "guard.lock").open("r+") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    continue
                if reap_leaks(root, table, settle=False):
                    print(
                        "test processes left by a previous run: cleanup attempted", file=sys.stderr
                    )
                if remove and not table.candidates(root):
                    shutil.rmtree(root)
        except OSError:
            continue


def run(args: list[str], root: Path, table: ProcessTable) -> int:
    home = root / "plak-isolated-home"
    home.mkdir(mode=0o700)
    gitconfig = root / "gitconfig"
    gitconfig.touch()
    env = dict(
        os.environ,
        TMPDIR=str(root),
        PLAK_TEST_TMPDIR=str(root),
        PLAK_HOME=str(home),
        GIT_CONFIG_GLOBAL=str(gitconfig),
    )
    child = None
    interrupted = None
    signals = 0

    def signal_child():
        if child is not None and child.poll() is None:
            try:
                os.killpg(child.pid, signal.SIGKILL if signals > 1 else interrupted)
            except OSError:
                pass

    def forward(signum, _frame):
        nonlocal interrupted, signals
        interrupted = signum
        signals += 1
        signal_child()
        # Do not spend a settle grace inside the supervisor's stop deadline.
        reap_leaks(root, table, settle=False)

    watched = (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT)
    previous = {signum: signal.signal(signum, forward) for signum in watched}
    try:
        child = subprocess.Popen(args, env=env, start_new_session=True)
        if interrupted is not None:
            signal_child()
        child.wait()
        leaked = reap_leaks(root, table, settle=interrupted is None)
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)
    if interrupted:
        return 128 + interrupted
    code = child.returncode
    return 128 - code if code < 0 else (code or int(leaked))


def main(args: list[str]) -> int:
    if args != ["--check"] and (not args or args[0] != "--"):
        raise SystemExit("usage: test-processes.py --check | -- command...")
    base = Path(os.environ["TMPDIR"]).resolve()
    table = ProcessTable()
    # Serialize publication with stale-run discovery: a freshly created run
    # must not be mistaken for an abandoned one before its lock is held.
    with (base / ".guard.lock").open("a") as publishing:
        fcntl.flock(publishing, fcntl.LOCK_EX)
        reap_previous(base, table, remove=args == ["--check"])
        if args == ["--check"]:
            return 0
        # Short nonces keep nested fixture homes within sockaddr_un limits.
        while True:
            root = base / f"run-{os.getpid()}-{secrets.token_hex(2)}"
            try:
                root.mkdir(mode=0o700)
                break
            except FileExistsError:
                continue
        lock = (root / "guard.lock").open("a")
        fcntl.flock(lock, fcntl.LOCK_EX)
    with lock:
        return run(args[1:], root, table)


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (OSError, RuntimeError) as error:
        print(f"test process guard: {error}", file=sys.stderr)
        raise SystemExit(2) from None
