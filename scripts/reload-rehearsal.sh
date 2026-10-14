#!/usr/bin/env bash
# Stage and rehearse private copies; --install installs only after a passing run.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$repo_root/tools/build/reload_rehearsal.py" "$@"
