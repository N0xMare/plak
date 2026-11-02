"""Cargo inputs shared by the workspace-check and package-verification receipts.

A path is identified by lstat, not by following it: type, size, mtime, and, for
a symlink, its readlink plus the identity of what it points to, link by link.
Absence is its own value. An unreadable path is a doubt, not a hit.

Credential variables never enter a key: they do not change what Cargo compiles,
and a receipt must not be able to carry them.
"""

from __future__ import annotations

import hashlib
import os
import stat
from pathlib import Path


class CacheDoubt(Exception):
    """An input could not be stated. Callers recompute instead of trusting a receipt."""


# Terminal decoration. It does not change what Cargo compiles or the checker reads.
IRRELEVANT_CARGO_ENV = frozenset(
    {
        "CARGO_TERM_COLOR",
        "CARGO_TERM_PROGRESS_WHEN",
        "CARGO_TERM_PROGRESS_WIDTH",
        "CARGO_TERM_VERBOSE",
    }
)

# Registry tokens, secret keys and credential providers, e.g. `CARGO_REGISTRY_TOKEN`,
# `CARGO_REGISTRIES_<name>_TOKEN`, `CARGO_REGISTRY_SECRET_KEY`.
_CREDENTIAL_MARKERS = ("TOKEN", "CREDENTIAL", "SECRET", "PASSWORD")

_EXTRA_ENV = ("RUSTC", "RUSTFLAGS", "RUSTUP_TOOLCHAIN")


def is_credential(name: str) -> bool:
    upper = name.upper()
    return any(marker in upper for marker in _CREDENTIAL_MARKERS)


def tracked_env() -> list[tuple[str, str]]:
    """`CARGO_*` except terminal decoration and credentials, plus the Rustc variables.

    An unset Rustc variable is the empty string.
    """
    names = {
        key
        for key in os.environ
        if key.startswith("CARGO_") and key not in IRRELEVANT_CARGO_ENV and not is_credential(key)
    }
    names.update(_EXTRA_ENV)
    return [(name, os.environ.get(name, "")) for name in sorted(names)]


def config_candidates(start: Path) -> list[Path]:
    """`$CARGO_HOME/config{,.toml}` and every ancestor `.cargo/config{,.toml}` through `/`."""
    home = os.environ.get("CARGO_HOME")
    cargo_home = Path(home) if home else Path.home() / ".cargo"
    paths = [cargo_home / "config.toml", cargo_home / "config"]
    current = Path(os.path.abspath(start))
    while True:
        paths.append(current / ".cargo" / "config.toml")
        paths.append(current / ".cargo" / "config")
        parent = current.parent
        if parent == current:
            break
        current = parent
    unique: list[Path] = []
    seen: set[str] = set()
    for path in paths:
        key = os.path.abspath(path)
        if key not in seen:
            seen.add(key)
            unique.append(Path(key))
    return unique


def _sha256(path: Path) -> str:
    hasher = hashlib.sha256()
    try:
        # Builtin open: a caller may be patching Path.open to record reads.
        with open(path, "rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                hasher.update(chunk)
    except OSError as error:
        raise CacheDoubt(str(error)) from error
    return hasher.hexdigest()


# Linux's own bound on links followed while resolving one path.
_MAX_LINKS = 40


def path_identity(path: Path, *, _links: int = 0) -> dict[str, object]:
    """Identity of one path. Missing is `absent`. A symlink keeps its own type.

    A symlink also records its target's identity, following the chain to its
    end, so a retargeted or rewritten link at any depth changes the identity.
    """
    try:
        described = os.lstat(path)
    except FileNotFoundError:
        return {"state": "absent"}
    except OSError as error:
        raise CacheDoubt(str(error)) from error
    info: dict[str, object] = {
        "state": "present",
        "type": stat.S_IFMT(described.st_mode),
        "size": described.st_size,
        "mtime_ns": described.st_mtime_ns,
    }
    if stat.S_ISLNK(described.st_mode):
        try:
            link = os.readlink(path)
        except OSError as error:
            raise CacheDoubt(str(error)) from error
        info["link"] = link
        if _links >= _MAX_LINKS:
            raise CacheDoubt(f"{path}: more than {_MAX_LINKS} symlinks")
        target = Path(link)
        if not target.is_absolute():
            target = path.parent / link
        info["target"] = path_identity(target, _links=_links + 1)
    elif stat.S_ISREG(described.st_mode):
        info["sha256"] = _sha256(path)
    return info


def directory_listing(path: Path) -> list[list[object]]:
    """Sorted entry names with lstat types and, for a symlink, its target."""
    entries: list[list[object]] = []
    try:
        with os.scandir(path) as iterator:
            for entry in iterator:
                described = entry.stat(follow_symlinks=False)
                item: list[object] = [entry.name, stat.S_IFMT(described.st_mode)]
                if stat.S_ISLNK(described.st_mode):
                    item.append(os.readlink(entry.path))
                entries.append(item)
    except OSError as error:
        raise CacheDoubt(str(error)) from error
    entries.sort(key=lambda item: str(item[0]))
    return entries
