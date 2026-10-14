#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd -P)"
# The prefix lives outside target/ so `cargo clean` does not destroy the tools.
prefix="$root/.dev-tools"
export PATH="$prefix/bin:$PATH"
nextest_version=0.9.100
deny_version=0.19.0

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

require_python() {
  command -v python3 >/dev/null 2>&1 || fail 'Python 3.11+ is required; put a working python3 on PATH'
  python3 -c 'import sys; raise SystemExit(sys.version_info < (3, 11))' >/dev/null 2>&1 || fail 'python3 is unusable or older than 3.11; put a working Python 3.11+ on PATH'
}

require_version() {
  local label="$1" minimum="$2" output major minor patch min_major min_minor min_patch
  shift 2
  output="$("$@" 2>/dev/null)" || fail "$label is unavailable; run just dev-tools install"
  [[ "$output" =~ ([0-9]+)\.([0-9]+)\.([0-9]+) ]] || fail "$label returned an unrecognized version; run just dev-tools install"
  major="${BASH_REMATCH[1]}"; minor="${BASH_REMATCH[2]}"; patch="${BASH_REMATCH[3]}"
  IFS=. read -r min_major min_minor min_patch <<< "$minimum"
  if (( 10#$major < 10#$min_major || (10#$major == 10#$min_major && 10#$minor < 10#$min_minor) || (10#$major == 10#$min_major && 10#$minor == 10#$min_minor && 10#$patch < 10#$min_patch) )); then
    fail "$label $minimum or newer is required; run just dev-tools install"
  fi
}

case "${1:-check}" in
  check)
    [[ $# -le 1 ]] || fail 'usage: dev-tools.sh check|install|deny'
    require_python
    require_version cargo-nextest "$nextest_version" cargo nextest --version
    printf 'Development prerequisites are available.\n'
    ;;
  deny)
    [[ $# -eq 1 ]] || fail 'usage: dev-tools.sh deny'
    require_version cargo-deny "$deny_version" cargo deny --version
    cd "$root"
    exec cargo deny check advisories bans licenses sources
    ;;
  install)
    [[ $# -eq 1 ]] || fail 'usage: dev-tools.sh install'
    cd "$root"
    require_python
    command -v cargo >/dev/null 2>&1 || fail 'Cargo is required; install the repository-pinned Rust toolchain'
    mkdir -p "$prefix" "$root/.dev-tools/build"
    CARGO_TARGET_DIR="$root/.dev-tools/build" cargo install --locked --root "$prefix" --version "$nextest_version" cargo-nextest
    CARGO_TARGET_DIR="$root/.dev-tools/build" cargo install --locked --root "$prefix" --version "$deny_version" cargo-deny
    python3 tools/checks/check_public_compile.py --fetch
    printf 'Installed verification tools in %s/bin; just check and just deny use this prefix.\n' "$prefix"
    ;;
  -h|--help)
    printf 'usage: dev-tools.sh check|install|deny\n'
    printf 'Install pins: cargo-nextest %s, cargo-deny %s. Installation is explicit and repository-local.\n' "$nextest_version" "$deny_version"
    ;;
  *) fail 'usage: dev-tools.sh check|install|deny' ;;
esac
