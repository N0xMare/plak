"""Cell trial shell for Full-cell `pmux run`.

Harbor wraps `exec_as_agent` in `bash -c`. TB instructions contain
apostrophes; never nest the prompt or trial body through `su -c`.
Write the instruction with `--prompt-file` and `su` a script file.
"""

from __future__ import annotations

import base64
import shlex

from harbor_adapter.tuples import (
    REMOTE_CLAUDE_SECURESTORAGE,
    REMOTE_PMUXD,
    REMOTE_PMUX_PROMPT,
    REMOTE_PMUX_RUNTIME,
    REMOTE_PMUX_SOCKET,
    REMOTE_PMUX_TMPDIR,
    REMOTE_PMUX_TRIAL,
)

def pmux_prompt_write_command(instruction: str) -> str:
    payload = base64.b64encode(instruction.encode("utf-8")).decode("ascii")
    runtime = shlex.quote(REMOTE_PMUX_RUNTIME)
    dest = shlex.quote(REMOTE_PMUX_PROMPT)
    return (
        f"mkdir -p {runtime} && "
        f"printf '%s' {payload} | base64 -d > {dest} && "
        f"chmod 0644 {dest}"
    )


def pmux_trial_command(
    quoted: str,
    stdout_path: str,
    stderr_path: str,
    turn_timeout_ms: int,
) -> str:
    runtime = shlex.quote(REMOTE_PMUX_RUNTIME)
    socket = shlex.quote(REMOTE_PMUX_SOCKET)
    pool = shlex.quote(f"{REMOTE_PMUX_RUNTIME}/pool")
    tmpdir = shlex.quote(REMOTE_PMUX_TMPDIR)
    pmuxd = shlex.quote(REMOTE_PMUXD)
    claude = "/installed-agent/claude-no-web"
    pin = shlex.quote(REMOTE_CLAUDE_SECURESTORAGE)
    trial = shlex.quote(REMOTE_PMUX_TRIAL)
    timeout_ms = int(turn_timeout_ms)
    if timeout_ms <= 0:
        raise ValueError("pmux requires a positive task-derived timeout")
    inner = f"""set -euo pipefail
unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL
export IS_SANDBOX=1
export PATH="/installed-agent/vendor/node/bin:/installed-agent/vendor/claude/bin:/usr/local/bin:${{PATH:-/usr/bin:/bin}}"
export TMPDIR={tmpdir} TMP={tmpdir} TEMP={tmpdir}
mkdir -p {runtime} {pool} {tmpdir} /logs/agent
chmod 700 {runtime} {pool} {tmpdir} {pin}
: > {stderr_path}
{pmuxd} serve \\
  --socket {socket} \\
  --runtime-parent {runtime} \\
  --pool-parent {pool} \\
  --pool-claude {shlex.quote(claude)} \\
  --stateful \\
  --pool-size 1 \\
  --pool-turn-timeout-ms {timeout_ms} \\
  --pool-securestorage-dir {pin} >> {stderr_path} 2>&1 &
daemon=$!
pmux_harvest() {{
  if [ -d {runtime}/logs ]; then
    mkdir -p /logs/agent/pmuxd-logs
    cp -a {runtime}/logs/. /logs/agent/pmuxd-logs/ 2>/dev/null || true
  fi
  mkdir -p /logs/agent/claude-transcripts
  cp -a {pool}/transcripts/. /logs/agent/claude-transcripts/ 2>/dev/null || true
  cp -a {pool}/stateful/*/root/projects/*/*.jsonl /logs/agent/claude-transcripts/ 2>/dev/null || true
}}
trap 'pmux_harvest' EXIT
ok=0
for _ in $(seq 1 80); do
  if [ -S {socket} ]; then ok=1; break; fi
  sleep 0.1
done
if [ "$ok" -ne 1 ]; then
  echo "pmuxd socket did not appear" >> {stderr_path}
  exit 1
fi
export PMUX_SOCKET={socket}
cd /app
mkdir -p /logs/agent/claude-transcripts
set +e
{quoted} > {stdout_path} 2>> {stderr_path}
status=$?
exit "$status"
"""
    encoded = base64.b64encode(inner.encode("utf-8")).decode("ascii")
    return f"""set -euo pipefail
[ "$(id -u)" -eq 0 ] || {{ echo 'eval requires root task user' >&2; exit 1; }}
mkdir -p {runtime}
printf '%s' {encoded} | base64 -d > {trial}
chmod 0700 {trial}
exec bash {trial}
"""
