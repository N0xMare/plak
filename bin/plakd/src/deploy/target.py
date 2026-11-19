"""Closed same-user SSH operations for the private deploy CLI."""

import contextlib
import hashlib
import json
import os
import platform
import pwd
import re
import socket
import stat
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

LIMIT = 1024 * 1024
NAMES = ("plakd", "plakous-js-host", "plakous-py-host")
UNIT = "plakd-host.service"
DIGEST = r"[a-f0-9]{64}"
INSTANCE = r"[1-9][0-9]*-[a-f0-9]{16}"
OAUTH = {"codex", "grok", "claude"}
RECIPES = ("install.sh", "tools/build/artifact_manifest.py")
NORMALIZE = (
    "host unit is not the generated stable-link unit: verify ~/.local/bin/plakd "
    "resolves to the running snapshot, move the pinned ExecStart drop-in aside, "
    "then run systemctl --user daemon-reload; do not restart"
)


class Refusal(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Refusal(message)


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest_file(path):
    with Path(path).open("rb") as stream:
        require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "expected a regular image")
        return hashlib.file_digest(stream, "sha256").hexdigest()


def contents(path):
    with Path(path).open("rb") as stream:
        require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "expected a regular file")
        data = stream.read(LIMIT + 1)
    require(len(data) <= LIMIT, "target input exceeds its bound")
    return data


def command(args, env, label, timeout=30, cwd=None, allow_failure=False):
    try:
        with tempfile.TemporaryFile() as output:
            process = subprocess.run(
                args,
                env=env,
                cwd=cwd,
                stdin=subprocess.DEVNULL,
                stdout=output,
                stderr=subprocess.DEVNULL,
                timeout=timeout,
                check=False,
            )
            output.seek(0)
            data = output.read(LIMIT + 1)
    except (OSError, subprocess.SubprocessError) as error:
        raise Refusal(f"{label} did not return a bounded verdict") from error
    require(len(data) <= LIMIT, f"{label} output exceeds its bound")
    if allow_failure:
        return process.returncode, data
    require(process.returncode == 0, f"{label} failed; inspect the named command on the target")
    return data


def one_line(data):
    require(data.endswith(b"\n") and data.count(b"\n") == 1, "expected one product reply line")
    return data[:-1].decode()


def read_frame(reader):
    line = reader.readline(LIMIT + 1)
    require(len(line) <= LIMIT and line.endswith(b"\n"), "incomplete service identity frame")
    return json.loads(line)


def wire_welcome(product, build, args):
    endpoint = product / "host/run" / (build["state_key"] + ".sock")
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(endpoint))
        connection.sendall(
            encoded(
                {
                    "frame": "hello",
                    "proto": build["wire"]["number"],
                    "binary_id": build["binary_id"],
                    "caps": [],
                }
            )
            + b"\n"
        )
        with connection.makefile("rb") as reader:
            frame = read_frame(reader)
        require(
            frame["frame"] == "welcome"
            and frame["binary_id"] == build["binary_id"]
            and frame["state_key"] == build["state_key"]
            and frame["proto"] == build["wire"]["number"],
            "host wire Welcome does not identify the serving coordinator",
        )
    require(
        stat.S_ISSOCK(Path(args["web_socket"]).stat().st_mode),
        "configured host web socket is not serving",
    )


def service_identity(product):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(product / "host/upgrade.sock"))
        with connection.makefile("rb") as reader:
            value = read_frame(reader)
    require(re.fullmatch(INSTANCE, value["instance"]), "invalid coordinator instance")
    return value


def validate_identity(node):
    require(
        platform.system() == "Linux" and platform.machine() == "x86_64",
        "target must be Linux x86_64",
    )
    require(os.geteuid() == node["uid"] and os.geteuid() != 0, "unexpected SSH effective UID")
    home = Path(node["home"])
    require(
        home == home.resolve(strict=True) == Path.home() == Path(pwd.getpwuid(os.geteuid()).pw_dir),
        "unexpected SSH home",
    )
    require(home.stat().st_uid == os.geteuid(), "unexpected SSH home owner")
    return home


def environment(home, product):
    runtime = f"/run/user/{os.geteuid()}"
    return {
        "HOME": str(home),
        "PLAK_HOME": str(product),
        "PATH": f"{home}/.local/bin:/usr/local/bin:/usr/bin:/bin",
        "XDG_RUNTIME_DIR": runtime,
        "DBUS_SESSION_BUS_ADDRESS": f"unix:path={runtime}/bus",
        "LANG": "C.UTF-8",
    }


def image(path, sha256):
    require(digest_file(path) == sha256, "target file digest differs")


def unit_properties(env, unit, names):
    result = command(
        ["systemctl", "--user", "show", unit, *(f"--property={name}" for name in names)],
        env,
        "systemctl --user show",
    )
    return dict(line.split("=", 1) for line in result.decode().splitlines())


def active_pid(properties):
    require(properties["ActiveState"] == "active", "expected an active user service")
    require(re.fullmatch(r"[1-9][0-9]*", properties["MainPID"]), "missing systemd service MainPID")
    return properties["MainPID"]


def build_facts(binary, env, product):
    probe = json.loads(
        command(
            [binary, "version", "--host-upgrade-probe"],
            env,
            "version --host-upgrade-probe",
            cwd=product,
        )
    )
    require(
        probe["host_available"] is True and probe["config_error"] is None,
        "serving image cannot qualify its host config",
    )
    rc, raw = command(
        [binary, "version", "--build-info"], env, "version --build-info", allow_failure=True
    )
    label = "build_info"
    wire = json.loads(command([binary, "wire", "schema"], env, "wire schema"))
    require(
        wire.pop("binary_id") == probe["binary_id"],
        "wire schema names a different installed binary",
    )
    info = {
        "state_key": probe["state_key"],
        "binary_id": probe["binary_id"],
        "host_upgrade": {"coordinator": probe["protocol"], "receipt": probe["receipt_version"]},
        "runner_triple": probe["runner_version"],
        "wire": {
            "number": wire["wire_version"],
            "schema_pin": hashlib.sha256(encoded(wire)).hexdigest(),
        },
        "export": {
            "read": probe["export_versions"],
            "write": int(
                one_line(
                    command(
                        [binary, "version", "--export-version"],
                        env,
                        "version --export-version",
                    )
                )
            ),
        },
    }
    require(
        probe["runner_protocol"] == info["runner_triple"]["protocol"], "inconsistent runner probe"
    )
    require(
        info["state_key"]
        == one_line(command([binary, "version", "--state-key"], env, "version --state-key")),
        "inconsistent state-key probe",
    )
    require(info["export"]["write"] in info["export"]["read"], "inconsistent export probe")
    if rc == 0:
        manifest = json.loads(raw)
        for key in ("state_key", "wire", "host_upgrade", "export"):
            require(
                manifest[key] == info[key], "build-info disagrees with independent product probes"
            )
        require(
            info["runner_triple"]
            == {
                "protocol": manifest["runner"]["protocol"],
                "source_revision": manifest["revision"],
                "flavor": manifest["flavor"],
            },
            "build-info disagrees with serving runner triple",
        )
    else:
        # A qualified legacy image exposes all facts above without build-info.
        # No unobserved capabilities are filled in.
        label = "legacy facts"
    return {"source": label, **info}


class Host:
    def __init__(self, home, spec):
        self.home = home
        self.spec = spec
        self.product = Path(spec["product_home"])
        self.env = environment(home, self.product)
        self.link = home / ".local/bin/plakd"
        require(
            self.product.resolve(strict=True) == self.product
            and self.product.stat().st_uid == os.geteuid(),
            "unexpected product home",
        )

    def plaid(self, binary, *args, **options):
        return command(
            [binary, *args],
            self.env,
            " ".join(str(arg) for arg in args[:2]),
            cwd=self.home,
            **options,
        )

    def gateway(self, binary):
        socket_path = self.spec["gateway_socket"]
        custody = f"via gateway unix:{socket_path}"
        status = self.plaid(binary, "gateway", "status").decode().splitlines()
        require(
            custody in status,
            "custody change: restore the enrolled local gateway; use the custody runbook",
        )
        instance = [line.split(" ", 1)[1] for line in status if line.startswith("instance ")]
        require(
            len(instance) == 1 and re.fullmatch(r"[a-f0-9-]{36}", instance[0]),
            "gateway instance is unavailable",
        )
        check = self.plaid(binary, "gateway", "check").decode().splitlines()
        protocols = [
            re.fullmatch(rf"gateway {instance[0]} protocol ([1-9][0-9]*(?:,[1-9][0-9]*)*)", row)
            for row in check
        ]
        accepted = [[int(version) for version in row[1].split(",")] for row in protocols if row]
        require(
            len(accepted) == 1 and custody in check,
            "cannot prove unchanged gateway holder/protocol",
        )
        properties = unit_properties(
            self.env, self.spec["gateway_unit"], ("ActiveState", "MainPID")
        )
        process = Path(f"/proc/{active_pid(properties)}/exe")
        require(
            digest_file(process) == self.spec["gateway_sha256"],
            "gateway image change: restore the enrolled gateway or use S3",
        )
        gateway_socket = Path(socket_path)
        require(
            stat.S_ISSOCK(gateway_socket.stat().st_mode),
            "custody change: enrolled gateway socket is not local",
        )
        config = [self.product / "config.toml", self.product / "gateway.toml"]
        stat_socket = gateway_socket.stat()
        custody_sha = hashlib.sha256(
            encoded(
                [
                    [digest_file(file) if file.exists() else None for file in config],
                    stat_socket.st_dev,
                    stat_socket.st_ino,
                    properties["MainPID"],
                ]
            )
        ).hexdigest()
        return instance[0], accepted[0], custody_sha

    def observe(self):
        props = unit_properties(
            self.env,
            UNIT,
            ("ActiveState", "MainPID", "FragmentPath", "DropInPaths", "NeedDaemonReload"),
        )
        expected_unit = self.home / ".config/systemd/user" / UNIT
        require(
            props["FragmentPath"] == str(expected_unit)
            and not props["DropInPaths"]
            and props["NeedDaemonReload"] == "no"
            and expected_unit.is_file()
            and not expected_unit.is_symlink(),
            NORMALIZE,
        )
        pid = active_pid(props)
        process = Path(f"/proc/{pid}/exe")
        binary = process.resolve(strict=True)
        roots = {self.home / ".local/lib/plak" / flavor for flavor in ("maintainer", "release")}
        require(
            binary.name == "plakd" and binary.parent.parent in roots,
            "host image is not an enrolled installed snapshot",
        )
        link = self.link.resolve(strict=True)
        require(
            self.link.is_symlink() and link.name == "plakd" and link.parent.parent in roots,
            NORMALIZE,
        )
        sha256 = digest_file(process)
        image(binary, sha256)
        sidecars = {}
        for name in NAMES[1:]:
            worker = self.link.with_name(name)
            if not worker.is_symlink():
                continue
            resolved = worker.resolve(strict=True)
            sidecars[name] = digest_file(resolved)
        unit = self.plaid(
            binary,
            "host",
            "install",
            "--dry-run",
            "--web",
            "unix:" + self.spec["web_socket"],
            "--ui",
            self.spec["ui"],
        )
        require(unit == contents(expected_unit), NORMALIZE)
        facts = build_facts(binary, self.env, self.product)
        identity = service_identity(self.product)
        require(
            identity["binary"] == sha256[:12] == facts["binary_id"]
            and identity["instance"].split("-", 1)[0] == pid
            and identity["key"] == facts["state_key"]
            and identity["protocol"] == facts["host_upgrade"]["coordinator"],
            "systemd image and coordinator identity differ",
        )
        wire_welcome(self.product, facts, self.spec)
        instance, protocol, custody = self.gateway(binary)
        config_path = self.product / "config.toml"
        config = tomllib.loads(contents(config_path).decode()) if config_path.exists() else {}
        canary = self.canary_routes(binary)
        require(
            service_identity(self.product) == identity and digest_file(process) == sha256,
            "host changed during observation",
        )
        return binary, {
            "sha256": sha256,
            "link_sha256": digest_file(link),
            "link_sidecars": sidecars,
            "instance": identity["instance"],
            "facts": facts,
            "unit_sha256": hashlib.sha256(unit).hexdigest(),
            "gateway_instance": instance,
            "gateway_versions": protocol,
            "custody_sha256": custody,
            "paired": bool(config.get("runners")),
            "canary": canary,
        }

    def current(self, expected):
        binary, observed = self.observe()
        require(observed["sha256"] == expected, "serving host changed; replan")
        return binary

    def generations(self, binary, routes):
        values = {}
        for route in routes:
            account = route["account"]
            kind = account.split("/", 1)[0]
            status = self.plaid(binary, "status", account).decode().splitlines()
            require(
                any(
                    re.match(rf"{re.escape(account)}: .*\bcredential=ready\b", line)
                    for line in status
                ),
                f"credential not Ready for {kind}",
            )
            values[kind] = None
            if kind in OAUTH:
                # Read through the gateway facts probe, not the credential file.
                generation = one_line(self.plaid(binary, "deploy-credential-generation", account))
                require(
                    generation.isascii() and generation.isdecimal(),
                    "unavailable OAuth credential generation",
                )
                values[kind] = int(generation)
        return values

    def health(self, expected, routes):
        binary = self.current(expected)
        doctor = self.plaid(binary, "doctor", timeout=45).decode()
        require(
            doctor.strip() and not re.search(r"\tERROR(?:\t|$)", doctor, re.MULTILINE),
            "doctor reported an error",
        )
        status = self.plaid(binary, "status").decode().splitlines()
        require(
            f"via gateway unix:{self.spec['gateway_socket']}" in status,
            "status does not affirm enrolled custody",
        )
        bindings = self.plaid(binary, "session", "check-binding", "--all", timeout=75).splitlines()
        require(
            bindings and json.loads(bindings[-1])["compatible"] is True,
            "session check-binding --all is not compatible",
        )
        return {
            "doctor": "pass",
            "status": "pass",
            "bindings": "pass",
            "generations": self.generations(binary, routes),
        }

    def canary_routes(self, binary):
        code, reply = self.plaid(binary, "deploy-canary", "--capabilities", allow_failure=True)
        if code != 0:
            return None
        version, *routes = reply.decode().splitlines()
        require(
            version == "plakd-deploy-canary-v1"
            and all(re.fullmatch(r"[a-z-]+", route) for route in routes),
            "unrecognized canary capability verdict",
        )
        return routes

    def qualify_canary(self, binary, route):
        routes = self.canary_routes(binary)
        require(
            routes is not None,
            "candidate has no bounded canary; build an S2 or later revision",
        )
        require(route["account"].split("/", 1)[0] in routes, "route has no bounded canary adapter")
        self.plaid(binary, "deploy-canary", "--preflight", *route_arguments(route), timeout=30)

    def canary_preflight(self, expected, route):
        binary = self.current(expected)
        self.qualify_canary(binary, route)
        return binary

    def destination_canary_preflight(self, snapshot, sha256, route):
        self.qualify_canary(self.snapshot(snapshot, sha256), route)
        return {}

    def canary(self, expected, route):
        binary = self.canary_preflight(expected, route)
        before = next(iter(self.generations(binary, [route]).values()))
        verdict = self.plaid(binary, "deploy-canary", *route_arguments(route), timeout=105)
        require(verdict == b"plakd-deploy-canary-pass\n", "invalid canary terminal/cleanup verdict")
        after = next(iter(self.generations(binary, [route]).values()))
        return {"adapter": "bounded_cli_v1", "generation_before": before, "generation_after": after}

    def snapshot(self, snapshot, sha256):
        require(re.fullmatch(DIGEST, snapshot), "invalid snapshot")
        root = self.home / ".local/lib/plak/release" / snapshot
        require(root.resolve(strict=True) == root, "snapshot path is not immutable")
        binary = root / "plakd"
        require(not binary.is_symlink(), "unexpected staged image link")
        image(binary, sha256)
        facts = "".join(f"{name} {digest_file(root / name)}\n" for name in NAMES)
        require(
            hashlib.sha256(facts.encode()).hexdigest() == snapshot, "staged snapshot digest differs"
        )
        return binary

    def stage_host(self, files, manifest, manifest_sha256, installer):
        require(
            hashlib.sha256(manifest.encode()).hexdigest() == manifest_sha256,
            "transferred manifest digest differs",
        )
        with tempfile.TemporaryDirectory() as stage:
            root = Path(stage)
            require(set(files) == set(NAMES), "incorrect shipping transfer")
            for name, value in files.items():
                os.replace(value, root / name)
            (root / "manifest.json").write_text(manifest)
            script = materialize_recipes(root, installer)
            actual = one_line(
                command(
                    ["sh", script, "install", "--from", root, "--no-link"],
                    self.env,
                    "install.sh --no-link",
                    timeout=240,
                )
            )
            data = json.loads(manifest)
            binary = self.snapshot(data["snapshot_id"], data["files"]["plakd"]["sha256"])
            require(actual == str(binary.parent), "installer reported a different snapshot")
        return {}

    def upgrade(self, snapshot, sha256, drain, previous):
        binary = self.snapshot(snapshot, sha256)
        require(self.observe()[1] == previous, "serving facts changed before handoff")
        self.plaid(
            binary,
            "host",
            "upgrade",
            "--expect-binary",
            sha256[:12],
            "--drain",
            f"{drain}s",
            timeout=720 + drain,
        )
        return {}

    def host_link(self, snapshot, sha256, instance, installer):
        self.snapshot(snapshot, sha256)
        current = self.observe()[1]
        require(
            current["sha256"] == sha256 and current["instance"] == instance,
            "replacement is no longer Ready",
        )
        with tempfile.TemporaryDirectory() as directory:
            script = materialize_recipes(Path(directory), installer)
            command(["sh", script, "link", snapshot], self.env, "install.sh link")
        require(digest_file(self.link.resolve(strict=True)) == sha256, "target stable link differs")
        return {}


def route_arguments(route):
    return [f"--{key}={value}" for key, value in route.items() if value]


def materialize_recipes(root, recipes):
    require(set(recipes) == set(RECIPES), "installer recipe is incomplete")
    for name, content in recipes.items():
        target = root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
    return root / "install.sh"


def secure_directory(path, home, create=False):
    for parts in reversed(list(path.parents)):
        if parts.is_relative_to(home):
            check_directory(parts, create)
    check_directory(path, create)


def check_directory(path, create):
    if create and not path.exists():
        path.mkdir(mode=0o700)
    info = path.lstat()
    require(
        stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid() and not info.st_mode & 0o022,
        "client path must use owned non-writable-by-others directories",
    )


class Client:
    def __init__(self, home, link, manifest, source_sha256):
        self.home = home
        self.link = Path(link)
        self.env = environment(home, home / ".plak")
        self.file = manifest["file"]
        self.wire = manifest["wire"]
        self.source = source_sha256
        require(
            self.link.parent == home / ".local/bin" and self.link.name not in NAMES,
            "client link must be a dedicated ~/.local/bin link",
        )
        self.blob = home / ".local/lib/plak/clients" / self.file["sha256"] / "client"

    def observe(self):
        secure_directory(self.link.parent, self.home)
        require(
            self.link.is_symlink(), "client link is not enrolled; enroll a dedicated link first"
        )
        resolved = self.link.resolve(strict=True)
        observed = digest_file(resolved)
        require(
            observed in (self.source, self.file["sha256"]),
            "unexpected client link digest; inspect and re-enroll",
        )
        return {"current": observed == self.file["sha256"] and resolved == self.blob}

    def stage(self, files):
        require(set(files) == {"client"}, "incorrect client transfer")
        verified_client(files["client"], self.file)
        secure_directory(self.blob.parent, self.home, create=True)
        if not self.blob.exists():
            with tempfile.NamedTemporaryFile(dir=self.blob.parent, delete=False) as output:
                unpublished = Path(output.name)
                try:
                    with files["client"].open("rb") as source:
                        while block := source.read(LIMIT):
                            output.write(block)
                    output.flush()
                    os.fchmod(output.fileno(), 0o500)
                    os.fsync(output.fileno())
                    verified_client(unpublished, self.file)
                    try:
                        os.link(unpublished, self.blob)
                    except FileExistsError:
                        pass
                finally:
                    unpublished.unlink()
        verified_client(self.blob, self.file)
        return {}

    def activate(self, connection):
        self.observe()
        self.check_image(self.blob, connection)
        with tempfile.TemporaryDirectory(dir=self.link.parent) as directory:
            temporary = Path(directory) / self.link.name
            temporary.symlink_to(self.blob)
            os.replace(temporary, self.link)
        return {}

    def check_image(self, binary, connection):
        verified_client(binary, self.file)
        require(
            connection["wire"] == self.wire,
            "serving host is not an exact wire match for this client",
        )
        verdict = json.loads(
            command(
                [binary, "--check-connection"], self.env, "client --check-connection", cwd=self.home
            )
        )
        require(
            verdict == {"connected": True, **connection},
            "client did not prove its exact host connection",
        )

    def check(self, connection):
        require(self.observe()["current"], "client link did not converge")
        self.check_image(self.link.resolve(strict=True), connection)
        return {}


def verified_client(path, facts):
    require(
        not path.is_symlink() and path.stat().st_size == facts["length"],
        "client blob length or path differs",
    )
    image(path, facts["sha256"])


def receive_files(raw, entries, root):
    received = {}
    for entry in entries:
        name = entry["name"]
        require(name in (*NAMES, "client") and name not in received, "invalid transfer filename")
        require(
            type(entry["length"]) is int and 0 < entry["length"] <= 2**31, "invalid transfer length"
        )
        require(re.fullmatch(DIGEST, entry["sha256"]), "invalid transfer digest")
        path = root / name
        with path.open("xb") as output:
            remaining = entry["length"]
            digest = hashlib.sha256()
            while remaining:
                block = raw.read(min(remaining, LIMIT))
                require(block, "transfer ended before blob")
                remaining -= len(block)
                digest.update(block)
                output.write(block)
        require(digest.hexdigest() == entry["sha256"], "transferred blob digest differs")
        received[name] = path
    require(not raw.read(1), "trailing transfer data")
    return received


def execute(header, raw):
    home = validate_identity(header["node"])
    operation = header["op"]
    args = header["args"]
    with contextlib.ExitStack() as stack:
        if header["files"]:
            require(
                operation in ("stage_host", "client_stage"),
                "read-only operation cannot accept a transfer",
            )
            root = Path(stack.enter_context(tempfile.TemporaryDirectory()))
            files = receive_files(raw, header["files"], root)
        else:
            require(not raw.read(1), "unexpected SSH request body")
            files = {}
        if operation.startswith("client_"):
            client = Client(home, args.pop("link"), args.pop("manifest"), args.pop("source_sha256"))
            if operation == "client_observe":
                return client.observe()
            if operation == "client_stage":
                return client.stage(files)
            if operation == "client_link":
                return client.activate(**args)
            if operation == "client_check":
                return client.check(**args)
        else:
            host = Host(home, args.pop("host"))
            if operation == "observe":
                return host.observe()[1]
            if operation == "health":
                return host.health(**args)
            if operation == "canary_preflight":
                host.canary_preflight(**args)
                return {}
            if operation == "canary":
                return host.canary(**args)
            if operation == "destination_canary_preflight":
                return host.destination_canary_preflight(**args)
            if operation == "stage_host":
                return host.stage_host(files, **args)
            if operation == "upgrade":
                return host.upgrade(**args)
            if operation == "host_link":
                return host.host_link(**args)
        raise Refusal("unknown typed target operation")


def main():
    try:
        raw = sys.stdin.buffer
        reply = {"ok": execute(read_frame(raw), raw)}
    except Refusal as error:
        reply = {"error": str(error)}
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        reply = {"error": "cannot verify target facts or complete target operation"}
    print(encoded(reply).decode())


if __name__ == "__main__":
    main()
