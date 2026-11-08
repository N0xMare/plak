#!/usr/bin/env bash
# Offline test environment: per-checkout TMPDIR, isolated PLAK_HOME, empty git config.
#
#   test_env="$(tools/gate/test-env.sh [--fresh])"; eval "$test_env"
#   tools/gate/test-env.sh [--fresh] -- command  # export, then replace this process
#   tools/gate/test-env.sh [--fresh] --json      # one JSON object for Python
#
# --fresh empties the shared per-checkout directory first (the gate). PLAK_HOME is
# $TMPDIR/plak-isolated-home, mode 0700. GIT_CONFIG_GLOBAL is an empty file in
# TMPDIR, truncated on every call, and GIT_CONFIG_NOSYSTEM is 1. HOME is untouched.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
fresh=()
mode=shell
cmd=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --fresh) fresh=(--fresh); shift ;;
    --json) mode=json; shift ;;
    --) shift; cmd=("$@"); break ;;
    *) echo "usage: test-env.sh [--fresh] [--json | -- command...]" >&2; exit 2 ;;
  esac
done

# bash, not a direct exec: callers copy the selector without its executable bit.
if [[ ${#fresh[@]} -gt 0 && -f "$here/test-processes.py" ]]; then
  # Check before --fresh erases the homes and lock files needed for diagnosis.
  previous_tmp="$(bash "$here/test-tmpdir.sh")"
  TMPDIR="$previous_tmp" python3 "$here/test-processes.py" --check
fi
TMPDIR="$(bash "$here/test-tmpdir.sh" ${fresh[@]+"${fresh[@]}"})"
export TMPDIR
isolated="$TMPDIR/plak-isolated-home"
mkdir -p "$isolated"
chmod 700 "$isolated"
: >"$TMPDIR/gitconfig"
export PLAK_HOME="$isolated"
export GIT_CONFIG_GLOBAL="$TMPDIR/gitconfig"
export GIT_CONFIG_NOSYSTEM=1

# A leaked override would shorten every test. The shell snippet unsets them in
# the caller; `-- command` unsets them before the command.
if [[ ${#cmd[@]} -gt 0 ]]; then
  unset PLAK_TEST_SESSION_LEASE_SECONDS PLAK_TEST_CELL_HELLO_MS
  exec "${cmd[@]}"
fi
if [[ "$mode" == json ]]; then
  python3 -c 'import json, os; print(json.dumps({k: os.environ[k] for k in ("TMPDIR", "PLAK_HOME", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_NOSYSTEM")}))'
  exit 0
fi
printf 'unset PLAK_TEST_SESSION_LEASE_SECONDS PLAK_TEST_CELL_HELLO_MS\n'
printf 'TMPDIR=%q\nPLAK_HOME=%q\nGIT_CONFIG_GLOBAL=%q\nGIT_CONFIG_NOSYSTEM=%q\n' \
  "$TMPDIR" "$PLAK_HOME" "$GIT_CONFIG_GLOBAL" "$GIT_CONFIG_NOSYSTEM"
