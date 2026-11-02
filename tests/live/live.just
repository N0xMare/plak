# Opt-in live runs under tests/live. Paid except where a recipe says otherwise.
# Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set working-directory := '../..'

[private]
default:
    @just --justfile "{{justfile()}}" --list live

# test-hooks plak plus the JavaScript and Monty workers, for PLAK_TEST_FAULTS.
_test-hooks:
    CARGO_INCREMENTAL=0 tools/build/build-plak.sh --release --features plak/test-hooks

# Parent only. The MCP OAuth driver does not start workers.
_test-hooks-plak:
    tools/build/build-plak.sh --release --features plak/test-hooks --plak-only

# Live smoke: a few minutes of short scenarios (coding task, resume, compaction,
# cancel, injected faults, a 2-child wave, the TUI) on Grok, OpenRouter,
# Codex or Claude OAuth. `--only` also selects opt-in long-session scenarios
# (tests/live/long_session.py); they are not in the default twelve.
# Paid, never in a gate. See tests/live/README.md. Artifacts under target/live/.
[doc("short live scenarios on one route; paid, not a gate")]
smoke *ARGS: _test-hooks
    tests/live/smoke.py {{ARGS}}

# Live soak: one long session of real tasks on a pinned more-itertools checkout,
# with faults, SIGSTOP/SIGCONT sleeps, compactions and one plak rebuild.
# Usage: just live soak grok --hours 2
# `--long-session --only <names>` runs tests/live/long_session.py instead of
# the hour loop. Paid, never in a gate.
[doc("one long live session with faults and a rebuild; paid, not a gate")]
soak *ARGS: _test-hooks
    CARGO_INCREMENTAL=0 tests/live/soak.py {{ARGS}}

# Multi-hour live TUI session with durable-terminal and independent file/test
# oracles, one simulated sleep, and a quit/reopen. Paid, never in a gate.
[doc("multi-hour live TUI session with file and test oracles; paid, not a gate")]
tui-soak *ARGS: _test-hooks
    tests/live/tui_soak.py {{ARGS}}

# Bounded live background-job scenarios (promotion, steer, notices, crash,
# child settle, Ctrl+B, Processes S, idle wake and HUD). Paid, never in a gate.
# Usage: just live jobs grok --only promotion_budget
[doc("bounded live background-job scenarios; paid, not a gate")]
jobs *ARGS: _test-hooks
    tests/live/jobs.py {{ARGS}}

# Opt-in local authorization servers + browser. Builds plak only (no workers).
# No accounts or inference; no gate.
[doc("local MCP OAuth servers and a browser; no accounts or inference")]
mcp-oauth *ARGS: _test-hooks-plak
    tests/live/mcp_oauth/run.py {{ARGS}}

# Opt-in census of the bundled API model tables. Networked, never a gate.
# Usage: just live census
#        just live census --write
[doc("diff the bundled API model tables; networked, not a gate")]
census *ARGS:
    tools/dev/api_model_census.py {{ARGS}}

# Dry-run spend for the planned live matrix. No build, no network.
# Usage: just live estimate --rate-card rates.json
[doc("dry-run spend for the planned live matrix; no build or network")]
estimate *ARGS:
    tests/live/estimate.py {{ARGS}}

# Read-only daily-driver log evidence. No build or inference.
daily-driver *ARGS:
    tests/live/daily_driver.py {{ARGS}}

# One qualification cell: retain a first model or provider miss and rerun once.
[doc("rerun one qualification cell after a model or provider miss")]
qualify *ARGS:
    tests/live/qualification.py {{ARGS}}
