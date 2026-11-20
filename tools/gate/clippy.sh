#!/usr/bin/env bash
# The static group's all-feature clippy: the workspace or selected packages.
# Usage: tools/gate/clippy.sh [--status-file PATH] [--packages PACKAGE...]
# On failure the status file receives `clippy` and the exit status is 1.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"
status_file=""
scoped=false
selected=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --status-file) status_file="${2:?--status-file needs a path}"; shift 2 ;;
    --packages) scoped=true; shift; selected=("$@"); set -- ;;
    *) echo "usage: clippy.sh [--status-file PATH] [--packages PACKAGE...]" >&2; exit 2 ;;
  esac
done

target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
  target_args=(--target "$CARGO_BUILD_TARGET")
fi
failed=""
selection=(--workspace)
if [[ "$scoped" == true ]]; then
  if [[ ${#selected[@]} -eq 0 ]]; then
    echo "skip clippy: no Rust package touched"
    [[ -z "$status_file" ]] || : >"$status_file"
    exit 0
  fi
  selection=()
  for package in "${selected[@]}"; do selection+=(-p "$package"); done
fi
started=$SECONDS
echo "== clippy =="
if ! cargo clippy --locked ${selection[@]+"${selection[@]}"} --all-features --all-targets \
  ${target_args[@]+"${target_args[@]}"} -- -D warnings; then
  failed="clippy"
fi
echo "== clippy time: selected $((SECONDS - started)) s =="
if [[ -n "$status_file" ]]; then
  printf '%s' "$failed" >"$status_file"
fi
[[ -z "$failed" ]]
