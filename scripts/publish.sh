#!/usr/bin/env bash
# Package, verify and (only when asked) publish the crate cohort to crates.io.
# Usage: scripts/publish.sh [--gate | --execute]
#   (none)     dry run: refuse a dirty tree or one without passing `just full`
#              evidence, then package and verify every publishable crate
#   --gate     the gate's `package` group: package and verify, no preconditions
#   --execute  the dry run, then `cargo publish` each crate in dependency order
#              with CARGO_REGISTRY_TOKEN, waiting for each to appear in the
#              index before its dependents; crates already indexed are skipped
# PLAK_PUBLISH_INDEX (default https://index.crates.io) and PLAK_PUBLISH_TIMEOUT
# (seconds per crate, default 600) shape the index wait.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$root"

mode=dry-run
case "${1:-}" in
  '') ;;
  --gate) mode=gate; shift ;;
  --execute) mode=execute; shift ;;
  -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
esac
if [[ $# -gt 0 ]]; then echo "unknown argument: $1" >&2; exit 2; fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
refuse() { echo "publish: $*" >&2; exit 1; }

if [[ "$mode" != gate ]]; then
  if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
    refuse "the working tree is dirty; publish only a committed tree"
  fi
  tree="$(git rev-parse 'HEAD^{tree}')"
  evidence="$target_dir/gate-evidence/full"
  if [[ ! -f "$evidence" || "$(cat "$evidence")" != "$tree" ]]; then
    refuse "no passing \`just full\` evidence for tree $tree in $evidence"
  fi
fi
if [[ "$mode" == execute && -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  refuse "--execute needs CARGO_REGISTRY_TOKEN"
fi

# The published `plak` manifest exists only in a staged copy of the tree.
scratch="$(mktemp -d "${TMPDIR:-/tmp}/plak-publish.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
stage="$scratch/stage"
python3 tools/release/publish_shape.py stage "$stage"
cd "$stage"
# Verification builds live beside the gate's artifacts, apart from them.
export CARGO_TARGET_DIR="$target_dir/package"

python3 "$root/tools/release/publish_plan.py" order >"$scratch/order"
order=()
while read -r name version; do order+=("$name $version"); done <"$scratch/order"
if [[ ${#order[@]} -eq 0 ]]; then refuse "no publishable workspace member"; fi
selection=()
for entry in "${order[@]}"; do selection+=(-p "${entry%% *}"); done
echo "== publish order (${#order[@]} crates) =="
printf '  %s\n' "${order[@]}"

# One invocation packages the cohort: Cargo resolves the members against a
# local overlay of the packages it just built and verifies each in order.
# Cargo unpacks overlay packages into the cargo home under name-version and
# reuses an existing unpacked copy, so a cohort version packaged by an earlier
# run would be verified in place of this tree's. Drop every unpacked cohort
# copy first; Cargo unpacks again from the package or its cache.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
for entry in "${order[@]}"; do
  for unpacked in "$cargo_home"/registry/src/*/"${entry%% *}-${entry#* }"; do
    if [[ -d "$unpacked" ]]; then rm -rf -- "$unpacked"; fi
  done
done
echo "== cargo package --locked =="
cargo package --locked "${selection[@]}"

if [[ "$mode" != execute ]]; then
  echo "== ok: $mode packaged and verified ${#order[@]} crates; nothing uploaded =="
  exit 0
fi

index="${PLAK_PUBLISH_INDEX:-https://index.crates.io}"
timeout="${PLAK_PUBLISH_TIMEOUT:-600}"
indexed() { python3 "$root/tools/release/publish_plan.py" indexed "$index" "$1" "$2"; }
for entry in "${order[@]}"; do
  name="${entry%% *}"; version="${entry#* }"
  if indexed "$name" "$version"; then
    echo "== $name $version: already in the index, skipped =="
    continue
  fi
  echo "== cargo publish $name $version =="
  cargo publish --locked --no-verify -p "$name"
  waited=0
  until indexed "$name" "$version"; do
    if (( waited >= timeout )); then
      refuse "$name $version did not appear in $index within ${timeout}s; rerun to resume"
    fi
    sleep 5
    waited=$((waited + 5))
  done
done
echo "== ok: published ${#order[@]} crates =="
