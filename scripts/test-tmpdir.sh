#!/usr/bin/env bash
# Prints the physical scratch directory for offline test runs, creating it.
#
# Every test entry point (dev-check.sh, the `test` and `coverage` recipes,
# perf-smoke.sh, crash-sweep.py) exports TMPDIR from this one selector.
# `PLAK_TEST_TMPDIR` wins. Otherwise Linux uses a per-checkout directory under
# RAM-backed /dev/shm and macOS one under /private/tmp, outside the operator's
# HOME so a spawned binary's project-discovery walk cannot reach `~/.plak`.
# The shared directory must be owned by the caller, mode 0700, and reachable
# without symlinks: the SQLite stores open with NOFOLLOW and the credential
# walk refuses symlinked parents. When it is not, the checkout's
# target/plak-test-tmp is used instead.
#
# Usage: scripts/test-tmpdir.sh [--fresh] [--system NAME] [--shared-root DIR]
#   --fresh        empty the shared per-checkout directory first
#   --system       stand-in for `uname -s` (tooling tests)
#   --shared-root  stand-in for /dev/shm or /private/tmp (tooling tests)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd -P)"
fresh=0
system="$(uname -s)"
shared_root=""
while (($#)); do
  case "$1" in
    --fresh) fresh=1; shift ;;
    --system) system="${2:?--system needs a value}"; shift 2 ;;
    --shared-root) shared_root="${2:?--shared-root needs a value}"; shift 2 ;;
    *) echo "usage: test-tmpdir.sh [--fresh] [--system NAME] [--shared-root DIR]" >&2; exit 2 ;;
  esac
done

physical() { (cd "$1" && pwd -P); }

if [[ -n "${PLAK_TEST_TMPDIR:-}" ]]; then
  mkdir -p "$PLAK_TEST_TMPDIR"
  physical "$PLAK_TEST_TMPDIR"
  exit 0
fi

if [[ -z "$shared_root" ]]; then
  case "$system" in
    Linux) shared_root=/dev/shm ;;
    Darwin) shared_root=/private/tmp ;;
  esac
fi

# Succeeds and prints the directory only when it is a real, caller-owned,
# 0700 directory whose physical path is its literal path.
shared_dir() {
  local base="$1" dir
  [[ -d "$base" && ! -L "$base" && -w "$base" ]] || return 1
  [[ "$(physical "$base")" == "$base" ]] || return 1
  dir="$base/plak-test-$(id -u)-$(printf '%s' "$root" | cksum | cut -d' ' -f1)"
  if ((fresh)); then rm -rf "$dir" 2>/dev/null || true; fi
  mkdir -m 700 "$dir" 2>/dev/null || true
  [[ -d "$dir" && ! -L "$dir" && -O "$dir" ]] || return 1
  chmod 700 "$dir"
  [[ "$(physical "$dir")" == "$dir" ]] || return 1
  printf '%s\n' "$dir"
}

if [[ -n "$shared_root" ]]; then
  if shared_dir "$shared_root"; then exit 0; fi
  echo "test-tmpdir: $shared_root is unusable; using target/plak-test-tmp" >&2
fi
mkdir -p "$root/target/plak-test-tmp"
physical "$root/target/plak-test-tmp"
