#!/usr/bin/env bash
# Native Linux/macOS build and install of plak and its rquickjs and pinned
# upstream Monty sidecars. --install is the plain (non-maintainer) release
# build; the other modes are maintainer builds. --artifacts is the --install
# build and linkage check without installing; it prints the output directory.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
profile=debug
snapshot_flavor=maintainer
plak_features=(--features plak/maintainer)
artifacts_only=
case "${1:-}" in
  --release) profile=release; shift ;;
  --maintainer) profile=maintainer; shift ;;
  --install) profile=release; snapshot_flavor=release; plak_features=(); shift ;;
  --artifacts) profile=release; plak_features=(); artifacts_only=1; shift ;;
esac
if [[ $# -ne 0 ]]; then
  echo "usage: $0 [--release|--maintainer|--install|--artifacts]" >&2
  exit 2
fi

if [[ -z "$artifacts_only" ]]; then
# BEGIN LINKED WORKTREE GUARD
# This command also updates the installed symlinks. A build from a
# linked worktree must never silently repoint those daily-use commands away from
# the primary checkout.
git_dir="$(git rev-parse --absolute-git-dir)"
git_common_dir="$(git rev-parse --path-format=absolute --git-common-dir)"
if [[ "$git_dir" != "$git_common_dir" ]]; then
  echo "build-plak: refusing to build and relink installed binaries from a linked worktree" >&2
  exit 2
fi
# END LINKED WORKTREE GUARD
fi

profile_flag=()
if [[ "$profile" != debug ]]; then
  profile_flag=(--profile "$profile")
fi

# Separate resolutions keep monty-proto/worker features out of the parent.
cargo build --locked -p plak ${plak_features[@]+"${plak_features[@]}"} ${profile_flag[@]+"${profile_flag[@]}"}
cargo build --locked -p plakous-js-runtime -p plakous-py-worker ${profile_flag[@]+"${profile_flag[@]}"}
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"
output_dir="$target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}${profile}"
bin="$output_dir/plak"
js_host="$output_dir/plakous-js-host"
monty="$output_dir/monty"
if [[ ! -x "$bin" ]]; then
  echo "build-plak: cargo did not write $bin" >&2
  exit 2
fi
if [[ ! -x "$monty" ]]; then
  echo "build-plak: cargo did not write $monty" >&2
  exit 2
fi
if [[ ! -x "$js_host" ]]; then
  echo "build-plak: cargo did not write $js_host" >&2
  exit 2
fi

# Inspect raw symbols on both ELF and Mach-O; require worker positive controls.
python3 tools/build/check-artifact-linkage.py "$output_dir"
if [[ -n "$artifacts_only" ]]; then
  printf '%s\n' "$output_dir"
  exit 0
fi

if [[ "$profile" == debug ]]; then
  echo "build-plak: debug binary (slow TUI cold start); daily PATH CLI is just build-release" >&2
fi

install_link() {
  local dest="$1"
  local src="$2"
  local dest_dir
  if [[ -e "$dest" || -L "$dest" ]]; then
    if [[ ! -L "$dest" ]] || ! python3 - "$dest" "$HOME/.local/lib/plak/maintainer" "$HOME/.local/lib/plak/release" "$HOME/.local/lib/plak/sidecars" "$target_dir" <<'PY'
import os
import sys

target = os.path.realpath(sys.argv[1])
roots = [os.path.realpath(root) for root in sys.argv[2:]]
raise SystemExit(0 if any(os.path.commonpath([target, root]) == root for root in roots) else 1)
PY
    then
      echo "build-plak: refusing to replace non-Plak destination $dest; move it aside explicitly" >&2
      return 2
    fi
  fi
  dest_dir="$(dirname "$dest")"
  mkdir -p "$dest_dir"
  ln -sfn "$src" "$dest"
  echo "build-plak: installed $dest -> $src" >&2
}

# BEGIN SNAPSHOT INSTALL
# Install an immutable snapshot keyed by the SHA-256 of all three executables,
# so a rebuild that changes only a sidecar is a new snapshot too. Rebuilding
# never rewrites a file a running session executes; new launches (and /reload)
# pick up the new snapshot.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}
snapshot_root="${HOME}/.local/lib/plak/${snapshot_flavor}"
snapshot_id="$(printf 'plak %s\nplakous-js-host %s\nmonty %s\n' \
  "$(sha256_of "$bin")" "$(sha256_of "$js_host")" "$(sha256_of "$monty")" | sha256_of -)"
require_artifact_directory() {
  local facts mode
  [[ -d "$1" && ! -L "$1" ]] || { echo "build-plak: sidecar cache directory refused: $1" >&2; return 1; }
  facts="$(stat -f '%u %Lp' "$1" 2>/dev/null)" || facts="$(stat -c '%u %a' "$1")"
  mode="${facts##* }"
  if [[ "${facts%% *}" != "$(id -u)" ]] || (( (8#$mode & 0022) != 0 )); then
    echo "build-plak: unsafe sidecar cache directory refused: $1" >&2
    return 1
  fi
}
# Create missing ancestors individually: mkdir -p's intermediate directories
# inherit the umask. Resolve links before validating the physical owned chain,
# preserving linked homes without trusting writable installation ancestors.
ensure_install_directory() {
  local directory="$1" physical parent facts mode owner
  if [[ ! -d "$directory" ]]; then
    parent="$(dirname "$directory")"
    ensure_install_directory "$parent" || return 1
    mkdir -m 0755 "$directory" 2>/dev/null || [[ -d "$directory" ]] || return 1
  fi
  physical="$(cd "$directory" && pwd -P)" || return 1
  while [[ "$physical" != / ]]; do
    # BSD %Lp omits the sticky bit that %Mp carries.
    facts="$(stat -f '%u %Mp%Lp' "$physical" 2>/dev/null)" || facts="$(stat -c '%u %a' "$physical")"
    owner="${facts%% *}"
    mode="${facts##* }"
    # Root-owned sticky temporary roots are trusted system anchors, not
    # installation directories. Other foreign ownership fails closed.
    if [[ "$owner" == 0 ]] && (( (8#$mode & 01000) != 0 )); then
      :
    elif [[ "$owner" == "$(id -u)" ]]; then
      require_artifact_directory "$physical" || return 1
    elif [[ "$owner" != 0 ]] || (( (8#$mode & 0022) != 0 && (8#$mode & 01000) == 0 )); then
      echo "build-plak: unsafe installation ancestor refused: $physical" >&2
      return 1
    fi
    physical="$(dirname "$physical")"
  done
}
ensure_install_directory "$snapshot_root"
snapshot_root="$(cd "$snapshot_root" && pwd -P)"
require_artifact_directory "$snapshot_root"
artifact_root="$(dirname "$snapshot_root")/sidecars"
[[ ! -L "$artifact_root" ]] || { echo 'build-plak: symlinked sidecar cache refused' >&2; exit 1; }
mkdir -p -m 0755 "$artifact_root"
require_artifact_directory "$artifact_root"
artifact_root="$(cd "$artifact_root" && pwd -P)"
# The canonical worker path is runtime authority. A parent-only rebuild must
# not relocate unchanged workers, and historical workers are never auto-pruned.
install_sidecar() {
  local source="$1" name="$2" digest destination staging facts
  digest="$(sha256_of "$source")"
  destination="$artifact_root/$name/$digest"
  if [[ -L "$artifact_root/$name" || -L "$destination" ]]; then
    echo "build-plak: symlinked sidecar artifact directory refused: $destination" >&2
    return 1
  fi
  mkdir -p -m 0755 "$artifact_root/$name"
  require_artifact_directory "$artifact_root/$name" || return 1
  if [[ ! -d "$destination" ]]; then
    staging="$(mktemp -d "$artifact_root/.staging.XXXXXX")"
    cp "$source" "$staging/$name"
    chmod 0555 "$staging/$name"
    mv "$staging" "$destination" 2>/dev/null || true
    rm -rf "${destination:?}/$(basename "$staging")" "$staging"
  fi
  require_artifact_directory "$destination" || return 1
  if [[ ! -f "$destination/$name" || -L "$destination/$name" \
    || "$(sha256_of "$destination/$name")" != "$digest" ]]; then
    echo "build-plak: sidecar artifact integrity refused: $destination/$name" >&2
    return 1
  fi
  facts="$(stat -f '%u %Lp' "$destination/$name" 2>/dev/null)" \
    || facts="$(stat -c '%u %a' "$destination/$name")"
  if [[ "$facts" != "$(id -u) 555" ]]; then
    echo "build-plak: sidecar artifact permissions refused: $destination/$name" >&2
    return 1
  fi
  printf '%s\n' "$destination/$name"
}
installed_js="$(install_sidecar "$js_host" plakous-js-host)"
installed_monty="$(install_sidecar "$monty" monty)"
snapshot="$snapshot_root/$snapshot_id"
if [[ ! -d "$snapshot" ]]; then
  staging="$(mktemp -d "$snapshot_root/.staging.XXXXXX")"
  cp "$bin" "$staging/"
  chmod 0555 "$staging/plak"
  ln -s "$installed_js" "$staging/plakous-js-host"
  ln -s "$installed_monty" "$staging/monty"
  # The state key lets a later build find the snapshot that can still open a
  # session this one wrote, to export it for carry-forward.
  "$staging/plak" version --state-key >"$staging/state-key" 2>/dev/null \
    || rm -f "$staging/state-key"
  "$staging/plak" version --export-version >"$staging/export-version" 2>/dev/null \
    || rm -f "$staging/export-version"
  # A concurrent build of the same bytes may have won the rename; then mv nests
  # the staging copy inside it, and that duplicate is removed.
  mv "$staging" "$snapshot" 2>/dev/null || true
  rm -rf "${snapshot:?}/$(basename "$staging")" "$staging"
  [[ -x "$snapshot/plak" ]] || { echo "build-plak: could not install $snapshot" >&2; exit 1; }
fi
# Retain snapshots and historical producers until explicit cleanup. A stale
# pruning listing could otherwise delete a snapshot another installer reused.
bin="$snapshot/plak"
js_host="$installed_js"
monty="$installed_monty"
# END SNAPSHOT INSTALL


install_link "${HOME}/.local/bin/plak" "$bin"
install_link "${HOME}/.local/bin/plakous-js-host" "$js_host"
install_link "${HOME}/.local/bin/monty" "$monty"
cargo_plak="${CARGO_HOME:-$HOME/.cargo}/bin/plak"
if [[ -e "$cargo_plak" || -L "$cargo_plak" ]]; then
  install_link "$cargo_plak" "$bin"
fi
cargo_js="${CARGO_HOME:-$HOME/.cargo}/bin/plakous-js-host"
if [[ -e "$cargo_js" || -L "$cargo_js" ]]; then
  install_link "$cargo_js" "$js_host"
fi
cargo_monty="${CARGO_HOME:-$HOME/.cargo}/bin/monty"
if [[ -e "$cargo_monty" || -L "$cargo_monty" ]]; then
  install_link "$cargo_monty" "$monty"
fi
