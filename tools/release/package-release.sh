#!/usr/bin/env bash
# Build and pack the release tarball for this host's supported target:
# x86_64-unknown-linux-musl on Linux x86_64 (the native musl toolchain of
# scripts/musl-toolchain.sh, static PIE), aarch64-apple-darwin on Apple Silicon.
# Writes plak-<version>-<triple>.tar.gz and SHA256SUMS into the output
# directory (default: <target dir>/release-dist).
#
# The tree must be clean and HEAD must carry the tag v<version>. --untagged
# drops only the tag requirement, for rehearsing a release build.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo_root"
tagged=1
out=
while [[ $# -gt 0 ]]; do
  case "$1" in
    --untagged) tagged=; shift ;;
    --out) out="${2:?--out needs a directory}"; shift 2 ;;
    *) echo "usage: $0 [--untagged] [--out DIR]" >&2; exit 2 ;;
  esac
done

if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  echo "package-release: the working tree is not clean" >&2
  exit 2
fi
version="$(cargo metadata --locked --no-deps --format-version 1 \
  | python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "plak"))')"
if [[ -n "$tagged" ]] && ! git tag --points-at HEAD | grep -qx "v$version"; then
  echo "package-release: HEAD is not tagged v$version (use --untagged to rehearse)" >&2
  exit 2
fi

static=()
case "$(uname -s) $(uname -m)" in
  "Linux x86_64")
    source scripts/musl-toolchain.sh
    static=(--require-static) ;;
  "Darwin arm64")
    export CARGO_BUILD_TARGET=aarch64-apple-darwin ;;
  *)
    echo "package-release: release builds run on Linux x86_64 or Apple Silicon macOS" >&2
    exit 2 ;;
esac
triple="$CARGO_BUILD_TARGET"

# Keep the build host's home and checkout paths out of the shipped binaries
# (panic locations name dependency and workspace sources). Cargo ignores
# RUSTFLAGS once CARGO_ENCODED_RUSTFLAGS is set, so caller flags go there.
if [[ -n "${RUSTFLAGS:-}" ]]; then
  echo "package-release: pass extra rustc flags in CARGO_ENCODED_RUSTFLAGS, not RUSTFLAGS" >&2
  exit 2
fi
cargo_home="$(cd "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)"
remap=(--remap-path-prefix="$cargo_home=/cargo" --remap-path-prefix="$repo_root=/plak")
for flag in "${remap[@]}"; do
  CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+$CARGO_ENCODED_RUSTFLAGS$'\x1f'}$flag"
done
export CARGO_ENCODED_RUSTFLAGS

artifacts="$(bash tools/build/build-plak.sh --artifacts)"
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"
python3 tools/release/release_tarball.py --version "$version" --triple "$triple" \
  --artifacts "$artifacts" --out "${out:-$target_dir/release-dist}" \
  --mtime "$(git log -1 --format=%ct HEAD)" ${static[@]+"${static[@]}"}
