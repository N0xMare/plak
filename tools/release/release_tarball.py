#!/usr/bin/env python3
"""Pack built release executables into `plak-<version>-<triple>.tar.gz`.

The tarball holds one top-level directory `plak-<version>-<triple>/` with the
three executables (`plak`, `plakous-js-host`, `monty`), the repository
`LICENSE`, `THIRD-PARTY` (every package outside this repository that the three
executables link for the target, from `cargo metadata`), and
`licenses/<name>-<version>/` with the license and NOTICE files those packages
ship, vendored code included. The archive is deterministic: sorted entries,
fixed owner and modes, and one modification time (the release commit's).
`SHA256SUMS` in the output directory is rewritten over every release tarball
there, so tarballs built on different hosts can be collected into one
directory and summed together.

`--require-static` refuses an ELF executable with a program interpreter: the
Linux tarball must run on any distribution.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import re
import struct
import subprocess
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXECUTABLES = ("plak", "plakous-js-host", "monty")
ROOT_PACKAGES = ("plak", "plakous-js-runtime", "plakous-py-worker")
LICENSE_FILE = re.compile(r"(^|[-_.])(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|UNLICENSE)", re.I)


class ReleaseError(Exception):
    pass


def load_metadata(triple: str, path: Path | None) -> dict:
    if path is not None:
        return json.loads(path.read_text())
    command = ["cargo", "metadata", "--locked", "--format-version", "1",
               "--filter-platform", triple]
    return json.loads(subprocess.run(command, cwd=ROOT, check=True, text=True,
                                     stdout=subprocess.PIPE).stdout)


def third_party(metadata: dict) -> list[dict]:
    """Packages outside this repository reachable by normal dependency edges."""
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    workspace_root = Path(metadata["workspace_root"])
    vendor = workspace_root / "vendor"
    members = set(metadata["workspace_members"])
    by_name = {packages[id_]["name"]: id_ for id_ in members}
    pending = [by_name[name] for name in ROOT_PACKAGES]
    seen: set[str] = set()
    while pending:
        id_ = pending.pop()
        if id_ in seen:
            continue
        seen.add(id_)
        for dep in nodes[id_]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                pending.append(dep["pkg"])

    def ours(id_: str) -> bool:
        directory = Path(packages[id_]["manifest_path"]).parent
        return id_ in members and vendor not in directory.parents

    found = [packages[id_] for id_ in seen if not ours(id_)]
    return sorted(found, key=lambda package: (package["name"], package["version"]))


def license_files(package: dict) -> list[Path]:
    directory = Path(package["manifest_path"]).parent
    return sorted(path for path in directory.iterdir()
                  if path.is_file() and LICENSE_FILE.search(path.name))


def third_party_text(triple: str, packages: list[dict]) -> str:
    lines = [
        f"Packages outside this repository linked into plak, plakous-js-host and monty for {triple}.",
        "License and NOTICE files each package ships are under licenses/<name>-<version>/.",
        "",
    ]
    for package in packages:
        license_ = package.get("license") or "see licenses/"
        origin = package.get("repository") or ("vendored" if package["source"] is None else "")
        lines.append(f"{package['name']} {package['version']}  {license_}  {origin}".rstrip())
    return "\n".join(lines) + "\n"


def require_static(path: Path) -> None:
    data = path.read_bytes()
    if data[:4] != b"\x7fELF":
        return
    if data[4] != 2 or data[5] != 1:
        raise ReleaseError(f"{path.name}: expected a 64-bit little-endian ELF")
    phoff, = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    for index in range(phnum):
        kind, = struct.unpack_from("<I", data, phoff + index * phentsize)
        if kind == 3:  # PT_INTERP
            raise ReleaseError(f"{path.name}: dynamically linked (has a program interpreter)")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def pack(version: str, triple: str, artifacts: Path, out: Path, mtime: int,
         metadata: dict, static: bool) -> Path:
    for name in EXECUTABLES:
        path = artifacts / name
        if not path.is_file():
            raise ReleaseError(f"missing executable {path}")
        if static:
            require_static(path)
    packages = third_party(metadata)
    stem = f"plak-{version}-{triple}"
    files: dict[str, tuple[bytes, int]] = {}
    for name in EXECUTABLES:
        files[name] = ((artifacts / name).read_bytes(), 0o755)
    files["LICENSE"] = ((ROOT / "LICENSE").read_bytes(), 0o644)
    files["THIRD-PARTY"] = (third_party_text(triple, packages).encode(), 0o644)
    for package in packages:
        for path in license_files(package):
            files[f"licenses/{package['name']}-{package['version']}/{path.name}"] = (
                path.read_bytes(), 0o644)
    directories = {stem} | {f"{stem}/{parent}" for name in files
                            for parent in map(str, Path(name).parents) if parent != "."}

    def info(name: str, kind: bytes, mode: int, size: int = 0) -> tarfile.TarInfo:
        entry = tarfile.TarInfo(name)
        entry.type, entry.mode, entry.size, entry.mtime = kind, mode, size, mtime
        entry.uid = entry.gid = 0
        entry.uname = entry.gname = ""
        return entry

    buffer = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=buffer, mtime=mtime) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for directory in sorted(directories):
                archive.addfile(info(directory, tarfile.DIRTYPE, 0o755))
            for name in sorted(files):
                data, mode = files[name]
                archive.addfile(info(f"{stem}/{name}", tarfile.REGTYPE, mode, len(data)),
                                io.BytesIO(data))
    out.mkdir(parents=True, exist_ok=True)
    tarball = out / f"{stem}.tar.gz"
    tarball.write_bytes(buffer.getvalue())
    write_sums(out)
    return tarball


def write_sums(out: Path) -> None:
    tarballs = sorted(out.glob("plak-*.tar.gz"))
    (out / "SHA256SUMS").write_text("".join(f"{sha256(path)}  {path.name}\n" for path in tarballs))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--triple", required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--mtime", type=int, required=True)
    parser.add_argument("--metadata", type=Path, help="cargo metadata JSON (default: run cargo)")
    parser.add_argument("--require-static", action="store_true")
    args = parser.parse_args(argv)
    try:
        metadata = load_metadata(args.triple, args.metadata)
        tarball = pack(args.version, args.triple, args.artifacts, args.out, args.mtime,
                       metadata, args.require_static)
    except (ReleaseError, OSError, subprocess.CalledProcessError) as error:
        print(f"release-tarball: {error}", file=sys.stderr)
        return 1
    print(tarball)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
