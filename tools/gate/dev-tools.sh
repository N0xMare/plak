#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd -P)"
# The prefix lives outside target/ so `cargo clean` does not destroy the tools.
source "$root/tools/gate/dev-tools-prefix.sh"
prefix="$(plak_dev_tools_prefix "$root")"
export PATH="$prefix/bin:$PATH"
nextest_version=0.9.100
deny_version=0.19.0
ruff_version=0.17.3

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

# Ruff is exact: a newer formatter can rewrite the tree the pin does not.
require_exact_version() {
  local label="$1" expected="$2" output
  shift 2
  output="$("$@" 2>/dev/null)" || fail "$label is unavailable; run just dev-tools install"
  [[ "$output" =~ ([0-9]+)\.([0-9]+)\.([0-9]+) ]] || fail "$label returned an unrecognized version; run just dev-tools install"
  if [[ "${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}" != "$expected" ]]; then
    fail "$label $expected is required exactly; run just dev-tools install"
  fi
}

case "${1:-check}" in
  check)
    [[ $# -le 1 ]] || fail 'usage: dev-tools.sh check|check-nextest|install|deny'
    require_python
    require_version cargo-nextest "$nextest_version" cargo nextest --version
    require_exact_version ruff "$ruff_version" ruff --version
    printf 'Development prerequisites are available.\n'
    ;;
  check-nextest)
    [[ $# -eq 1 ]] || fail 'usage: dev-tools.sh check|check-nextest|install|deny'
    require_version cargo-nextest "$nextest_version" cargo nextest --version
    printf 'cargo-nextest %s or newer is available.\n' "$nextest_version"
    ;;
  deny)
    [[ $# -eq 1 ]] || fail 'usage: dev-tools.sh check|check-nextest|install|deny'
    require_version cargo-deny "$deny_version" cargo deny --version
    cd "$root"
    exec cargo deny check --config "$root/.config/deny.toml" advisories bans licenses sources
    ;;
  install)
    [[ $# -eq 1 ]] || fail 'usage: dev-tools.sh check|check-nextest|install|deny'
    cd "$root"
    require_python
    command -v cargo >/dev/null 2>&1 || fail 'Cargo is required; install the repository-pinned Rust toolchain'
    mkdir -p "$prefix" "$prefix/build"
    CARGO_TARGET_DIR="$prefix/build" cargo install --locked --root "$prefix" --version "$nextest_version" cargo-nextest
    CARGO_TARGET_DIR="$prefix/build" cargo install --locked --root "$prefix" --version "$deny_version" cargo-deny
    CARGO_TARGET_DIR="$prefix/build" cargo install --locked --root "$prefix" --version "$ruff_version" ruff
    python3 tools/gate/check_public_compile.py --fetch
    python3 tools/dev/jj-tools.py install "$prefix"
    printf 'Installed verification tools in %s/bin; just check and just dev-tools deny use this prefix.\n' "$prefix"
    ;;
  -h|--help)
    printf 'usage: dev-tools.sh check|check-nextest|install|deny\n'
    printf 'Install pins: cargo-nextest %s, cargo-deny %s, ruff %s. Installation is explicit and repository-local.\n' "$nextest_version" "$deny_version" "$ruff_version"
    ;;
  *) fail 'usage: dev-tools.sh check|check-nextest|install|deny' ;;
esac
