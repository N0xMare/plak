#!/usr/bin/env bash
# Opt-in jj compatibility lane under the isolated test environment.
set -euo pipefail
cd "$(dirname "$0")/../.."
# shellcheck disable=SC1091
source tools/gate/dev-tools-prefix.sh
exec tools/dev/jj-tools.py compat "$(plak_dev_tools_prefix "$PWD")"
