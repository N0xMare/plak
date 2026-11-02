#!/usr/bin/env bash
# `just build <profile>`: debug, release, maintainer, or profiling.
set -euo pipefail
cd "$(dirname "$0")/../.."
case "${1:-}" in
  debug) exec tools/build/build-plak.sh ;;
  release) exec tools/build/build-plak.sh --release ;;
  maintainer) exec tools/build/build-plak.sh --maintainer ;;
  profiling) exec tools/build/build-plak.sh --profiling ;;
  *) echo "usage: just build debug|release|maintainer|profiling" >&2; exit 2 ;;
esac
