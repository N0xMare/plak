"""The same pinned search executable on PATH for every arm."""

from __future__ import annotations

import hashlib
import io
import json
import os
import shlex
import sys
import tarfile
import urllib.request
from pathlib import Path
from typing import Any

from harbor_adapter.arch import installed_root, require_elf_arch, resolve_arch
from harbor_adapter.artifact_identity import digest_file
from harbor_adapter.campaign import load_campaign
from harbor_adapter.credentials import require_native_oauth_lifetime
from harbor_adapter.preflight import PreflightError, task_budgets


RG_VERSION = "15.1.0"
# Official BurntSushi/ripgrep release assets; do not repackage them.
RG_PINS = {
    "amd64": ("x86_64-unknown-linux-musl",
              "1c9297be4a084eea7ecaedf93eb03d058d6faae29bbc57ecdaf5063921491599",
              "ebeaf56f8a25e102e9419933423738b3a2a613a444fd749d695e15eba53f71f2"),
    "arm64": ("aarch64-unknown-linux-gnu",
              "2b661c6ef508e902f388e9098d9c4c5aca72c87b55922d94abdba830b4dc885e",
              "968cabe8efed72fd8fd482cb76b6084fcb695fc5293af7fb62296b02f487fb69"),
}


async def record_cell(agent: Any, environment: Any) -> None:
    """Probe immediately before dispatch, after every adapter's installation."""
    plan = load_campaign()
    arm = plan["arms"].get(os.environ.get("HARBOR_ARM"))
    if arm is None:
        raise PreflightError("HARBOR_ARM must name a frozen campaign arm")
    remotes = {"plak": "/installed-agent/plak", "plakous-js-host": "/installed-agent/plakous-js-host",
               "node": "/installed-agent/vendor/node/bin/node", "node-archive": "/installed-agent/vendor/node.tar.gz"}
    for cli in ("codex", "grok", "pi", "opencode", "claude"):
        remotes[cli] = "/usr/local/bin/" + cli
        remotes[cli + "-archive"] = "/installed-agent/vendor/" + cli + ("" if cli == "grok" else ".tar.gz")
    for name in ("pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook"):
        remotes[name] = "/installed-agent/" + name
    for name, digest in arm["binaries"].items():
        if name not in remotes:
            raise PreflightError("unknown executed binary pin")
        line = shlex.quote(digest + "  " + remotes[name])
        checked = await environment.exec(command=f"printf '%s\\n' {line} | sha256sum -c - >/dev/null")
        if checked.return_code:
            raise PreflightError("executed binary differs from frozen campaign pin")
    command = (
        "set -e; id -u; for tool in rg git python3 curl bash; do command -v $tool; done; "
        "rg --version | head -n 1; "
        "dpkg-query -W -f='${Package} ${Version}\\n' | LC_ALL=C sort | sha256sum; "
        "for name in cpu.max memory.max; do cat /sys/fs/cgroup/$name; done"
    )
    result = await environment.exec(command=command)
    if result.return_code:
        raise PreflightError("cell tool/package/resource probe failed")
    uid = (result.stdout or "").splitlines()[0]
    actual_user = uid
    if type(agent).__name__ == "FrozenPmux":
        probe = await agent.exec_as_agent(environment, command=(
            'if [ "$(id -u)" -eq 0 ]; then su -s /bin/bash pmux -c "id -un"; else id -un; fi'
        ))
        if probe.return_code:
            raise PreflightError("pmux run-user probe failed")
        actual_user = (probe.stdout or "").strip()
    expected_user = str(arm["policy"]["user"])
    if expected_user == "root":
        expected_user = "0"
    if actual_user != expected_user or arm["policy"]["leftover_processes"] != "preserve":
        raise PreflightError("actual run user or adapter lifecycle policy differs from campaign")
    agent_timeout, verifier_timeout = task_budgets(environment)
    require_native_oauth_lifetime(agent, arm["account"], agent_timeout, verifier_timeout)
    record = {
        "schemaVersion": 1,
        "cell_manifest_sha256": hashlib.sha256((result.stdout or "").encode()).hexdigest(),
        "image": environment.task_env_config.docker_image,
        "agent_timeout": agent_timeout, "verifier_timeout": verifier_timeout,
    }
    (agent.logs_dir / "cell-preflight.json").write_text(json.dumps(record, indent=2) + "\n")
    task = environment.environment_dir.parent.name
    matches = [value for key, value in plan["tasks"].items() if key.rsplit("/", 1)[-1] == task]
    if len(matches) != 1 or any(matches[0].get(k) != record[k] for k in
                              ("cell_manifest_sha256", "image", "agent_timeout", "verifier_timeout")):
        raise PreflightError("actual cell differs from campaign image, policy, or timeout manifest")


async def prepare_cell(agent: Any, environment: Any, arch: str) -> None:
    if load_campaign()["arch"] != arch:
        raise PreflightError("cell architecture differs from frozen campaign")
    path = installed_root(arch) / "tools" / "rg"
    digest = digest_file(path)
    if digest is None or digest.sha256 != RG_PINS[arch][2]:
        raise PreflightError("missing or unpinned cell rg; run just fetch-harbor-tools")
    require_elf_arch(path, arch, "cell rg")
    await environment.upload_file(str(path), "/usr/local/bin/rg")
    expected = shlex.quote(RG_PINS[arch][2] + "  /usr/local/bin/rg")
    await agent.exec_as_root(environment, command=(
        f"printf '%s\\n' {expected} | sha256sum -c - >/dev/null && "
        "chmod 0755 /usr/local/bin/rg && timeout -k 2s 15s /usr/local/bin/rg --version >/dev/null"
    ))


def fetch_tools(arch: str, directory: Path) -> None:
    """Explicit opt-in public release download, never called by a trial."""
    target, archive_sha, binary_sha = RG_PINS[arch]
    name = f"ripgrep-{RG_VERSION}-{target}"
    url = f"https://github.com/BurntSushi/ripgrep/releases/download/{RG_VERSION}/{name}.tar.gz"
    with urllib.request.urlopen(url, timeout=60) as response:
        archive = response.read()
    if hashlib.sha256(archive).hexdigest() != archive_sha:
        raise PreflightError("rg release archive digest mismatch")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        stream = tar.extractfile(name + "/rg")
        if stream is None:
            raise PreflightError("rg release has no executable")
        binary = stream.read()
    if hashlib.sha256(binary).hexdigest() != binary_sha:
        raise PreflightError("rg release executable digest mismatch")
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "rg").write_bytes(binary)
    (directory / "rg").chmod(0o755)


if __name__ == "__main__":
    arch = resolve_arch(sys.argv[1] if len(sys.argv) > 1 else None)
    fetch_tools(arch, installed_root(arch) / "tools")
