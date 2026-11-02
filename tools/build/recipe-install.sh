#!/usr/bin/env bash
# `just install <flavor>`: release or maintainer.
set -euo pipefail
cd "$(dirname "$0")/../.."
case "${1:-}" in
  release) exec tools/build/build-plak.sh --install ;;
  maintainer) exec tools/build/build-plak.sh --install-maintainer ;;
  *) echo "usage: just install release|maintainer" >&2; exit 2 ;;
esac
