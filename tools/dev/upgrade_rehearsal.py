#!/usr/bin/env python3
"""Rehearse a test-only cross-key host move using isolated real binaries on Linux."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

SOURCE_KEY = "r27.e37.p37.t10"
DESTINATION_KEY = "r28.e38.p38.t11"
CUTS = (
    "move-after-source-prepare",
    "move-after-commit",
    "move-after-tree-seed",
    "move-after-publish",
)
IMAGES = ("plak", "plakous-js-host", "plakous-py-host")


class Refused(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def run(command, *, env=None, cwd=None, timeout=120, check=True):
    result = subprocess.run(
        command, env=env, cwd=cwd, text=True, capture_output=True, timeout=timeout
    )
    if check and result.returncode:
        raise Refused(
            f"command exited {result.returncode}: {command}\n{result.stdout}\n{result.stderr}"
        )
    return result


def wait_for(predicate, description, seconds=60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise Refused(f"deadline: {description}")


def prepare_root(path):
    path = path.absolute()
    require(path == path.resolve(), "scratch root must not traverse symlinks")
    require(
        path != Path.home() and Path.home() not in path.parents[:1],
        "use a dedicated scratch directory",
    )
    forbidden = [Path.home() / ".plak"]
    if os.environ.get("PLAK_HOME"):
        forbidden.append(Path(os.environ["PLAK_HOME"]).resolve())
    require(
        not any(path == item or item in path.parents or path in item.parents for item in forbidden),
        "scratch root overlaps an operator home",
    )
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    require(
        path.stat().st_uid == os.getuid() and path.stat().st_mode & 0o077 == 0,
        "scratch root must be owned and private (0700)",
    )
    owned = path / "owned"
    marker = path / "rehearsal-owner.json"
    if owned.exists():
        require(
            marker.is_file()
            and json.loads(marker.read_text()) == {"root": str(path), "uid": os.getuid()},
            "refusing an unrecognized existing scratch directory",
        )
    else:
        write_json(marker, {"root": str(path), "uid": os.getuid()})
        owned.mkdir(mode=0o700)
    return path


def build(args, root):
    """Never install, move PATH links, or build in the operator's checkout."""
    owned = root / "owned"
    repository = owned / "repository"
    if not repository.exists():
        run(
            ["git", "clone", "--bare", str(args.repository.resolve()), str(repository)], timeout=300
        )
    manifest = {}
    for name, revision in [("A", args.source), ("B", args.destination)]:
        commit = run(
            ["git", "--git-dir", str(repository), "rev-parse", f"{revision}^{{commit}}"]
        ).stdout.strip()
        checkout = owned / f"checkout-{name}"
        if checkout.exists():
            require(
                run(["git", "-C", str(checkout), "rev-parse", "HEAD"]).stdout.strip() == commit,
                f"{name}: existing checkout is a different revision",
            )
            require(
                not run(["git", "-C", str(checkout), "status", "--porcelain"]).stdout.strip(),
                f"{name}: checkout is dirty",
            )
        else:
            run(
                [
                    "git",
                    "--git-dir",
                    str(repository),
                    "worktree",
                    "add",
                    "--detach",
                    str(checkout),
                    commit,
                ]
            )
        environment = os.environ.copy()
        environment.update(CARGO_TARGET_DIR=str(owned / "target"), CARGO_BUILD_JOBS=str(args.jobs))
        command = [
            "bash",
            "tools/build/build-plak.sh",
            "--maintainer",
            "--features",
            "plak/maintainer,plak/test-hooks",
        ]
        print(f"BUILD {name} {commit}: {' '.join(command)}", flush=True)
        with (root / f"build-{name}.log").open("w") as log:
            result = subprocess.run(
                command, cwd=checkout, env=environment, stdout=log, stderr=subprocess.STDOUT
            )
        require(result.returncode == 0, f"{name}: build failed; see build-{name}.log")
        snapshot = owned / "images" / name
        snapshot.mkdir(parents=True, exist_ok=True)
        for image in IMAGES:
            shutil.copy2(owned / "target" / "maintainer" / image, snapshot / image)
        digest = hashlib.sha256((snapshot / "plak").read_bytes()).hexdigest()
        manifest[name] = {
            "revision": commit,
            "sha256": digest,
            "binary_id": digest[:12],
            "binary": str(snapshot / "plak"),
            "profile": "maintainer",
            "features": ["maintainer", "test-hooks"],
        }
        write_json(root / "builds.json", manifest)
        print(f"BUILT {name}: {digest[:12]}", flush=True)
    return manifest


def environment(case):
    """An allowlist, not a copy of the caller's credentials or operator settings."""
    names = {
        "HOME": "user",
        "PLAK_HOME": "home",
        "TMPDIR": "tmp",
        "XDG_CONFIG_HOME": "config",
        "XDG_DATA_HOME": "data",
        "XDG_CACHE_HOME": "cache",
        "XDG_STATE_HOME": "state",
        "XDG_RUNTIME_DIR": "runtime",
    }
    env = {"PATH": os.defpath, "LANG": "C.UTF-8", "GIT_CONFIG_NOSYSTEM": "1"}
    for key, name in names.items():
        path = case / name
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        env[key] = str(path)
    empty_git = case / "gitconfig"
    empty_git.touch()
    env.update(
        GIT_CONFIG_GLOBAL=str(empty_git),
        PLAK_TEST_MODEL="offline-loopback-v1",
        PLAK_TEST_SCRIPT=str(case / "script.json"),
        PLAK_TEST_RUNNER_DETACHED="1",
        PLAK_TEST_EVIDENCE=str(case / "dispatches.jsonl"),
        PLAK_TEST_HOST_UPGRADE_DORMANT="1",
    )
    return env


def owned_processes(case):
    """Read only same-user process metadata; retain PID birth identity for signaling."""
    found = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal() or int(entry.name) == os.getpid():
            continue
        try:
            if entry.stat().st_uid != os.getuid():
                continue
            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
            if fields[0] == "Z":
                continue
            cwd = (entry / "cwd").resolve()
            inside = cwd == case or case in cwd.parents
            if not inside:
                executable = (entry / "cmdline").read_bytes().split(b"\0", 1)[0]
                if Path(os.fsdecode(executable)).name == "plak":
                    expected = os.fsencode(f"PLAK_HOME={case / 'home'}")
                    inside = expected in (entry / "environ").read_bytes().split(b"\0")
            if inside:
                found[int(entry.name)] = fields[19]
        except (OSError, ValueError, IndexError):
            continue
    return found


def cleanup(case, children):
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pid, identity in owned_processes(case).items():
            if owned_processes(case).get(pid) == identity:
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 3
        while owned_processes(case) and time.monotonic() < deadline:
            time.sleep(0.05)
    for child in children:
        child.wait(timeout=10)
    require(not owned_processes(case), "scratch processes survived cleanup")
    shutil.rmtree(case)
    require(not case.exists(), "scratch state survived cleanup")


def read_rows(path, query, parameters=()):
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=5) as db:
        db.row_factory = sqlite3.Row
        return [dict(row) for row in db.execute(query, parameters)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["build", "run", "all", "cleanup"])
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--repository", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--source", default="main")
    parser.add_argument("--destination", default="HEAD")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--arm", choices=["all", "bootstrap", "dormant"], default="all")
    parser.add_argument(
        "--case", action="append", help="select a named case (repeatable); zero matches refuse"
    )
    args = parser.parse_args()
    require(sys.platform == "linux", "this real-binary rehearsal requires scratch Linux")
    require(args.jobs > 0, "--jobs must be positive")
    root = prepare_root(args.root)
    if args.command in {"build", "all"}:
        build(args, root)
    if args.command in {"run", "all"}:
        from upgrade_rehearsal_cases import rehearse

        rehearse(args, root, sys.modules[__name__])
    if args.command == "cleanup":
        require(
            not owned_processes(root / "owned"),
            "stop rehearsal processes before removing build artifacts",
        )
        shutil.rmtree(root / "owned")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"upgrade rehearsal: {error}", file=sys.stderr)
        raise SystemExit(1) from error
