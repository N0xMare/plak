#!/usr/bin/env bash
# Run one exact-tree Mac gate in a dedicated, never-shared build directory.
# Usage: mac-checkout.sh REVISION COMMAND [ARG...]
set -euo pipefail
source_root="$(git rev-parse --show-toplevel)"
revision="$(git rev-parse --verify "${1:?expected revision}^{commit}")"
shift
[[ $# -gt 0 ]] || { echo 'expected a gate command' >&2; exit 2; }
checkout="${PLAK_MAC_GATE_CHECKOUT:-$source_root/.plak/gate-macos}"
case "$checkout" in /*) ;; *) checkout="$source_root/$checkout" ;; esac
mkdir -p "$(dirname "$checkout")"
lock="$checkout.lock"
if ! mkdir "$lock" 2>/dev/null; then
  echo "Mac gate checkout is locked: $lock (inspect its owner before removing it)" >&2
  exit 3
fi
echo "$$" >"$lock/pid"
trap 'rm -f "$lock/pid"; rmdir "$lock"' EXIT
trap 'exit 143' INT TERM HUP
if [[ -e "$checkout" ]]; then
  [[ -f "$checkout/.git" ]] || {
    echo 'Mac gate destination must be a dedicated linked worktree' >&2; exit 3;
  }
  if git -C "$checkout" symbolic-ref -q HEAD >/dev/null; then
    echo 'Mac gate destination must stay detached' >&2; exit 3
  fi
  actual="$(git -C "$checkout" rev-parse --show-toplevel)"
  [[ "$actual" == "$(cd "$checkout" && pwd -P)" ]] || {
    echo 'Mac gate destination is not a worktree root' >&2; exit 3;
  }
  [[ "$(git -C "$checkout" rev-parse --path-format=absolute --git-common-dir)" == \
     "$(git rev-parse --path-format=absolute --git-common-dir)" ]] || {
    echo 'Mac gate destination belongs to another repository' >&2; exit 3;
  }
  [[ -z "$(git -C "$checkout" status --porcelain --untracked-files=normal)" ]] || {
    echo 'Mac gate checkout is dirty; refusing to discard work' >&2; exit 3;
  }
  git -C "$checkout" checkout --detach "$revision"
else
  git worktree add --detach "$checkout" "$revision"
fi
if [[ ! -e "$checkout/.dev-tools" && -d "$source_root/.dev-tools/bin" ]]; then
  # Keep a real ignored directory: a top-level directory symlink is not
  # matched by the repository's .dev-tools/ ignore rule.
  mkdir "$checkout/.dev-tools"
  ln -s "$source_root/.dev-tools/bin" "$checkout/.dev-tools/bin"
  if [[ -d "$source_root/.dev-tools/jj" ]]; then
    ln -s "$source_root/.dev-tools/jj" "$checkout/.dev-tools/jj"
  fi
fi
cd "$checkout"
export CARGO_TARGET_DIR="$checkout/target"
echo "== Mac gate checkout: $(git rev-parse HEAD) tree=$(git rev-parse HEAD^{tree}) target=$CARGO_TARGET_DIR =="
"$@"
