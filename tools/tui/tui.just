# Local TUI review. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set working-directory := '../..'

[private]
default:
    @just --justfile "{{justfile()}}" --list tui

# Drive one declarative offline story through the real Plak PTY and write a
# normalized private trace. The default story covers settings and one turn.
[doc("drive one offline TUI story and write a normalized trace")]
story story="bin/plak/tests/stories/offline-control-center.json" trace="target/plak-test-tmp/tui-story-trace.json":
    tools/tui/story.sh {{quote(story)}} {{quote(trace)}}

# Render checked-in terminal frames for local visual review (never committed).
frames:
    tools/tui/frames.sh
