#!/usr/bin/env bash
# Report only; two clean detached checkouts and separate retained target dirs.
set -euo pipefail
if [[ $# != 1 || "$1" == --help ]]; then
  echo "usage: bash $0 NEW_EVIDENCE_DIRECTORY"
  [[ "${1:-}" == --help ]] && exit 0
  exit 2
fi
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo"
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
  echo "compare-shipping-builds: the working tree is not clean" >&2
  exit 2
fi
revision="$(git rev-parse HEAD)"
mkdir "$1"
evidence="$(cd "$1" && pwd -P)"
first="$evidence/checkout one"
second="$evidence/other-checkout"
cleanup() {
  for checkout in "$first" "$second"; do
    if [[ -f "$checkout/.git" ]]; then git worktree remove "$checkout"; fi
  done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
git worktree add --detach "$first" "$revision" >&2
git worktree add --detach "$second" "$revision" >&2
if [[ "$(uname -s)" == Linux ]]; then source tools/musl/musl-toolchain.sh; fi
export CARGO_INCREMENTAL=0
A="$(CARGO_TARGET_DIR="$evidence/target-a" bash "$first/tools/build/build-plakd.sh" --offline --artifacts)"
cp "$A/manifest.json" "$evidence/manifest-a.json"
B="$(CARGO_TARGET_DIR="$evidence/target-b" bash "$second/tools/build/build-plakd.sh" --offline --artifacts)"
cp "$B/manifest.json" "$evidence/manifest-b.json"
python3 - "$A/manifest.json" "$B/manifest.json" <<'PY'
import json
import sys
from pathlib import Path

left, right = (json.loads(Path(path).read_bytes()) for path in sys.argv[1:])
first = None
for name, facts in left["files"].items():
    a, b = facts["sha256"], right["files"][name]["sha256"]
    print(f"{name}: {'equal' if a == b else 'DIFFERENT'} {a} {b}")
    if a != b and first is None:
        first = name
print(f"First differing file: {first or 'none'}")
print(f"Canonical manifests: {'equal' if left == right else 'different'}")
PY
