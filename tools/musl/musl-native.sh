#!/usr/bin/env bash
# Native Linux x86_64 musl evidence; by default build and test the full platform.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"
source tools/musl/musl-toolchain.sh
[[ $# -gt 0 ]] || set -- --steps static,builds,tests-parallel,tests-durability,tests-serial,tests-install
exec bash tools/gate/dev-check.sh "$@"
