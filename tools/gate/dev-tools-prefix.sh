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

# The jj the tests run: PLAK_TEST_JJ when set, even empty; else jj on PATH;
# else the newest pinned jj that `just dev-tools install` unpacked under the
# prefix; else jj, which then fails as missing.
plak_test_jj() {
  local root=$1 prefix tool version major minor patch key best=-1 newest=''
  if [[ -n "${PLAK_TEST_JJ+set}" ]]; then
    printf '%s\n' "$PLAK_TEST_JJ"
    return
  fi
  if ! command -v jj >/dev/null 2>&1; then
    prefix="$(plak_dev_tools_prefix "$root")"
    while read -r tool version _; do
      [[ "$tool" == jj && -x "$prefix/jj/$version/jj" ]] || continue
      IFS=. read -r major minor patch <<<"$version"
      key=$((10#$major * 1000000 + 10#$minor * 1000 + 10#$patch))
      if ((key > best)); then
        best=$key
        newest="$prefix/jj/$version/jj"
      fi
    done <"$root/tools/gate/dev-tools.pins"
  fi
  printf '%s\n' "${newest:-jj}"
}
