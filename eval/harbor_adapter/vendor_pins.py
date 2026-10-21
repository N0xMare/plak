"""Pinned vendor CLIs for Harbor bakeoff cells.

The prefixes are architecture-specific: `HARBOR_ARCH` (see `arch.py`) picks
the Node tarball, the artifact root and the `e_machine` the `grok` binary must
carry. A manifest built for the other architecture fails closed.

Harbor's stock Codex/Pi/OpenCode agents run `nvm install` and a floating
global npm install inside every task container. Grok Build runs `curl | bash`
the same way. That wastes minutes per trial, floats the CLI mid-campaign, and
mixes installer wall time into harness comparison.

v1 copies a once-fetched prefix into `/installed-agent/vendor` and runs the
stock `codex exec` / `grok --single` / `pi --print` / `opencode run` path.
Missing or mismatched artifacts fail closed. An unpinned latest tag is not a pin.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path

from harbor_adapter.campaign import load_campaign, sha256

from harbor_adapter.arch import (
    NODE_ARCH_SUFFIXES,
    installed_root,
    require_elf_arch,
    resolve_arch,
)

# Node 22 is what Harbor's nvm snippet installs; pin the official tarball for
# the selected architecture so cells do not fetch nodejs.org at trial time.
NODE_VERSION = "22.20.0"
NODE_SHA256 = {
    "amd64": "eeaccb0378b79406f2208e8b37a62479c70595e20be6b659125eb77dd1ab2a29",
    "arm64": "4181609e03dcb9880e7e5bf956061ecc0503c77a480c6631d868cb1f65a2c7dd",
}

# Match the host native-home CLIs at the time these pins were frozen.
# Do not silently follow whatever Harbor `@latest` resolves to.
CLI_PINS = {
    "codex": "0.159.3",
    "grok": "1.0.30",
    "pi": "0.87.1",
    "opencode": "1.18.30",
    "claude": "2.1.280",
}

NPM_PACKAGES = {
    "codex": "@openai/codex",
    "pi": "@earendil-works/pi-coding-agent",
    "opencode": "opencode-ai",
    "claude": "@anthropic-ai/claude-code",
}

CLI_BINARIES = {
    "codex": "codex",
    "pi": "pi",
    "opencode": "opencode",
    "grok": "grok",
    "claude": "claude",
}

NODE_CLIS = ("codex", "pi", "opencode", "claude")
ALL_CLIS = ("codex", "grok", "pi", "opencode", "claude")

REMOTE_VENDOR = "/installed-agent/vendor"

FORBIDDEN_INSTALL_MARKERS = (
    "nvm install",
    "@latest",
    "curl -fsSL",
    "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm",
    "install.sh | bash",
    "npm install -g",
    "npm i -g",
)


class VendorArtifactError(RuntimeError):
    pass


@dataclass(frozen=True)
class FrozenArtifacts:
    root: Path
    node_tarball: Path
    cli_archive: Path
    cli: str
    version: str
    arch: str


def default_vendor_root(arch: str | None = None) -> Path:
    return installed_root(arch) / "vendor"


def vendor_root(root: Path | None = None, arch: str | None = None) -> Path:
    return Path(root) if root is not None else default_vendor_root(arch)


def node_tarball_name(arch: str | None = None) -> str:
    suffix = NODE_ARCH_SUFFIXES[resolve_arch(arch)]
    return f"node-v{NODE_VERSION}-{suffix}.tar.gz"


def node_sha256(arch: str | None = None) -> str:
    return NODE_SHA256[resolve_arch(arch)]


def node_url(arch: str | None = None) -> str:
    return f"https://nodejs.org/dist/v{NODE_VERSION}/{node_tarball_name(arch)}"


def cli_archive_name(cli: str) -> str:
    if cli == "grok":
        return "grok"
    if cli not in CLI_PINS:
        raise VendorArtifactError(f"unknown vendor CLI {cli!r}")
    return f"{cli}.tar.gz"


def require_known_cli(cli: str) -> str:
    if cli not in CLI_PINS:
        raise VendorArtifactError(f"unknown vendor CLI {cli!r}")
    return cli


def load_manifest(
    root: Path | None = None, arch: str | None = None
) -> dict[str, object]:
    path = vendor_root(root, arch) / "manifest.json"
    if not path.is_file():
        raise VendorArtifactError(
            f"missing {path}; run `just fetch-harbor-vendor-clis`"
        )
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise VendorArtifactError(f"invalid vendor manifest {path}: {exc}") from exc
    if not isinstance(payload, dict):
        raise VendorArtifactError(f"vendor manifest {path} is not an object")
    return payload


def require_frozen_artifacts(
    cli: str, root: Path | None = None, arch: str | None = None
) -> FrozenArtifacts:
    """Fail closed if the on-disk prefix is missing, misversioned or cross-built."""
    require_known_cli(cli)
    selected = resolve_arch(arch)
    base = vendor_root(root, selected)
    manifest = load_manifest(base, selected)
    recorded = manifest.get("arch")
    if recorded != selected:
        raise VendorArtifactError(
            f"vendor manifest in {base} is {recorded!r}, but the eval plane is "
            f"{selected!r}; run `just fetch-harbor-vendor-clis` with "
            f"HARBOR_ARCH={selected}"
        )
    tarball = node_tarball_name(selected)
    node_file = base / tarball
    if cli in NODE_CLIS:
        node = manifest.get("node")
        if not isinstance(node, dict):
            raise VendorArtifactError("vendor manifest is missing node")
        if node.get("version") != NODE_VERSION:
            raise VendorArtifactError(
                f"vendor node is {node.get('version')!r}, pin is {NODE_VERSION}"
            )
        if node.get("file") != tarball:
            raise VendorArtifactError(
                f"vendor node is {node.get('file')!r}, pin for {selected} is "
                f"{tarball}"
            )
        if node.get("sha256") != node_sha256(selected):
            raise VendorArtifactError("vendor node sha256 does not match the pin")
        node_file = base / str(node.get("file") or tarball)
        if not node_file.is_file():
            raise VendorArtifactError(
                f"missing {node_file}; run `just fetch-harbor-vendor-clis`"
            )

    clis = manifest.get("clis")
    if not isinstance(clis, dict):
        raise VendorArtifactError("vendor manifest is missing clis")
    entry = clis.get(cli)
    if not isinstance(entry, dict):
        raise VendorArtifactError(f"vendor manifest is missing {cli}")
    if entry.get("version") != CLI_PINS[cli]:
        raise VendorArtifactError(
            f"frozen {cli} is {entry.get('version')!r}, pin is {CLI_PINS[cli]}"
        )
    archive = base / str(entry.get("file") or cli_archive_name(cli))
    if not archive.is_file():
        raise VendorArtifactError(
            f"missing {archive}; run `just fetch-harbor-vendor-clis`"
        )
    if cli == "grok":
        # The only vendor CLI shipped as a bare native binary; the npm
        # prefixes carry their own platform packages inside the tarball.
        try:
            require_elf_arch(archive, selected, "frozen grok")
        except RuntimeError as exc:
            raise VendorArtifactError(str(exc)) from exc
    if os.environ.get("HARBOR_CAMPAIGN"):
        pins = load_campaign()["artifact_sha256"]
        for path in ([node_file, archive] if cli in NODE_CLIS else [archive]):
            key = f".plak/installed/{selected}/vendor/{path.name}"
            if pins.get(key) != sha256(path):
                raise VendorArtifactError("vendor archive does not match the reviewed campaign pin table")
    return FrozenArtifacts(
        root=base,
        node_tarball=node_file,
        cli_archive=archive,
        cli=cli,
        version=CLI_PINS[cli],
        arch=selected,
    )


def docker_build_args(arch: str | None = None) -> dict[str, str]:
    selected = resolve_arch(arch)
    # No arch build arg: the Dockerfile stamps the manifest from buildx's own
    # TARGETARCH, so the manifest describes its build rather than its caller.
    # `require_frozen_artifacts` still refuses a manifest whose arch is not the
    # selected one.
    return {
        "NODE_VERSION": NODE_VERSION,
        "NODE_TARBALL": node_tarball_name(selected),
        "NODE_SHA256": node_sha256(selected),
        "NODE_URL": node_url(selected),
        "CODEX_VERSION": CLI_PINS["codex"],
        "GROK_VERSION": CLI_PINS["grok"],
        "PI_VERSION": CLI_PINS["pi"],
        "OPENCODE_VERSION": CLI_PINS["opencode"],
        "CLAUDE_VERSION": CLI_PINS["claude"],
        "CODEX_PACKAGE": NPM_PACKAGES["codex"],
        "PI_PACKAGE": NPM_PACKAGES["pi"],
        "OPENCODE_PACKAGE": NPM_PACKAGES["opencode"],
        "CLAUDE_PACKAGE": NPM_PACKAGES["claude"],
    }
