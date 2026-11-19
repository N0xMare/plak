"""A real, isolated credential gateway for product process fixtures."""

from contextlib import contextmanager
import json
import subprocess
import tempfile
import time
from pathlib import Path


@contextmanager
def running_gateway(binary, home, cwd, environment):
    # Keep Unix socket names short even when the fixture itself is nested.
    with tempfile.TemporaryDirectory(prefix="gw-") as directory, tempfile.TemporaryFile() as log:
        socket = Path(directory).resolve() / "g.sock"
        (home / "gateway.toml").write_text(f"socket = {json.dumps(str(socket))}\n")
        config = f"\n[gateway]\naddress = {json.dumps(f'unix:{socket}')}\n"
        (home / "config.toml").write_text(config)
        gateway = subprocess.Popen(
            [binary, "gateway", "serve"],
            cwd=cwd,
            env=dict(environment, PLAK_TEST_FAULTS="fault=connect"),
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
                gateway.kill()
            gateway.wait(timeout=10)
