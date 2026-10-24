# Local TUI review. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

[private]
default:
    @just --justfile "{{justfile()}}" --list tui

# Drive one declarative offline story through the real Plak PTY and write a
# normalized private trace. The default story covers settings and one turn.
[doc("drive one offline TUI story and write a normalized trace")]
story story="bin/plak/tests/stories/offline-control-center.json" trace="target/plak-test-tmp/tui-story-trace.json":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "$(dirname {{quote(trace)}})"
    rm -f {{quote(trace)}}
    PLAK_TUI_STORY="$PWD/{{story}}" PLAK_TUI_TRACE="$PWD/{{trace}}" \
      cargo test --locked -p plak --features test-hooks --test offline_e2e \
      run_declarative_tui_story -- --ignored --exact --nocapture
    echo "trace: {{trace}}"

# Render checked-in terminal frames for local visual review (never committed).
frames:
    #!/usr/bin/env bash
    set -euo pipefail
    out=target/tui-frame-svg
    rm -rf "$out" && mkdir -p "$out"
    for frame in bin/plak/tests/goldens/*.frame; do
      python3 tools/tui/frame_to_svg.py --input "$frame" \
        --output "$out/$(basename "${frame%.frame}").svg"
    done
    echo "SVG review set: $out"
