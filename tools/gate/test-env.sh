#!/usr/bin/env bash
# Offline test environment: per-checkout TMPDIR, isolated PLAK_HOME, empty git config.
#
#   test_env="$(tools/gate/test-env.sh [--fresh])"; eval "$test_env"
#   tools/gate/test-env.sh [--fresh] -- command  # export, then replace this process
#   tools/gate/test-env.sh [--fresh] --json      # one JSON object for Python
#   tools/gate/test-env.sh --unset-names         # what the other forms unset, one per line
#
# --fresh empties the shared per-checkout directory first (the gate). PLAK_HOME is
# $TMPDIR/plak-isolated-home, mode 0700. GIT_CONFIG_GLOBAL is an empty file in
# TMPDIR, truncated on every call, and GIT_CONFIG_NOSYSTEM is 1. HOME is untouched.
# Tests also run jj by name, through the product's shims, so every form puts a
# directory holding only a jj link first on PATH. The link names the jj the tests
# use: PLAK_TEST_JJ when set, else, with no jj on PATH, the newest pinned jj
# `just dev-tools install` unpacked, when there is one. No other tool moves.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
source "$here/dev-tools-prefix.sh"
# A leaked override would shorten every test, and a provider key the operator
# exported would make tests see credentials. The shell snippet unsets them in
# the caller; `-- command` unsets them before the command. --json keeps them:
# the live drivers need real keys, and the offline Python drivers that take
# --json drop the names --unset-names prints.
leaked=(PLAK_TEST_SESSION_LEASE_SECONDS PLAK_TEST_CELL_HELLO_MS PLAK_TEST_HOST_IDLE_EXIT_SECONDS
  OPENAI_API_KEY XAI_API_KEY ANTHROPIC_API_KEY OPENROUTER_API_KEY)
fresh=()
mode=shell
cmd=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --fresh) fresh=(--fresh); shift ;;
    --json) mode=json; shift ;;
    --unset-names) printf '%s\n' "${leaked[@]}"; exit 0 ;;
    --) shift; cmd=("$@"); break ;;
    *) echo "usage: test-env.sh [--fresh] [--json | -- command...] | --unset-names" >&2; exit 2 ;;
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
jj_dir=''
if [[ -n "${PLAK_TEST_JJ+set}" ]] || ! command -v jj >/dev/null 2>&1; then
  jj="$(command -v -- "$(plak_test_jj "$(cd "$here/../.." && pwd -P)")" || true)"
  if [[ "$jj" == */* && -f "$jj" && -x "$jj" ]]; then
    [[ "$jj" == /* ]] || jj="$PWD/$jj"
    jj_dir="$TMPDIR/plak-test-jj"
    mkdir -p "$jj_dir"
    # A nested call finds this link as the jj on PATH; relinking would loop it.
    [[ "$jj" -ef "$jj_dir/jj" ]] || ln -sfn "$jj" "$jj_dir/jj"
    export PATH="$jj_dir:$PATH"
  fi
fi

if [[ ${#cmd[@]} -gt 0 ]]; then
  unset "${leaked[@]}"
  exec "${cmd[@]}"
fi
if [[ "$mode" == json ]]; then
  python3 -c 'import json, os, sys; print(json.dumps({k: os.environ[k] for k in sys.argv[1:]}))' \
    TMPDIR PLAK_HOME GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM ${jj_dir:+PATH}
  exit 0
fi
printf 'unset %s\n' "${leaked[*]}"
printf 'TMPDIR=%q\nPLAK_HOME=%q\nGIT_CONFIG_GLOBAL=%q\nGIT_CONFIG_NOSYSTEM=%q\n' \
  "$TMPDIR" "$PLAK_HOME" "$GIT_CONFIG_GLOBAL" "$GIT_CONFIG_NOSYSTEM"
if [[ -n "$jj_dir" ]]; then
  printf 'export PATH=%q\n' "$PATH"
fi
