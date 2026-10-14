"""Which pseudomux commit the frozen pmux companions were built from.

`just fetch-harbor-pmux` builds the Full-cell binaries with `docker buildx`
from a pseudomux checkout passed as a *directory context*. A directory context
carries no git history, so the artifacts that land in
`.plak/installed/<arch>/pmux/` used to record nothing about their source: the
pin check confirmed the five files existed and were ELF for the right machine,
which a build from any commit, or from a dirty tree, satisfies equally well.

Provenance is part of an artifact's identity the same way architecture is. The
fetch resolves the checkout's `HEAD`, refuses a dirty worktree unless the
operator names an explicit override, and writes both facts into a manifest
beside the binaries. The pin check then requires that manifest to exist and to
name `PSEUDOMUX_PIN`.

The pin lives here and nowhere else. `eval/README.md` quotes the short form
through `PSEUDOMUX_PIN_SHORT`, which is derived rather than typed again.
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from harbor_adapter.arch import resolve_arch
from harbor_adapter.vendor_pins import CLI_PINS, VendorArtifactError


# The pseudomux commit the eval plane's Full-cell companions are built from.
# One place. Changing it is a deliberate re-pin, and every artifact built
# before the change stops verifying until it is refetched.
PSEUDOMUX_PIN = "fc0e8cbf7227ca24b37f036a97e2b79fe565ef78"
PSEUDOMUX_PIN_SHORT = PSEUDOMUX_PIN[:7]

MANIFEST_NAME = "manifest.json"
SCHEMA_VERSION = 1

# Building from a dirty pseudomux worktree produces an artifact that no commit
# describes. It is allowed only when the operator says so by name, and the
# manifest records that it happened.
DIRTY_OVERRIDE_ENV = "PMUX_ALLOW_DIRTY"
# Verifying an artifact whose manifest does not name the pin. Same rule.
UNPINNED_OVERRIDE_ENV = "PMUX_ALLOW_UNPINNED"


class PmuxProvenanceError(VendorArtifactError):
    pass


@dataclass(frozen=True)
class SourceCommit:
    commit: str
    dirty: bool
    override: str = ""

    @property
    def short(self) -> str:
        return self.commit[:7]

    def as_dict(self) -> dict[str, Any]:
        return {
            "commit": self.commit,
            "dirty": self.dirty,
            "dirty_override": self.override,
        }


def _git(checkout: Path, *args: str) -> str:
    try:
        result = subprocess.run(
            ["git", "-C", str(checkout), *args],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        raise PmuxProvenanceError(
            f"cannot read the pseudomux checkout at {checkout}: {exc}"
        ) from exc
    if result.returncode != 0:
        raise PmuxProvenanceError(
            f"git {' '.join(args)} failed in {checkout}: "
            f"{(result.stderr or '').strip()}"
        )
    return (result.stdout or "").strip()


def head_commit(checkout: Path) -> str:
    commit = _git(checkout, "rev-parse", "HEAD")
    if len(commit) != 40 or not all(c in "0123456789abcdef" for c in commit):
        raise PmuxProvenanceError(
            f"{checkout} did not resolve HEAD to a full commit: {commit!r}"
        )
    return commit


def worktree_is_dirty(checkout: Path) -> bool:
    """True when tracked files differ from HEAD, or untracked files exist."""
    return bool(_git(checkout, "status", "--porcelain"))


def resolve_source_commit(
    checkout: Path, *, allow_dirty: bool | None = None
) -> SourceCommit:
    """Resolves what the build is about to be made from, failing closed.

    `allow_dirty` defaults to whether the operator set the override
    environment variable. A dirty build is recorded as dirty, so it can never
    later be mistaken for a clean build of the commit it sits on.
    """
    checkout = Path(checkout)
    if not (checkout / ".git").exists():
        raise PmuxProvenanceError(
            f"{checkout} is not a git checkout; the pmux build records the "
            "pseudomux commit, so PMUX_SRC must be one"
        )
    override = os.environ.get(DIRTY_OVERRIDE_ENV, "").strip()
    if allow_dirty is None:
        allow_dirty = bool(override)
    commit = head_commit(checkout)
    dirty = worktree_is_dirty(checkout)
    if dirty and not allow_dirty:
        raise PmuxProvenanceError(
            f"the pseudomux checkout at {checkout} is dirty, so the binaries "
            f"would not match commit {commit[:7]}; commit or stash the "
            f"changes, or set {DIRTY_OVERRIDE_ENV}=1 to record an "
            "explicitly unreproducible build"
        )
    return SourceCommit(
        commit=commit,
        dirty=dirty,
        override=override if dirty else "",
    )


def manifest_path(pmux_root: Path) -> Path:
    return Path(pmux_root) / MANIFEST_NAME


def build_manifest(
    *, arch: str, source: SourceCommit, bins: tuple[str, ...]
) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "arch": resolve_arch(arch),
        "source": {"repo": "pseudomux", **source.as_dict()},
        "claude_code": CLI_PINS["claude"],
        "bins": list(bins),
    }


def record_provenance(
    pmux_root: Path,
    *,
    arch: str,
    source: SourceCommit,
    bins: tuple[str, ...],
) -> Path:
    """Writes the manifest beside the freshly built binaries."""
    path = manifest_path(pmux_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(build_manifest(arch=arch, source=source, bins=bins), indent=2)
        + "\n",
        encoding="utf-8",
    )
    return path


def read_manifest(pmux_root: Path) -> dict[str, Any]:
    path = manifest_path(pmux_root)
    if not path.is_file():
        raise PmuxProvenanceError(
            f"missing {path}; these pmux binaries record no pseudomux commit. "
            "Run `just fetch-harbor-pmux` to rebuild and record one"
        )
    try:
        loaded = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise PmuxProvenanceError(f"{path} is not readable JSON: {exc}") from exc
    if not isinstance(loaded, dict):
        raise PmuxProvenanceError(f"{path} is not a JSON object")
    return loaded


def require_pinned_provenance(
    pmux_root: Path,
    *,
    arch: str,
    allow_unpinned: bool | None = None,
) -> SourceCommit:
    """Requires the recorded commit to be `PSEUDOMUX_PIN` and the build clean."""
    manifest = read_manifest(pmux_root)
    recorded_arch = manifest.get("arch")
    selected = resolve_arch(arch)
    if recorded_arch != selected:
        raise PmuxProvenanceError(
            f"{manifest_path(pmux_root)} records arch {recorded_arch!r}, "
            f"but this lane is {selected!r}"
        )
    source = manifest.get("source")
    if not isinstance(source, dict):
        raise PmuxProvenanceError(
            f"{manifest_path(pmux_root)} records no source commit"
        )
    commit = str(source.get("commit") or "")
    dirty = bool(source.get("dirty"))
    if allow_unpinned is None:
        allow_unpinned = bool(os.environ.get(UNPINNED_OVERRIDE_ENV, "").strip())
    if not allow_unpinned:
        if commit != PSEUDOMUX_PIN:
            raise PmuxProvenanceError(
                f"pmux binaries were built from pseudomux {commit[:7] or '?'}, "
                f"but the eval plane pins {PSEUDOMUX_PIN_SHORT}; refetch with "
                f"`just fetch-harbor-pmux`, or set {UNPINNED_OVERRIDE_ENV}=1 "
                "to run an explicitly off-pin lane"
            )
        if dirty:
            raise PmuxProvenanceError(
                f"pmux binaries were built from a dirty pseudomux worktree at "
                f"{commit[:7]}, so no commit describes them; refetch from a "
                f"clean checkout, or set {UNPINNED_OVERRIDE_ENV}=1"
            )
    return SourceCommit(
        commit=commit,
        dirty=dirty,
        override=str(source.get("dirty_override") or ""),
    )
