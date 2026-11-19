"""A real, isolated credential gateway for product process fixtures."""

from contextlib import contextmanager
import json
import subprocess
import tempfile
import time
from pathlib import Path


@contextmanager
def running_gateway(binary, home, cwd, environment, *, socket_path=None, faults="fault=connect"):
    # Keep Unix socket names short even when the fixture itself is nested.
    with tempfile.TemporaryDirectory(prefix="gw-") as directory, tempfile.TemporaryFile() as log:
        socket = socket_path or Path(directory).resolve() / "g.sock"
        (home / "gateway.toml").write_text(f"socket = {json.dumps(str(socket))}\n")
        config = f"\n[gateway]\naddress = {json.dumps(f'unix:{socket}')}\n"
        configure_caller(home, socket)
        child_env = dict(environment, TOKIO_WORKER_THREADS="2")
        child_env.pop("PLAK_TEST_FAULTS", None)
        if faults:
            child_env["PLAK_TEST_FAULTS"] = faults
        gateway = subprocess.Popen(
            [binary, "gateway", "serve"],
            cwd=cwd,
            env=child_env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
        try:
            deadline = time.monotonic() + 15
            while True:
                checked = subprocess.run(
                    [binary, "gateway", "check"],
                    cwd=cwd,
                    env=environment,
                    capture_output=True,
                    timeout=10,
                    check=False,
                )
                if checked.returncode == 0:
                    break
                if gateway.poll() is not None or time.monotonic() >= deadline:
                    log.seek(0)
                    raise RuntimeError(
                        f"fixture gateway did not become ready: {log.read().decode()}"
                    )
                time.sleep(0.02)
            yield config
        finally:
            if gateway.poll() is None:
                gateway.terminate()
            try:
                gateway.wait(timeout=35)
            except subprocess.TimeoutExpired:
                gateway.kill()
                gateway.wait(timeout=5)


def configure_caller(home, socket):
    path = home / "config.toml"
    existing = path.read_text() if path.exists() else ""
    config = f"\n[gateway]\naddress = {json.dumps(f'unix:{socket}')}\n"
    if config in existing:
        return
    if "[gateway]" in existing:
        raise RuntimeError("fixture caller already names a different gateway")
    path.write_text(existing + config)
