"""One switch for the Harbor eval plane's cell architecture.

The eval plane packages a frozen `plak`, a frozen `plakous-js-host`, frozen
vendor CLI prefixes and frozen pmux companions, uploads them into a Terminal-
Bench cell, and runs them there. Those artifacts are native binaries, so the
whole plane has to agree on one architecture.

`HARBOR_ARCH` is that agreement. It is `amd64` or `arm64` and nothing else.
Unset, it resolves to the Docker daemon's own architecture, so an Apple
Silicon host with a native `linux/arm64` daemon gets `arm64` and a Linux
x86_64 host gets `amd64`. Every other consumer (buildx platform, Node tarball
suffix, ELF `e_machine`, artifact root) derives from it here, once.

Architecture is part of an artifact's identity, not a preference: an ELF built
for the wrong machine fails closed rather than surfacing as a cell crash.
"""

from __future__ import annotations

import os
import platform
import shlex
import subprocess
from pathlib import Path

ARCHES = ("amd64", "arm64")

DOCKER_PLATFORMS = {"amd64": "linux/amd64", "arm64": "linux/arm64"}
NODE_ARCH_SUFFIXES = {"amd64": "linux-x64", "arm64": "linux-arm64"}
# ELF e_machine: EM_X86_64 / EM_AARCH64.
ELF_MACHINES = {"amd64": 0x3E, "arm64": 0xB7}

ELF_MAGIC = b"\x7fELF"

# Only used to normalise a probed machine name, never an explicit HARBOR_ARCH.
_PROBE_ALIASES = {
    "amd64": "amd64",
    "x86_64": "amd64",
    "x64": "amd64",
    "arm64": "arm64",
    "aarch64": "arm64",
}

DEFAULT_INSTALLED_ROOT = Path(".plak/installed")


class ArchError(RuntimeError):
    pass


def require_arch(value: str) -> str:
    """Validate an explicit HARBOR_ARCH value. Aliases are not accepted."""
    if value not in ARCHES:
        raise ArchError(
            f"HARBOR_ARCH must be one of {' | '.join(ARCHES)}; got {value!r}"
        )
    return value


def _probe_machine(name: str) -> str | None:
    return _PROBE_ALIASES.get(name.strip().lower())


def docker_daemon_arch() -> str | None:
    """The architecture of the daemon that will run the cells, or None."""
    try:
        result = subprocess.run(
            ["docker", "version", "--format", "{{.Server.Arch}}"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return _probe_machine(result.stdout)


def resolve_arch(value: str | None = None, *, env: dict[str, str] | None = None) -> str:
    """The cell architecture: explicit value, then HARBOR_ARCH, then the daemon."""
    if value is not None:
        return require_arch(value)
    environ = os.environ if env is None else env
    raw = (environ.get("HARBOR_ARCH") or "").strip()
    if raw:
        return require_arch(raw)
    probed = docker_daemon_arch()
    if probed is not None:
        return probed
    host = _probe_machine(platform.machine())
    if host is not None:
        return host
    raise ArchError(
        "cannot determine the eval cell architecture; set HARBOR_ARCH to "
        f"{' or '.join(ARCHES)}"
    )


def docker_platform(arch: str | None = None) -> str:
    return DOCKER_PLATFORMS[resolve_arch(arch)]


def node_arch_suffix(arch: str | None = None) -> str:
    return NODE_ARCH_SUFFIXES[resolve_arch(arch)]


def elf_machine(arch: str | None = None) -> int:
    return ELF_MACHINES[resolve_arch(arch)]


def installed_root(arch: str | None = None, *, base: Path | None = None) -> Path:
    """Per-architecture artifact root. There is no flat-layout fallback."""
    root = DEFAULT_INSTALLED_ROOT if base is None else Path(base)
    return root / resolve_arch(arch)


def read_elf_machine(path: Path) -> int:
    """`e_machine` of a little-endian ELF, or raise if it is not one."""
    try:
        with open(path, "rb") as handle:
            header = handle.read(20)
    except OSError as exc:
        raise ArchError(f"cannot read {path}: {exc}") from exc
    if len(header) < 20 or header[:4] != ELF_MAGIC:
        raise ArchError(f"{path} is not an ELF binary")
    if header[5] != 1:
        raise ArchError(f"{path} is not a little-endian ELF")
    return int.from_bytes(header[18:20], "little")


def require_elf_arch(path: Path, arch: str, label: str) -> None:
    """Fail closed when an artifact was built for the wrong machine."""
    want = ELF_MACHINES[require_arch(arch)]
    try:
        got = read_elf_machine(Path(path))
    except ArchError as exc:
        raise ArchError(
            f"{label} at {path} is not a linux ELF ({exc}); Harbor cells are "
            f"linux/{arch}, so do not upload a Darwin build"
        ) from exc
    if got != want:
        other = next(
            (name for name, value in ELF_MACHINES.items() if value == got), None
        )
        built = f"linux/{other}" if other else f"e_machine 0x{got:x}"
        raise ArchError(
            f"{label} at {path} is {built}, but the eval plane is linux/{arch}; "
            "rebuild it with that HARBOR_ARCH"
        )


REMOTE_PLATFORM_RECORD = "/logs/agent/platform.txt"


def platform_record_command(
    arch: str, path: str = REMOTE_PLATFORM_RECORD
) -> str:
    """Write the cell's resolved platform next to the other agent logs.

    Harbor uploads `/logs/agent` with the trial, so a result record says for
    itself which architecture produced it. ARM and x86 timings are separate
    lanes and must never be read as one series.
    """
    directory = os.path.dirname(path) or "/"
    return (
        f"mkdir -p {shlex.quote(directory)} && "
        f"printf '%s\\n' linux/{require_arch(arch)} > {shlex.quote(path)}"
    )
