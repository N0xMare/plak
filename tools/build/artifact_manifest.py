"""Emit a closed manifest for shipping builds."""

import argparse
import hashlib
import json
import mmap
import os
import re
import subprocess
import tempfile
import tomllib
from pathlib import Path

NAMES = ("plakd", "plakous-js-host", "plakous-py-host")
HEX = r"[0-9a-f]"
REVISION = rf"(?:{HEX}{{40}}|{HEX}{{64}})"
SHA256 = rf"{HEX}{{64}}"
VERSION = r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.+-]+)?"
TRIPLE = r"[a-z0-9_]+(?:-[a-z0-9_]+){2,3}"
STATE_KEY = r"r[0-9]+\.e[0-9]+\.p[0-9]+\.t[0-9]+"


def canonical(data):
    return (
        json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
    ).encode()


def unique_fields(pairs):
    result = dict(pairs)
    if len(result) != len(pairs):
        raise ValueError("duplicate JSON field")
    return result


def read_canonical(data):
    document = json.loads(data, object_pairs_hook=unique_fields)
    if canonical(document) != data:
        raise ValueError("noncanonical JSON")
    return document


def require(condition, message):
    if not condition:
        raise ValueError(message)


def fields(data, names):
    require(
        isinstance(data, dict) and data.keys() == set(names.split()), f"expected fields: {names}"
    )


def matches(value, pattern, field):
    require(isinstance(value, str) and re.fullmatch(pattern, value), f"invalid {field}")


def positive(value, field):
    require(type(value) is int and value > 0, f"invalid {field}")


def versions(values, field):
    require(isinstance(values, list) and values, f"missing {field} versions")
    for value in values:
        positive(value, field)
    require(values == sorted(set(values)), f"{field} versions must be sorted and unique")


def validate_build_info(info):
    fields(
        info,
        "export flavor gateway host_upgrade omitted_features product_version revision runner state_key test_hooks wire",
    )
    require(info["flavor"] == "release", "expected plain release build")
    require(info["test_hooks"] is False, "shipping artifacts must not include test hooks")
    require(info["omitted_features"] == [], "shipping artifacts must not omit product features")
    matches(info["revision"], REVISION, "revision")
    matches(info["product_version"], VERSION, "product version")
    matches(info["state_key"], STATE_KEY, "state key")
    fields(info["export"], "read write")
    versions(info["export"]["read"], "export read")
    positive(info["export"]["write"], "export write")
    require(info["export"]["write"] in info["export"]["read"], "written export is not readable")
    fields(info["host_upgrade"], "coordinator receipt")
    for key, value in info["host_upgrade"].items():
        positive(value, key)
    fields(info["wire"], "number schema_pin")
    positive(info["wire"]["number"], "wire number")
    matches(info["wire"]["schema_pin"], SHA256, "wire schema pin")
    fields(info["runner"], "protocol retention")
    positive(info["runner"]["protocol"], "runner protocol")
    for label, facts in (("gateway", info["gateway"]), ("retention", info["runner"]["retention"])):
        fields(facts, "served spoken")
        for direction in ("served", "spoken"):
            versions(facts[direction], f"{label} {direction}")


def validate_toolchain(toolchain):
    fields(toolchain, "cargo rustc")
    fields(toolchain["rustc"], "release commit_hash host llvm_version")
    fields(toolchain["cargo"], "release commit_hash")
    for name, facts in toolchain.items():
        matches(facts["release"], VERSION, f"{name} release")
        matches(facts["commit_hash"], rf"{HEX}{{{40 if name == 'rustc' else 9}}}", f"{name} commit")
    matches(toolchain["rustc"]["llvm_version"], r"[0-9]+\.[0-9]+\.[0-9]+", "LLVM version")
    matches(toolchain["rustc"]["host"], TRIPLE, "rustc host")


def snapshot_id(files):
    data = "".join(f"{name} {files[name]['sha256']}\n" for name in NAMES)
    return hashlib.sha256(data.encode()).hexdigest()


def validate_manifest(data):
    fields(
        data,
        "schema revision platform flavor product_version lock_sha256 toolchain files snapshot_id build_info",
    )
    require(type(data["schema"]) is int and data["schema"] == 1, "unknown manifest schema")
    validate_build_info(data["build_info"])
    for key in ("flavor", "revision", "product_version"):
        require(data[key] == data["build_info"][key], f"manifest and build-info {key} differ")
    matches(data["platform"], TRIPLE, "platform")
    matches(data["lock_sha256"], SHA256, "lock digest")
    validate_toolchain(data["toolchain"])
    fields(data["files"], " ".join(NAMES))
    for name, facts in data["files"].items():
        fields(facts, "length sha256 display_id")
        positive(facts["length"], f"{name} length")
        matches(facts["sha256"], SHA256, f"{name} digest")
        require(facts["display_id"] == facts["sha256"][:12], f"{name} display id differs")
    require(data["snapshot_id"] == snapshot_id(data["files"]), "snapshot id differs")


def collect_toolchain():
    rustc = dict(
        line.split(": ", 1)
        for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
        if ": " in line
    )
    cargo = subprocess.check_output(["cargo", "-V"], text=True).strip()
    match = re.fullmatch(rf"cargo ({VERSION}) \(({HEX}{{9}}) [0-9-]+\)", cargo)
    require(match, "unrecognized cargo -V")
    return {
        "cargo": {"release": match[1], "commit_hash": match[2]},
        "rustc": {
            "release": rustc["release"],
            "commit_hash": rustc["commit-hash"],
            "host": rustc["host"],
            "llvm_version": rustc["LLVM version"],
        },
    }


def file_facts(path):
    with path.open("rb") as source:
        size = os.fstat(source.fileno()).st_size
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    return {"length": size, "sha256": digest, "display_id": digest[:12]}


def check_paths(directory, forbidden):
    for name in NAMES:
        with (
            (directory / name).open("rb") as source,
            mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as binary,
        ):
            for path in forbidden:
                require(
                    binary.find(os.fsencode(path)) < 0,
                    f"{name} contains a physical build path: {path}",
                )


def build_info(plakd):
    return read_canonical(subprocess.check_output([plakd, "version", "--build-info"]))


def make_manifest(directory, platform, revision, product_version, lock_sha256, toolchain, info):
    files = {name: file_facts(directory / name) for name in NAMES}
    document = {
        "schema": 1,
        "revision": revision,
        "platform": platform,
        "flavor": "release",
        "product_version": product_version,
        "lock_sha256": lock_sha256,
        "toolchain": toolchain,
        "files": files,
        "snapshot_id": snapshot_id(files),
        "build_info": info,
    }
    validate_manifest(document)
    return document


def emit(directory, platform, revision, cargo_home, target):
    repo = Path(__file__).resolve().parents[2]
    check_paths(directory, (repo, cargo_home, target))
    lock = file_facts(repo / "Cargo.lock")["sha256"]
    product_version = tomllib.loads((repo / "Cargo.toml").read_text())["workspace"]["package"][
        "version"
    ]
    document = make_manifest(
        directory,
        platform,
        revision,
        product_version,
        lock,
        collect_toolchain(),
        build_info(directory / "plakd"),
    )
    with tempfile.NamedTemporaryFile(dir=directory, delete=False) as pending:
        path = Path(pending.name)
        try:
            pending.write(canonical(document))
            pending.close()
            path.replace(directory / "manifest.json")
        finally:
            path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    emitter = commands.add_parser("emit")
    emitter.add_argument("directory", type=Path)
    emitter.add_argument("platform")
    emitter.add_argument("revision")
    emitter.add_argument("cargo_home", type=Path)
    emitter.add_argument("target", type=Path)
    args = vars(parser.parse_args())
    args.pop("action")
    try:
        emit(**args)
    except (
        ValueError,
        KeyError,
        OSError,
        subprocess.CalledProcessError,
    ) as error:
        parser.exit(1, f"artifact manifest: {error}\n")


if __name__ == "__main__":
    main()
