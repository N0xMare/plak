#!/usr/bin/env bash
# Per-install native Linux x86_64 musl evidence. Default tier: --platform.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"
source tools/musl/musl-toolchain.sh
[[ $# -gt 0 ]] || set -- --platform
exec bash tools/gate/dev-check.sh "$@"
