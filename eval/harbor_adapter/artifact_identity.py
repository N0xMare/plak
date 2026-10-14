"""Which bytes a trial actually ran.

A Harbor trial records the agent name, the model and the reward, but nothing
about the binaries that were uploaded into the cell. When an arm passes in the
morning and times out in the evening, the first question is whether the
artifact changed, and until now that could only be answered by hoping the
files on disk had not been rebuilt since.

So every trial records the identity of what it uploaded: sha256 and byte size
per binary, plus the pseudomux commit for the pmux companions. This is a
statement about the bytes that ran, not about what is on disk now.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


SCHEMA_VERSION = 1

# Read in chunks: pmux-rmuxd is ~22 MB and there is no reason to hold it.
_CHUNK = 1024 * 1024


@dataclass(frozen=True)
class ArtifactDigest:
    name: str
    sha256: str
    bytes: int

    def as_dict(self) -> dict[str, Any]:
        return {"sha256": self.sha256, "bytes": self.bytes}


def digest_file(path: Path, *, name: str | None = None) -> ArtifactDigest | None:
    """sha256 and size of one artifact, or None when it is not there.

    Returning None rather than raising is deliberate: an identity record is
    evidence, and failing to collect it must never be what fails a trial.
    """
    path = Path(path)
    label = name or path.name
    try:
        digest = hashlib.sha256()
        size = 0
        with path.open("rb") as handle:
            while True:
                chunk = handle.read(_CHUNK)
                if not chunk:
                    break
                size += len(chunk)
                digest.update(chunk)
    except OSError:
        return None
    return ArtifactDigest(name=label, sha256=digest.hexdigest(), bytes=size)


def digest_all(items: Iterable[tuple[str, Path]]) -> dict[str, dict[str, Any]]:
    """Digests several named artifacts, skipping any that cannot be read."""
    recorded: dict[str, dict[str, Any]] = {}
    for name, path in items:
        found = digest_file(path, name=name)
        if found is not None:
            recorded[name] = found.as_dict()
    return recorded


def artifact_record(
    *,
    harness: str,
    binaries: dict[str, dict[str, Any]],
    source_commit: str = "",
    source_dirty: bool = False,
    arch: str = "",
) -> dict[str, Any]:
    record: dict[str, Any] = {
        "schemaVersion": SCHEMA_VERSION,
        "harness": harness,
        "binaries": binaries,
    }
    if arch:
        record["arch"] = arch
    if source_commit:
        record["source"] = {
            "repo": "pseudomux",
            "commit": source_commit,
            "dirty": source_dirty,
        }
    return record


def pmux_artifact_record(bins: Any, *, arch: str = "") -> dict[str, Any]:
    """Identity of the five Full-cell companions FrozenPmux uploads."""
    binaries = digest_all(
        (
            ("pmux", Path(bins.pmux)),
            ("pmuxd", Path(bins.pmuxd)),
            ("pmux-rmuxd", Path(bins.rmuxd)),
            ("pmux-launcher", Path(bins.launcher)),
            ("pmux-hook", Path(bins.hook)),
        )
    )
    return artifact_record(
        harness="pmux",
        binaries=binaries,
        source_commit=getattr(bins, "commit", "") or "",
        source_dirty=bool(getattr(bins, "dirty", False)),
        arch=arch or getattr(bins, "arch", "") or "",
    )


def plak_artifact_record(
    binary: Path, js_host: Path | None = None, *, arch: str = ""
) -> dict[str, Any]:
    """Identity of the frozen plak binary, and the JS host when one is used."""
    items = [("plak", Path(binary))]
    if js_host is not None:
        items.append(("plakous-js-host", Path(js_host)))
    return artifact_record(harness="plak", binaries=digest_all(items), arch=arch)


def short_digest(record: dict[str, Any], name: str) -> str:
    """The first 12 hex characters of one binary's digest, for a table cell."""
    entry = ((record or {}).get("binaries") or {}).get(name) or {}
    sha = str(entry.get("sha256") or "")
    return sha[:12]


def record_source_commit(record: dict[str, Any]) -> str:
    return str(((record or {}).get("source") or {}).get("commit") or "")
