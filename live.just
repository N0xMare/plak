# Opt-in live runs under tests/live. Paid except where a recipe says otherwise.
# Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# test-hooks plak plus the JavaScript and Monty workers, for PLAK_TEST_FAULTS.
_test-hooks:
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plak --features plak/test-hooks
    CARGO_INCREMENTAL=0 cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker

# Live smoke: a few minutes of short scenarios (coding task, resume, compaction,
# cancel, injected faults, a 2-child wave, the TUI) on Grok, OpenRouter,
# Codex or Claude OAuth. `--only` also selects opt-in long-session scenarios
# (tests/live/long_session.py); they are not in the default twelve.
# Paid, never in a gate. See tests/live/README.md. Artifacts under target/live/.
smoke *ARGS: _test-hooks
    python3 tests/live/smoke.py {{ARGS}}

# Live soak: one long session of real tasks on a pinned more-itertools checkout,
# with faults, SIGSTOP/SIGCONT sleeps, compactions and one plak rebuild.
# Usage: just live soak grok --hours 2
# `--long-session --only <names>` runs tests/live/long_session.py instead of
# the hour loop. Paid, never in a gate.
soak *ARGS: _test-hooks
    CARGO_INCREMENTAL=0 python3 tests/live/soak.py {{ARGS}}

# Multi-hour live TUI session with durable-terminal and independent file/test
# oracles, one simulated sleep, and a quit/reopen. Paid, never in a gate.
tui-soak *ARGS: _test-hooks
    python3 tests/live/tui_soak.py {{ARGS}}

# Bounded live background-job scenarios (promotion, steer, notices, crash,
# child settle, Ctrl+B, Processes S, idle wake and HUD). Paid, never in a gate.
# Usage: just live jobs grok --only promotion_budget
jobs *ARGS: _test-hooks
    python3 tests/live/jobs.py {{ARGS}}

# Opt-in local authorization servers + browser. Builds plak only (no workers).
# No accounts or inference; no gate.
mcp-oauth *ARGS:
    cargo build --release --locked -p plak --features plak/test-hooks
    python3 tests/live/mcp_oauth/run.py {{ARGS}}

# Opt-in census of the bundled API model tables. Networked, never a gate.
# Usage: just live census
#        just live census --write
census *ARGS:
    python3 scripts/api_model_census.py {{ARGS}}

# Dry-run spend for the planned live matrix. No build, no network.
# Usage: just live estimate --rate-card rates.json
estimate *ARGS:
    python3 tests/live/estimate.py {{ARGS}}

# Read-only daily-driver log evidence. No build or inference.
daily-driver *ARGS:
    python3 tests/live/daily_driver.py {{ARGS}}
