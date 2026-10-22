#!/usr/bin/env python3
"""Pack built release executables into `plak-<version>-<triple>.tar.gz`.

The tarball holds one top-level directory `plak-<version>-<triple>/` with the
three executables (`plak`, `plakous-js-host`, `monty`), the repository
`LICENSE`, `THIRD-PARTY` (every package outside this repository that the three
executables reach through normal dependency edges for the target), and
`licenses/<name>-<version>/` with the license and NOTICE files those packages
ship, vendored code included. The closure is the release build's, not the
workspace-unified resolve: the same two Cargo resolutions as
`tools/build/build-plak.sh --artifacts` (`plak` at its default features, then
`plakous-js-runtime` and `plakous-py-worker` together), from `cargo tree
-e normal`, with each package's license and repository taken from `cargo
metadata`. One workspace `cargo metadata` unifies member defaults and
dev-dependencies into those edges, which pulls in jj-lib (and the gix family)
and test-support that the git-only release does not link. The archive is
deterministic: sorted entries, fixed owner and modes, and one modification
time (the release commit's).
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
# Two resolutions, matching `build-plak.sh --artifacts`: sidecar features are
# not unified into plak, and neither resolution is the whole workspace.
RELEASE_RESOLUTIONS = (
    ("plak",),
    ("plakous-js-runtime", "plakous-py-worker"),
)
LICENSE_FILE = re.compile(r"(^|[-_.])(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|UNLICENSE)", re.I)


class ReleaseError(Exception):
    pass


def load_metadata(triple: str, path: Path | None) -> dict:
    if path is not None:
        return json.loads(path.read_text())
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
               "--filter-platform", triple]
    return json.loads(subprocess.run(command, cwd=ROOT, check=True, text=True,
                                     stdout=subprocess.PIPE).stdout)


def release_tree_command(triple: str, packages: tuple[str, ...]) -> list[str]:
    """`cargo tree` for one release resolution: normal edges, that target, default features."""
    command = [
        "cargo", "tree", "--locked", "--offline",
        "-e", "normal",
        "--target", triple,
        "--prefix", "none",
        "--charset", "ascii",
        "-f", "{p}",
    ]
    for package in packages:
        command.extend(["-p", package])
    return command


def parse_tree_package(line: str) -> tuple[str, str]:
    """One `cargo tree -f {p}` line: `name vversion`, then source or dedup suffixes."""
    text = line.strip()
    while text.endswith(")"):
        split = text.rfind(" (")
        if split < 0:
            break
        text = text[:split]
    match = re.fullmatch(r"(\S+) v(\S+)", text)
    if match is None:
        raise ReleaseError(f"unparsed cargo tree line: {line.strip()}")
    return match.group(1), match.group(2)


def parse_tree_packages(text: str) -> set[tuple[str, str]]:
    found: set[tuple[str, str]] = set()
    for line in text.splitlines():
        if line.strip():
            found.add(parse_tree_package(line))
    return found


def release_closure(triple: str) -> set[tuple[str, str]]:
    """`(name, version)` of every package the release build reaches for `triple`."""
    found: set[tuple[str, str]] = set()
    for packages in RELEASE_RESOLUTIONS:
        command = release_tree_command(triple, packages)
        try:
            result = subprocess.run(command, cwd=ROOT, check=True, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        except subprocess.CalledProcessError as error:
            detail = (error.stderr or "").strip()
            raise ReleaseError(detail or f"cargo tree failed for {', '.join(packages)}") from error
        parsed = parse_tree_packages(result.stdout)
        reported = {name for name, _version in parsed}
        missing = [package for package in packages if package not in reported]
        if missing:
            raise ReleaseError(f"cargo tree did not report {', '.join(missing)} for {triple}")
        found.update(parsed)
    return found


def _outside_repository(package: dict, members: set[str], vendor: Path) -> bool:
    """True for registry crates and for vendored workspace members."""
    directory = Path(package["manifest_path"]).parent
    return not (package["id"] in members and vendor not in directory.parents)


def third_party(metadata: dict, linked: set[tuple[str, str]] | None = None) -> list[dict]:
    """Packages outside this repository reachable by normal dependency edges.

    `linked` is the release build's closure (`release_closure`). Without it,
    the normal edges of `metadata`'s resolve are used; that is right only when
    the resolve is not workspace-unified.
    """
    members = set(metadata["workspace_members"])
    vendor = Path(metadata["workspace_root"]) / "vendor"
    if linked is None:
        packages = {package["id"]: package for package in metadata["packages"]}
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        by_name = {packages[id_]["name"]: id_ for id_ in members}
        pending = [by_name[name] for name in ROOT_PACKAGES]
        seen: set[str] = set()
        while pending:
            id_ = pending.pop()
            if id_ in seen:
                continue
            seen.add(id_)
            for dep in nodes[id_]["deps"]:
                # `kind` null is a normal edge. Build and dev edges are not linked.
                if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                    pending.append(dep["pkg"])
        found = [packages[id_] for id_ in seen if _outside_repository(packages[id_], members, vendor)]
    else:
        chosen: dict[tuple[str, str], dict] = {}
        for package in metadata["packages"]:
            key = (package["name"], package["version"])
            if key not in linked:
                continue
            if key in chosen:
                raise ReleaseError(f"ambiguous package {package['name']} {package['version']}")
            chosen[key] = package
        missing = sorted(linked - chosen.keys())
        if missing:
            sample = ", ".join(f"{name} {version}" for name, version in missing[:5])
            raise ReleaseError(f"cargo tree package not in metadata: {sample}")
        found = [package for package in chosen.values()
                 if _outside_repository(package, members, vendor)]
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
         metadata: dict, static: bool, linked: set[tuple[str, str]] | None = None) -> Path:
    for name in EXECUTABLES:
        path = artifacts / name
        if not path.is_file():
            raise ReleaseError(f"missing executable {path}")
        if static:
            require_static(path)
    packages = third_party(metadata, linked)
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
    parser.add_argument("--metadata", type=Path,
                        help="cargo metadata JSON; its normal edges are the closure "
                             "(default: metadata for licenses, cargo tree for the release closure)")
    parser.add_argument("--require-static", action="store_true")
    args = parser.parse_args(argv)
    try:
        metadata = load_metadata(args.triple, args.metadata)
        # A supplied graph is trusted (fixtures). The release build's closure is
        # cargo tree, so workspace-unified metadata cannot widen THIRD-PARTY.
        linked = None if args.metadata is not None else release_closure(args.triple)
        tarball = pack(args.version, args.triple, args.artifacts, args.out, args.mtime,
                       metadata, args.require_static, linked)
    except (ReleaseError, OSError, subprocess.CalledProcessError) as error:
        print(f"release-tarball: {error}", file=sys.stderr)
        return 1
    print(tarball)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
