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

# Claude Code refuses --dangerously-skip-permissions as uid 0. Harbor
# ubuntu cells are root; Full-cell `pmux run` must drop to this user.
# Passwordless sudo lets that user's tools mutate the TB sysadmin world
# (apt-get). User+default ACL on /etc (except sudo control files), /var, /opt, and
# /usr/local lets Write match root arms after package installs. Never
# `sudo claude` / `sudo pmux`.
PMUX_CELL_USER = "pmux"
PMUX_CELL_HOME = "/home/pmux"
PMUX_SUDOERS = "/etc/sudoers.d/pmux"
# FrozenPmux daemon default is 600s. Harbor TB 2.1 A/B agent timeout is 900s.
HARBOR_PMUX_TURN_TIMEOUT_MS = 900_000


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
    turn_timeout_ms: int = HARBOR_PMUX_TURN_TIMEOUT_MS,
) -> str:
    runtime = shlex.quote(REMOTE_PMUX_RUNTIME)
    socket = shlex.quote(REMOTE_PMUX_SOCKET)
    pool = shlex.quote(f"{REMOTE_PMUX_RUNTIME}/pool")
    tmpdir = shlex.quote(REMOTE_PMUX_TMPDIR)
    pmuxd = shlex.quote(REMOTE_PMUXD)
    claude = "/usr/local/bin/claude"
    pin = shlex.quote(REMOTE_CLAUDE_SECURESTORAGE)
    user = PMUX_CELL_USER
    home = PMUX_CELL_HOME
    trial = shlex.quote(REMOTE_PMUX_TRIAL)
    sudoers = shlex.quote(PMUX_SUDOERS)
    timeout_ms = int(turn_timeout_ms)
    if timeout_ms <= 0:
        timeout_ms = HARBOR_PMUX_TURN_TIMEOUT_MS
    inner = f"""set -euo pipefail
unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL
export PATH="/installed-agent/vendor/node/bin:/installed-agent/vendor/claude/bin:/usr/local/bin:${{PATH:-/usr/bin:/bin}}"
export TMPDIR={tmpdir} TMP={tmpdir} TEMP={tmpdir}
mkdir -p {runtime} {pool} {tmpdir} /logs/agent
chmod 700 {runtime} {pool} {tmpdir} {pin}
: > {stderr_path}
if [ "$(id -u)" -ne 0 ]; then
  if ! command -v sudo >/dev/null 2>&1 || ! sudo -n true; then
    echo "pmux sudo is not passwordless" >> {stderr_path}
    exit 1
  fi
fi
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
    dropped = (
        f"export HOME={shlex.quote(home)} USER={shlex.quote(user)} "
        f"LOGNAME={shlex.quote(user)}\n{inner}"
    )
    encoded = base64.b64encode(dropped.encode("utf-8")).decode("ascii")
    return f"""set -euo pipefail
mkdir -p {runtime}
printf '%s' {encoded} | base64 -d > {trial}
chmod 0755 {trial}
if [ "$(id -u)" -eq 0 ]; then
  if [ ! -x /bin/ps ]; then
    if command -v ps >/dev/null 2>&1; then
      ln -sfn "$(command -v ps)" /bin/ps
    elif command -v apt-get >/dev/null 2>&1; then
      apt-get update -qq && apt-get install -y -qq procps >/dev/null
    fi
  fi
  if ! command -v sudo >/dev/null 2>&1; then
    if command -v apt-get >/dev/null 2>&1; then
      apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo >/dev/null
    fi
  fi
  id -u {shlex.quote(user)} >/dev/null 2>&1 || useradd --system --create-home --home-dir {shlex.quote(home)} --shell /bin/bash {shlex.quote(user)}
  printf '%s\\n' 'Defaults:{user} !requiretty' '{user} ALL=(ALL) NOPASSWD:ALL' > {sudoers}
  chmod 0440 {sudoers}
  mkdir -p {runtime} {pool} {tmpdir} /logs/agent {shlex.quote(home)}
  ulimit -n 8192 || true
  chown -R {shlex.quote(user)}:{shlex.quote(user)} {runtime} {pool} {tmpdir} {pin} /app /logs/agent {shlex.quote(home)}
  if ! command -v setfacl >/dev/null 2>&1; then
    if command -v apt-get >/dev/null 2>&1; then
      apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq acl >/dev/null || true
    fi
  fi
  for _dir in /var /opt /usr/local; do
    if command -v setfacl >/dev/null 2>&1; then
      setfacl -R -m u:{user}:rwx -m d:u:{user}:rwx "$_dir" 2>/dev/null || chown -R {user}:{user} "$_dir" 2>/dev/null || true
    else
      chown -R {user}:{user} "$_dir" 2>/dev/null || true
    fi
  done
  if command -v setfacl >/dev/null 2>&1; then
    # sudo warns about writable sudo.conf. Preserve control files' original
    # ACLs while allowing Claude's Write tool in the rest of /etc.
    find /etc -path '/etc/sudo*' -prune -o -type d -exec setfacl -m u:{user}:rwx -m d:u:{user}:rwx {{}} + 2>/dev/null || true
    find /etc -path '/etc/sudo*' -prune -o -type f -exec setfacl -m u:{user}:rwx {{}} + 2>/dev/null || true
  else
    find /etc -path '/etc/sudo*' -prune -o -exec chown {user}:{user} {{}} + 2>/dev/null || true
  fi
  chown root:root /etc/sudoers /etc/sudoers.d {sudoers} 2>/dev/null || true
  chmod 0440 /etc/sudoers {sudoers} 2>/dev/null || true
  chmod 0755 /etc/sudoers.d 2>/dev/null || true
  exec su -s /bin/bash {shlex.quote(user)} -- {trial}
fi
exec bash {trial}
"""
