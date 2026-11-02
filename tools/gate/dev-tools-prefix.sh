#!/usr/bin/env bash
# Sourced by gates and tool installation; copies reuse their launch's pins.
plak_dev_tools_prefix() {
  local root=$1 launch
  if [[ -d "$root/.dev-tools/bin" ]]; then
    printf '%s\n' "$root/.dev-tools"
    return
  fi
  case "$root" in
    */.plak/worktrees/*) launch=${root%%/.plak/worktrees/*} ;;
    */.plak/workspaces/*) launch=${root%%/.plak/workspaces/*} ;;
    *) launch=$root ;;
  esac
  printf '%s\n' "$launch/.dev-tools"
}
