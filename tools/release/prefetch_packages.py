#!/usr/bin/env python3
"""Warm archive verification inputs without inheriting workspace patches."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tomllib


def prefetch(destination: Path, archives: list[Path]) -> None:
    members = {}
    for archive in archives:
        with tarfile.open(archive) as source:
            source.extractall(destination, filter="data")
        manifest = destination / archive.stem / "Cargo.toml"
        package = tomllib.loads(manifest.read_text())["package"]
        members[package["name"]] = manifest
    # Unpublished siblings are available only in Cargo's package overlay.
    # Substitute the same archives locally for fetch; only these disposable
    # locks change source identities. Verification uses the original archives.
    patches = "\n[patch.crates-io]\n" + "".join(
        f"{json.dumps(name)} = {{ path = {json.dumps(str(manifest.parent))} }}\n"
        for name, manifest in members.items()
    )
    for manifest in members.values():
        with manifest.open("a") as stream:
            stream.write(patches)
        command = ["cargo", "fetch", "--manifest-path", str(manifest)]
        offline = subprocess.run([*command, "--offline"])
        if offline.returncode:
            subprocess.run(command, check=True)


if __name__ == "__main__":
    prefetch(Path(sys.argv[1]), [Path(arg) for arg in sys.argv[2:]])
