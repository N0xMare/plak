#!/usr/bin/env bash
# Build and pack the release tarball for this host's supported target:
# x86_64-unknown-linux-musl on Linux x86_64 (the native musl toolchain of
# tools/musl/musl-toolchain.sh, static PIE), aarch64-apple-darwin on Apple Silicon.
# Writes plakd-<version>-<triple>.tar.gz, its wire-schema.json and SHA256SUMS into the output
# directory (default: <target dir>/release-dist).
#
# The tree must be clean and HEAD must carry the tag v<version>, or the tag
# named by --tag: v<version> or a release candidate v<version>-rc.<n>, whose
# tarball is named plakd-<version>-rc.<n>-<triple> while the binaries still
# report <version>. --untagged drops only the tag requirement, for rehearsing
# a release build.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo_root"
tagged=1
tag=
out=
while [[ $# -gt 0 ]]; do
  case "$1" in
    --untagged) tagged=; shift ;;
    --tag) tag="${2:?--tag needs a tag name}"; shift 2 ;;
    --out) out="${2:?--out needs a directory}"; shift 2 ;;
    *) echo "usage: $0 [--untagged | --tag TAG] [--out DIR]" >&2; exit 2 ;;
  esac
done

if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  echo "package-release: the working tree is not clean" >&2
  exit 2
fi
version="$(cargo metadata --locked --no-deps --format-version 1 \
  | python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "plakd"))')"
release="$version"
if [[ -n "$tag" ]]; then
  if [[ -z "$tagged" ]]; then
    echo "package-release: --tag and --untagged exclude each other" >&2
    exit 2
  fi
  rc="${tag#"v$version-rc."}"
  if ! [[ "$tag" == "v$version" || ( "$rc" != "$tag" && "$rc" =~ ^[1-9][0-9]*$ ) ]]; then
    echo "package-release: tag $tag is neither v$version nor v$version-rc.<n>" >&2
    exit 2
  fi
  release="${tag#v}"
else
  tag="v$version"
fi
if [[ -n "$tagged" ]] && ! git tag --points-at HEAD | grep -qx -- "$tag"; then
  echo "package-release: HEAD is not tagged $tag (use --untagged to rehearse)" >&2
  exit 2
fi

static=()
case "$(uname -s) $(uname -m)" in
  "Linux x86_64")
    source tools/musl/musl-toolchain.sh
    static=(--require-static) ;;
  "Darwin arm64")
    export CARGO_BUILD_TARGET=aarch64-apple-darwin ;;
  *)
    echo "package-release: release builds run on Linux x86_64 or Apple Silicon macOS" >&2
    exit 2 ;;
esac
triple="$CARGO_BUILD_TARGET"

target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"

artifacts="$(bash tools/build/build-plakd.sh --artifacts)"
release_out="${out:-$target_dir/release-dist}"
mkdir -p "$release_out"
cp "$artifacts/manifest.json" "$release_out/plakd-$release-$triple-manifest.json"
"$artifacts/plakd" wire schema >"$release_out/plakd-$release-$triple-wire-schema.json"
python3 tools/release/release_tarball.py --version "$release" --triple "$triple" \
  --artifacts "$artifacts" --out "$release_out" \
  --mtime "$(git log -1 --format=%ct HEAD)" ${static[@]+"${static[@]}"}
