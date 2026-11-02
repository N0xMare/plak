#!/usr/bin/env bash
# Drive one declarative offline story through the real Plak PTY.
set -euo pipefail
cd "$(dirname "$0")/../.."
story="${1:-bin/plak/tests/stories/offline-control-center.json}"
trace="${2:-target/plak-test-tmp/tui-story-trace.json}"
mkdir -p "$(dirname "$trace")"
rm -f "$trace"
PLAK_TUI_STORY="$PWD/$story" PLAK_TUI_TRACE="$PWD/$trace" \
  cargo test --locked -p plak --features test-hooks --test offline_e2e \
  run_declarative_tui_story -- --ignored --exact --nocapture
echo "trace: $trace"
