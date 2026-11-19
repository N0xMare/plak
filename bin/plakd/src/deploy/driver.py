"""Private operator-side deployment driver (embedded by plakd)."""

import fcntl
import hashlib
import json
import os
import re
import shlex
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid
from pathlib import Path

NAMES = ("plakd", "plakous-js-host", "plakous-py-host")
HEX = r"[a-f0-9]"
DIGEST = rf"{HEX}{{64}}"
REVISION = rf"(?:{HEX}{{40}}|{HEX}{{64}})"
ID = r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}"
ACCOUNT = r"(?:codex|claude|grok|openai|anthropic|openrouter|xai-api)(?:/[A-Za-z0-9_-]+)?"
PLATFORM = "x86_64-unknown-linux-musl"
LIMIT = 1024 * 1024
EVIDENCE_SCHEMA = 2
RECIPES = ("install.sh", "tools/build/artifact_manifest.py")
TARGET_LOADER = (
    "import sys; r=sys.stdin.buffer; n=int(r.readline(16)); "
    "assert 0 < n <= 1048576; s=r.read(n); assert len(s) == n; "
    "exec(compile(s, '<deploy target>', 'exec'), {'__name__':'__main__'})"
)
BOOTSTRAP = (
    "bootstrap_required: upgrade the pre-S2 incumbent using the existing manual same-key "
    "procedure (stage with install.sh --no-link, run the destination's "
    "host upgrade --expect-binary <id> --drain <duration>, then run install.sh link "
    "<snapshot-id> after Ready); re-run deploy plan with an S2 incumbent"
)


class Halt(Exception):
    def __init__(self, message, state="blocked"):
        super().__init__(message)
        self.state = state


def need(condition, message, state="blocked"):
    if not condition:
        raise Halt(message, state)


def closed(value, keys, label):
    need(isinstance(value, dict) and value.keys() == set(keys.split()), f"invalid {label} fields")


def match(value, pattern, label):
    need(isinstance(value, str) and re.fullmatch(pattern, value), f"invalid {label}")


def path(value):
    need(
        isinstance(value, str)
        and value.startswith("/")
        and value == str(Path(value))
        and ".." not in Path(value).parts
        and not any(ord(char) < 32 or ord(char) == 127 for char in value),
        "expected a normalized absolute path",
    )
    return Path(value)


def integer(value, low, high, label):
    need(type(value) is int and low <= value <= high, f"invalid {label}")


def local_file(filename, limit):
    fd = os.open(filename, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        need(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "expected a regular input file")
        data = stream.read(limit + 1)
    need(len(data) <= limit, "input exceeds its size limit")
    return data


def private_file(filename):
    info = Path(filename).lstat()
    need(
        info.st_uid == os.geteuid() and stat.S_ISREG(info.st_mode) and not info.st_mode & 0o077,
        "receipt or evidence must be an owned private regular file",
    )


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def unique(pairs):
    result = dict(pairs)
    need(len(result) == len(pairs), "duplicate JSON fields")
    return result


def decode(data):
    return json.loads(data, object_pairs_hook=unique)


def node(value):
    closed(value, "ssh login home uid", "node")
    match(value["ssh"], ID, "SSH host")
    match(value["login"], r"[a-z_][a-z0-9_-]{0,63}", "SSH login")
    path(value["home"])
    integer(value["uid"], 1, 2**32 - 2, "UID")


def inventory(filename, stage):
    data = tomllib.loads(local_file(filename, LIMIT).decode())
    closed(data, "schema repo revision artifact manifest_sha256 staging production", "inventory")
    integer(data["schema"], 1, 1, "inventory schema")
    match(data["revision"], REVISION, "revision")
    match(data["manifest_sha256"], DIGEST, "manifest digest")
    for key in ("repo", "artifact"):
        path(data[key])
    # An unused stage is an empty table, not a second source of defaults.
    configuration = data[stage]
    for name in ("staging", "production"):
        configured = data[name]
        if name != stage and configured == {}:
            continue
        validate_stage(configured, name)
    if data["staging"] and data["production"]:
        need(
            data["staging"]["host"]["node"]["ssh"] != data["production"]["host"]["node"]["ssh"],
            "staging and production must use distinct enrolled SSH host aliases",
        )
    need(
        configuration["evidence"] != data["staging"].get("receipt"),
        "evidence and receipt paths must differ",
    )
    return data, configuration


def validate_stage(value, stage):
    closed(value, "host routes clients drain_seconds evidence receipt", "stage")
    path(value["receipt"])
    path(value["evidence"])
    need(value["receipt"] != value["evidence"], "evidence and receipt paths must differ")
    integer(value["drain_seconds"], 0, 120, "drain")
    host = value["host"]
    closed(
        host,
        "node product_home source_sha256 web_socket ui gateway_unit gateway_socket gateway_sha256",
        "host",
    )
    node(host["node"])
    for key in ("product_home", "web_socket", "ui", "gateway_socket"):
        path(host[key])
    for key in ("source_sha256", "gateway_sha256"):
        match(host[key], DIGEST, key)
    match(host["gateway_unit"], rf"{ID}\.service", "gateway unit")
    need(host["gateway_unit"] != "plakd-host.service", "gateway and host unit must differ")
    routes = value["routes"]
    need(isinstance(routes, list) and len(routes) <= 32, "expected at most 32 canary accounts")
    for route in routes:
        closed(route, "account model effort", "canary route")
        match(route["account"], ACCOUNT, "canary account")
        match(route["model"], r"[A-Za-z0-9][A-Za-z0-9_./:-]{0,127}", "canary model")
        need(route["effort"] in ("", "none", "low"), "canary effort must be omitted, none, or low")
    need(len({route["account"] for route in routes}) == len(routes), "duplicate canary account")
    clients = value["clients"]
    need(isinstance(clients, list) and len(clients) <= 8, "expected at most eight clients")
    for client in clients:
        closed(client, "id node link blob manifest manifest_sha256 source_sha256", "client")
        match(client["id"], ID, "client id")
        node(client["node"])
        for key in ("link", "blob", "manifest"):
            path(client[key])
        for key in ("manifest_sha256", "source_sha256"):
            match(client[key], DIGEST, key)
    need(len({client["id"] for client in clients}) == len(clients), "duplicate client id")
    links = [(client["node"]["ssh"], client["link"]) for client in clients]
    need(len(set(links)) == len(links), "duplicate client link")
    if stage == "production":
        need(
            value["receipt"] != value["evidence"], "production must read a separate staging receipt"
        )


def git(repo, *args):
    try:
        return subprocess.check_output(
            ["git", "-C", repo, *args],
            stderr=subprocess.DEVNULL,
            timeout=30,
            env={**os.environ, "GIT_NO_REPLACE_OBJECTS": "1", "GIT_TERMINAL_PROMPT": "0"},
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise Halt(
            "cannot verify origin/main; fetch origin main in the inventory repo and retry"
        ) from error


def from_main(repo, revision):
    upstream = git(repo, "ls-remote", "--exit-code", "origin", "refs/heads/main").decode()
    parts = upstream.split()
    need(len(parts) == 2 and parts[1] == "refs/heads/main", "ambiguous origin/main")
    match(parts[0], REVISION, "advertised main revision")
    history = git(repo, "rev-list", "--first-parent", parts[0]).decode().splitlines()
    need(revision in history, "revision is not on origin/main; build an exact origin/main commit")


def verify_file(filename, facts, *, allow_symlink=False):
    candidate = Path(filename)
    need(
        allow_symlink or (candidate.is_file() and not candidate.is_symlink()),
        "blob must be a regular file",
    )
    with candidate.open("rb") as stream:
        observed = os.fstat(stream.fileno())
        need(
            stat.S_ISREG(observed.st_mode) and observed.st_size == facts["length"],
            "blob length differs",
        )
        need(
            hashlib.file_digest(stream, "sha256").hexdigest() == facts["sha256"],
            "blob digest differs",
        )
        stream.seek(0)


class Artifacts:
    def __init__(self, data, configuration):
        self.repo = data["repo"]
        self.root = Path(data["artifact"])
        self.pins = {"host": data["manifest_sha256"], "clients": {}}
        revision = data["revision"]
        from_main(data["repo"], revision)
        self.recipes = {name: git(data["repo"], "show", f"{revision}:{name}") for name in RECIPES}
        self.raw = local_file(self.root / "manifest.json", LIMIT)
        need(sha(self.raw) == self.pins["host"], "host manifest digest differs")
        # Use S1's closed schema from the same origin/main revision as the installer.
        schema = {"__name__": "shipping_manifest"}
        exec(compile(self.recipes[RECIPES[1]], RECIPES[1], "exec"), schema)  # noqa: S102
        self.host = schema["read_canonical"](self.raw)
        schema["validate_manifest"](self.host)
        need(
            self.host["revision"] == revision and self.host["platform"] == PLATFORM,
            "wrong host revision or target",
        )
        for name in NAMES:
            verify_file(self.root / name, self.host["files"][name])
        self.clients = {}
        for client in configuration["clients"]:
            raw = local_file(client["manifest"], LIMIT)
            need(sha(raw) == client["manifest_sha256"], "client manifest digest differs")
            manifest = decode(raw)
            closed(manifest, "schema platform wire file", "client manifest")
            integer(manifest["schema"], 1, 1, "client manifest schema")
            need(manifest["platform"] == PLATFORM, "client target must be Linux x86_64")
            need(
                manifest["wire"] == self.host["build_info"]["wire"],
                "client wire/schema pin differs; build an exact-match client",
            )
            closed(manifest["file"], "length sha256", "client file")
            match(manifest["file"]["sha256"], DIGEST, "client file digest")
            integer(manifest["file"]["length"], 1, 2**31, "client length")
            verify_file(client["blob"], manifest["file"])
            self.clients[client["id"]] = (raw, manifest["file"])
            self.pins["clients"][client["id"]] = client["manifest_sha256"]

    def installer(self):
        return {name: value.decode() for name, value in self.recipes.items()}


class SSH:
    def __init__(self, script):
        self.command = "python3 -I -c " + shlex.quote(TARGET_LOADER)
        self.script = script.encode()
        need(0 < len(self.script) <= LIMIT, "target script exceeds its size bound")
        self.operator_machine = None
        if sys.platform == "linux":
            with Path("/etc/machine-id").open("rb") as stream:
                machine = stream.read(65).strip()
            need(
                re.fullmatch(rb"[0-9a-f]{32}", machine),
                "cannot establish operator machine identity",
            )
            self.operator_machine = sha(machine)

    def call(self, node, operation, arguments, files=(), timeout=45):
        header = {
            "op": operation,
            "node": node,
            "args": arguments,
            "files": [],
            "operator_machine": self.operator_machine,
        }
        with tempfile.TemporaryFile() as upload:
            for name, filename, facts in files:
                header["files"].append({"name": name, **facts})
            payload = encode(header)
            need(len(payload) <= LIMIT, "SSH request exceeds its size limit")
            upload.write(str(len(self.script)).encode() + b"\n")
            upload.write(self.script)
            upload.write(payload + b"\n")
            for _, filename, facts in files:
                with Path(filename).open("rb") as source:
                    digest = hashlib.sha256()
                    remaining = facts["length"]
                    while remaining:
                        block = source.read(min(remaining, LIMIT))
                        need(block, "blob shortened during transfer")
                        upload.write(block)
                        digest.update(block)
                        remaining -= len(block)
                    need(
                        not source.read(1) and digest.hexdigest() == facts["sha256"],
                        "blob changed during transfer",
                    )
            upload.seek(0)
            args = [
                "ssh",
                "-T",
                "-F",
                "/dev/null",
                "-oBatchMode=yes",
                "-oControlMaster=no",
                "-oControlPath=none",
                "-oStrictHostKeyChecking=yes",
                "-oConnectTimeout=10",
                "-oServerAliveInterval=5",
                "-oServerAliveCountMax=2",
                "-oForwardAgent=no",
                "-oClearAllForwardings=yes",
                "-l",
                node["login"],
                "--",
                node["ssh"],
                self.command,
            ]
            try:
                with tempfile.TemporaryFile() as reply, tempfile.TemporaryFile() as stderr:
                    result = subprocess.run(
                        args,
                        stdin=upload,
                        stdout=reply,
                        stderr=stderr,
                        timeout=timeout,
                        check=False,
                    )
                    reply.seek(0)
                    data = reply.read(LIMIT + 1)
                    stderr.seek(0)
                    failure = ssh_failure(stderr.read(4096))
                need(
                    result.returncode == 0 and len(data) <= LIMIT,
                    f"SSH {operation} returned no bounded verdict ({failure})",
                )
                response = decode(data)
                need(isinstance(response, dict), f"invalid SSH {operation} reply")
                if set(response) == {"error"}:
                    raise Halt(response["error"])
                closed(response, "ok", "target response")
                return response["ok"]
            except subprocess.TimeoutExpired as error:
                raise Halt(f"SSH {operation} exceeded its deadline") from error
            except (OSError, subprocess.SubprocessError, ValueError) as error:
                raise Halt(f"SSH {operation} returned no bounded verdict") from error


def ssh_failure(stderr):
    for needle, failure in (
        (b"host key verification failed", "host_key"),
        (b"remote host identification has changed", "host_key"),
        (b"permission denied", "authentication"),
        (b"could not resolve hostname", "name_resolution"),
        (b"connection refused", "connection_refused"),
        (b"timed out", "timeout"),
        (b"python3", "remote_runtime"),
        (b"broken pipe", "transport_lost"),
        (b"closed by remote host", "transport_lost"),
    ):
        if needle in stderr.lower():
            return failure
    return "remote_command_or_transport"


class Evidence:
    def __init__(self, path):
        self.path = Path(path)
        self.file = None
        self.rows = []
        self.stage_identity = None

    def __enter__(self):
        fd = os.open(
            self.path, os.O_RDWR | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600
        )
        self.file = os.fdopen(fd, "r+b")
        try:
            info = os.fstat(fd)
            need(
                info.st_uid == os.geteuid()
                and stat.S_ISREG(info.st_mode)
                and not info.st_mode & 0o077,
                "evidence log must be private and owned",
            )
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise Halt("another deploy holds the evidence log") from error
            self.rows = self.read(self.path)
        except BaseException:
            self.file.close()
            raise
        return self

    def __exit__(self, *_):
        self.file.close()

    @staticmethod
    def read(filename):
        if not Path(filename).exists():
            return []
        private_file(filename)
        data = local_file(filename, 32 * LIMIT)
        need(
            not data or data.endswith(b"\n"),
            "incomplete evidence log; reconcile before proceeding",
            "needs_reconcile",
        )
        rows = [decode(line) for line in data.splitlines()]
        need(
            all(isinstance(row, dict) and row.get("schema") == EVIDENCE_SCHEMA for row in rows),
            "unrecognized evidence schema; reconcile before proceeding",
            "needs_reconcile",
        )
        return rows

    def record(self, intent, phase, **facts):
        row = {
            "schema": EVIDENCE_SCHEMA,
            "intent": intent,
            "time": int(time.time()),
            "phase": phase,
            **facts,
        }
        if self.stage_identity is not None:
            row["stage_identity"] = self.stage_identity
        self.file.write(encode(row) + b"\n")
        self.file.flush()
        os.fsync(self.file.fileno())
        self.rows.append(row)


def publish(filename, report):
    path = Path(filename)
    if os.path.lexists(path):
        private_file(path)
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as output:
            temp = Path(output.name)
            output.write(encode(report) + b"\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temp, path)
    finally:
        if "temp" in locals():
            temp.unlink(missing_ok=True)


class Deploy:
    def __init__(self, ssh, configuration, artifacts, stage, report):
        self.ssh = ssh
        self.config = configuration
        self.artifacts = artifacts
        self.stage = stage
        self.report = report
        self.host = configuration["host"]
        self.dest = artifacts.host
        self.want = self.dest["files"]["plakd"]["sha256"]
        self.sidecars = {name: self.dest["files"][name]["sha256"] for name in NAMES[1:]}
        self.intent = sha(encode([stage, configuration, artifacts.pins]))
        self.routes = []

    def host_call(self, operation, arguments=None, **options):
        return self.ssh.call(
            self.host["node"], operation, {"host": self.host, **(arguments or {})}, **options
        )

    def client_call(self, client, operation, arguments=None, **options):
        manifest, _ = self.artifacts.clients[client["id"]]
        return self.ssh.call(
            client["node"],
            operation,
            {
                "link": client["link"],
                "manifest": manifest.decode(),
                "manifest_sha256": client["manifest_sha256"],
                "source_sha256": client["source_sha256"],
                **(arguments or {}),
            },
            **options,
        )

    def observe(self):
        result = self.host_call("observe")
        self.report["observed"] = result
        return result

    def plan(self, rows, evidence=None):
        prior = pending_upgrade(rows)
        try:
            observed = self.observe()
        except Halt as error:
            if prior:
                raise Halt(
                    "recorded upgrade outcome is unresolved: " + str(error), "needs_reconcile"
                ) from error
            raise
        facts = observed["facts"]
        wanted = self.dest["build_info"]
        self.report["stage_identity"] = observed["stage_identity"]
        if evidence:
            evidence.stage_identity = observed["stage_identity"]
        need(
            self.stage != "staging" or not observed["production_history"],
            "target has production history; use an isolated staging host",
        )
        need(
            facts["state_key"] == wanted["state_key"],
            "cross-key: use the settled host upgrade runbook",
        )
        if (
            facts["runner_triple"]
            != {
                "protocol": wanted["runner"]["protocol"],
                "source_revision": wanted["revision"],
                "flavor": "release",
            }
            and observed["paired"]
        ):
            raise Halt("paired-runner triple change: settle and qualify the old placements; use S3")
        need(not observed["paired"], "paired runner placement is outside S2; use S3")
        need(
            facts["runner_triple"]["protocol"] == wanted["runner"]["protocol"],
            "home-local runner protocol change: settle jobs and use the runner upgrade runbook",
        )
        need(
            facts["host_upgrade"] == wanted["host_upgrade"],
            "coordinator/receipt change: enroll a compatible incumbent first",
        )
        enrolled = {route["account"]: route for route in self.config["routes"]}
        in_use = {
            account for account, status in observed["accounts"].items() if status != "not_enrolled"
        }
        missing = in_use - enrolled.keys()
        need(not missing, "in-use accounts missing from inventory: " + ", ".join(sorted(missing)))
        versions = set(observed["gateway_versions"]) & set(wanted["gateway"]["spoken"])
        minimum = 2 if {account.split("/", 1)[0] for account in in_use} & {"codex", "grok"} else 1
        need(
            any(version >= minimum for version in versions),
            f"in-use accounts require unchanged gateway protocol v{minimum} or newer; use S3",
        )
        self.routes = [enrolled[account] for account in sorted(in_use)]
        need(
            observed["sha256"] in (self.want, self.host["source_sha256"]),
            "unexpected serving image; inspect and re-enroll source",
        )
        need(
            observed["link_sha256"] in (self.want, self.host["source_sha256"]),
            "unexpected stable link; inspect and re-enroll source",
        )
        if (
            prior
            and evidence
            and prior["intent"] == self.intent
            and (
                observed["instance"] == prior["instance"]
                and observed["sha256"] == prior["source_sha256"]
                and observed["sha256"] == observed["link_sha256"]
                and observed["upgrade_idle"]
                and stable_dependencies(observed) == prior["dependencies"]
            )
        ):
            evidence.record(self.intent, "withdrawn", instance=prior["instance"])
            self.report["withdrawn"] = True
            prior = None
        if prior:
            need(
                prior["intent"] == self.intent
                and observed["sha256"] == self.want
                and observed["instance"] != prior["instance"]
                and stable_dependencies(observed) == prior["dependencies"],
                "recorded upgrade outcome is unresolved; inspect the serving coordinator and unit before any new handoff",
                "needs_reconcile",
            )
        else:
            need(
                observed["sha256"] == observed["link_sha256"],
                "serving image and stable link differ; reconcile before applying",
                "needs_reconcile",
            )
        clients = []
        for client in self.config["clients"]:
            current = self.client_call(client, "client_observe")
            if not current["current"]:
                clients.append(client["id"])
        actions = []
        if observed["sha256"] != self.want:
            actions.extend(("baseline", "stage_host"))
        if clients:
            if not actions and not prior:
                actions.append("baseline")
            actions.append("stage_clients")
        if observed["sha256"] != self.want:
            actions.append("host_upgrade")
        if observed["link_sha256"] != self.want or observed["link_sidecars"] != self.sidecars:
            if not actions and not prior:
                actions.append("baseline")
            actions.append("host_link")
        if clients:
            actions.append("client_links")
        actions.extend(("verify",) if actions or prior else ("verify_read_only",))
        self.report["plan"] = actions
        need(observed["canary"] is not None, BOOTSTRAP, "bootstrap_required")
        source = facts["runner_triple"]["source_revision"]
        match(source, REVISION, "serving source revision; enroll a shipping build first")
        if observed["sha256"] != self.want:
            history = (
                git(self.artifacts.repo, "rev-list", self.dest["revision"]).decode().splitlines()
            )
            need(
                source in history,
                "revision downgrade or divergence: choose a descendant of the serving revision",
            )
        missing = {route["account"].split("/", 1)[0] for route in self.routes}.difference(
            observed["canary"]
        )
        need(
            not missing,
            "no bounded deploy canary for "
            + ", ".join(sorted(missing))
            + "; use the manual same-key procedure for this fleet",
        )
        self.report["source_transition"] = (
            prior["source_transition"] if prior else source_transition(observed)
        )
        self.report["canary_models"] = canary_models(self.routes)
        return observed, actions, clients, prior["baseline"] if prior else None

    def health(self, observed, paid, label):
        self.report["step"] = f"{label}_doctor_status_bindings"
        checks = self.host_call(
            "health", {"expected": observed["sha256"], "routes": self.routes}, timeout=150
        )
        generations = checks["generations"]
        if paid:
            checks["canaries"] = {}
            for route in self.routes:
                account = route["account"]
                self.report["step"] = f"{label}_canary_{account}"
                result = self.host_call(
                    "canary", {"expected": observed["sha256"], "route": route}, timeout=125
                )
                need(
                    result["generation_before"] is None
                    or result["generation_after"] >= result["generation_before"],
                    "OAuth generation regressed",
                )
                if generations[account] is not None:
                    need(
                        result["generation_before"] >= generations[account],
                        "OAuth generation regressed",
                    )
                generations[account] = result["generation_after"]
                checks["canaries"][account] = result["adapter"]
        self.report[label] = checks
        return checks

    def stage_host(self):
        files = []
        for name in NAMES:
            facts = self.dest["files"][name]
            files.append(
                (
                    name,
                    self.artifacts.root / name,
                    {"sha256": facts["sha256"], "length": facts["length"]},
                )
            )
        self.host_call(
            "stage_host",
            {
                "manifest": self.artifacts.raw.decode(),
                "manifest_sha256": self.artifacts.pins["host"],
                "installer": self.artifacts.installer(),
            },
            files=files,
            timeout=300,
        )

    def check_receipt(self):
        if self.stage != "production":
            return
        filename = self.config["receipt"]
        missing = "production requires a fresh green staging receipt for these exact manifest digests and routes"
        need(Path(filename).is_file(), missing)
        private_file(filename)
        receipt = decode(local_file(filename, LIMIT))
        age = int(time.time()) - receipt.get("completed_at", 0)
        need(
            receipt.get("schema") == EVIDENCE_SCHEMA
            and receipt.get("stage") == "staging"
            and receipt.get("result") == "green"
            and receipt.get("host_transition") is True
            and manifest_digests(receipt.get("manifests", {}))
            == manifest_digests(self.artifacts.pins)
            and 0 <= age <= 86400
            and receipt.get("stage_identity") != self.report["stage_identity"]
            and receipt.get("source_transition") == self.report["source_transition"]
            and receipt.get("canary_models") == self.report["canary_models"]
            and canary_coverage(receipt),
            missing,
        )

    def apply(self, evidence):
        observed, actions, clients, resumed = self.plan(evidence.rows, evidence)
        if self.report.get("withdrawn"):
            self.report["result"] = "withdrawn"
            return
        if "verify" in actions:
            self.check_receipt()
            if self.stage == "production":
                self.host_call("mark_production", {"expected": observed["stage_identity"]})
                observed["production_history"] = True
        self.report["step"] = "baseline"
        if resumed:
            self.report["baseline"] = resumed
        elif "baseline" in actions:
            for route in self.routes:
                self.host_call("canary_preflight", {"expected": observed["sha256"], "route": route})
            self.health(observed, True, "baseline")
        for step in ("stage_host", "stage_clients"):
            if step not in actions:
                continue
            self.report["step"] = step
            if step == "stage_host":
                self.stage_host()
            else:
                for client in self.config["clients"]:
                    if client["id"] in clients:
                        _, file = self.artifacts.clients[client["id"]]
                        self.client_call(
                            client,
                            "client_stage",
                            files=(("client", client["blob"], file),),
                            timeout=240,
                        )
            evidence.record(self.intent, step)
        if "host_upgrade" in actions:
            self.report["step"] = "destination_unit"
            self.host_call(
                "destination_unit", {"snapshot": self.dest["snapshot_id"], "sha256": self.want}
            )
            self.report["step"] = "destination_canary_preflight"
            for route in self.routes:
                self.host_call(
                    "destination_canary_preflight",
                    {
                        "route": route,
                        "sha256": self.want,
                        "snapshot": self.dest["snapshot_id"],
                    },
                )
            self.upgrade(evidence, observed)
        if "host_link" in actions:
            self.report["step"] = "host_link"
            self.link(
                self.host_call,
                "host_link",
                {
                    "snapshot": self.dest["snapshot_id"],
                    "sha256": self.want,
                    "instance": self.report["observed"]["instance"],
                    "installer": self.artifacts.installer(),
                },
            )
            evidence.record(self.intent, "host_link")
        current = self.observe()
        need(
            current["sha256"] == self.want
            and current["link_sha256"] == self.want
            and current["link_sidecars"] == self.sidecars,
            "target host or link did not converge",
            "needs_reconcile",
        )
        need(
            stable_dependencies(current) == stable_dependencies(observed),
            "gateway/custody/deployment dependencies changed",
        )
        for client in self.config["clients"]:
            if client["id"] in clients:
                self.report["step"] = f"client_link_{client['id']}"
                self.link(
                    self.client_call, client, "client_link", {"connection": connection(current)}
                )
                evidence.record(self.intent, "client_link", client=client["id"])
            self.report["step"] = f"client_check_{client['id']}"
            self.client_call(client, "client_check", {"connection": connection(current)})
        self.health(current, "verify" in actions, "verification")
        for account, generation in self.report.get("baseline", {}).get("generations", {}).items():
            need(
                generation is None
                or self.report["verification"]["generations"][account] >= generation,
                "OAuth generation regressed",
            )
        self.report["host_transition"] = "host_upgrade" in actions or bool(resumed)
        self.report["result"] = "green" if "verify" in actions else "no_op"
        self.report["completed_at"] = int(time.time())
        self.report.pop("step", None)
        if self.stage == "staging" and self.report["host_transition"]:
            publish(self.config["receipt"], self.report)
        evidence.record(self.intent, "verified", result=self.report["result"])

    @staticmethod
    def link(call, *args):
        try:
            call(*args)
        except (Halt, KeyboardInterrupt) as error:
            raise Halt(
                "link outcome is unresolved; use deploy status before continuing", "needs_reconcile"
            ) from error

    def upgrade(self, evidence, observed):
        self.report["step"] = "host_upgrade"
        need(self.observe() == observed, "serving facts changed after BASELINE; replan")
        evidence.record(
            self.intent,
            "upgrade_intent",
            instance=observed["instance"],
            source_sha256=observed["sha256"],
            source_transition=self.report["source_transition"],
            dependencies=stable_dependencies(observed),
            baseline=self.report["baseline"],
        )
        try:
            self.host_call(
                "upgrade",
                {
                    "snapshot": self.dest["snapshot_id"],
                    "sha256": self.want,
                    "previous": observed,
                    "drain": self.config["drain_seconds"],
                },
                timeout=750 + self.config["drain_seconds"],
            )
            current = self.observe()
            need(
                current["sha256"] == self.want and current["instance"] != observed["instance"],
                "missing replacement instance after Ready",
            )
            need(
                stable_dependencies(current) == stable_dependencies(observed),
                "gateway/custody/deployment dependencies changed during handoff",
            )
        except (Halt, KeyboardInterrupt) as error:
            raise Halt(
                "host upgrade outcome is unresolved; use deploy status before any new handoff",
                "needs_reconcile",
            ) from error
        evidence.record(self.intent, "upgrade_ready", instance=current["instance"])


def stable_dependencies(observed):
    return {
        key: observed[key]
        for key in (
            "unit_sha256",
            "custody_sha256",
            "gateway_instance",
            "gateway_versions",
            "stage_identity",
        )
    }


def connection(observed):
    return {"binary_id": observed["sha256"][:12], "wire": observed["facts"]["wire"]}


def manifest_digests(pins):
    return pins.get("host"), set(pins.get("clients", {}).values())


def source_transition(observed):
    facts = observed["facts"]
    return {
        "wire": facts["wire"],
        "coordinator": facts["host_upgrade"],
        "runner_protocol": facts["runner_triple"]["protocol"],
        "gateway_versions": sorted(observed["gateway_versions"]),
    }


def canary_models(routes):
    # Account names may be different in the isolated staging home. Preserve
    # multiplicity, so one named account cannot qualify several production accounts.
    return sorted(
        [route["account"].split("/", 1)[0], route["model"], route["effort"]] for route in routes
    )


def canary_coverage(receipt):
    kinds = sorted(row[0] for row in receipt["canary_models"])
    return all(
        sorted(account.split("/", 1)[0] for account in receipt.get(phase, {}).get("canaries", {}))
        == kinds
        for phase in ("baseline", "verification")
    )


def pending_upgrade(rows):
    prior = None
    for row in rows:
        if row["phase"] == "upgrade_intent":
            prior = row
        elif row["phase"] in ("verified", "withdrawn"):
            prior = None
    return prior


def main(target_script, verb, stage, filename):
    report = {"schema": EVIDENCE_SCHEMA, "stage": stage, "verb": verb, "result": "blocked"}
    report["retry"] = shlex.join(
        ["plakd", "deploy", "status", "--inventory", filename, "--stage", stage]
    )
    try:
        data, selected = inventory(filename, stage)
        if stage == "production":
            staging = data["staging"]
            if staging:
                need(
                    selected["evidence"] != staging["evidence"],
                    "staging and production evidence paths must differ",
                )
                need(
                    selected["receipt"] == staging["receipt"],
                    "production must name the staging receipt",
                )
        if verb != "apply":
            rows = Evidence.read(selected["evidence"])
            if verb == "status" and rows:
                report["last_evidence"] = rows[-1]
        artifacts = Artifacts(data, selected)
        report.update(revision=data["revision"], manifests=artifacts.pins)
        deployment = Deploy(SSH(target_script), selected, artifacts, stage, report)
        if verb == "apply":
            report["operation"] = str(uuid.uuid4())
            with Evidence(selected["evidence"]) as evidence:
                try:
                    deployment.apply(evidence)
                except Halt as error:
                    evidence.record(
                        deployment.intent, "halt", step=report.get("step"), result=error.state
                    )
                    raise
        elif verb == "status" and pending_upgrade(rows):
            with Evidence(selected["evidence"]) as evidence:
                deployment.plan(evidence.rows, evidence)
                report["last_evidence"] = evidence.rows[-1]
            report["result"] = "ready"
        else:
            deployment.plan(rows)
            report["result"] = "ready"
        report.pop("step", None)
    except Halt as error:
        report.update(result=error.state, reason=str(error))
    except (
        KeyError,
        TypeError,
        ValueError,
        OSError,
        subprocess.SubprocessError,
        KeyboardInterrupt,
    ):
        report.update(
            result="blocked",
            reason="invalid, unavailable or interrupted deploy input; inspect inventory, evidence and target",
        )
    print(encode(report).decode())
    return 0 if report["result"] in ("ready", "green", "no_op", "withdrawn") else 1


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
