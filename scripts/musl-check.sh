#!/usr/bin/env bash
# Offline Linux x86_64 musl qualification. Default tier: --full.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$root"
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo "musl qualification requires a Linux x86_64 Docker host" >&2
  exit 2
fi
tools="${PLAK_MUSL_TOOLS:-$HOME/plak-musl/bin}"
if [[ ! -x "$tools/cargo-nextest" ]]; then
  echo "musl cargo-nextest 0.9.100 required in $tools" >&2
  exit 2
fi
tools="$(cd "$tools" && pwd -P)"
target="${PLAK_MUSL_TARGET_DIR:-$root/target/musl-gate}"
mkdir -p "$target" "$root/target"
target="$(cd "$target" && pwd -P)"
image=plak-qual-base:185eaa28
# One Cargo cache per pinned image: a bumped image never inherits old crates.
cargo_home="$target/cargo-home-${image##*:}"
mkdir -p "$cargo_home"
source="$(mktemp -d "$root/target/musl-source.XXXXXX")"

# Keep the working-tree diff but never mount ignored host tools or artifacts.
git ls-files --cached --others --exclude-standard --deduplicate -z |
  while IFS= read -r -d '' path; do
    if [[ -e "$path" || -L "$path" ]]; then printf '%s\0' "$path"; fi
  done | tar --null -T - -cf - | tar -C "$source" -xf -
echo "musl source export: $source (retained)"
[[ $# -gt 0 ]] || set -- --full
status=0
docker run --rm --platform linux/amd64 --pull never --network none \
  --user "$(id -u):$(id -g)" \
  --tmpfs /dev/shm:rw,exec,size=16g \
  -v "$source:/qual/src" -v "$target:/qual/target" -v "$tools:/qual/tools:ro" \
  -v "$cargo_home:/qual/cargo-home" \
  -e "PLAK_PUBLIC_CONTRACTS_DIR=/qual/target/public-contracts-${image##*:}" \
  -e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN)}" \
  --entrypoint /bin/sh "$image" -c '
    set -eu
    export HOME="$(mktemp -d /tmp/plak-musl-home.XXXXXX)"
    export CARGO_HOME=/qual/cargo-home RUSTUP_HOME=/usr/local/rustup
    # Seed the writable offline cache once, not once per qualification run.
    if [ ! -f "$CARGO_HOME/.seeded" ]; then
      cp -R /usr/local/cargo/registry "$CARGO_HOME/"
      touch "$CARGO_HOME/.seeded"
    fi
    cd /qual/src
    # The timing ledger is beside Cargo artifacts, both on the writable mount.
    export PATH=/qual/tools:$PATH CARGO_TARGET_DIR=/qual/target/artifacts CARGO_NET_OFFLINE=true
    export CARGO_BUILD_TARGET=x86_64-unknown-linux-musl
    rustc -V; rustc -Vv | grep "^host:"; cargo nextest --version; python3 -V
    bash scripts/dev-check.sh "$@"' sh "$@" || status=$?
echo "EXIT $status"
exit "$status"
