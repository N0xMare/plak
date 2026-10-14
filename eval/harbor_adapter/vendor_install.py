"""Shell that extracts a frozen vendor prefix into a Harbor task cell."""

from __future__ import annotations

import shlex

from harbor_adapter.vendor_pins import (
    CLI_BINARIES,
    REMOTE_VENDOR,
    require_known_cli,
)


def nvm_stub_body(cli: str) -> str:
    """PATH-only stub. Harbor's Pi/Codex/OpenCode `run()` sources ~/.nvm/nvm.sh."""
    require_known_cli(cli)
    node_bin = f"{REMOTE_VENDOR}/node/bin"
    cli_bin = f"{REMOTE_VENDOR}/{cli}/bin"
    return f"export PATH={node_bin}:{cli_bin}:$PATH\n"


def node_cli_install_command(cli: str) -> str:
    """Root install: unpack Node + npm prefix, stub nvm, put the binary on PATH."""
    require_known_cli(cli)
    binary = CLI_BINARIES[cli]
    stub = nvm_stub_body(cli)
    quoted_stub = shlex.quote(stub)
    node_tar = f"{REMOTE_VENDOR}/node.tar.gz"
    cli_tar = f"{REMOTE_VENDOR}/{cli}.tar.gz"
    node_dir = f"{REMOTE_VENDOR}/node"
    cli_dir = f"{REMOTE_VENDOR}/{cli}"
    bin_path = f"{cli_dir}/bin/{binary}"
    return (
        "set -euo pipefail; "
        f"mkdir -p {shlex.quote(node_dir)} {shlex.quote(cli_dir)}; "
        f"tar -xzf {shlex.quote(node_tar)} -C {shlex.quote(node_dir)} "
        "--strip-components=1; "
        f"tar -xzf {shlex.quote(cli_tar)} -C {shlex.quote(cli_dir)}; "
        f"chmod -R a+rX {shlex.quote(REMOTE_VENDOR)}; "
        f"chmod 0755 {shlex.quote(bin_path)} "
        f"{shlex.quote(node_dir)}/bin/node; "
        f"ln -sfn {shlex.quote(node_dir)}/bin/node /usr/local/bin/node; "
        f"ln -sfn {shlex.quote(bin_path)} /usr/local/bin/{binary}; "
        "for home in /root /home/*; do "
        '  if [ -d "$home" ]; then '
        '    mkdir -p "$home/.nvm"; '
        f"    printf '%s' {quoted_stub} > \"$home/.nvm/nvm.sh\"; "
        "  fi; "
        "done; "
        'mkdir -p "$HOME/.nvm"; '
        f"printf '%s' {quoted_stub} > \"$HOME/.nvm/nvm.sh\"; "
        f"command -v {binary}"
    )


def grok_install_command() -> str:
    remote_bin = f"{REMOTE_VENDOR}/grok"
    return (
        "set -euo pipefail; "
        f"chmod 0755 {shlex.quote(remote_bin)}; "
        f"ln -sfn {shlex.quote(remote_bin)} /usr/local/bin/grok; "
        "for home in /root /home/*; do "
        '  if [ -d "$home" ]; then '
        '    mkdir -p "$home/.grok/bin"; '
        f"    ln -sfn {shlex.quote(remote_bin)} \"$home/.grok/bin/grok\"; "
        "  fi; "
        "done; "
        'mkdir -p "$HOME/.grok/bin"; '
        f"ln -sfn {shlex.quote(remote_bin)} \"$HOME/.grok/bin/grok\"; "
        "command -v grok"
    )
