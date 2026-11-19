"""Run the target driver against disposable systemctl, proc images and Unix sockets."""

import contextlib
import hashlib
import json
import socket
import sys
import tempfile
import threading
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / "bin/plakd/src/deploy/target.py"
KEY = "r28.e39.p39.t11"
INSTANCE = "100-0000000000000001"
GATEWAY = "00000000-0000-0000-0000-000000000001"


def write_program(path, source):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!{sys.executable}\n" + source)
    path.chmod(0o700)


@contextlib.contextmanager
def socket_server(path, document, *, hello=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(str(path))
        server.listen()
        server.settimeout(0.1)
        stop = threading.Event()
        failures = []

        def serve():
            while not stop.is_set():
                try:
                    peer, _ = server.accept()
                except TimeoutError:
                    continue
                with peer:
                    try:
                        if hello:
                            with peer.makefile("rb") as reader:
                                request = json.loads(reader.readline())
                            assert request["frame"] == "hello" and request["proto"] == 11
                        peer.sendall(json.dumps(document).encode() + b"\n")
                    except (OSError, ValueError, AssertionError) as error:
                        failures.append(error)

        thread = threading.Thread(target=serve)
        thread.start()
        try:
            yield
        finally:
            stop.set()
            thread.join(3)
            assert not thread.is_alive() and not failures, failures


def product_program(key, unit):
    return f"""
import hashlib, json, os, sys
from pathlib import Path
args = sys.argv[1:]
binary = hashlib.sha256(Path(sys.argv[0]).read_bytes()).hexdigest()[:12]
key = {key!r}
home = Path(os.environ["PLAK_HOME"])
socket = home / "gateway.sock"
if args[:3] == ["host", "install", "--dry-run"]:
    print({unit!r}, end="")
elif args == ["version", "--host-upgrade-probe"]:
    print(json.dumps(dict(host_available=True, config_error=None, protocol=3,
      receipt_version=3, binary_id=binary, state_key=key, export_versions=[8],
      runner_protocol=9, runner_version=dict(protocol=9, source_revision="b"*40, flavor="release"))))
elif args == ["version", "--state-key"]:
    print(key)
elif args == ["version", "--export-version"]:
    print(8)
elif args == ["version", "--build-info"]:
    sys.exit(2)
elif args == ["wire", "schema"]:
    print(json.dumps(dict(binary_id=binary, wire_version=11)))
elif args == ["deploy-canary", "--capabilities"]:
    print("plakd-deploy-canary-v1\\nclaude\\ngrok")
elif args == ["gateway", "status"]:
    print("via gateway unix:" + str(socket))
    print("instance {GATEWAY}")
elif args == ["gateway", "check"]:
    print("via gateway unix:" + str(socket))
    print("gateway {GATEWAY} protocol 1,2")
    print("account claude/work\\naccount claude/spare")
elif args == ["status"]:
    print("via gateway unix:" + str(socket))
    print("claude/work: credential=ready\\nclaude/spare: credential=signed-out")
elif args == ["status", "claude/work"]:
    print("claude/work: credential=ready")
elif args == ["deploy-credential-generation", "claude/work"]:
    print(18)
elif args == ["doctor"]:
    print("layout\\tOK")
elif args == ["session", "check-binding", "--all", "--summary"]:
    print(json.dumps(dict(compatible=True, count=200_000)))
elif args[:2] == ["host", "upgrade"]:
    (home / "surrendered").touch()
else:
    sys.exit(8)
"""


def release(releases, product):
    root = releases / "unpublished"
    root.mkdir(parents=True, exist_ok=True)
    write_program(root / "plakd", product)
    for name in ("plakous-js-host", "plakous-py-host"):
        (root / name).write_text(name)

    def digest(value):
        return hashlib.sha256(value).hexdigest()

    snapshot = digest(
        "".join(
            f"{name} {digest((root / name).read_bytes())}\n"
            for name in ("plakd", "plakous-js-host", "plakous-py-host")
        ).encode()
    )
    path = releases / snapshot
    root.rename(path)
    return path / "plakd", digest((path / "plakd").read_bytes())


def test_target():
    target = types.ModuleType("deploy_target_under_test")
    exec(compile(SOURCE.read_bytes(), str(SOURCE), "exec"), target.__dict__)
    with tempfile.TemporaryDirectory(prefix="dt") as directory:
        home = Path(directory).resolve()
        product = home / "p"
        releases = home / ".local/lib/plak/release"
        source, sha = release(releases, product_program(KEY, "generated\n"))
        drifting, drift_sha = release(releases, product_program(KEY, "different\n"))
        cross_key, cross_sha = release(releases, product_program("r99.e39.p39.t11", "generated\n"))
        gateway = home / "gateway"
        gateway.write_bytes(b"test gateway image")
        unit = home / ".config/systemd/user/plakd-host.service"
        unit.parent.mkdir(parents=True)
        unit.write_text("generated\n")
        link = home / ".local/bin/plakd"
        link.parent.mkdir(parents=True)
        link.symlink_to(source)
        proc = home / "proc"
        for pid, image in [("100", source), ("101", gateway)]:
            path = proc / pid / "exe"
            path.parent.mkdir(parents=True)
            path.symlink_to(image)
        target.proc_image = lambda pid: proc / pid / "exe"
        real_contents = target.contents
        target.contents = lambda path: (
            b"0" * 32 + b"\n" if path == "/etc/machine-id" else real_contents(path)
        )
        properties = {
            "plakd-host.service": {
                "ActiveState": "active",
                "MainPID": "100",
                "FragmentPath": str(unit),
                "DropInPaths": "",
                "NeedDaemonReload": "no",
            },
            "gateway.service": {
                "ActiveState": "active",
                "MainPID": "101",
                "NeedDaemonReload": "no",
                "ExecStart": "{ path="
                + str(gateway)
                + " ; argv[]="
                + str(gateway)
                + " gateway serve ; }",
            },
        }
        manager_facts = home / "unit-properties.json"
        manager_facts.write_text(json.dumps(properties))
        write_program(
            home / ".local/bin/systemctl",
            f"""
import json, sys
from pathlib import Path
assert sys.argv[1:3] == ["--user", "show"]
fields = json.loads(Path({str(manager_facts)!r}).read_text())[sys.argv[3]]
for name in sys.argv[4:]:
    key = name.removeprefix("--property=")
    print(key + "=" + fields[key])
""",
        )
        identity = dict(protocol=3, key=KEY, instance=INSTANCE, binary=sha[:12], deploy_idle=True)
        welcome = dict(frame="welcome", proto=11, binary_id=sha[:12], state_key=KEY)
        spec = dict(
            product_home=str(product),
            web_socket=str(product / "web.sock"),
            ui=str(home / "ui"),
            gateway_socket=str(product / "gateway.sock"),
            gateway_unit="gateway.service",
            gateway_sha256=hashlib.sha256(gateway.read_bytes()).hexdigest(),
        )
        (product / "host/run").mkdir(parents=True)
        with contextlib.ExitStack() as stack:
            stack.enter_context(socket_server(product / "host/upgrade.sock", identity))
            stack.enter_context(
                socket_server(product / "host/run" / (KEY + ".sock"), welcome, hello=True)
            )
            for name in ("web.sock", "gateway.sock"):
                endpoint = stack.enter_context(socket.socket(socket.AF_UNIX))
                endpoint.bind(str(product / name))
            host = target.Host(home, spec)
            observed = host.observe()[1]
            assert observed["accounts"] == {"claude/work": "ready", "claude/spare": "not_enrolled"}
            assert observed["upgrade_idle"] and observed["facts"]["source"] == "legacy facts"
            health = host.health(sha, [{"account": "claude/work"}])
            assert health["binding_count"] == 200_000 and health["generations"] == {
                "claude/work": 18
            }
            try:
                host.destination_unit(drifting.parent.name, drift_sha)
            except target.Refusal as error:
                assert "unit template differs" in str(error)
            else:
                raise AssertionError("drifted candidate would be used")
            try:
                host.upgrade(cross_key.parent.name, cross_sha, 10, observed)
            except target.Refusal as error:
                assert "cross-key" in str(error)
            else:
                raise AssertionError("cross-key candidate would be used")
            assert not (product / "surrendered").exists()
            properties["gateway.service"]["ExecStart"] = (
                "{ path=" + str(source) + " ; argv[]=wrong ; }"
            )
            manager_facts.write_text(json.dumps(properties))
            try:
                host.observe()
            except target.Refusal as error:
                assert "ExecStart does not resolve" in str(error)
            else:
                raise AssertionError("drifted gateway unit would be used")


if __name__ == "__main__":
    test_target()
