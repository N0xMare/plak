"""Exercise the real CLI through a local SSH substitute, with no inference."""

import hashlib
import json
import os
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

REVISION = "a" * 40
SOURCE = "b" * 64
GATEWAY = "c" * 64
SCHEMA = "d" * 64
INSTANCE = "10-" + "e" * 16
REPLACEMENT = "10-" + "f" * 16
STATE_KEY = "r28.e39.p39.t11"
PLATFORM = "x86_64-unknown-linux-musl"
WIRE = {"number": 11, "schema_pin": SCHEMA}
COORDINATOR = {"coordinator": 3, "receipt": 3}
RUNNER = {"protocol": 9, "retention": {"served": [1], "spoken": [1]}}
NAMES = ("plakd", "plakous-js-host", "plakous-py-host")
ROOT = Path(__file__).resolve().parents[3]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode() + b"\n"


def write(path, value):
    path.write_bytes(encoded(value))


def file_facts(data):
    return {"length": len(data), "sha256": sha(data)}


def fake_git(args):
    assert args[0] == "-C"
    action = args[2]
    if action == "ls-remote":
        assert args[3:] == ["--exit-code", "origin", "refs/heads/main"]
        print(f"{REVISION}\trefs/heads/main")
    elif action == "rev-list":
        assert args[3:] == ["--first-parent", REVISION]
        print(REVISION)
    elif action == "show":
        revision, filename = args[3].split(":", 1)
        assert filename in ("install.sh", "tools/build/artifact_manifest.py")
        assert revision == REVISION
        sys.stdout.buffer.write((ROOT / filename).read_bytes())
    else:
        raise AssertionError(args)


def target(args):
    assert "-oStrictHostKeyChecking=yes" in args and "-oForwardAgent=no" in args
    assert args[-2] in ("example-host", "example-client")
    assert args[-1].startswith("python3 -I -c ")
    raw = sys.stdin.buffer
    request = json.loads(raw.readline())
    inputs = {}
    for entry in request["files"]:
        blob = raw.read(entry["length"])
        assert len(blob) == entry["length"] and sha(blob) == entry["sha256"]
        inputs[entry["name"]] = blob
    assert raw.read() == b""
    path = Path(os.environ["FAKE_FLEET"])
    fleet = json.loads(path.read_bytes())
    operation = request["op"]
    payload = request["args"]
    reply = {}
    if operation == "observe":
        if fleet.get("refusal"):
            print(json.dumps({"error": fleet["refusal"]}))
            return 0
        reply = fleet["host"]
    elif operation == "client_observe":
        reply = {"current": fleet["client"] == "current"}
    else:
        fleet["events"].append(operation)
        if operation == "canary_preflight":
            assert payload["expected"] == fleet["host"]["sha256"]
        elif operation == "health":
            assert payload["expected"] == fleet["host"]["sha256"]
            reply = {
                "doctor": "pass",
                "status": "pass",
                "bindings": "pass",
                "generations": {"grok": 5},
            }
        elif operation == "canary":
            assert payload["expected"] == fleet["host"]["sha256"]
            reply = {"adapter": "bounded_cli_v1", "generation_before": 5, "generation_after": 6}
        elif operation == "stage_host":
            manifest = json.loads(payload["manifest"])
            assert sha(payload["manifest"].encode()) == payload["manifest_sha256"]
            assert set(inputs) == set(NAMES)
            fleet["candidate"] = manifest["files"]["plakd"]["sha256"]
            fleet["candidate_sidecars"] = {
                name: manifest["files"][name]["sha256"] for name in NAMES[1:]
            }
        elif operation == "stage_clients":
            raise AssertionError("expected only one typed transfer per client")
        elif operation == "client_stage":
            assert set(inputs) == {"client"} and fleet["candidate"]
            fleet["client"] = "staged"
        elif operation == "destination_canary_preflight":
            assert fleet["candidate"] == payload["sha256"]
        elif operation == "upgrade":
            assert fleet["candidate"] == payload["sha256"]
            assert fleet["client"] == "staged"
            assert payload["previous"] == fleet["host"]
            fleet["host"].update(sha256=fleet["candidate"], instance=REPLACEMENT)
            fleet["host"]["facts"]["runner_triple"]["source_revision"] = REVISION
            if fleet.get("lose_reply"):
                write(path, fleet)
                return 255
        elif operation == "host_link":
            assert fleet["host"]["instance"] == payload["instance"] == REPLACEMENT
            fleet["host"]["link_sha256"] = payload["sha256"]
            fleet["host"]["link_sidecars"] = fleet["candidate_sidecars"]
        elif operation == "client_link":
            assert fleet["host"]["link_sha256"] == fleet["candidate"]
            assert payload["connection"]["wire"] == WIRE
            fleet["client"] = "current"
        elif operation == "client_check":
            assert fleet["client"] == "current"
        else:
            raise AssertionError(operation)
        write(path, fleet)
    print(json.dumps({"ok": reply}))
    return 0


class Fleet:
    def __init__(self, root, binary):
        self.root = root
        self.binary = binary
        self.state = root / "fleet.json"
        self.inventory = root / "private.toml"
        self.receipt = root / "receipt.json"
        self.evidence = root / "evidence.jsonl"
        self.manifest = self.artifacts()
        self.env = {
            **os.environ,
            "PATH": str(root) + os.pathsep + os.environ["PATH"],
            "FAKE_FLEET": str(self.state),
        }
        for name in ("git", "ssh"):
            launcher = root / name
            launcher.write_text(f"#!/bin/sh\nexec '{sys.executable}' '{__file__}' {name} \"$@\"\n")
            launcher.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)
        self.configure()
        self.reset()

    def artifacts(self):
        root = self.root / "host"
        root.mkdir()
        files = {}
        for name in NAMES:
            blob = name.encode()
            (root / name).write_bytes(blob)
            file = file_facts(blob)
            files[name] = {**file, "display_id": file["sha256"][:12]}
        snapshot = sha("".join(f"{name} {files[name]['sha256']}\n" for name in NAMES).encode())
        build = {
            "state_key": STATE_KEY,
            "export": {"read": [7, 8], "write": 8},
            "flavor": "release",
            "gateway": {"served": [1, 2], "spoken": [1, 2]},
            "host_upgrade": COORDINATOR,
            "omitted_features": [],
            "product_version": "0.0.4",
            "revision": REVISION,
            "runner": RUNNER,
            "test_hooks": False,
            "wire": WIRE,
        }
        manifest = {
            "schema": 1,
            "revision": REVISION,
            "platform": PLATFORM,
            "flavor": "release",
            "product_version": "0.0.4",
            "lock_sha256": "e" * 64,
            "toolchain": {
                "rustc": {
                    "release": "1.100.0",
                    "commit_hash": "a" * 40,
                    "host": PLATFORM,
                    "llvm_version": "22.1.0",
                },
                "cargo": {"release": "1.100.0", "commit_hash": "b" * 9},
            },
            "files": files,
            "snapshot_id": snapshot,
            "build_info": build,
        }
        write(root / "manifest.json", manifest)
        client = b"exact-match client fixture\n"
        (self.root / "client").write_bytes(client)
        write(
            self.root / "client.json",
            {
                "schema": 1,
                "platform": PLATFORM,
                "wire": WIRE,
                "file": file_facts(client),
            },
        )
        return manifest

    def configure(self, revision=REVISION):
        root = str(self.root)
        host_pin = sha((self.root / "host/manifest.json").read_bytes())
        client_pin = sha((self.root / "client.json").read_bytes())
        self.inventory.write_text(f"""
schema = 1
repo = "{root}"
revision = "{revision}"
artifact = "{root}/host"
manifest_sha256 = "{host_pin}"
[production]
[staging]
drain_seconds = 60
evidence = "{self.evidence}"
receipt = "{self.receipt}"
[staging.host]
product_home = "/srv/example/.plak"
source_sha256 = "{SOURCE}"
web_socket = "/srv/example/host.sock"
ui = "/srv/example/ui"
gateway_unit = "example-gateway.service"
gateway_socket = "/srv/example/gateway.sock"
gateway_sha256 = "{GATEWAY}"
[staging.host.node]
ssh = "example-host"
home = "/srv/example"
uid = 1000
[[staging.routes]]
account = "grok"
model = "example-model"
effort = "low"
protocol = ""
[[staging.clients]]
id = "example"
link = "/srv/example/.local/bin/example-client"
blob = "{root}/client"
manifest = "{root}/client.json"
manifest_sha256 = "{client_pin}"
source_sha256 = "{SOURCE}"
[staging.clients.node]
ssh = "example-client"
home = "/srv/example"
uid = 1000
""")

    def reset(self, *, host=None, facts=None, **state):
        current = {
            "sha256": SOURCE,
            "link_sha256": SOURCE,
            "link_sidecars": {name: SOURCE for name in NAMES[1:]},
            "instance": INSTANCE,
            "unit_sha256": SOURCE,
            "custody_sha256": GATEWAY,
            "gateway_instance": "example",
            "gateway_versions": [1],
            "paired": False,
            "canary": ["claude", "grok"],
            "facts": {
                "source": "legacy facts",
                "state_key": STATE_KEY,
                "wire": WIRE,
                "host_upgrade": COORDINATOR,
                "runner_triple": {"protocol": 9, "source_revision": "b" * 40, "flavor": "release"},
                **(facts or {}),
            },
            **(host or {}),
        }
        write(self.state, {"host": current, "events": [], "client": "source", **state})

    def invoke(self, verb, result, contains=None, *, stage="staging"):
        command = [self.binary, "deploy"]
        if self.binary == "-":
            # Local Python tooling may exercise the same driver without a Cargo build.
            deploy = ROOT / "bin/plakd/src/deploy"
            command = [
                sys.executable,
                str(deploy / "driver.py"),
                (deploy / "target.py").read_text(),
            ]
        arguments = (
            [stage, str(self.inventory)]
            if self.binary == "-"
            else [
                "--inventory",
                str(self.inventory),
                "--stage",
                stage,
            ]
        )
        output = subprocess.run(
            [*command, verb, *arguments], env=self.env, capture_output=True, check=False
        )
        assert output.stdout, output.stderr.decode()
        report = json.loads(output.stdout)
        assert report["result"] == result, (report, output.stderr)
        assert (output.returncode == 0) == (result in ("ready", "green", "no_op")), output
        if contains:
            assert contains in report["reason"], report
        return report

    def read(self):
        return json.loads(self.state.read_bytes())


def fleet_case(fleet):
    fleet.configure("f" * 40)
    fleet.invoke("plan", "blocked", "not on origin/main")
    fleet.configure()
    for kwargs, reason in [
        ({"facts": {"state_key": "r29.e39.p39.t11"}}, "cross-key"),
        ({"host": {"paired": True}}, "paired-runner triple change"),
        ({"refusal": "gateway image change: use S3"}, "gateway image change"),
        ({"refusal": "custody change: use the custody runbook"}, "custody change"),
    ]:
        fleet.reset(**kwargs)
        fleet.invoke("plan", "blocked", reason)
        assert not fleet.evidence.exists() and fleet.read()["events"] == []
    fleet.reset(host={"canary": None})
    fleet.invoke("plan", "bootstrap_required", "existing manual same-key procedure")
    fleet.invoke("apply", "bootstrap_required", "existing manual same-key procedure")
    assert fleet.read()["events"] == []
    fleet.reset()
    plan = fleet.invoke("plan", "ready")
    assert plan["plan"] == [
        "baseline",
        "stage_host",
        "stage_clients",
        "host_upgrade",
        "host_link",
        "client_links",
        "verify",
    ]
    assert fleet.read()["events"] == []
    host_blob = fleet.root / "host/plakd"
    original = host_blob.read_bytes()
    host_blob.write_bytes(b"X" * len(original))
    fleet.invoke("plan", "blocked", "blob digest differs")
    assert fleet.read()["events"] == []
    host_blob.write_bytes(original)
    report = fleet.invoke("apply", "green")
    assert json.loads(fleet.receipt.read_bytes()) == report
    assert fleet.read()["events"] == [
        "canary_preflight",
        "health",
        "canary",
        "stage_host",
        "client_stage",
        "destination_canary_preflight",
        "upgrade",
        "host_link",
        "client_link",
        "client_check",
        "health",
        "canary",
    ]
    before = fleet.receipt.read_bytes()
    fleet.invoke("apply", "no_op")
    assert fleet.read()["events"][-2:] == ["client_check", "health"]
    assert fleet.receipt.read_bytes() == before
    production = fleet.inventory.read_text().split("[staging]\n", 1)[1]
    production = ("[production]\n" + production).replace("[staging.", "[production.")
    production = production.replace(str(fleet.evidence), str(fleet.root / "production.jsonl"))
    inventory = fleet.inventory.read_text().replace("[production]\n", "")
    fleet.inventory.write_text(inventory + "\n" + production)
    fleet.reset()
    invalid = json.loads(before)
    invalid["manifests"]["host"] = SOURCE
    write(fleet.receipt, invalid)
    fleet.invoke("apply", "blocked", "exact manifest digests", stage="production")
    assert fleet.read()["events"] == []
    fleet.receipt.write_bytes(before)
    assert fleet.invoke("apply", "green", stage="production")["stage"] == "production"


def requester_loss(fleet):
    fleet.reset(lose_reply=True)
    halted = fleet.invoke("apply", "needs_reconcile")
    assert halted["step"] == "host_upgrade" and "deploy status" in halted["retry"]
    before = fleet.read()
    assert before["host"]["link_sha256"] == SOURCE
    assert not fleet.receipt.exists()
    plan = fleet.invoke("plan", "ready")
    assert "host_upgrade" not in plan["plan"]
    assert fleet.read() == before
    fleet.invoke("apply", "green")
    assert fleet.read()["events"].count("upgrade") == 1
    assert fleet.read()["events"].count("canary") == 2


if __name__ == "__main__":
    case = sys.argv[1]
    if case == "git":
        fake_git(sys.argv[2:])
    elif case == "ssh":
        sys.exit(target(sys.argv[2:]))
    else:
        with tempfile.TemporaryDirectory() as directory:
            fleet = Fleet(Path(directory), sys.argv[2])
            {"fleet": fleet_case, "requester-loss": requester_loss}[case](fleet)
