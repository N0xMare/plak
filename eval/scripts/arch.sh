#!/usr/bin/env bash
# Shared HARBOR_ARCH resolution for the opt-in Harbor eval plane.
#
# The plane uploads native binaries into a Terminal-Bench cell, so one
# architecture has to govern the buildx platform, the artifact root and every
# pin check. HARBOR_ARCH is that switch: `amd64` or `arm64`, nothing else.
# Unset, it follows the Docker daemon's own architecture, so an Apple Silicon
# host with a native daemon gets arm64 and a Linux x86_64 host gets amd64.
# The Python side resolves the same way in eval/harbor_adapter/arch.py.

harbor_arch() {
  local want="${HARBOR_ARCH:-}"
  if [[ -n "$want" ]]; then
    case "$want" in
      amd64|arm64)
        printf '%s\n' "$want"
        return 0
        ;;
      *)
        echo "eval: HARBOR_ARCH must be amd64 or arm64 (got '$want')" >&2
        return 2
        ;;
    esac
  fi
  local probed
  probed="$(docker version --format '{{.Server.Arch}}' 2>/dev/null || true)"
  case "$probed" in
    amd64|x86_64|x64) printf 'amd64\n' ;;
    arm64|aarch64) printf 'arm64\n' ;;
    *)
      echo "eval: cannot read the Docker daemon architecture; set HARBOR_ARCH to amd64 or arm64" >&2
      return 2
      ;;
  esac
}

harbor_platform() {
  printf 'linux/%s\n' "$1"
}

harbor_installed_root() {
  printf '%s/.plak/installed/%s\n' "$1" "$2"
}

# ARM is a separately labelled lane. amd64 job names keep their existing
# spelling so the archived runs and any tooling that reads them stay valid.
harbor_job_suffix() {
  if [[ "$1" == "amd64" ]]; then
    printf ''
  else
    printf -- '-%s' "$1"
  fi
}
