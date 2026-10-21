#!/bin/sh
# Install a Plak release from GitHub Releases into ~/.local without sudo:
#   curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/main/install.sh | sh
# PLAK_VERSION=0.0.1 selects a release (default: latest). The layout is the one
# `just install` writes: an immutable snapshot under ~/.local/lib/plak/release,
# content-addressed workers under ~/.local/lib/plak/sidecars, and links in
# ~/.local/bin. PLAK_RELEASE_BASE replaces https://github.com/<repo> (mirrors,
# tests). When `gh` is installed and signed in, the tarball's GitHub build
# attestation is verified too; PLAK_SKIP_ATTESTATION=1 skips that check.
set -eu
umask 022
repo=N0xMare/plak
base="${PLAK_RELEASE_BASE:-https://github.com/$repo}"
source_doc="https://github.com/$repo/blob/main/docs/user-guide/getting-started.md"

die() { echo "plak install: $*" >&2; exit 1; }
say() { echo "plak install: $*" >&2; }

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
command -v curl >/dev/null || die "curl is required"
command -v tar >/dev/null || die "tar is required"
[ -n "${HOME:-}" ] || die "HOME is not set"
fetch() {
  if [ -n "${PLAK_RELEASE_BASE:-}" ]; then curl -fsSL "$@"
  else curl -fsSL --proto =https --tlsv1.2 "$@"; fi
}

sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

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

work="$(mktemp -d "${TMPDIR:-/tmp}/plak-install.XXXXXX")"
trap 'rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM
stem="plak-$version-$triple"
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
for name in plak plakous-js-host monty; do
  [ -f "$work/$stem/$name" ] && [ ! -L "$work/$stem/$name" ] || die "$stem.tar.gz lacks $name"
done

# Same rules as tools/build/build-plak.sh: every installation directory and
# its physical ancestors are owned by this user (or root) and not group- or
# world-writable, except root-owned sticky temporary roots.
# "<uid> <mode>", with the sticky bit when $2 is "sticky". GNU `stat -f`
# reports the file system instead; that output is discarded.
owner_mode() {
  bsd='%Lp'; [ "${2:-}" != sticky ] || bsd='%Mp%Lp'
  facts="$(stat -f "%u $bsd" "$1" 2>/dev/null)" || facts="$(stat -c '%u %a' "$1")"
  echo "$facts"
}
safe_dir() {
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
}
make_dir() { mkdir -p "$1" && safe_dir "$1"; }

make_dir "$HOME/.local/lib/plak/release"
lib="$(cd "$HOME/.local/lib/plak" && pwd -P)"
snapshot_root="$lib/release"
make_dir "$lib/sidecars"

# Workers live at sidecars/<name>/<sha256>/<name>, mode 0555, shared with
# source installs; an existing artifact must match its digest.
install_sidecar() {
  digest="$(sha256_of "$work/$stem/$1")"
  dest="$lib/sidecars/$1/$digest"
  make_dir "$lib/sidecars/$1"
  if [ ! -d "$dest" ]; then
    staging="$(mktemp -d "$lib/sidecars/.staging.XXXXXX")"
    cp "$work/$stem/$1" "$staging/$1" && chmod 0555 "$staging/$1"
    mv "$staging" "$dest" 2>/dev/null || true
    rm -rf "${dest:?}/$(basename "$staging")" "$staging"
  fi
  safe_dir "$dest"
  [ -f "$dest/$1" ] && [ ! -L "$dest/$1" ] && [ "$(sha256_of "$dest/$1")" = "$digest" ] \
    && [ "$(owner_mode "$dest/$1")" = "$(id -u) 555" ] || die "sidecar artifact refused: $dest/$1"
}
install_sidecar plakous-js-host
installed_js="$dest/plakous-js-host"
install_sidecar monty
installed_monty="$dest/monty"

snapshot_id="$(printf 'plak %s\nplakous-js-host %s\nmonty %s\n' "$(sha256_of "$work/$stem/plak")" \
  "$(sha256_of "$work/$stem/plakous-js-host")" "$(sha256_of "$work/$stem/monty")" \
  | { if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi; } | cut -d' ' -f1)"
snapshot="$snapshot_root/$snapshot_id"
if [ ! -d "$snapshot" ]; then
  staging="$(mktemp -d "$snapshot_root/.staging.XXXXXX")"
  cp "$work/$stem/plak" "$staging/plak" && chmod 0555 "$staging/plak"
  ln -s "$installed_js" "$staging/plakous-js-host"
  ln -s "$installed_monty" "$staging/monty"
  "$staging/plak" version --write-binary-manifest "$staging" 2>/dev/null \
    || rm -f "$staging/binary-id"
  "$staging/plak" version --state-key >"$staging/state-key" 2>/dev/null || rm -f "$staging/state-key"
  "$staging/plak" version --export-version >"$staging/export-version" 2>/dev/null \
    || rm -f "$staging/export-version"
  mv "$staging" "$snapshot" 2>/dev/null || true
  rm -rf "${snapshot:?}/$(basename "$staging")" "$staging"
fi
[ -x "$snapshot/plak" ] || die "could not install $snapshot"

# Replace only links that already point into the Plak installation roots.
link() {
  if [ -e "$1" ] || [ -L "$1" ]; then
    current="$(readlink "$1" 2>/dev/null)" || die "refusing to replace non-Plak $1; move it aside"
    case "$current" in
      "$lib/maintainer/"*|"$lib/release/"*|"$lib/sidecars/"*) ;;
      *) die "refusing to replace non-Plak $1; move it aside" ;;
    esac
  fi
  ln -sfn "$2" "$1"
}
make_dir "$HOME/.local/bin"
link "$HOME/.local/bin/plak" "$snapshot/plak"
link "$HOME/.local/bin/plakous-js-host" "$installed_js"
link "$HOME/.local/bin/monty" "$installed_monty"
say "installed plak $version: $HOME/.local/bin/plak -> $snapshot/plak"
case ":${PATH:-}:" in
  *":$HOME/.local/bin:"*) ;;
  *) say "add $HOME/.local/bin to PATH" ;;
esac
found="$(command -v plak 2>/dev/null || true)"
[ -z "$found" ] || [ "$found" = "$HOME/.local/bin/plak" ] || say "note: plak on PATH is $found"
say "next: plak doctor"
