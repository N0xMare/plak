#!/usr/bin/env python3
"""Build and cache the production predecessor's offline test images."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[2]
BINARIES = ("plakd", "plakous-js-host", "plakous-py-host")
PROFILE = "dev"


def revision():
    pin = (ROOT / ".config/install-predecessor").read_text()
    if not re.fullmatch(r"[0-9a-f]{40}\n", pin):
        raise ValueError("expected one full Git revision in .config/install-predecessor")
    return pin.strip()


def archive(output, rev):
    subprocess.run(
        ["git", "archive", "--format=tar", f"--output={output}", rev], cwd=ROOT, check=True
    )


def build_recipe(triple, build_target):
    args = [
        "build",
        "--locked",
        "--offline",
        *build_target,
        "-p",
        "plakd",
        "-p",
        "plakous-js-runtime",
        "-p",
        "plakous-py-worker",
        "--profile",
        PROFILE,
        "--features",
        "plakd/maintainer,plakd/test-support",
        *(arg for name in BINARIES for arg in ("--bin", name)),
    ]
    rustflags = (
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        f"CARGO_TARGET_{triple.upper().replace('-', '_')}_RUSTFLAGS",
    )
    env = {key: os.environ.get(key) for key in rustflags}
    env.update(
        (key, value)
        for key, value in os.environ.items()
        if key.startswith(f"CARGO_PROFILE_{PROFILE.upper()}_")
    )
    recipe = json.dumps((args, env), sort_keys=True)
    return args, hashlib.sha256(recipe.encode()).hexdigest()[:16]


def build_inputs(target, rev):
    channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    triple = os.environ.get("CARGO_BUILD_TARGET")
    build_target = ["--target", triple] if triple else []
    if not triple:
        rustc = subprocess.check_output(["rustc", f"+{channel}", "-vV"], text=True)
        triple = next(line.split()[1] for line in rustc.splitlines() if line.startswith("host: "))
    args, recipe = build_recipe(triple, build_target)
    root = target.resolve().parent / f"{target.name}-predecessor"
    return root, root / rev / channel / triple / recipe, channel, build_target, args


def cache_ready(cache):
    return all((cache / name).is_file() and os.access(cache / name, os.X_OK) for name in BINARIES)


def prune_revisions(root, rev):
    previous = sorted(
        (
            path
            for path in root.iterdir()
            if path.name != rev
            and not path.is_symlink()
            and path.is_dir()
            and re.fullmatch(r"[0-9a-f]{40}", path.name)
        ),
        key=lambda path: (path.stat().st_mtime_ns, path.name),
        reverse=True,
    )
    for path in previous[1:]:
        shutil.rmtree(path)


def build(root, cache, channel, target_args, args, rev, source_archive):
    root.mkdir(parents=True, exist_ok=True)
    with (root / "build.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        # Only the lock holder can have an active scratch directory here.
        for scratch in root.glob("build-*"):
            if scratch.is_dir() and not scratch.is_symlink():
                shutil.rmtree(scratch)
        started = time.monotonic()
        if cache_ready(cache):
            print(
                f"== predecessor {rev}: cache hit ({time.monotonic() - started:.2f} s) ==",
                file=sys.stderr,
            )
            return cache
        if cache.exists():
            raise ValueError(f"incomplete predecessor cache: {cache}")
        with tempfile.TemporaryDirectory(prefix="build-", dir=root) as temp:
            temp = Path(temp)
            src, artifacts, publish = (temp / name for name in ("source", "target", "bin"))
            src.mkdir()
            publish.mkdir()
            if source_archive is None:
                source_archive = temp / "predecessor.tar"
                archive(source_archive, rev)
            else:
                with source_archive.open("rb") as stream:
                    archived_revision = subprocess.check_output(
                        ["git", "get-tar-commit-id"], stdin=stream, text=True
                    ).strip()
                if archived_revision != rev:
                    raise ValueError(
                        "predecessor archive does not match .config/install-predecessor"
                    )
            subprocess.run(["tar", "-xf", source_archive, "-C", src], check=True)
            env = dict(os.environ, CARGO_TARGET_DIR=str(artifacts), PLAK_SOURCE_REVISION=rev)
            cargo = ["cargo", f"+{channel}"]
            fetch = [*cargo, "fetch", "--locked", *target_args]
            kwargs = dict(cwd=src, env=env, stdout=sys.stderr)
            if subprocess.run([*fetch, "--offline"], **kwargs).returncode:
                subprocess.run(fetch, check=True, **kwargs)
            subprocess.run(
                [*cargo, *args],
                check=True,
                **kwargs,
            )
            binaries = (artifacts / target_args[1] if target_args else artifacts) / (
                "debug" if PROFILE == "dev" else PROFILE
            )
            for name in BINARIES:
                shutil.move(binaries / name, publish / name)
            cache.parent.mkdir(parents=True, exist_ok=True)
            publish.rename(cache)
        os.utime(root / rev, None)
        prune_revisions(root, rev)
        print(
            f"== predecessor {rev}: cold build ({time.monotonic() - started:.2f} s) ==",
            file=sys.stderr,
        )
        return cache


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target-dir", type=Path, default=Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    )
    archive_mode = parser.add_mutually_exclusive_group()
    archive_mode.add_argument("--write-archive", type=Path)
    archive_mode.add_argument("--source-archive", type=Path)
    archive_mode.add_argument(
        "--check", action="store_true", help="report a cache hit without building"
    )
    args = parser.parse_args()
    try:
        rev = revision()
        if args.write_archive is not None:
            archive(args.write_archive, rev)
            return
        root, cache, channel, target_args, recipe = build_inputs(args.target_dir, rev)
        if args.check:
            if not cache_ready(cache):
                raise SystemExit(1)
            print(f"== predecessor {rev}: cache hit ==", file=sys.stderr)
            print(cache)
            return
        print(build(root, cache, channel, target_args, recipe, rev, args.source_archive))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(
            f"install suite requires a build of its pinned predecessor: {error}"
        ) from error


if __name__ == "__main__":
    main()
