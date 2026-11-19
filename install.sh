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
# Physical installation ancestors are user/root-owned and not writable by
# others, except root-owned sticky roots.
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

if [ "$mode" != remote ]; then
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
# Extract only the bootstrap image and its pinned manifest, without creating
# any archive paths. The Rust installer reads and checks the three members.
tar -xOf "$work/$stem.tar.gz" -- "$stem/plakd" >"$work/plakd" \
  || die "release lacks the installer image"
tar -xOf "$work/$stem.tar.gz" -- "$stem/manifest.json" >"$work/manifest.json" \
  || die "release lacks the shipping manifest"
chmod 0500 "$work/plakd"
snapshot="$("$work/plakd" snapshot install --release-download \
  --from "$work/$stem.tar.gz" --manifest "$work/manifest.json")"
say "installed plakd $version: $HOME/.local/bin/plakd -> $snapshot/plakd"
case ":${PATH:-}:" in
  *":$HOME/.local/bin:"*) ;;
  *) say "add $HOME/.local/bin to PATH" ;;
esac
found="$(command -v plakd 2>/dev/null || true)"
[ -z "$found" ] || [ "$found" = "$HOME/.local/bin/plakd" ] || say "note: plakd on PATH is $found"
say "next: plakd doctor"
exit 0
fi

if [ "$mode" = link ]; then
  bin="$HOME/.local/lib/plak/release/$snapshot_id/plakd"
  [ -x "$bin" ] && [ ! -L "$bin" ] || die "staged snapshot is missing"
  exec "$bin" snapshot link "$snapshot_id"
fi

if [ -d "$local_source" ]; then
  bin="$local_source/plakd"
else
  [ -n "$manifest" ] || die "a local tarball requires --manifest FILE"
  # Make the pathname unambiguous to tar even when the caller used a dash.
  case "$local_source" in /*) ;; *) local_source="$PWD/$local_source" ;; esac
  work="$(mktemp -d "${TMPDIR:-/tmp}/plakd-install.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
  trap 'exit 1' HUP INT TERM
  member="$(tar -tf "$local_source" | awk '
    { path = $0; sub(/^\.\//, "", path); n = split(path, parts, "/") }
    (n == 1 && path == "plakd") || (n == 2 && parts[1] != "" && parts[1] != "." && parts[1] != ".." && parts[2] == "plakd") { print }
  ')"
  [ -n "$member" ] || die "artifact archive lacks the installer image"
  tar -xOf "$local_source" -- "$member" >"$work/plakd" || die "cannot extract installer image"
  bin="$work/plakd"
  chmod 0500 "$bin"
fi
[ -x "$bin" ] && [ ! -L "$bin" ] || die "artifact has no executable plakd installer"
set -- snapshot install --from "$local_source"
if [ -n "$manifest" ]; then set -- "$@" --manifest "$manifest"; fi
if [ -n "$no_link" ]; then set -- "$@" --no-link; fi
"$bin" "$@"
