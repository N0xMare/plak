#!/usr/bin/env bash
# The static group's clippy: the workspace, then each supported package alone.
# Usage: tools/gate/clippy.sh [--status-file PATH]
# On failure the status file receives the same names the gate summarizes
# (`clippy` and `clippy:<package>`), and the exit status is 1. Every package
# still runs. A supported-package list that does not match the manifest writes
# `clippy:packages` and exits before cargo.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"
status_file=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --status-file) status_file="${2:?--status-file needs a path}"; shift 2 ;;
    *) echo "usage: clippy.sh [--status-file PATH]" >&2; exit 2 ;;
  esac
done

target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
  target_args=(--target "$CARGO_BUILD_TARGET")
fi
# Same five as dev-check.sh supported_packages (spec 06 §3). The manifest
# class is the source of truth; a copied gate under test has no workspace
# manifest, and the real tree does.
packages=(plakous plakous-agent plakous-model plakous-tools plak)
if [[ -f Cargo.toml ]]; then
  if ! python3 - "${packages[@]}" <<'PY'
import sys
import tomllib
from pathlib import Path

expected = sys.argv[1:]
root = Path(".").resolve()
workspace = tomllib.loads((root / "Cargo.toml").read_text())
found = []
for member in workspace["workspace"]["members"]:
    manifest = tomllib.loads((root / member / "Cargo.toml").read_text())
    package = manifest.get("package") or {}
    meta = package.get("metadata", {}).get("plakous", {})
    if meta.get("class") == "supported":
        found.append(package["name"])
if sorted(found) != sorted(expected):
    raise SystemExit(
        "supported packages "
        + " ".join(expected)
        + " do not match manifest class supported: "
        + " ".join(sorted(found))
    )
PY
  then
    # Write before exiting. An empty status file is not a failed step.
    if [[ -n "$status_file" ]]; then
      printf '%s' 'clippy:packages' >"$status_file"
    fi
    exit 1
  fi
fi
failed=""
echo "== clippy =="
if ! cargo clippy --locked --workspace --all-features --all-targets \
  ${target_args[@]+"${target_args[@]}"} -- -D warnings; then
  failed="clippy"
fi
for package in "${packages[@]}"; do
  echo "== clippy: $package alone =="
  if ! cargo clippy --locked -p "$package" --all-targets \
    ${target_args[@]+"${target_args[@]}"} -- -D warnings; then
    failed="${failed:+$failed }clippy:$package"
  fi
done
if [[ -n "$status_file" ]]; then
  printf '%s' "$failed" >"$status_file"
fi
[[ -z "$failed" ]]
