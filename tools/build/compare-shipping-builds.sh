#!/usr/bin/env bash
# Report only; two clean detached checkouts and separate Cargo homes and targets.
set -euo pipefail
if [[ $# != 1 || -z "$1" || "$1" == --help ]]; then
  echo "usage: $0 NEW_EVIDENCE_DIRECTORY"
  [[ "${1:-}" == --help ]] && exit 0
  exit 2
fi
evidence="$(python3 -c 'import sys; from pathlib import Path; print(Path(sys.argv[1]).resolve())' "$1")"
cache="${CARGO_HOME:-$HOME/.cargo}/registry"
[[ "$cache" == /* ]] || cache="$PWD/$cache"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo"
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
  echo "compare-shipping-builds: the working tree is not clean" >&2
  exit 2
fi
revision="$(git rev-parse HEAD)"
umask 077
mkdir "$evidence"
first="$evidence/checkout one"
second="$evidence/other-checkout"
cargo_a="$evidence/cargo-a"
cargo_b="$evidence/cargo-b"
# Copy only registry inputs, not the caller's config, credentials or unpacked sources.
for cargo_home in "$cargo_a" "$cargo_b"; do
  mkdir -p "$cargo_home/registry"
  cp -a "$cache/cache" "$cache/index" "$cargo_home/registry/"
done
cleanup() {
  for checkout in "$first" "$second"; do
    if [[ -f "$checkout/.git" ]]; then git worktree remove "$checkout"; fi
  done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
git worktree add --detach "$first" "$revision" >&2
git worktree add --detach "$second" "$revision" >&2
export CARGO_INCREMENTAL=0
A="$(CARGO_HOME="$cargo_a" CARGO_TARGET_DIR="$evidence/target-a" bash "$first/tools/build/build-plakd.sh" --offline --artifacts)"
cp "$A/manifest.json" "$evidence/manifest-a.json"
B="$(CARGO_HOME="$cargo_b" CARGO_TARGET_DIR="$evidence/target-b" bash "$second/tools/build/build-plakd.sh" --offline --artifacts)"
cp "$B/manifest.json" "$evidence/manifest-b.json"
python3 - "$repo/tools/build" "$A" "$B" "$first" "$second" <<'PY'
import json
import sys
from pathlib import Path

module_dir, artifacts_a, artifacts_b, checkout_a, checkout_b = sys.argv[1:]
sys.path.insert(0, module_dir)
from artifact_manifest import check_paths

check_paths(Path(artifacts_a), (checkout_b,))
check_paths(Path(artifacts_b), (checkout_a,))
left, right = (
    json.loads((Path(directory) / "manifest.json").read_bytes())
    for directory in (artifacts_a, artifacts_b)
)
first = None
for name, facts in left["files"].items():
    a, b = facts["sha256"], right["files"][name]["sha256"]
    print(f"{name}: {'equal' if a == b else 'DIFFERENT'} {a} {b}")
    if a != b and first is None:
        first = name
print(f"First differing file: {first or 'none'}")
print(f"Canonical manifests: {'equal' if left == right else 'different'}")
PY
