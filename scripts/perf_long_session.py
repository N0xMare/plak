#!/usr/bin/env python3
"""Long-session persistence measurement over the scripted model.

Drives `plak agent` through TURNS prompts of CALLS loopback tool calls each
(two tool events per call: running, then done; the scripted binding allows 32
model rounds per turn) and reports, as one JSON line:

- the per-tool-event interval distribution, overall and for the first and last
  window of events, so growth over the session is visible;
- the whole run's wall time;
- product.sqlite3 size (with its WAL) after a clean quit;
- when FSYNC_TRACE=1 on Linux, the fsync/fdatasync count under strace.

Usage: perf_long_session.py <plak-binary> <scratch-root> [--turns N] [--calls N]
"""

import argparse
import glob
import json
import os
import shutil
import statistics
import subprocess
import sys
import time


def percentile(values, fraction):
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def write_home(root):
    home = os.path.join(root, "home")
    os.makedirs(os.path.join(home, "agents"), exist_ok=True)
    for sub in ("ws", "tmp", "fake-home"):
        os.makedirs(os.path.join(root, sub), exist_ok=True)
    with open(os.path.join(home, "config.toml"), "w") as handle:
        handle.write('[root]\nagent = "lead"\n\n[orchestration]\nsubagents = false\n')
    with open(os.path.join(home, "agents", "lead.toml"), "w") as handle:
        handle.write(
            'account = "codex"\nmodel = "offline-loopback-v1"\neffort = "medium"\n'
            'composition = "direct"\ndescription = "long-session perf lead"\n'
        )
    os.chmod(home, 0o700)
    os.chmod(os.path.join(home, "agents"), 0o700)
    return home


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("plak")
    parser.add_argument("root")
    parser.add_argument("--turns", type=int, default=34)
    parser.add_argument("--calls", type=int, default=30)
    args = parser.parse_args()

    root = os.path.abspath(args.root)
    shutil.rmtree(root, ignore_errors=True)
    home = write_home(root)
    steps = []
    for _ in range(args.turns):
        steps += [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}},
             "finish": "tool_calls"}
        ] * args.calls
        steps.append({"output": {"text": "turn done"}})
    script = os.path.join(root, "long.json")
    with open(script, "w") as handle:
        json.dump({"version": 1, "steps": steps}, handle)

    env = {"PATH": os.environ["PATH"], "HOME": os.path.join(root, "fake-home"),
           "TMPDIR": os.path.join(root, "tmp"), "PLAK_HOME": home,
           "PLAK_TEST_MODEL": "offline-loopback-v1", "PLAK_TEST_SCRIPT": script}
    command = [args.plak, "agent"]
    trace = os.path.join(root, "strace.txt")
    if os.environ.get("FSYNC_TRACE") == "1":
        command = ["strace", "-f", "-c", "-o", trace,
                   "-e", "trace=fsync,fdatasync,msync,sync_file_range"] + command

    child = subprocess.Popen(command, env=env, cwd=os.path.join(root, "ws"),
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
    for line in child.stdout:
        if json.loads(line).get("kind") == "ready":
            break
    began = time.monotonic()
    stamps = []
    for turn in range(args.turns):
        child.stdin.write(json.dumps({"v": 1, "kind": "prompt", "text": f"go {turn}"}) + "\n")
        child.stdin.flush()
        for line in child.stdout:
            record = json.loads(line)
            if record.get("kind") == "tool":
                stamps.append(time.monotonic())
            if record.get("kind") == "turn_lifecycle" and record.get("phase") != "admitted":
                if record.get("phase") != "completed":
                    raise SystemExit(f"perf long session: turn {turn} ended {record}")
                break
    wall = time.monotonic() - began
    child.stdin.write('{"v":1,"kind":"quit"}\n')
    child.stdin.flush()
    child.stdin.close()
    child.wait(timeout=120)

    intervals = [(b - a) * 1000.0 for a, b in zip(stamps, stamps[1:])]
    window = max(1, min(200, len(intervals) // 5))
    out = {
        "tool_events": len(stamps),
        "wall_s": round(wall, 3),
        "event_ms_p50": round(percentile(intervals, 0.5), 3),
        "event_ms_p90": round(percentile(intervals, 0.9), 3),
        "event_ms_p99": round(percentile(intervals, 0.99), 3),
        "event_ms_max": round(max(intervals), 3),
        "first_window_ms_median": round(statistics.median(intervals[:window]), 3),
        "last_window_ms_median": round(statistics.median(intervals[-window:]), 3),
    }
    databases = glob.glob(os.path.join(home, "state", "*", "product.sqlite3"))
    if databases:
        size = 0
        for suffix in ("", "-wal", "-shm"):
            path = databases[0] + suffix
            if os.path.exists(path):
                size += os.path.getsize(path)
        out["product_sqlite_bytes"] = size
    if os.path.exists(trace):
        calls = 0
        for line in open(trace):
            parts = line.split()
            if parts and parts[-1] in ("fsync", "fdatasync", "msync", "sync_file_range"):
                calls += int(parts[3]) if len(parts) >= 6 else int(parts[-2])
        out["fsync_calls"] = calls
    print(json.dumps(out, sort_keys=True))


if __name__ == "__main__":
    sys.exit(main())
