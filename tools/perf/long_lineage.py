#!/usr/bin/env python3
"""Informational release-product timings with 400 and 1000 retained prompts."""

import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import threading
import time

from perf_long_session import write_home


def measure(binary: str, root: Path, machine: str, profile: str, prompts: int) -> None:
    home = write_home(str(root))
    script = root / "script.json"
    script.write_text(json.dumps({
        "version": 1,
        "steps": [{"output": {"text": "ok"}}] * prompts
                 + [{"output": "history_report"}],
    }))
    env = {"PATH": os.environ["PATH"], "HOME": str(root / "fake-home"),
           "TMPDIR": str(root / "tmp"), "PLAK_HOME": home,
           "PLAK_TEST_MODEL": "offline-loopback-v1", "PLAK_TEST_SCRIPT": str(script)}
    with (root / "stderr.log").open("w") as stderr:
        child = subprocess.Popen([binary, "agent"], env=env, cwd=root / "ws",
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=stderr, text=True)
        # Bound a missing stdout record too, not just a successful child.wait.
        watchdog = threading.Timer(600, child.kill)
        watchdog.start()
        try:
            for line in child.stdout:
                if json.loads(line).get("kind") == "ready":
                    break
            samples = []
            history = ""
            for ordinal in range(prompts + 1):
                began = time.monotonic()
                child.stdin.write(json.dumps({"v": 1, "kind": "prompt",
                                              "text": f"lineage-prompt-{ordinal:04}"}) + "\n")
                child.stdin.flush()
                for line in child.stdout:
                    event = json.loads(line)
                    if ordinal == prompts and event.get("kind") == "assistant":
                        history += event.get("text", "")
                    if event.get("kind") == "turn_lifecycle" and event.get("phase") != "admitted":
                        if event.get("phase") != "completed":
                            raise RuntimeError(f"lineage turn {ordinal}: {event}")
                        break
                else:
                    raise RuntimeError("lineage process exited before its terminal record")
                samples.append((time.monotonic() - began) * 1000)
            for ordinal in range(prompts):
                if f"lineage-prompt-{ordinal:04}" not in history:
                    raise RuntimeError(f"request omitted carried prompt {ordinal}")
            child.stdin.write('{"v":1,"kind":"quit"}\n')
            child.stdin.flush()
            child.stdin.close()
            if child.wait(timeout=30):
                raise RuntimeError("lineage process failed at quit")
        finally:
            watchdog.cancel()
            if child.poll() is None:
                child.kill()
            child.wait()
    for label, value in (("total", sum(samples)),
                         ("first_20_median", statistics.median(samples[:20])),
                         ("last_20_median", statistics.median(samples[-21:-1]))):
        print(json.dumps({"metric": f"lineage_{prompts}_{label}_ms", "value": round(value, 3),
                          "unit": "ms", "machine": machine, "profile": profile}, sort_keys=True), flush=True)


if __name__ == "__main__":
    for size in (400, 1000):
        measure(str(Path(sys.argv[1]).resolve()), Path(sys.argv[2]) / str(size),
                sys.argv[3], sys.argv[4], size)
