#!/usr/bin/env python3
"""Receipt for one successful cohort verification.

`publish.sh` captures the body before packaging. Dry runs and `--gate` skip
packaging when that same body still matches the receipt. `--execute` never
reads it. The receipt is written only after verification succeeds, from the
body captured beforehand, so a failed verification leaves no receipt and a
mutation during packaging cannot be recorded as the tree that was verified.

Every body field is a SHA-256. Neither the body nor the receipt holds an
environment value, and credential variables are not part of the key at all.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import subprocess
import sys
from pathlib import Path

_GATE = Path(__file__).resolve().parent.parent / "gate"
sys.path.insert(0, str(_GATE))

import cargo_key  # noqa: E402


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _file_hash(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def staged_tree_hash(stage: Path) -> str:
    """Hash every staged entry in path order: its lstat kind and its content.

    A regular file is its bytes and executable bit, a symlink its `readlink`
    text, a directory its name. A missing or unreadable entry is a miss.
    """
    digest = hashlib.sha256()
    for path in sorted(stage.rglob("*")):
        described = path.lstat()
        if stat.S_ISLNK(described.st_mode):
            entry = f"link:{os.readlink(path)}"
        elif stat.S_ISREG(described.st_mode):
            executable = "x" if described.st_mode & 0o111 else "-"
            entry = f"file:{executable}:{_file_hash(path)}"
        elif stat.S_ISDIR(described.st_mode):
            entry = "dir"
        else:
            raise cargo_key.CacheDoubt(f"{path}: not a file, directory or symlink")
        digest.update(path.relative_to(stage).as_posix().encode())
        digest.update(b"\0")
        digest.update(entry.encode())
        digest.update(b"\0")
    return digest.hexdigest()


def toolchain_identity() -> str:
    cargo = subprocess.check_output(["cargo", "--version"], text=True)
    rustc = subprocess.check_output(["rustc", "-vV"], text=True)
    return _sha256(f"{cargo}\n{rustc}".encode())


def _config_label(path: Path, stage: Path) -> str:
    """A label that survives a freshly created stage directory.

    Cargo-home files are relative to `CARGO_HOME`. Ancestors of the stage are
    counted up from the stage, so the random staging directory does not change
    the key when those levels stay absent.
    """
    home = os.environ.get("CARGO_HOME")
    cargo_home = Path(os.path.abspath(home)) if home else Path.home() / ".cargo"
    try:
        return "cargo-home/" + path.relative_to(cargo_home).as_posix()
    except ValueError:
        pass
    current = stage
    depth = 0
    while True:
        try:
            return f"{depth}:{path.relative_to(current).as_posix()}"
        except ValueError:
            parent = current.parent
            if parent == current:
                return f"abs:{path}"
            current = parent
            depth += 1


def _canonical_hash(value: object) -> str:
    return _sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


def config_snapshot(stage: Path) -> str:
    stage = Path(os.path.abspath(stage))
    entries = {
        _config_label(path, stage): cargo_key.path_identity(path)
        for path in cargo_key.config_candidates(stage)
    }
    return _canonical_hash(entries)


def env_snapshot() -> str:
    return _canonical_hash(dict(cargo_key.tracked_env()))


def receipt_body(stage: Path, order: Path) -> dict[str, str]:
    if not order.is_file():
        raise FileNotFoundError("cohort order is missing")
    lock = stage / "Cargo.lock"
    # Absence is its own value, so a tree that gains a lock cannot reuse a receipt.
    lock_hash = _file_hash(lock) if lock.is_file() else "absent"
    return {
        "staged": staged_tree_hash(stage),
        "lock": lock_hash,
        "toolchain": toolchain_identity(),
        "cohort": _file_hash(order),
        "env": env_snapshot(),
        "configs": config_snapshot(stage),
    }


def _checksum(body: dict[str, str]) -> str:
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":")).encode()
    return _sha256(encoded)


def load_receipt(path: Path) -> dict[str, str] | None:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(payload, dict):
            return None
        checksum = payload.pop("checksum")
        if not isinstance(checksum, str):
            return None
        if any(
            not isinstance(key, str) or not isinstance(value, str) for key, value in payload.items()
        ):
            return None
        if checksum != _checksum(payload):
            return None
        return payload
    except (
        OSError,
        json.JSONDecodeError,
        KeyError,
        TypeError,
        UnicodeError,
        cargo_key.CacheDoubt,
    ):
        return None


def write_receipt(path: Path, body: dict[str, str]) -> None:
    payload = dict(body)
    payload["checksum"] = _checksum(body)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(payload), encoding="utf-8")
    os.replace(temporary, path)


def read_body(path: Path) -> dict[str, str]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict) or any(
        not isinstance(key, str) or not isinstance(value, str) for key, value in payload.items()
    ):
        raise ValueError("verification body is not an object of strings")
    return payload


def matches(body: dict[str, str], receipt: Path) -> bool:
    recorded = load_receipt(receipt)
    return recorded == body


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("capture", "hit", "record"))
    parser.add_argument("--stage", type=Path)
    parser.add_argument("--order", type=Path)
    parser.add_argument("--body", type=Path)
    parser.add_argument("--receipt", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.action == "capture":
            if args.stage is None or args.order is None or args.body is None:
                return 2
            args.body.parent.mkdir(parents=True, exist_ok=True)
            args.body.write_text(
                json.dumps(receipt_body(args.stage, args.order), sort_keys=True),
                encoding="utf-8",
            )
            return 0
        if args.body is None or args.receipt is None:
            return 2
        body = read_body(args.body)
        if args.action == "hit":
            return 0 if matches(body, args.receipt) else 1
        write_receipt(args.receipt, body)
    except (
        OSError,
        subprocess.CalledProcessError,
        UnicodeError,
        FileNotFoundError,
        json.JSONDecodeError,
        ValueError,
        cargo_key.CacheDoubt,
    ):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
