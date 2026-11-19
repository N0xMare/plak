#!/bin/sh
# Install a Plak release from GitHub Releases into ~/.local without sudo:
#   curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/main/install.sh | sh
# PLAK_VERSION=0.0.4 selects a release (default: latest). The layout is the one
# `just install release` writes: an immutable snapshot under ~/.local/lib/plak/release,
# content-addressed workers under ~/.local/lib/plak/sidecars, and links in
# ~/.local/bin. PLAK_RELEASE_BASE replaces https://github.com/<repo> (mirrors,
# tests). When `gh` is installed and signed in, the tarball's GitHub build
# attestation is verified too; PLAK_SKIP_ATTESTATION=1 skips that check.
set -eu
umask 022
repo=N0xMare/plak
base="${PLAK_RELEASE_BASE:-https://github.com/$repo}"
source_doc="https://github.com/$repo/blob/main/docs/user-guide/getting-started.md"

die() { echo "plakd install: $*" >&2; exit 1; }
say() { echo "plakd install: $*" >&2; }
usage() {
  die "usage: sh install.sh [install [--from DIR|TARBALL [--manifest FILE] [--no-link]] | link SNAPSHOT_ID | uninstall [--dry-run]]"
}

# "<uid> <mode>", with the sticky bit when $2 is "sticky". GNU `stat -f`
# reports the file system instead; that output is discarded.
owner_mode() {
  bsd='%Lp'; [ "${2:-}" != sticky ] || bsd='%Mp%Lp'
  facts="$(stat -f "%u $bsd" "$1" 2>/dev/null)" || facts="$(stat -c '%u %a' "$1")"
  echo "$facts"
}
# Same rules as tools/build/build-plakd.sh: physical installation ancestors
# are user/root-owned and not writable by others, except root-owned sticky roots.
safe_dir() (
  [ -d "$1" ] && [ ! -L "$1" ] || die "unsafe or symlinked directory refused: $1"
  dir="$(cd "$1" && pwd -P)"
  while :; do
    facts="$(owner_mode "$dir" sticky)"; owner="${facts% *}"; mode="${facts#* }"
    if [ "$owner" = 0 ] && [ $((0$mode & 01000)) -ne 0 ]; then :
    elif [ "$owner" != 0 ] && [ "$owner" != "$(id -u)" ]; then die "foreign installation ancestor refused: $dir"
    elif [ $((0$mode & 022)) -ne 0 ]; then die "writable installation ancestor refused: $dir"
    fi
    [ "$dir" != / ] || break
    dir="$(dirname "$dir")"
  done
)

uninstall_link() {
  link=$1
  [ -L "$link" ] || return 0
  target="$(readlink "$link")"
  case "$target" in /*) ;; *) target="$(dirname "$link")/$target" ;; esac
  # Resolve the parent, not the executable: stale executable links still belong
  # to an existing snapshot, and HOME may contain a platform symlink like /tmp.
  if physical_parent="$(cd "$(dirname "$target")" 2>/dev/null && pwd -P)"; then
    target="$physical_parent/$(basename "$target")"
    case "$target" in
      "$lib/release/"*|"$lib/maintainer/"*|"$lib/sidecars/"*)
        if [ "$dry_run" = 1 ]; then say "would remove link $link"
        else say "removing link $link"; rm "$link"; fi
        return 0 ;;
    esac
  fi
  say "retained unrelated link: $link"
}

mode=remote
snapshot_flavor=release
source_link_target=
no_link=
local_source=
manifest=
case "${1:-}" in
  uninstall)
    shift
    dry_run=0
    if [ "${1:-}" = --dry-run ]; then dry_run=1; shift; fi
    [ "$#" -eq 0 ] || die "usage: sh install.sh uninstall [--dry-run] (product home is always retained)"
    case "${HOME:-}" in /*) [ "$HOME" != / ] || die "HOME must not be /" ;; *) die "HOME must be absolute" ;; esac
    lib="$HOME/.local/lib/plak"
    if [ -e "$lib" ] || [ -L "$lib" ]; then
      safe_dir "$lib"
      lib="$(cd "$lib" && pwd -P)"
    fi
    bindir="$HOME/.local/bin"
    if [ -e "$bindir" ] || [ -L "$bindir" ]; then safe_dir "$bindir"; fi
    cargo_bindir="${CARGO_HOME:-$HOME/.cargo}/bin"
    if [ -e "$cargo_bindir" ] || [ -L "$cargo_bindir" ]; then safe_dir "$cargo_bindir"; fi
    # Validate all removal roots before changing links or snapshots.
    for name in release maintainer sidecars; do
      if [ -e "$lib/$name" ] || [ -L "$lib/$name" ]; then safe_dir "$lib/$name"; fi
    done
    for name in plakd plakous-js-host plakous-py-host monty; do
      uninstall_link "$bindir/$name"
    done
    for name in plakd plakous-js-host plakous-py-host; do
      uninstall_link "$cargo_bindir/$name"
    done
    for name in release maintainer sidecars; do
      if [ -d "$lib/$name" ]; then
        if [ "$dry_run" = 1 ]; then say "would remove root $lib/$name"
        else say "removing $lib/$name"; rm -rf "$lib/$name"; fi
      fi
    done
    if [ "$dry_run" = 1 ]; then say "dry run; nothing removed; product home and credentials retained"
    else say "uninstalled; product home and credentials retained"; fi
    exit 0 ;;
  link)
    [ "$#" -eq 2 ] || usage
    mode=link
    snapshot_id=$2
    case "$snapshot_id" in *[!0-9a-f]*|'') die "invalid snapshot id" ;; esac
    [ "${#snapshot_id}" -eq 64 ] || die "invalid snapshot id" ;;
  --source-build)
    # Internal build-plakd.sh entry; its linked-worktree guard precedes this.
    [ "$#" -eq 4 ] || usage
    source_link_target="$(cd "$4" && pwd -P)"
    case "$3" in
      release) mode=local; local_source=$2 ;;
      maintainer) mode=maintainer; snapshot_flavor=maintainer; artifacts=$2 ;;
      *) usage ;;
    esac ;;
  ''|install|--from|--no-link|--manifest)
    if [ "${1:-}" = install ]; then shift; fi
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --from) [ "$#" -ge 2 ] && [ -z "$local_source" ] || usage; local_source=$2; shift ;;
        --manifest) [ "$#" -ge 2 ] && [ -z "$manifest" ] || usage; manifest=$2; shift ;;
        --no-link) [ -z "$no_link" ] || usage; no_link=1 ;;
        *) usage ;;
      esac
      shift
    done
    if [ -n "$local_source" ]; then mode=local
    else [ -z "$manifest$no_link" ] || usage; fi ;;
  *) usage ;;
esac

if [ "$mode" = local ] || [ "$mode" = link ]; then
  case "${HOME:-}" in /*) [ "$HOME" != / ] || die "HOME must not be /" ;; *) die "HOME must be absolute" ;; esac
fi
if [ "$mode" = remote ] || [ "$mode" = local ]; then
case "$(uname -s) $(uname -m)" in
  "Darwin arm64") triple=aarch64-apple-darwin ;;
  # An x86_64 shell under Rosetta still runs on Apple Silicon.
  "Darwin x86_64")
    [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ] \
      || die "Intel macOS is not supported; build from source: $source_doc"
    triple=aarch64-apple-darwin ;;
  "Linux x86_64") triple=x86_64-unknown-linux-musl ;;
  *) die "no release for $(uname -s) $(uname -m); build from source: $source_doc" ;;
esac
fi
[ -n "${HOME:-}" ] || die "HOME is not set"
if [ "$mode" = remote ]; then
command -v curl >/dev/null || die "curl is required"
command -v tar >/dev/null || die "tar is required"
fi
fetch() {
  if [ -n "${PLAK_RELEASE_BASE:-}" ]; then curl -fsSL "$@"
  else curl -fsSL --proto =https --tlsv1.2 "$@"; fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then checksum="$(sha256sum "$1")" || return 1
  else checksum="$(shasum -a 256 "$1")" || return 1; fi
  printf '%s\n' "${checksum%% *}"
}

if [ "$mode" = remote ]; then
if [ -n "${PLAK_VERSION:-}" ]; then
  version="${PLAK_VERSION#v}"
else
  # releases/latest redirects to releases/tag/v<version>.
  latest="$(fetch -o /dev/null -w '%{url_effective}' "$base/releases/latest")" \
    || die "could not resolve the latest release"
  version="${latest##*/tag/v}"
  [ "$version" != "$latest" ] || die "could not resolve the latest release from $latest"
fi
case "$version" in *[!0-9A-Za-z.+-]*|'') die "invalid version: $version" ;; esac

work="$(mktemp -d "${TMPDIR:-/tmp}/plakd-install.XXXXXX")"
trap 'rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM
stem="plakd-$version-$triple"
say "downloading $stem"
fetch -o "$work/$stem.tar.gz" "$base/releases/download/v$version/$stem.tar.gz" \
  || die "could not download $stem.tar.gz"
fetch -o "$work/SHA256SUMS" "$base/releases/download/v$version/SHA256SUMS" \
  || die "could not download SHA256SUMS"
expected="$(awk -v name="$stem.tar.gz" '$2 == name { print $1 }' "$work/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS has no entry for $stem.tar.gz"
[ "$(sha256_of "$work/$stem.tar.gz")" = "$expected" ] || die "checksum mismatch for $stem.tar.gz"
if [ "${PLAK_SKIP_ATTESTATION:-}" = 1 ]; then
  say "attestation check skipped (PLAK_SKIP_ATTESTATION=1)"
elif command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  gh attestation verify "$work/$stem.tar.gz" --repo "$repo" >/dev/null \
    || die "GitHub attestation verification failed for $stem.tar.gz"
  say "attestation verified"
else
  say "attestation not checked (needs a signed-in gh); checksum verified"
fi
tar -xzf "$work/$stem.tar.gz" -C "$work"
for name in plakd plakous-js-host plakous-py-host; do
  [ -f "$work/$stem/$name" ] && [ ! -L "$work/$stem/$name" ] || die "$stem.tar.gz lacks $name"
done
artifacts="$work/$stem"
elif [ "$mode" = local ]; then
  if [ -d "$local_source" ]; then
    manifest="${manifest:-$local_source/manifest.json}"
  else
    [ -n "$manifest" ] || die "a local tarball requires --manifest FILE"
  fi
  command -v python3 >/dev/null || die "Python 3.11+ is required for local artifacts"
  installer_dir="$(cd "$(dirname "$0")" && pwd -P)"
  work="$(mktemp -d "${TMPDIR:-/tmp}/plakd-install.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
  trap 'exit 1' HUP INT TERM
  expected_snapshot="$(python3 "$installer_dir/tools/build/artifact_manifest.py" stage \
    "$local_source" "$manifest" "$work" "$triple")"
  artifacts="$work"
  strict_markers=1
fi

if [ "$mode" != link ]; then
  bin="$artifacts/plakd"
  js_host="$artifacts/plakous-js-host"
  py_host="$artifacts/plakous-py-host"
fi

# BEGIN SNAPSHOT INSTALL
snapshot_digest() (
  parent_digest="$(sha256_of "$1")" || exit 1
  js_digest="$(sha256_of "$2")" || exit 1
  py_digest="$(sha256_of "$3")" || exit 1
  printf 'plakd %s\nplakous-js-host %s\nplakous-py-host %s\n' \
    "$parent_digest" "$js_digest" "$py_digest" | sha256_of -
)
require_owned_directory() {
  [ -d "$1" ] && [ ! -L "$1" ] || die "unsafe or symlinked directory refused: $1"
  safe_dir "$1" || exit 1
  [ "$(id -u)" = "$(owner_mode "$1" | cut -d' ' -f1)" ] || die "foreign directory refused: $1"
}
# Create missing ancestors individually, independent of the caller's umask.
# Linked homes resolve to the physical installation, checked by safe_dir.
ensure_install_directory() (
  if [ ! -d "$1" ]; then
    ensure_install_directory "$(dirname "$1")"
    mkdir -m 0755 "$1" 2>/dev/null || [ -d "$1" ] || exit 1
  fi
  safe_dir "$(cd "$1" && pwd -P)"
)
check_sidecar() (
  require_owned_directory "$(dirname "$(dirname "$1")")" || exit 1
  require_owned_directory "$(dirname "$1")" || exit 1
  [ -f "$1" ] && [ ! -L "$1" ] && [ "$(sha256_of "$1")" = "$2" ] \
    || die "sidecar artifact integrity refused: $1"
  [ "$(owner_mode "$1")" = "$(id -u) 555" ] || die "sidecar artifact permissions refused: $1"
)
install_sidecar() (
  source=$1 name=$2
  digest="$(sha256_of "$source")" || exit 1
  dest="$artifact_root/$name/$digest"
  mkdir -p -m 0755 "$artifact_root/$name" || exit 1
  require_owned_directory "$artifact_root/$name" || exit 1
  [ ! -L "$dest" ] || die "symlinked sidecar artifact directory refused: $dest"
  if [ ! -d "$dest" ]; then
    staging="$(mktemp -d "$artifact_root/.staging.XXXXXX")" || exit 1
    trap 'rm -rf "$staging"' EXIT
    cp "$source" "$staging/$name" && chmod 0555 "$staging/$name" || exit 1
    mv "$staging" "$dest" 2>/dev/null || true
    rm -rf "${dest:?}/$(basename "$staging")"
  fi
  check_sidecar "$dest/$name" "$digest" || exit 1
  printf '%s\n' "$dest/$name"
)
write_markers() (
  "$1/plakd" version --write-binary-manifest "$1" 2>/dev/null || rm -f "$1/binary-id"
  "$1/plakd" version --state-key >"$1/state-key" 2>/dev/null || rm -f "$1/state-key"
  "$1/plakd" version --export-version >"$1/export-version" 2>/dev/null || rm -f "$1/export-version"
  if [ -n "${strict_markers:-}" ]; then check_markers "$1"; fi
)
check_markers() (
  for marker in binary-id state-key export-version; do
    [ -f "$1/$marker" ] && [ ! -L "$1/$marker" ] && [ -s "$1/$marker" ] \
      || die "missing or invalid target marker: $1/$marker"
  done
)
publish_snapshot() (
  staging="$(mktemp -d "$snapshot_root/.staging.XXXXXX")"
  trap 'rm -rf "$staging"' EXIT
  cp "$bin" "$staging/plakd"
  chmod 0555 "$staging/plakd"
  ln -s "$installed_js" "$staging/plakous-js-host"
  ln -s "$installed_py" "$staging/plakous-py-host"
  write_markers "$staging"
  # A concurrent writer of identical bytes can win the directory rename.
  mv "$staging" "$snapshot" 2>/dev/null || true
  rm -rf "${snapshot:?}/$(basename "$staging")"
)
check_snapshot() (
  require_owned_directory "$snapshot"
  [ -f "$snapshot/plakd" ] && [ ! -L "$snapshot/plakd" ] \
    && [ "$(owner_mode "$snapshot/plakd")" = "$(id -u) 555" ] || die "plakd artifact refused: $snapshot"
  if [ -n "${strict_markers:-}" ]; then
    for name in plakous-js-host plakous-py-host; do
      worker="$(readlink "$snapshot/$name")" || die "sidecar link refused: $name"
      digest="$(basename "$(dirname "$worker")")"
      case "$digest" in *[!0-9a-f]*|'') die "sidecar digest refused: $name" ;; esac
      [ "${#digest}" -eq 64 ] && [ "$worker" = "$artifact_root/$name/$digest/$name" ] \
        || die "sidecar link refused: $name"
      check_sidecar "$worker" "$digest"
    done
    check_markers "$snapshot"
  fi
  [ "$(snapshot_digest "$snapshot/plakd" "$snapshot/plakous-js-host" "$snapshot/plakous-py-host")" = "$snapshot_id" ] \
    || die "snapshot integrity refused: $snapshot"
)
snapshot_root="$HOME/.local/lib/plak/$snapshot_flavor"
if [ "${mode:-}" != link ]; then
  snapshot_id="$(snapshot_digest "$bin" "$js_host" "$py_host")"
  [ "$snapshot_id" = "${expected_snapshot:-$snapshot_id}" ] || die "snapshot id differs after verification"
  ensure_install_directory "$snapshot_root"
fi
require_owned_directory "$snapshot_root"
snapshot_root="$(cd "$snapshot_root" && pwd -P)"
lib="$(dirname "$snapshot_root")"
artifact_root="$lib/sidecars"
if [ "${mode:-}" != link ]; then mkdir -p -m 0755 "$artifact_root"; fi
require_owned_directory "$artifact_root"
snapshot="$snapshot_root/$snapshot_id"
if [ "${mode:-}" = link ]; then
  strict_markers=1
else
  installed_js="$(install_sidecar "$js_host" plakous-js-host)"
  installed_py="$(install_sidecar "$py_host" plakous-py-host)"
  [ ! -L "$snapshot" ] || die "symlinked snapshot refused: $snapshot"
  if [ ! -d "$snapshot" ]; then publish_snapshot; fi
fi
check_snapshot
bin="$snapshot/plakd"
js_host="$(readlink "$snapshot/plakous-js-host")"
py_host="$(readlink "$snapshot/plakous-py-host")"
# END SNAPSHOT INSTALL

if [ -n "$no_link" ]; then
  printf '%s\n' "$snapshot"
  exit 0
fi

# Replace only links that already point into the Plak installation roots.
check_link() (
  if [ -e "$1" ] || [ -L "$1" ]; then
    current="$(readlink "$1" 2>/dev/null)" || die "refusing to replace non-Plak $1; move it aside"
    if [ "$mode" != remote ]; then
      current="$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1")"
    fi
    case "$current" in
      "$lib/maintainer/"*|"$lib/release/"*|"$lib/sidecars/"*) ;;
      *)
        [ -n "$source_link_target" ] || die "refusing to replace non-Plak $1; move it aside"
        case "$current" in
          "$source_link_target/"*) ;;
          *) die "refusing to replace non-Plak $1; move it aside" ;;
        esac ;;
    esac
  fi
)
link() {
  check_link "$1"
  if [ "$mode" = local ] || [ "$mode" = link ]; then
    python3 - "$1" "$2" <<'PY'
import os
import sys
import tempfile
from pathlib import Path

dest = Path(sys.argv[1])
with tempfile.TemporaryDirectory(dir=dest.parent) as directory:
    pending = Path(directory) / dest.name
    pending.symlink_to(sys.argv[2])
    os.replace(pending, dest)
PY
  else
    ln -sfn "$2" "$1"
  fi
}
bindir="$HOME/.local/bin"
cargo_bindir="${CARGO_HOME:-$HOME/.cargo}/bin"
ensure_install_directory "$bindir"
safe_dir "$bindir"
if [ "$mode" != remote ]; then
  # Refuse foreign destinations before moving the first link. Cargo links
  # are updated only if already present; no new Cargo commands are installed.
  for name in plakd plakous-js-host plakous-py-host; do
    check_link "$bindir/$name"
    if [ -e "$cargo_bindir/$name" ] || [ -L "$cargo_bindir/$name" ]; then
      safe_dir "$cargo_bindir"
      check_link "$cargo_bindir/$name"
    fi
  done
fi
remove_obsolete_worker_link() {
  obsolete_worker="$HOME/.local/bin/monty"
  [ -L "$obsolete_worker" ] || return 0
  obsolete_target="$(readlink "$obsolete_worker")"
  case "$obsolete_target" in
    "$HOME/.local/lib/plak/sidecars/"*|"$HOME/.local/lib/plak/maintainer/"*|"$HOME/.local/lib/plak/release/"*)
      rm "$obsolete_worker" ;;
  esac
}
if [ "$mode" = remote ] || [ -n "$source_link_target" ]; then remove_obsolete_worker_link; fi
link "$bindir/plakd" "$bin"
link "$bindir/plakous-js-host" "$js_host"
link "$bindir/plakous-py-host" "$py_host"
if [ "$mode" != remote ]; then
  for name in plakd plakous-js-host plakous-py-host; do
    if [ -e "$cargo_bindir/$name" ] || [ -L "$cargo_bindir/$name" ]; then
      case "$name" in
        plakd) link "$cargo_bindir/$name" "$bin" ;;
        plakous-js-host) link "$cargo_bindir/$name" "$js_host" ;;
        plakous-py-host) link "$cargo_bindir/$name" "$py_host" ;;
      esac
    fi
  done
  say "installed snapshot $snapshot"
  exit 0
fi
say "installed plakd $version: $HOME/.local/bin/plakd -> $snapshot/plakd"
case ":${PATH:-}:" in
  *":$HOME/.local/bin:"*) ;;
  *) say "add $HOME/.local/bin to PATH" ;;
esac
found="$(command -v plakd 2>/dev/null || true)"
[ -z "$found" ] || [ "$found" = "$HOME/.local/bin/plakd" ] || say "note: plakd on PATH is $found"
say "next: plakd doctor"
