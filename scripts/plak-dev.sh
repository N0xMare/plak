#!/usr/bin/env bash
# Isolated maintainer playground: same CLI/TUI as product, wipeable PLAK_HOME,
# login-user OAuth (not PLAK_HOME). Does not touch ~/.plak.
#
# Usage:
#   just build-release         # build release maintainer (target/release/plak)
#   just build                 # debug maintainer (target/debug/plak)
#   PLAK_DEV_PROFILE=debug     # use target/debug/plak instead
#   PLAK_DEV_PROFILE=maintainer  # use target/maintainer/plak (just build-maintainer)
#   just plak-dev-install      # symlink ~/.local/bin/plak-dev
#   just plak-dev-reset        # wipe sqlite/sessions; keep config.toml
#   plak-dev …                 # run like plak (cwd is the session workspace)
set -euo pipefail

script_path="$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "${BASH_SOURCE[0]}")"
repo_root="$(cd "$(dirname "$script_path")/.." && pwd)"
operator_home="$(
  python3 -c 'import os, pwd; print(pwd.getpwuid(os.getuid()).pw_dir)'
)"
product_home="${PLAK_DEV_PLAK_HOME:-$operator_home/plak-dev-home}"
profile="${PLAK_DEV_PROFILE:-release}"
case "$profile" in
  debug | release | maintainer) ;;
  *)
    echo "plak-dev: PLAK_DEV_PROFILE must be debug, release or maintainer (got: $profile)" >&2
    exit 2
    ;;
esac
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"
bin="$target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}${profile}/plak"

case "$product_home" in
  "$repo_root" | "$repo_root"/*)
    echo "plak-dev: PLAK_HOME must stay outside the git tree ($product_home)" >&2
    exit 2
    ;;
esac

if [[ "$product_home" != /* ]]; then
  echo "plak-dev: PLAK_HOME must be absolute" >&2
  exit 2
fi

# Seed the playground config from the annotated reference preset; there is
# no second copy of the schema here. `bin/plak/src/product_config.rs`
# (`ProductConfig::parse` and `parse_template`) is the schema authority and
# `configs/reference/` loads under it in tests.
seed_config() {
  local dest="$1"
  cp "$repo_root/configs/reference/config.toml" "$dest"
  chmod 600 "$dest"
}

# The reference preset's four role templates: root-agent delegates,
# worker/reviewer/scout are leaves.
seed_templates() {
  local dir="$1"
  mkdir -m 700 -p "$dir"
  cp "$repo_root"/configs/reference/agents/*.toml "$dir/"
  chmod 600 "$dir"/*.toml
}

ensure_home() {
  mkdir -m 700 -p "$product_home"
  chmod 700 "$product_home"
  if [[ ! -f "$product_home/config.toml" ]]; then
    seed_config "$product_home/config.toml"
  fi
  if [[ ! -d "$product_home/agents" ]]; then
    seed_templates "$product_home/agents"
  fi
}

reset_home() {
  ensure_home
  # The whole state root plus the disposable telemetry/log roots; `state/`
  # holds every build's keyed state directory (databases and sessions).
  rm -rf "$product_home/state" "$product_home/telemetry" "$product_home/logs" \
    "$product_home/secrets"
  echo "plak-dev: reset $product_home (kept config.toml and agents/; OAuth untouched)"
}

install_shim() {
  local bindir="$operator_home/.local/bin"
  local shim="$bindir/plak-dev"
  local target="$repo_root/scripts/plak-dev.sh"
  mkdir -p "$bindir"
  # Repair, do not merely overwrite. An install from before this script existed
  # left `plak-dev` pointing at another script; `ln -sfn` cannot replace a
  # symlink that resolves to a directory, and a stray regular file would be
  # kept. Remove whatever is there, then verify what we created.
  if [[ -e "$shim" || -L "$shim" ]]; then
    local previous
    previous="$(readlink "$shim" || echo "<not a symlink>")"
    if [[ "$previous" != "$target" ]]; then
      echo "plak-dev: replacing stale $shim -> $previous"
    fi
    # Never recursive: a real directory at the shim path is an operator's, not
    # ours to delete. Refuse and say so.
    if [[ -d "$shim" && ! -L "$shim" ]]; then
      echo "plak-dev: $shim is a directory; refusing to remove it" >&2
      exit 2
    fi
    rm -f "$shim"
  fi
  ln -s "$target" "$shim"
  local resolved
  resolved="$(readlink "$shim")"
  if [[ "$resolved" != "$target" ]]; then
    echo "plak-dev: $shim resolves to $resolved, not $target" >&2
    exit 2
  fi
  # The playground home is part of the install, not a side effect of the first
  # launch: `just plak-dev-install` must leave a usable PLAK_HOME behind.
  ensure_home
  echo "plak-dev: installed $shim -> $target"
  echo "plak-dev: PLAK_HOME is $product_home (config.toml seeded)"
  case ":$PATH:" in
    *":$bindir:"*) ;;
    *)
      echo "plak-dev: add $bindir to PATH if \`plak-dev\` is not found" >&2
      ;;
  esac
}

if [[ "${1:-}" == "--dev-reset-home" ]]; then
  reset_home
  exit 0
fi
if [[ "${1:-}" == "--dev-install" ]]; then
  install_shim
  exit 0
fi

ensure_home

if [[ ! -x "$bin" ]]; then
  echo "plak-dev: missing $bin — run \`just build\` or \`just build-release\` (profile=$profile) from the repo" >&2
  exit 2
fi

export HOME="$operator_home"
export PLAK_HOME="$product_home"
unset OPENAI_API_KEY XAI_API_KEY ANTHROPIC_API_KEY OPENROUTER_API_KEY
# Ordinary development uses the maintainer binary's passive, home-local
# capture, not the fail-closed qualification hook or the compiled checkout.
# Do not change cwd or wrap the binary's signals.
unset PLAK_TEST_EXCHANGE_CAPTURE_DIR
exec "$bin" "$@"
