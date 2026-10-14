"""Pinned Full-cell pmux companions for FrozenPmux Harbor cells.

Claude Code itself is the frozen npm prefix in vendor_pins (`claude` 2.1.280).
These binaries are built from the sibling pseudomux checkout for the
`HARBOR_ARCH` the cells run on. Darwin Mach-O release bins are refused, and so
is an ELF built for the other machine. `pmux-mcp` is ask/Messages and is not
admitted.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from harbor_adapter.arch import installed_root, require_elf_arch, resolve_arch
from harbor_adapter.pmux_provenance import (
    PmuxProvenanceError,
    require_pinned_provenance,
)
from harbor_adapter.vendor_pins import CLI_PINS, VendorArtifactError

CLAUDE_CODE_PIN = CLI_PINS["claude"]
FULL_CELL_BINS = (
    "pmux",
    "pmuxd",
    "pmux-rmuxd",
    "pmux-launcher",
    "pmux-hook",
)


class PmuxArtifactError(VendorArtifactError):
    pass


@dataclass(frozen=True)
class FrozenPmuxBins:
    root: Path
    pmux: Path
    pmuxd: Path
    rmuxd: Path
    launcher: Path
    hook: Path
    arch: str
    # The pseudomux commit these binaries were built from, read back from the
    # manifest the fetch wrote. File presence and ELF machine do not identify
    # an artifact; the source commit does.
    commit: str = ""
    dirty: bool = False


def default_pmux_root(arch: str | None = None) -> Path:
    return installed_root(arch) / "pmux"


def pmux_root(root: Path | None = None, arch: str | None = None) -> Path:
    return Path(root) if root is not None else default_pmux_root(arch)


def _require_linux_elf(path: Path, label: str, arch: str) -> None:
    if not path.is_file():
        raise PmuxArtifactError(
            f"missing {path}; run `just fetch-harbor-pmux`"
        )
    try:
        require_elf_arch(path, arch, label)
    except RuntimeError as exc:
        raise PmuxArtifactError(str(exc)) from exc


def require_frozen_pmux(
    root: Path | None = None, arch: str | None = None
) -> FrozenPmuxBins:
    selected = resolve_arch(arch)
    base = pmux_root(root, selected)
    paths = {}
    for name in FULL_CELL_BINS:
        path = base / name
        _require_linux_elf(path, name, selected)
        paths[name] = path
    # Provenance failures reach callers as PmuxArtifactError, the way the ELF
    # check does; a caller catching the pin error must not miss this one.
    try:
        source = require_pinned_provenance(base, arch=selected)
    except PmuxProvenanceError as exc:
        raise PmuxArtifactError(str(exc)) from exc
    return FrozenPmuxBins(
        root=base,
        arch=selected,
        pmux=paths["pmux"],
        pmuxd=paths["pmuxd"],
        rmuxd=paths["pmux-rmuxd"],
        launcher=paths["pmux-launcher"],
        hook=paths["pmux-hook"],
        commit=source.commit,
        dirty=source.dirty,
    )
