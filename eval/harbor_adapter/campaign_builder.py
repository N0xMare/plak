"""Generate private, reviewable campaign pins from local frozen evidence (no inference)."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tomllib
import uuid

from harbor_adapter.campaign import sha256, validate_campaign
from harbor_adapter.cell_prep import CELL_PROBE, CELL_REQUIREMENTS
from harbor_adapter.preflight import PreflightError
from harbor_adapter.tuples import RUNG_A_AB_TASKS, RUNG_A_EXPANDED_TASKS, RUNG_A_PLUMBING_TASKS

SLICES = {"plumbing": RUNG_A_PLUMBING_TASKS, "ab": RUNG_A_AB_TASKS, "expanded": RUNG_A_EXPANDED_TASKS}
PACKAGES = "ca-certificates coreutils procps curl git python3 openssl bash"
ACCOUNTS = {"plak-codex": "codex", "codex": "codex", "pi-codex-oauth": "codex", "opencode-codex-oauth": "codex",
            "plak-grok": "grok", "grok-build": "grok", "plak-claude": "claude", "claude-code": "claude"}


def docker(*args: str, **kwargs) -> str:
    return subprocess.check_output(["docker", *args], text=True, **kwargs).strip()


def private_json(path: Path, value: dict) -> str:
    data = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
    with open(path, "xb", opener=lambda p, f: os.open(p, f, 0o600)) as stream:
        stream.write(data)
    return hashlib.sha256(data).hexdigest()


def artifact_digest(root: Path, arch: str, source) -> str:
    relative = source if isinstance(source, str) else source["archive"]
    path = (root / relative).resolve()
    if not path.is_relative_to((root / ".plak/installed" / arch).resolve()):
        raise PreflightError("artifact is outside the frozen architecture prefix")
    if isinstance(source, str):
        return sha256(path)
    with tarfile.open(path, "r:gz") as archive:
        stream = archive.extractfile(source["member"])
        if stream is None:
            raise PreflightError("archive binary member missing")
        digest = hashlib.sha256()
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
        return digest.hexdigest()


def frozen_binaries(root: Path, arch: str, name: str) -> dict:
    from harbor_adapter.vendor_pins import CLI_PINS, NODE_VERSION, load_manifest, node_sha256, node_tarball_name
    prefix = Path(".plak/installed") / arch
    if name.startswith("plak-"):
        return {key: str(prefix / key) for key in ("plak", "plakous-js-host")}
    cli = {"codex": "codex", "pi-codex-oauth": "pi", "opencode-codex-oauth": "opencode", "grok-build": "grok", "claude-code": "claude"}[name]
    manifest = load_manifest(root / prefix / "vendor", arch)
    entry = manifest.get("clis", {}).get(cli, {})
    expected_file = "grok" if cli == "grok" else cli + ".tar.gz"
    if manifest.get("arch") != arch or entry.get("version") != CLI_PINS[cli] or entry.get("file") != expected_file:
        raise PreflightError("frozen vendor version or architecture does not match the source pin")
    if cli == "grok":
        from harbor_adapter.arch import require_elf_arch
        require_elf_arch(root / prefix / "vendor/grok", arch, "frozen grok")
        return {"grok": str(prefix / "vendor/grok"), "grok-archive": str(prefix / "vendor/grok")}
    node = manifest.get("node", {})
    if (node.get("version") != NODE_VERSION or node.get("file") != node_tarball_name(arch)
            or sha256(root / prefix / "vendor" / node_tarball_name(arch)) != node_sha256(arch)):
        raise PreflightError("frozen Node archive differs from the compiled official pin")
    paths = {"node": prefix / "vendor" / node_tarball_name(arch), cli: prefix / "vendor" / (cli + ".tar.gz")}
    result = {}
    for binary, path in paths.items():
        with tarfile.open(root / path, "r:gz") as archive:
            members = [m.name for m in archive.getmembers()
                       if (m.name.lstrip("./").endswith("/bin/node") if binary == "node"
                           else m.name.lstrip("./") == "bin/" + cli)]
        if len(members) != 1:
            raise PreflightError("frozen archive has no unique executable member")
        result[binary] = {"archive": str(path), "member": members[0]}
        result[binary + "-archive"] = str(path)
    if name == "claude-code":
        from harbor_adapter.pmux_pins import require_frozen_pmux
        from harbor_adapter.pmux_provenance import PSEUDOMUX_PIN
        pmux = require_frozen_pmux(root / prefix / "pmux", arch)
        if pmux.commit != PSEUDOMUX_PIN or pmux.dirty:
            raise PreflightError("campaigns require the exact clean pmux pin, without overrides")
        result.update({key: str(prefix / "pmux" / key)
                       for key in ("pmux", "pmuxd", "pmux-rmuxd", "pmux-launcher", "pmux-hook")})
    return result


def inventory(task_root: Path, arch: str, slice_name: str, arms: list[str], calibrations: Path | None) -> dict:
    tasks = {}
    for name in SLICES[slice_name]:
        matches = [path.parent for path in task_root.rglob("task.toml") if path.parent.name == name.rsplit("/", 1)[-1]]
        if len(matches) != 1:
            raise PreflightError("task root must contain one frozen checkout of every selected task")
        tasks[name] = {"path": str(matches[0].resolve())}
    if any(name not in ACCOUNTS for name in arms):
        raise PreflightError("unknown or disabled arm")
    return {"arch": arch, "tasks": tasks, "arms": {name: {
        "account": ACCOUNTS[name],
        **({"calibration_file": str((calibrations / (name + ".json")).resolve())} if calibrations else {}),
    } for name in arms}}


def task_binding(root: Path, arch: str, task: dict) -> dict:
    from dirhash import dirhash
    directory = Path(task["path"]).resolve()
    config = tomllib.loads((directory / "task.toml").read_text())
    environment = config.get("environment", {})
    image = task["image"]
    info = json.loads(docker("image", "inspect", image))[0]
    if info["Id"] != image or info["Architecture"] != arch:
        raise PreflightError("task image must be a local native immutable ID")
    if any((directory / "environment" / name).exists() for name in
           ("docker-compose.yaml", "docker-compose.yml", "compose.yaml", "compose.yml")):
        raise PreflightError("compose tasks are not admitted")
    resources = []
    if environment.get("cpus"):
        resources += ["--cpus", str(environment["cpus"])]
    if environment.get("memory_mb"):
        resources += ["--memory", str(environment["memory_mb"]) + "m"]
    rg = (root / ".plak/installed" / arch / "tools/rg").resolve()
    command = CELL_REQUIREMENTS + "; " + CELL_PROBE
    manifest = docker("run", "--rm", "--network=none", "--entrypoint=/bin/sh", *resources,
                      "--mount", f"type=bind,src={rg},dst=/usr/local/bin/rg,readonly",
                      image, "-c", command)
    # check_output's trailing newline is part of the actual cell probe digest.
    digest = hashlib.sha256((manifest + "\n").encode()).hexdigest()
    return {"image": image, "checksum": dirhash(directory, "sha256"),
            "cell_manifest_sha256": digest,
            "agent_timeout": config.get("agent", {}).get("timeout_sec", 600),
            "verifier_timeout": config.get("verifier", {}).get("timeout_sec", 600)}


def generate(root: Path, inventory: dict, *, phase: str, slice_name: str, blocks: int) -> dict:
    arch = inventory["arch"]
    selected = inventory["tasks"]
    if phase == "calibration":
        if set(selected) == set(RUNG_A_PLUMBING_TASKS):
            selected = {"write-hello": selected["write-hello"]}
        if len(selected) != 1 or not set(selected) <= set(RUNG_A_PLUMBING_TASKS):
            raise PreflightError("calibration selects exactly one plumbing task")
    elif set(selected) != set(SLICES[slice_name]):
        raise PreflightError("inventory must contain the exact frozen slice")
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    dirty = subprocess.check_output(["git", "status", "--porcelain", "--", "eval", "Justfile"], cwd=root)
    if dirty:
        raise PreflightError("commit eval sources before generating a campaign")
    plan = {"schemaVersion": 1, "phase": phase, "slice": slice_name, "source_commit": source,
            "arch": arch, "concurrency": 1, "blocks": blocks,
            "tasks": {name: task_binding(root, arch, task) for name, task in selected.items()},
            "artifact_sha256": {path: artifact_digest(root, arch, path)
                                for path in inventory.get("artifact_paths", [])}, "arms": {}}
    for name, entry in inventory["arms"].items():
        if "openrouter" in name or entry["account"] == "openrouter":
            raise PreflightError("OpenRouter comparisons remain disabled")
        if name not in ACCOUNTS or entry["account"] != ACCOUNTS[name]:
            raise PreflightError("arm/account does not match the subscription recipe")
        binaries = entry.get("binaries") or frozen_binaries(root, arch, name)
        for path in binaries.values():
            path = path if isinstance(path, str) else path["archive"]
            plan["artifact_sha256"][path] = artifact_digest(root, arch, path)
        arm = {"account": entry["account"], "policy": {"user": "root", "web_tools": False,
               "leftover_processes": "preserve", "cell_prep": "frozen-common-v1"},
               "binaries": {key: artifact_digest(root, arch, path)
                            for key, path in binaries.items()}}
        if name == "claude-code":
            arm["binaries"]["claude-policy-launcher"] = sha256(Path(__file__).with_name("claude-no-web.sh"))
            manifest = str(Path(".plak/installed") / arch / "pmux/manifest.json")
            plan["artifact_sha256"][manifest] = artifact_digest(root, arch, manifest)
        if name.startswith("plak-"):
            from harbor_adapter.artifact_identity import require_plak_provenance
            binary = root / ".plak/installed" / arch / "plak"
            provenance = require_plak_provenance(binary, binary.with_name("plakous-js-host"))
            if provenance["source"]["commit"] != source:
                raise PreflightError("Plak must be built at this campaign source commit")
            manifest = str(binary.with_name("plak-build.json"))
            plan["artifact_sha256"][manifest] = artifact_digest(root, arch, manifest)
        if phase != "calibration":
            path = Path(entry["calibration_file"]).resolve()
            reduced = json.loads(path.read_text())
            if reduced.get("source") != "exchange" or not reduced.get("wire_verified"):
                raise PreflightError("calibration must come from actual wire bytes")
            if reduced.get("calibration_binding") != {"source_commit": source, "arch": arch, "binaries": arm["binaries"]}:
                raise PreflightError("calibration belongs to different source, architecture or executed bytes")
            if name.startswith("plak-"):
                composition = reduced.get("resolved_composition")
                if composition not in ("direct", "native-ptc"):
                    raise PreflightError("calibration has no observed shipped Plak composition")
                arm["resolved_composition"] = composition
            arm.update(calibration_file=str(path), calibration_sha256=sha256(path),
                       observed_config=reduced["first_request"])
        plan["arms"][name] = arm
    if phase == "null":
        plan["arms"] = {"plak-codex": plan["arms"]["plak-codex"],
                        "plak-codex-aa": plan["arms"]["plak-codex"]}
    if inventory.get("null_receipt_file"):
        receipt = Path(inventory["null_receipt_file"]).resolve()
        plan.update(null_receipt_file=str(receipt), null_receipt_sha256=sha256(receipt))
    failures = validate_campaign(plan)
    if failures:
        raise PreflightError("campaign refused: " + ",".join(failures))
    return plan


def freeze(inventory: dict, *, build_tasks: bool = False) -> dict:
    """Build each task once, then bake the same support packages into every image."""
    for task in inventory["tasks"].values():
        directory = Path(task["path"]).resolve()
        config = tomllib.loads((directory / "task.toml").read_text())
        environment = config.get("environment", {})
        if any((directory / "environment" / name).exists() for name in
               ("docker-compose.yaml", "docker-compose.yml", "compose.yaml", "compose.yml")):
            raise PreflightError("compose tasks are not admitted")
        base = None if build_tasks else environment.get("docker_image")
        if base:
            docker("pull", "--platform", "linux/" + inventory["arch"], base)
            base = json.loads(docker("image", "inspect", base))[0]["Id"]
        else:
            base = docker("build", "-q", "--platform", "linux/" + inventory["arch"],
                          str(directory / "environment")).splitlines()[-1]
        # BuildKit treats FROM sha256:... as a registry tag, not a local ID.
        # Keep a unique base pin: deleting its last tag could also delete an
        # otherwise untagged image ID that another campaign still references.
        tag = "plak-harbor-freeze-" + uuid.uuid4().hex + ":base"
        docker("tag", base, tag)
        task.update(base_image=base, base_tag=tag)
        dockerfile = (f"FROM {tag}\nUSER root\nRUN apt-get update -qq && "
                      f"DEBIAN_FRONTEND=noninteractive apt-get install -y -qq {PACKAGES}\n")
        task["image"] = docker("build", "-q", "--platform", "linux/" + inventory["arch"],
                               "-", input=dockerfile).splitlines()[-1]
    return inventory


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("inventory", "freeze", "generate"))
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--task-root", type=Path)
    parser.add_argument("--arch", choices=("amd64", "arm64"), default="amd64")
    parser.add_argument("--arms", nargs="+", default=["plak-codex", "codex"])
    parser.add_argument("--calibrations", type=Path)
    parser.add_argument("--null-receipt", type=Path)
    parser.add_argument("--build-tasks", action="store_true",
                        help="build task Dockerfiles once instead of pulling prebuilt images")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--phase", choices=("calibration", "null", "scored"), default="scored")
    parser.add_argument("--slice", choices=tuple(SLICES), default="expanded")
    parser.add_argument("--blocks", type=int, default=5)
    args = parser.parse_args()
    if args.action == "inventory":
        if not args.task_root:
            parser.error("inventory requires --task-root")
        result = inventory(args.task_root, args.arch, args.slice, args.arms, args.calibrations)
        print(private_json(args.output, result) + "  " + str(args.output))
        return
    if not args.inventory:
        parser.error("freeze and generate require --inventory")
    inputs = json.loads(args.inventory.read_text())
    if args.null_receipt:
        inputs["null_receipt_file"] = str(args.null_receipt)
    root = Path(__file__).resolve().parents[2]
    result = (freeze(inputs, build_tasks=args.build_tasks) if args.action == "freeze" else
              generate(root, inputs, phase=args.phase, slice_name=args.slice, blocks=args.blocks))
    print(private_json(args.output, result) + "  " + str(args.output))


if __name__ == "__main__":
    main()
