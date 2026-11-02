# Local prompt renders. Never committed. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set working-directory := '../..'

# Render prompt/wire/lifecycle artifacts into target/prompt-renders (git-ignored).
# Correctness is pinned in code (sha256 tables in rendered_prompts.rs /
# rendered_lifecycle.rs); this target only writes the human-readable render.
# Markdown files open with a harness-environment footprint (instructions + tools;
# excludes enrolled AGENTS.md and the scenario user/assignment message).
[doc("render prompt, wire and lifecycle artifacts into target/prompt-renders")]
render:
    PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_

# Diff a fresh render against the previous one (target/prompt-renders.prev).
diff:
    tools/prompts/diff-renders.sh

# Print the in-code prompt/lifecycle sha256 pin tables for re-pinning.
pins:
    PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_ -- --nocapture

# Capture native request shapes into target/prompt-captures/native/.
# Local mock responses; not live provider qualification.
[doc("capture native request shapes with local mock responses")]
native-compare *ARGS:
    tools/prompts/native_compare_prompts.py {{ARGS}}
