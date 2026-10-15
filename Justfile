# Plak / Plakous local recipes (Linux/macOS). `just changed` for scoped verification,
# `just check` before each commit, `just full` at batch boundaries and before a push.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# fmt/clippy, input-scoped workspace/features checks, then the parallel lane over packages
# the working-tree diff touches (or the named ones) and their dependents. The
# serial wall-clock-budget lane is left to `just check`.
fast *PACKAGES:
    scripts/dev-check.sh --fast {{PACKAGES}}

# Linux pre-commit: fmt/clippy, structure checks, default/maintainer builds,
# parallel and serial lanes, public contracts and tooling. No durability lane.
check:
    scripts/dev-check.sh

# Conservative path-rule plan against BASE plus untracked files; prints reasons,
# then runs selected groups. Unknown paths widen to the whole check tier.
changed BASE="":
    scripts/dev-check.sh --changed {{quote(BASE)}}

# Exactly these comma-separated groups, in full-tier order (for independent lanes).
steps GROUPS:
    scripts/dev-check.sh --steps {{quote(GROUPS)}}

# Per install on macOS/musl: fmt/clippy, default/maintainer builds, parallel,
# serial and durability lanes. No platform-independent structure checkers or
# public compile contracts.
platform:
    scripts/dev-check.sh --platform

# Integrated union: check + rustdoc, durability lane,
# perf smoke and LOC report (both informational).
full:
    scripts/dev-check.sh --full

# Linux x86_64: pinned offline Docker musl qualification; defaults to full.
musl-check *ARGS:
    bash scripts/musl-check.sh {{ARGS}}

# Linux x86_64 with the rustup musl target and musl-gcc; defaults to platform.
musl-native *ARGS:
    bash scripts/musl-native.sh {{ARGS}}

dev-tools *ARGS:
    bash scripts/dev-tools.sh {{ARGS}}

# Offline nextest. Three invocations of one profile: the parallel lane, the
# serial wall-clock-budget lane, then the durability lane. `just check` omits
# the durability lane. See .config/nextest.toml and docs/spec/05-testing-and-evidence.md.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    TMPDIR="$(bash scripts/test-tmpdir.sh)"
    export TMPDIR
    isolated_home="$TMPDIR/plak-isolated-home"
    mkdir -p "$isolated_home"
    chmod 700 "$isolated_home"
    export PLAK_HOME="$isolated_home"
    cargo nextest run --locked --workspace --all-features --profile offline \
      -E "$(python3 tools/checks/budget_filterset.py --exclude)"
    cargo nextest run --locked --workspace --all-features --profile offline \
      --test-threads=1 -E "$(python3 tools/checks/budget_filterset.py)"
    cargo nextest run --locked --workspace --all-features --profile offline \
      --test-threads=1 -E "$(python3 tools/checks/budget_filterset.py --durability)"

# Line/region/function coverage for the offline gate, both lanes merged.
# Needs: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov
# Writes target/llvm-cov/html/html/index.html and target/llvm-cov/coverage.json.
# Uses its own CARGO_TARGET_DIR so it never invalidates the normal build.
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v cargo-llvm-cov >/dev/null || { echo "install: cargo install cargo-llvm-cov" >&2; exit 1; }
    mkdir -p target/llvm-cov
    TMPDIR="$(bash scripts/test-tmpdir.sh)"
    export TMPDIR
    isolated_home="$TMPDIR/plak-isolated-home"
    mkdir -p "$isolated_home"
    chmod 700 "$isolated_home"
    export PLAK_HOME="$isolated_home"
    export CARGO_TARGET_DIR="$PWD/target/llvm-cov"
    export CARGO_INCREMENTAL=0
    exclude="$(python3 tools/checks/budget_filterset.py --exclude)"
    include="$(python3 tools/checks/budget_filterset.py)"
    cargo llvm-cov clean --workspace
    cargo llvm-cov --no-report --no-cfg-coverage nextest --locked --workspace --all-features \
      --profile offline -E "$exclude"
    cargo llvm-cov --no-report --no-cfg-coverage nextest --locked --workspace --all-features \
      --profile offline --test-threads=1 -E "$include"
    cargo llvm-cov report --html --output-dir "$CARGO_TARGET_DIR/html"
    cargo llvm-cov report --json --output-path "$CARGO_TARGET_DIR/coverage.json"
    cargo llvm-cov report --summary-only

# Live smoke: a few minutes of short scenarios (coding task, resume, compaction,
# cancel, injected faults, a 2-child wave, the TUI) on Grok, OpenRouter,
# Codex or Claude OAuth. `--only` also selects opt-in long-session scenarios
# (tests/live/long_session.py); they are not in the default twelve.
# Paid, never in a gate. Builds a test-hooks plak for PLAK_TEST_FAULTS; see
# tests/live/README.md. Artifacts under target/live/.
smoke *ARGS:
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plak --features plak/test-hooks
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker
    python3 tests/live/smoke.py {{ARGS}}

# Live soak: one long session of real tasks on a pinned more-itertools checkout,
# with faults, SIGSTOP/SIGCONT sleeps, compactions and one plak rebuild.
# Usage: just soak grok --hours 2
# `--long-session --only <names>` runs tests/live/long_session.py instead of
# the hour loop. Paid, never in a gate.
soak *ARGS:
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plak --features plak/test-hooks
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker
    CARGO_INCREMENTAL=0 python3 tests/live/soak.py {{ARGS}}

# Multi-hour live TUI session with durable-terminal and independent file/test
# oracles, one simulated sleep, and a quit/reopen. Paid, never in a gate.
tui-soak *ARGS:
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plak --features plak/test-hooks
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker
    python3 tests/live/tui_soak.py {{ARGS}}

# Bounded live background-job scenarios (promotion, steer, notices, crash,
# child settle, Ctrl+B, Processes S, idle wake and HUD). Paid, never in a gate.
# Usage: just jobs grok --only promotion_budget
jobs *ARGS:
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plak --features plak/test-hooks
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker
    python3 tests/live/jobs.py {{ARGS}}

# Dry-run spend for the planned live matrix. No build, no network.
# Usage: just estimate --rate-card rates.json
estimate *ARGS:
    python3 tests/live/estimate.py {{ARGS}}

# Read-only daily-driver log evidence. No build or inference.
daily-driver *ARGS:
    python3 tests/live/daily_driver.py {{ARGS}}

# Offline performance smoke over the scripted model. Informational: it reports
# six metrics and compares them against this machine's seed row in
# tests/performance/baselines/offline-perf-smoke.json. See docs/spec/05.
perf-smoke *ARGS:
    scripts/perf-smoke.sh {{ARGS}}

fmt:
    cargo fmt --all

clippy:
    cargo clippy --locked --workspace --all-features --all-targets -- -D warnings

# Workspace rustdoc as the gate runs it: broken intra-doc links, links from
# public docs into private items, and bare URLs are errors.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked

# Plain release build of plak and its JavaScript and Monty workers, installed
# into ~/.local/bin as an immutable snapshot under ~/.local/lib/plak/release.
install:
    tools/build/build-plak.sh --install

# Native Linux/macOS maintainer binary (debug). Builds plak plus the JavaScript and Monty workers.
build:
    tools/build/build-plak.sh

# Same as `just build`, release profile (`target/release/plak`). Maintainer daily PATH CLI.
build-release:
    tools/build/build-plak.sh --release

# Dogfood build: release codegen without LTO, incremental, with line tables.
# Installs a new immutable snapshot; `/reload` in a running TUI resumes on it.
build-maintainer:
    tools/build/build-plak.sh --maintainer

# Offline preflight on a private backup. A changed state key rehearses
# carry-forward in disposable homes. --wait-idle retries three idle export refusals at
# 15 s intervals (--interval S overrides), avoiding large-store backup thrash;
# --detach --log <path> survives the caller's turn; --install installs on success.
reload-rehearsal uuid *ARGS:
    bash scripts/reload-rehearsal.sh {{quote(uuid)}} {{ARGS}}

# Symlink ~/.local/bin/plak-dev (repairing a stale one) and seed the isolated
# ~/plak-dev-home playground.
plak-dev-install:
    scripts/plak-dev.sh --dev-install

# Drive one declarative offline story through the real Plak PTY and write a
# normalized private trace. The default story covers settings and one turn.
tui-story story="bin/plak/tests/stories/offline-control-center.json" trace="target/plak-test-tmp/tui-story-trace.json":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "$(dirname {{quote(trace)}})"
    rm -f {{quote(trace)}}
    PLAK_TUI_STORY="$PWD/{{story}}" PLAK_TUI_TRACE="$PWD/{{trace}}" \
      cargo test --locked -p plak --features test-hooks --test offline_e2e \
      run_declarative_tui_story -- --ignored --exact --nocapture
    echo "trace: {{trace}}"

# Render checked-in terminal frames for local visual review (never committed).
tui-frame-svg:
    #!/usr/bin/env bash
    set -euo pipefail
    out=target/tui-frame-svg
    rm -rf "$out" && mkdir -p "$out"
    for frame in bin/plak/tests/goldens/*.frame; do
      python3 tools/tui/frame_to_svg.py --input "$frame" \
        --output "$out/$(basename "${frame%.frame}").svg"
    done
    echo "SVG review set: $out"

# Wipe ~/plak-dev-home sqlite/sessions; keep config.toml.
plak-dev-reset:
    scripts/plak-dev.sh --dev-reset-home

# Profiling binary: release codegen, full symbols, no LTO (target/profiling/plak).
build-profiling:
    cargo build --locked --profile profiling -p plak

# Sample a plak invocation with samply (Linux/macOS). Usage: just samply agent --help
samply *ARGS: build-profiling
    command -v samply >/dev/null || { echo "install: cargo install samply" >&2; exit 1; }
    samply record target/profiling/plak {{ARGS}}

# Flamegraph a plak invocation (perf on Linux, xctrace on macOS).
flamegraph *ARGS: build-profiling
    command -v cargo-flamegraph >/dev/null || { echo "install: cargo install flamegraph" >&2; exit 1; }
    cargo flamegraph --profile profiling --bin plak -- {{ARGS}}

# Hardware counters for a plak invocation (Linux only).
perf-stat *ARGS: build-profiling
    command -v perf >/dev/null || { echo "perf not installed" >&2; exit 1; }
    perf stat -e cycles,instructions,branches,branch-misses,cache-misses,context-switches,page-faults target/profiling/plak {{ARGS}}

# Binary size attribution for the default release build.
bloat:
    command -v cargo-bloat >/dev/null || { echo "install: cargo install cargo-bloat" >&2; exit 1; }
    cargo bloat --release -p plak --crates -n 40

# Production LOC density report (informational).
density:
    python3 tools/checks/check_prod_loc.py

# Render prompt/wire/lifecycle artifacts into target/prompt-renders (git-ignored).
# Correctness is pinned in code (sha256 tables in rendered_prompts.rs /
# rendered_lifecycle.rs); this target only writes the human-readable render.
# Markdown files open with a harness-environment footprint (instructions + tools;
# excludes enrolled AGENTS.md and the scenario user/assignment message).
prompts:
    PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_

# Diff a fresh render against the previous one (target/prompt-renders.prev).
prompts-diff:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf target/prompt-renders.prev
    if [ -d target/prompt-renders ]; then cp -a target/prompt-renders target/prompt-renders.prev; fi
    PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_
    if [ -d target/prompt-renders.prev ]; then
        diff -ru target/prompt-renders.prev target/prompt-renders || true
    else
        echo "no previous render; target/prompt-renders is the first one"
    fi

# Print the in-code prompt/lifecycle sha256 pin tables for re-pinning.
prompts-pins:
    PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_ -- --nocapture

# Capture native request shapes into target/prompt-captures/native/.
# Local mock responses; not live provider qualification.
native-compare-prompts *ARGS:
    python3 scripts/native_compare_prompts.py {{ARGS}}

# Opt-in census of the bundled API model tables. Networked, never a gate.
# Usage: just api-census
#        just api-census --write
api-census *ARGS:
    python3 scripts/api_model_census.py {{ARGS}}

# eval: the opt-in, paid Harbor / Terminal-Bench bakeoff. Not in any
# gate tier. See eval/README.md.

# Zero-inference adapter contracts for the Harbor plane.
harbor-test:
    python3 -m unittest discover -s eval/tests -t eval/tests

harbor-bootstrap:
    eval/scripts/harbor.sh bootstrap

check-harbor:
    eval/scripts/harbor.sh check

build-harbor-agent:
    eval/scripts/harbor.sh build-agent

fetch-harbor-vendor-clis:
    eval/scripts/harbor.sh fetch-vendor-clis

fetch-harbor-pmux:
    eval/scripts/harbor.sh fetch-pmux

harbor-eval *ARGS:
    eval/scripts/harbor.sh eval {{ARGS}}

harbor-view:
    eval/scripts/harbor.sh view

# Reward plus harness termination per trial, over archived job trees. No cost.
harbor-summarize *ARGS:
    python3 eval/scripts/summarize.py {{ARGS}}

harbor-smoke *ARGS:
    eval/scripts/smoke.sh {{ARGS}}

# Rung A: plumbing (local tiny tasks) then cheap TB 2.1 A/B. Paid, not just check.
harbor-rung-a *ARGS:
    eval/scripts/rung-a.sh {{ARGS}}

# Rung B: one TB 4.0 task to prove wiring. Never the 66-task campaign.
harbor-rung-b *ARGS:
    eval/scripts/rung-b.sh {{ARGS}}

# Dependency policy (when deps change).
deny:
    bash scripts/dev-tools.sh deny

# Opt-in indexed SQLite crash-point sweep of the real offline binary.
crash-sweep *ARGS:
    python3 scripts/crash-sweep.py {{ARGS}}
