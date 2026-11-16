#!/usr/bin/env python3
"""Rehearse a test-only cross-key host move using isolated real binaries on Linux."""

from __future__ import annotations

import argparse
import fcntl
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
SIDECAR_IMAGES = ("plakous-js-host", "plakous-py-host")


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


def check_probe_shape(source, destination, path="probe"):
    """Check the strict source probe's JSON shape before starting any host.

    This is a cheap structural guard, not a substitute for the actual source's
    capability admission during the bootstrap. config_error is Option<String>;
    the remaining fields (including VersionTriple) are required and strict.
    """
    if isinstance(source, dict):
        require(isinstance(destination, dict), f"{path}: expected an object")
        unknown = destination.keys() - source.keys()
        require(not unknown, f"{path}: source rejects unknown fields {sorted(unknown)}")
        missing = source.keys() - destination.keys()
        if path == "probe":
            missing -= {"config_error"}
        require(not missing, f"{path}: missing source fields {sorted(missing)}")
        for key, value in destination.items():
            check_probe_shape(source[key], value, f"{path}.{key}")
    elif path == "probe.config_error":
        require(
            destination is None or isinstance(destination, str), f"{path}: expected null/string"
        )
    elif type(source) is int:
        require(
            type(destination) is int and 0 <= destination <= 0xFFFF_FFFF, f"{path}: expected u32"
        )
    elif isinstance(source, list):
        require(isinstance(destination, list), f"{path}: expected an array")
        require(bool(source), f"{path}: cannot infer an empty source probe array")
        for index, value in enumerate(destination):
            check_probe_shape(source[0], value, f"{path}[{index}]")
    else:
        require(type(destination) is type(source), f"{path}: expected {type(source).__name__}")


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
        require(not owned.is_symlink(), "owned scratch directory must not be a symlink")
        require(
            marker.is_file()
            and json.loads(marker.read_text()) == {"root": str(path), "uid": os.getuid()},
            "refusing an unrecognized existing scratch directory",
        )
    else:
        write_json(marker, {"root": str(path), "uid": os.getuid()})
        owned.mkdir(mode=0o700)
    return path


def product_image(checkout):
    """Historical rehearsal revisions own their package and build-script layout."""
    products = [name for name in ("plak", "plakd") if (checkout / "bin" / name / "Cargo.toml").is_file()]
    require(len(products) == 1, f"{checkout}: expected exactly one product package")
    product = products[0]
    require((checkout / f"tools/build/build-{product}.sh").is_file(), f"{checkout}: missing product build script")
    return product


def host_is_serving(text):
    return any(f"{name} host: serving on " in text for name in ("plak", "plakd"))


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
        target = owned / "targets" / commit
        environment = os.environ.copy()
        environment.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS=str(args.jobs))
        product = product_image(checkout)
        command = [
            "bash",
            f"tools/build/build-{product}.sh",
            "--maintainer",
            "--features",
            f"{product}/maintainer,{product}/test-hooks",
        ]
        print(f"BUILD {name} {commit}: {' '.join(command)}", flush=True)
        with (root / f"build-{name}.log").open("w") as log:
            result = subprocess.run(
                command, cwd=checkout, env=environment, stdout=log, stderr=subprocess.STDOUT
            )
        require(result.returncode == 0, f"{name}: build failed; see build-{name}.log")
        snapshot = owned / "images" / name
        snapshot.mkdir(parents=True, exist_ok=True)
        for image in (product, *SIDECAR_IMAGES):
            shutil.copy2(target / "maintainer" / image, snapshot / image)
        digest = hashlib.sha256((snapshot / product).read_bytes()).hexdigest()
        manifest[name] = {
            "revision": commit,
            "sha256": digest,
            "binary_id": digest[:12],
            "binary": str(snapshot / product),
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
                if Path(os.fsdecode(executable)).name in {"plak", "plakd"}:
                    for value in (entry / "environ").read_bytes().split(b"\0"):
                        if value.startswith(b"PLAK_HOME="):
                            home = Path(os.fsdecode(value.split(b"=", 1)[1])).resolve()
                            inside = home == case or case in home.parents
                            break
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


def execute(args, root):
    try:
        if args.command in {"build", "all"}:
            build(args, root)
        if args.command in {"run", "all"}:
            from upgrade_rehearsal_cases import rehearse

            rehearse(args, root, sys.modules[__name__])
    finally:
        if args.command == "cleanup" or (args.command == "all" and not args.keep_builds):
            require(
                not owned_processes(root / "owned"),
                "owned processes remain; refusing artifact removal",
            )
            shutil.rmtree(root / "owned")
            require(not (root / "owned").exists(), "build artifacts survived cleanup")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["build", "run", "all", "cleanup"])
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--repository", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--source", default="9e9785efe0c6549ee8d1c143b81db00472031dab", help="literal pre-S4 r27 source revision")
    parser.add_argument("--destination", default="887f754e1c1e5126b586531a573ebd6fb8bc0189", help="S4 r28 destination revision")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument(
        "--keep-builds",
        action="store_true",
        help="retain scratch build artifacts after all (runtime state is still removed)",
    )
    parser.add_argument("--arm", choices=["all", "bootstrap", "dormant"], default="all")
    parser.add_argument(
        "--case", action="append", help="select a named case (repeatable); zero matches refuse"
    )
    args = parser.parse_args()
    require(sys.platform == "linux", "this real-binary rehearsal requires scratch Linux")
    require(args.jobs > 0, "--jobs must be positive")
    root = prepare_root(args.root)
    with (root / "rehearsal.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Refused("another rehearsal owns this root") from error
        return execute(args, root)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Refused, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"upgrade rehearsal: {error}", file=sys.stderr)
        raise SystemExit(1) from error
