#!/usr/bin/env bash
# Native Linux/macOS build and install of plakd and its rquickjs and pinned
# micro-monty sidecars.
#
# Build modes check linkage and stop. They do not write a snapshot or change PATH,
# and a linked worktree is allowed:
#   (no flags)    debug + maintainer features
#   --release     release + maintainer features
#   --maintainer  maintainer profile
#   --profiling   profiling profile, plakd only, default features, no linkage check
# Install modes write an immutable snapshot and point PATH at it:
#   --install             plain release, under the release snapshot root
#   --install-maintainer  maintainer profile, under the maintainer snapshot root
#   --artifacts           verified stripped release files and manifest; prints the directory
# The modes are exclusive. Additive flags, not with install, artifacts or profiling:
#   --features LIST  replace the parent feature set (workers stay unfeatured)
#   --plakd-only      parent only; no workers and no linkage check
#   --offline        pass Cargo --offline (any mode)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo_root"
profile=debug
snapshot_flavor=
plak_features=(--features plakd/maintainer)
artifacts_only=
install=
mode=
plak_only=0
features_set=0
offline=()
usage() {
  echo "usage: $0 [--offline] [--features LIST] [--plakd-only] [--release|--maintainer|--profiling|--install|--install-maintainer|--artifacts]" >&2
}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release|--maintainer|--profiling|--install|--install-maintainer|--artifacts)
      if [[ -n "$mode" ]]; then
        usage
        exit 2
      fi
      mode=$1
      ;;
    --offline) offline=(--offline) ;;
    --plakd-only) plak_only=1 ;;
    --features)
      if [[ $# -lt 2 ]]; then
        usage
        exit 2
      fi
      plak_features=(--features "$2")
      features_set=1
      shift
      ;;
    *)
      usage
      exit 2
      ;;
  esac
  shift
done
# Install and profiling keep their own feature sets. --plakd-only cannot install.
if [[ "$features_set" == 1 || "$plak_only" == 1 ]]; then
  case "$mode" in
    --install|--install-maintainer|--artifacts|--profiling)
      usage
      exit 2
      ;;
  esac
fi
case "$mode" in
  --release) profile=release ;;
  --maintainer) profile=maintainer ;;
  --profiling)
    profile=profiling
    plak_features=()
    plak_only=1
    ;;
  --install)
    profile=release
    snapshot_flavor=release
    plak_features=()
    install=1
    ;;
  --install-maintainer)
    profile=maintainer
    snapshot_flavor=maintainer
    install=1
    ;;
  --artifacts)
    profile=release
    plak_features=()
    artifacts_only=1
    ;;
  "") ;;
esac

if [[ -n "$install" ]]; then
# BEGIN LINKED WORKTREE GUARD
# This command also updates the installed symlinks. A build from a
# linked worktree must never silently repoint those daily-use commands away from
# the primary checkout.
if [[ ! -d "$repo_root/.git" ]]; then
  echo "build-plakd: refusing to build and relink installed binaries from a linked worktree" >&2
  exit 2
fi
# END LINKED WORKTREE GUARD
fi

profile_flag=()
if [[ "$profile" != debug ]]; then
  profile_flag=(--profile "$profile")
fi

# Build the host and its out-of-process evaluators separately.
if [[ "$profile" == release ]]; then
  # Linkage proof needs raw symbols; strip only the staged shipping copies.
  export CARGO_PROFILE_RELEASE_STRIP=debuginfo
fi
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"
clean_revision() {
  local revision status
  revision="$(git rev-parse HEAD)" || return 2
  status="$(git status --porcelain --untracked-files=all --ignore-submodules=none)" || return 2
  if [[ -n "$status" ]]; then
    echo "build-plakd: the working tree is not clean; commit or remove changes before building shipping artifacts" >&2
    return 2
  fi
  printf '%s\n' "$revision"
}
shipping=
if [[ "$mode" == --install || "$mode" == --artifacts ]]; then
  shipping=1
  if [[ "${RUSTFLAGS+x}" ]]; then
    echo "build-plakd: pass extra rustc flags in CARGO_ENCODED_RUSTFLAGS, not RUSTFLAGS" >&2
    exit 2
  fi
  PLAK_SOURCE_REVISION="$(clean_revision)"
  export PLAK_SOURCE_REVISION
  if [[ -n "$artifacts_only" && "$(uname -s)" == Linux ]]; then
    if [[ "${CARGO_BUILD_TARGET:-x86_64-unknown-linux-musl}" != x86_64-unknown-linux-musl ]]; then
      echo "build-plakd: Linux --artifacts requires x86_64-unknown-linux-musl" >&2
      exit 2
    fi
    source tools/musl/musl-toolchain.sh
  fi
  # An explicit target applies Rust flags to shipping code, not host build
  # scripts and proc macros. Keep Cargo's native target when not overridden.
  rustc_facts="$(rustc -vV)"
  triple="${CARGO_BUILD_TARGET:-$(sed -n 's/^host: //p' <<<"$rustc_facts")}"
  export CARGO_BUILD_TARGET="${triple:?could not determine Rust target}"
  cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  mkdir -p "$cargo_home" "$target_dir"
  cargo_home="$(cd "$cargo_home" && pwd -P)"
  target_dir="$(cd "$target_dir" && pwd -P)"
  export CARGO_HOME="$cargo_home" CARGO_TARGET_DIR="$target_dir"
  c_flags_var="CFLAGS_${triple//-/_}"
  cxx_flags_var="CXXFLAGS_${triple//-/_}"
  for path in "$cargo_home" "$repo_root" "$target_dir"; do
    case "$path" in *"="*|*$'\n'*)
      echo "build-plakd: paths containing '=' or a newline cannot be remapped" >&2
      exit 2 ;;
    esac
  done
  # Order by length so overlapping prefixes get the most specific match last.
  # Bash's %q and cc's shlex keep a prefix containing spaces one C argument.
  maps="$(python3 -c 'import sys; print(*sorted(sys.argv[1:], key=lambda mapping: len(mapping.split("=")[0])), sep="\n")' \
    "$cargo_home=/cargo" "$repo_root=/plakd" "$target_dir=/target")"
  while IFS= read -r map; do
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+$CARGO_ENCODED_RUSTFLAGS$'\x1f'}--remap-path-prefix=$map"
    printf -v escaped '%q' "-ffile-prefix-map=$map"
    printf -v "$c_flags_var" '%s' "${!c_flags_var:+${!c_flags_var} }$escaped"
    printf -v "$cxx_flags_var" '%s' "${!cxx_flags_var:+${!cxx_flags_var} }$escaped"
  done <<<"$maps"
  export CARGO_ENCODED_RUSTFLAGS CC_SHELL_ESCAPED_FLAGS=1 "${c_flags_var?}" "${cxx_flags_var?}"
elif [[ -z "$(git -C "$repo_root" status --porcelain 2>/dev/null)" ]]; then
  PLAK_SOURCE_REVISION="$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || true)"
  export PLAK_SOURCE_REVISION
fi
cargo build --locked ${offline[@]+"${offline[@]}"} -p plakd ${plak_features[@]+"${plak_features[@]}"} ${profile_flag[@]+"${profile_flag[@]}"}
output_dir="$target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}${profile}"
bin="$output_dir/plakd"
js_host="$output_dir/plakous-js-host"
py_host="$output_dir/plakous-py-host"
# Profiling and a parent-only rebuild do not build workers or check linkage.
if [[ "$plak_only" == 1 ]]; then
  if [[ ! -x "$bin" ]]; then
    echo "build-plakd: cargo did not write $bin" >&2
    exit 2
  fi
  exit 0
fi
cargo build --locked ${offline[@]+"${offline[@]}"} -p plakous-js-runtime -p plakous-py-worker ${profile_flag[@]+"${profile_flag[@]}"}
if [[ ! -x "$bin" ]]; then
  echo "build-plakd: cargo did not write $bin" >&2
  exit 2
fi
if [[ ! -x "$py_host" ]]; then
  echo "build-plakd: cargo did not write $py_host" >&2
  exit 2
fi
if [[ ! -x "$js_host" ]]; then
  echo "build-plakd: cargo did not write $js_host" >&2
  exit 2
fi

# Inspect raw symbols on both ELF and Mach-O; require worker positive controls.
# Report on stderr: --artifacts prints only the output directory on stdout.
python3 tools/build/check-artifact-linkage.py "$output_dir" >&2
if [[ "$profile" == debug ]]; then
  echo "build-plakd: debug binary (slower startup); daily PATH CLI is just install release or just install maintainer" >&2
fi
if [[ -z "$install" && -z "$artifacts_only" ]]; then
  exit 0
fi
if [[ "$profile" == release ]]; then
  output_dir="$output_dir/stripped"
  mkdir -p "$output_dir"
  rm -f "$output_dir/manifest.json"
  for artifact in "$bin" "$js_host" "$py_host"; do
    cp "$artifact" "$output_dir/$(basename "$artifact")"
    strip "$output_dir/$(basename "$artifact")"
  done
  bin="$output_dir/plakd"
  js_host="$output_dir/plakous-js-host"
  py_host="$output_dir/plakous-py-host"
fi
if [[ -n "$shipping" ]]; then
  revision="$(clean_revision)"
  if [[ "$revision" != "$PLAK_SOURCE_REVISION" ]]; then
    echo "build-plakd: HEAD changed during the shipping build" >&2
    exit 2
  fi
  python3 tools/build/artifact_manifest.py emit "$output_dir" "$triple" "$revision" "$cargo_home" "$target_dir"
fi
if [[ -n "$artifacts_only" ]]; then
  printf '%s\n' "$output_dir"
  exit 0
fi

exec "$bin" snapshot install --from "$output_dir" --source-build "$target_dir" --flavor "$snapshot_flavor"
