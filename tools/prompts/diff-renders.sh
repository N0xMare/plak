#!/usr/bin/env bash
# Diff a fresh prompt render against target/prompt-renders.prev.
set -euo pipefail
cd "$(dirname "$0")/../.."
rm -rf target/prompt-renders.prev
if [ -d target/prompt-renders ]; then cp -a target/prompt-renders target/prompt-renders.prev; fi
PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_
if [ -d target/prompt-renders.prev ]; then
  diff -ru target/prompt-renders.prev target/prompt-renders || true
else
  echo "no previous render; target/prompt-renders is the first one"
fi
