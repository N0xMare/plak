#!/usr/bin/env bash
# Offline performance smoke over the scripted model (`plak/test-hooks`).
#
# Six measurements of the real binary, each emitted as one JSON line carrying a
# machine id, so a number from one machine is never compared with a number from
# another. `tests/performance/baselines/offline-perf-smoke.json` holds the seed
# values. `--check` compares four of them against the recorded tolerance:
# transcript layout time, growth-turn peak RSS, bulk throughput and the
# two-tool turn, all of which sit at or under 6% run-to-run spread on an idle
# host. Cold start and the tool round trip print as informational, because
# their measured spread (12.6% and 38.9%) leaves no tolerance worth defending.
# The round trip is the median of five repeats. Wall time on a shared machine
# is evidence, not a gate: `just check` runs this informationally. See spec 05
# §2.7.
#
# Usage: scripts/perf-smoke.sh [--check] [--out <file>]
set -euo pipefail
cd "$(dirname "$0")/.."

TMPDIR="$(bash scripts/test-tmpdir.sh)"
export TMPDIR

# A `python3` on PATH may be a shim that cannot run (this repository has met
# one). Pick the first candidate that actually executes.
PY=""
for candidate in python3 /opt/homebrew/bin/python3 /usr/local/bin/python3 /usr/bin/python3; do
  if command -v "$candidate" >/dev/null 2>&1 && "$candidate" -c 'import hashlib, json' >/dev/null 2>&1; then
    PY="$candidate"
    break
  fi
done
if [[ -z "$PY" ]]; then
  echo "perf smoke: no usable python3" >&2
  exit 2
fi

check=0
out=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) check=1; shift ;;
    --out) out="$2"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

root="$TMPDIR/perf-smoke"
rm -rf "$root"
mkdir -p "$root/home/agents" "$root/ws" "$root/tmp"
cat >"$root/home/config.toml" <<'TOML'
[root]
agent = "lead"

[orchestration]
subagents = false

[compaction]
enabled = true
TOML
cat >"$root/home/agents/lead.toml" <<'TOML'
account = "codex"
model = "offline-loopback-v1"
effort = "medium"
composition = "direct"
description = "offline performance smoke lead"
TOML
chmod 700 "$root/home" "$root/home/agents"
chmod 600 "$root/home/config.toml" "$root/home/agents/lead.toml"

echo "== building the scripted binary ==" >&2
# The binary comes out of the gate's test build, so this reuses it. `-p plak`
# would resolve features for plak alone, and any `cargo build` leaves
# dev-dependency features off; either compiles a second variant of much of the
# dependency graph.
cargo test --no-run --locked --workspace --all-features >&2
# Copy the executable aside. The configured Cargo target's `debug/plak` is
# shared, and a concurrent build without `test-hooks` would replace it.
cargo_target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
cp "$cargo_target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug/plak" "$root/plak"
plak_bin="$root/plak"

machine="$("$PY" - <<'PY'
import hashlib, platform, subprocess
parts = [platform.system(), platform.machine()]
try:
    parts.append(subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"],
                                capture_output=True, text=True, check=True).stdout.strip())
except Exception:
    try:
        parts.append(open("/proc/cpuinfo").read().split("model name")[1].split("\n")[0])
    except Exception:
        parts.append("unknown-cpu")
print(hashlib.sha256("|".join(parts).encode()).hexdigest()[:12])
PY
)"

run_agent() {
  # $1 = script path, remaining = stdin frames file
  ( cat "$2"; sleep "${3:-6}" ) | env -i \
    PATH="$PATH" HOME="$root/fake-home" TMPDIR="$root/tmp" \
    PLAK_HOME="$root/home" PLAK_TEST_MODEL=offline-loopback-v1 \
    PLAK_TEST_SCRIPT="$1" PLAK_TEST_EVIDENCE="$root/evidence.jsonl" \
    "$plak_bin" agent 2>/dev/null
}

measurements="$root/measurements.jsonl"
: >"$measurements"

emit() {
  "$PY" -c '
import json, sys
print(json.dumps({"metric": sys.argv[1], "value": float(sys.argv[2]),
                  "unit": sys.argv[3], "machine": sys.argv[4]}, sort_keys=True))
' "$1" "$2" "$3" "$machine" >>"$measurements"
}

# ---------------------------------------------------------------------------
# 1. Cold start: process launch to the `ready` record.
# ---------------------------------------------------------------------------
cat >"$root/trivial.json" <<'JSON'
{"version":1,"steps":[{"output":{"text":"ok"}}]}
JSON
printf '' >"$root/empty-frames"
start_ms="$( (cd "$root/ws" && "$PY" - "$plak_bin" "$root" <<'PY'
import json, os, subprocess, sys, time
plak, root = sys.argv[1], sys.argv[2]
env = {"PATH": os.environ["PATH"], "HOME": root + "/fake-home", "TMPDIR": root + "/tmp",
       "PLAK_HOME": root + "/home", "PLAK_TEST_MODEL": "offline-loopback-v1",
       "PLAK_TEST_SCRIPT": root + "/trivial.json",
       "PLAK_TEST_EVIDENCE": root + "/evidence.jsonl"}
began = time.monotonic()
child = subprocess.Popen([plak, "agent"], env=env, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
ready = None
for line in child.stdout:
    if json.loads(line).get("kind") == "ready":
        ready = (time.monotonic() - began) * 1000.0
        break
child.stdin.write('{"v":1,"kind":"quit"}\n'); child.stdin.flush(); child.stdin.close()
child.wait(timeout=30)
print(f"{ready:.1f}")
PY
) )"
emit cold_start_to_ready_ms "$start_ms" ms

# ---------------------------------------------------------------------------
# 2/3. Tool round-trip overhead on each face.
# ---------------------------------------------------------------------------
cat >"$root/tools.json" <<'JSON'
{"version":1,"steps":[
 {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
 {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
 {"output":{"text":"done"}}]}
JSON
round_trip="$( (cd "$root/ws" && "$PY" - "$plak_bin" "$root" <<'PY'
import json, os, statistics, subprocess, sys, time
plak, root = sys.argv[1], sys.argv[2]
env = {"PATH": os.environ["PATH"], "HOME": root + "/fake-home", "TMPDIR": root + "/tmp",
       "PLAK_HOME": root + "/home", "PLAK_TEST_MODEL": "offline-loopback-v1",
       "PLAK_TEST_SCRIPT": root + "/tools.json",
       "PLAK_TEST_EVIDENCE": root + "/evidence.jsonl"}
# Measured five times and reported as the median. On an idle host this metric
# swings 38.9% run to run, and bimodally: four samples cluster and one lands
# far below. A single sample is therefore not a reading of anything. The median
# of five is stable enough to be worth printing, though still not worth gating
# (see spec 05 section 2.7).
REPEATS = 5


def one_round_trip():
    child = subprocess.Popen([plak, "agent"], env=env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    for line in child.stdout:
        if json.loads(line).get("kind") == "ready":
            break
    child.stdin.write('{"v":1,"kind":"prompt","text":"go"}\n'); child.stdin.flush()
    running = done = None
    deadline = time.monotonic() + 120.0
    for line in child.stdout:
        if time.monotonic() > deadline:
            child.kill()
            raise SystemExit("perf smoke: the tool turn did not finish inside 120s")
        record = json.loads(line)
        if record.get("kind") == "tool" and record.get("state") == "running" and running is None:
            running = time.monotonic()
        if record.get("kind") == "tool" and record.get("state") == "done" and done is None:
            done = time.monotonic()
        if record.get("kind") == "assistant_done":
            break
    child.stdin.write('{"v":1,"kind":"quit"}\n'); child.stdin.flush(); child.stdin.close()
    child.wait(timeout=30)
    if running is None or done is None:
        raise SystemExit("perf smoke: the tool turn reported no round trip")
    return (done - running) * 1000.0


print(f"{statistics.median(one_round_trip() for _ in range(REPEATS)):.3f}")
PY
) )"
emit jsonl_tool_round_trip_ms "$round_trip" ms  # median of five

cli_turn="$( (cd "$root/ws" && "$PY" - "$plak_bin" "$root" <<'PY'
import os, subprocess, sys, time
plak, root = sys.argv[1], sys.argv[2]
env = {"PATH": os.environ["PATH"], "HOME": root + "/fake-home", "TMPDIR": root + "/tmp",
       "PLAK_HOME": root + "/home", "PLAK_TEST_MODEL": "offline-loopback-v1",
       "PLAK_TEST_SCRIPT": root + "/tools.json",
       "PLAK_TEST_EVIDENCE": root + "/evidence.jsonl"}
began = time.monotonic()
subprocess.run([plak, "run", "--account", "codex", "--model", "offline-loopback-v1",
                "--effort", "medium", "--", "go"], env=env, input="y\n",
               capture_output=True, text=True, timeout=60)
print(f"{(time.monotonic() - began) * 1000.0:.1f}")
PY
) )"
emit cli_two_tool_turn_ms "$cli_turn" ms

# ---------------------------------------------------------------------------
# 4. JSONL throughput.
#
# Volume comes from assistant text. The product bounds one tool outcome at a
# mebibyte, refuses to group a pending exchange much above two hundred
# kilobytes, and clips a tool observation before it reaches the face, so a large
# tool body never becomes stream bytes. Assistant text is published verbatim up
# to a few hundred kilobytes. Ten answers of 100 kB is about a megabyte through
# the real writer.
# ---------------------------------------------------------------------------
"$PY" - "$root" <<'PY'
import json, sys
root = sys.argv[1]
json.dump({"version": 1, "repeat_last_step": True,
           "steps": [{"output": {"text": "z" * 100_000}}]},
          open(root + "/bulk.json", "w"))
PY
throughput="$( (cd "$root/ws" && "$PY" - "$plak_bin" "$root" <<'PY'
import json, os, subprocess, sys, time
plak, root = sys.argv[1], sys.argv[2]
PROMPTS = 10
env = {"PATH": os.environ["PATH"], "HOME": root + "/fake-home", "TMPDIR": root + "/tmp",
       "PLAK_HOME": root + "/home", "PLAK_TEST_MODEL": "offline-loopback-v1",
       "PLAK_TEST_SCRIPT": root + "/bulk.json",
       "PLAK_TEST_EVIDENCE": root + "/evidence.jsonl"}
child = subprocess.Popen([plak, "agent"], env=env, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
for line in child.stdout:
    if json.loads(line).get("kind") == "ready":
        break
began = time.monotonic()
for ordinal in range(PROMPTS):
    child.stdin.write(json.dumps({"v": 1, "kind": "prompt", "text": f"go {ordinal}"}) + "\n")
child.stdin.flush()
byte_count = 0
answered = 0
deadline = began + 120.0
for line in child.stdout:
    byte_count += len(line)
    if json.loads(line).get("kind") == "assistant_done":
        answered += 1
        if answered == PROMPTS:
            break
    if time.monotonic() > deadline:
        child.kill()
        raise SystemExit("perf smoke: the bulk turns did not finish inside 120s")
elapsed = time.monotonic() - began
child.stdin.write('{"v":1,"kind":"quit"}\n'); child.stdin.flush(); child.stdin.close()
child.wait(timeout=60)
print(f"{byte_count / elapsed / 1_000_000:.3f}")
PY
) )"
emit jsonl_bulk_throughput_mb_per_s "$throughput" mb_per_s

# ---------------------------------------------------------------------------
# 5. TUI layout over a 20,000-block transcript.
# ---------------------------------------------------------------------------
redraw="$(cargo bench --locked -q -p plak --bench tui_render --all-features 2>/dev/null || true)"
redraw_ms="$(printf '%s\n' "$redraw" | awk -F'  +' '/20k blocks/ {print $3}' | head -1)"
if [[ -n "$redraw_ms" ]]; then
  emit tui_transcript_20k_layout_ms "$redraw_ms" ms
fi

# ---------------------------------------------------------------------------
# 6. Peak resident set after a scripted growth turn through the placeholder rung.
# ---------------------------------------------------------------------------
"$PY" - "$root" <<'PY'
import json, sys
root = sys.argv[1]
steps = [{"output": {"tool": {"name": "loopback_effect", "arguments": {}}},
          "finish": "tool_calls"} for _ in range(6)]
steps.append({"output": {"text": "grown"}})
json.dump({"version": 1, "maximum_projected_input": 20_000,
           "scheduled_compaction": True, "content_sensitive_projection": True,
           "tool_result_bytes": 5_800, "repeat_last_step": True, "steps": steps},
          open(root + "/growth.json", "w"))
PY
# `/usr/bin/time` is not one program. BSD's `-l` reports "maximum resident set
# size" in **bytes**; GNU's `-f %M` reports peak RSS in **kilobytes**; on some
# Linux images `/usr/bin/time` is absent entirely. Probe for a usable one rather
# than assuming, and treat an absent measurement as *no measurement* rather than
# letting a failed pipeline read as a regression.
rss_mode=""
if /usr/bin/time -l true >/dev/null 2>&1; then
  rss_mode=bsd
elif /usr/bin/time -f 'plak-rss %M' true >/dev/null 2>&1; then
  rss_mode=gnu
fi

run_growth_turn() {
  (cd "$root/ws" && ( printf '{"v":1,"kind":"prompt","text":"grow"}\n'; sleep 10; \
    printf '{"v":1,"kind":"quit"}\n'; sleep 1 ) | env -i PATH="$PATH" \
    HOME="$root/fake-home" TMPDIR="$root/tmp" PLAK_HOME="$root/home" \
    PLAK_TEST_MODEL=offline-loopback-v1 PLAK_TEST_SCRIPT="$root/growth.json" \
    PLAK_TEST_EVIDENCE="$root/evidence.jsonl" \
    "$@" 2>&1 >/dev/null ) || true
}

rss_mb=""
case "$rss_mode" in
  bsd)
    # Bytes.
    raw="$(run_growth_turn /usr/bin/time -l "$plak_bin" agent \
      | awk '/maximum resident set size/ {print $1}' | head -1)"
    if [[ -n "$raw" ]]; then
      rss_mb="$("$PY" -c "print(f'{$raw/1048576:.1f}')")"
    fi
    ;;
  gnu)
    # Kilobytes, on a tagged line. GNU `time` writes `%M` to stderr, which is
    # merged with the child's own stderr here, so a bare number matched by
    # position could pick up a diagnostic instead. The tag makes it exact.
    raw="$(run_growth_turn /usr/bin/time -f 'plak-rss %M' "$plak_bin" agent \
      | awk '/^plak-rss /{value=$2} END {print value}')"
    if [[ "$raw" =~ ^[0-9]+$ ]]; then
      rss_mb="$("$PY" -c "print(f'{$raw/1024:.1f}')")"
    fi
    ;;
esac

if [[ -n "$rss_mb" ]]; then
  emit growth_turn_peak_rss_mb "$rss_mb" mb
else
  echo "perf smoke: no measurement for growth_turn_peak_rss_mb (no usable /usr/bin/time)" >&2
fi

if [[ -n "$out" ]]; then
  cp "$measurements" "$out"
fi
cat "$measurements"

if [[ "$check" -eq 1 ]]; then
  "$PY" tools/checks/check_perf_smoke.py "$measurements"
fi
