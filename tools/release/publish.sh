#!/usr/bin/env bash
# Package, verify and (only when asked) publish the crate cohort to crates.io.
# Usage: tools/release/publish.sh [--gate | --execute]
#   (none)     dry run: refuse a dirty tree or one without passing `just full`
#              evidence (for HEAD, or for a tree that differs from HEAD only in
#              documentation; see tools/release/gate_evidence.py), then package
#              and verify every publishable crate
#   --gate     package and verify without clean-tree/full-evidence preconditions
#   --execute  the dry run, then `cargo publish` each crate in dependency order
#              with CARGO_REGISTRY_TOKEN, waiting for each to appear in the
#              index before its dependents; crates already indexed are skipped
# PLAK_PUBLISH_INDEX (default https://index.crates.io) and PLAK_PUBLISH_TIMEOUT
# (seconds per crate, default 600) shape the index wait.
# Publication modes require more than a third of every compiled review window remaining.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"

mode=dry-run
case "${1:-}" in
  '') ;;
  --gate) mode=gate; shift ;;
  --execute) mode=execute; shift ;;
  -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
esac
if [[ $# -gt 0 ]]; then echo "unknown argument: $1" >&2; exit 2; fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
refuse() { echo "publish: $*" >&2; exit 1; }

# Qualification reports dates without a time-dependent gate; publication refuses stale windows.
if [[ "$mode" == gate ]]; then
  python3 tools/release/review_expiry.py --note
else
  python3 tools/release/review_expiry.py
fi

if [[ "$mode" != gate ]]; then
  if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
    refuse "the working tree is dirty; publish only a committed tree"
  fi
  # Prints the evidence tree, HEAD's tree and any documentation-only delta.
  python3 tools/release/gate_evidence.py verify --target-dir "$target_dir" || exit 1
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

# The body is the staged tree, lock, toolchain, cohort, Cargo environment and
# Cargo config, captured once before packaging. A later dry run or `--gate`
# reuses a receipt of that same body. `--execute` always verifies fresh. A
# miss, or any doubt, falls through and verifies. The receipt is written only
# after verification, from this body, and lives beside the gate target rather
# than in the package directory this script wipes.
receipt="$target_dir/gate-evidence/package-verification"
body="$scratch/verification-body.json"
if ! python3 "$root/tools/release/package_verification_cache.py" capture \
  --stage "$stage" --order "$scratch/order" --body "$body"; then
  echo "publish: verification inputs could not be fingerprinted; verifying fresh" >&2
  rm -f "$body"
fi
if [[ "$mode" != execute && -f "$body" ]] && python3 "$root/tools/release/package_verification_cache.py" hit \
  --body "$body" --receipt "$receipt"; then
  echo "publish: verification cache hit; skipping packaging and verification"
  echo "== ok: $mode reused the recorded verification of ${#order[@]} crates; nothing uploaded =="
  exit 0
fi

# One invocation packages the cohort: Cargo resolves the members against a
# local overlay of the packages it just built and verifies each in order.
# Cargo unpacks overlay packages into the cargo home under name-version and
# reuses an existing unpacked copy, and it treats a registry package's build in
# the verification target as fresh by package id alone. The cohort keeps its
# version across commits, so an earlier run's sources or artifacts would be
# verified in place of this tree's. Drop every unpacked cohort copy and the
# cohort's verification fingerprints first; Cargo unpacks and rebuilds them.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
for entry in "${order[@]}"; do
  name="${entry%% *}"
  for unpacked in "$cargo_home"/registry/src/*/"$name-${entry#* }" \
    "$CARGO_TARGET_DIR"/debug/.fingerprint/"$name"-*; do
    if [[ -d "$unpacked" ]]; then rm -rf -- "$unpacked"; fi
  done
done
# Resolve the actual archive graphs, which do not inherit workspace patches.
# Preparation never compiles or uploads; verification below stays offline.
echo "== package verification inputs (offline first) =="
python3 "$root/tools/release/offline_first.py" cargo package --locked --no-verify "${selection[@]}"
archives=()
for entry in "${order[@]}"; do
  archives+=("$CARGO_TARGET_DIR/package/${entry%% *}-${entry#* }.crate")
done
python3 "$root/tools/release/prefetch_packages.py" "$scratch/inputs" "${archives[@]}"
echo "== cargo package --locked (offline verification) =="
CARGO_NET_OFFLINE=true cargo package --locked "${selection[@]}"
if [[ -f "$body" ]] && ! python3 "$root/tools/release/package_verification_cache.py" record \
  --body "$body" --receipt "$receipt"; then
  echo "publish: verification succeeded but the receipt was not recorded" >&2
fi

if [[ "$mode" != execute ]]; then
  echo "== ok: $mode packaged and verified ${#order[@]} crates; nothing uploaded =="
  exit 0
fi

index="${PLAK_PUBLISH_INDEX:-https://index.crates.io}"
timeout="${PLAK_PUBLISH_TIMEOUT:-600}"
indexed() { python3 "$root/tools/release/publish_plan.py" indexed "$index" "$1" "$2"; }
publish_crate() {
  local name=$1 attempt=1 status delay diagnostic="$scratch/publish.log"
  while :; do
    if cargo publish --locked --no-verify -p "$name" >"$diagnostic" 2>&1; then
      cat "$diagnostic"
      return 0
    else
      status=$?
    fi
    cat "$diagnostic" >&2
    if (( attempt >= 5 )); then return "$status"; fi
    if ! delay=$(python3 "$root/tools/release/publish_retry.py" "$diagnostic" "$attempt"); then
      return "$status"
    fi
    echo "publish: $name rate limited; retry $attempt/4 in ${delay}s" >&2
    sleep "$delay"
    attempt=$((attempt + 1))
  done
}
for entry in "${order[@]}"; do
  name="${entry%% *}"; version="${entry#* }"
  if indexed "$name" "$version"; then
    echo "== $name $version: already in the index, skipped =="
    continue
  fi
  echo "== cargo publish $name $version =="
  publish_crate "$name"
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
