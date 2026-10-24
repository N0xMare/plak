# Offline performance and size. Not a gate. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

[private]
default:
    @just --justfile "{{justfile()}}" --list perf

# Profiling binary: release codegen, full symbols, no LTO (target/profiling/plak).
_profiling:
    cargo build --locked --profile profiling -p plak

# Offline performance smoke over the scripted model, not in any gate tier. It reports
# six metrics and compares them against this machine's seed row in
# tests/performance/baselines/offline-perf-smoke.json. See docs/spec/05.
[doc("offline performance smoke against this machine's seed row; not a gate")]
smoke *ARGS:
    scripts/perf-smoke.sh {{ARGS}}

# Sample a plak invocation with samply (Linux/macOS). Usage: just perf samply agent --help
samply *ARGS: _profiling
    command -v samply >/dev/null || { echo "install: cargo install samply" >&2; exit 1; }
    samply record target/profiling/plak {{ARGS}}

# Flamegraph a plak invocation (perf on Linux, xctrace on macOS).
flamegraph *ARGS: _profiling
    command -v cargo-flamegraph >/dev/null || { echo "install: cargo install flamegraph" >&2; exit 1; }
    cargo flamegraph --profile profiling --bin plak -- {{ARGS}}

# Hardware counters for a plak invocation (Linux only).
stat *ARGS: _profiling
    command -v perf >/dev/null || { echo "perf not installed" >&2; exit 1; }
    perf stat -e cycles,instructions,branches,branch-misses,cache-misses,context-switches,page-faults target/profiling/plak {{ARGS}}

# Binary size attribution for the default release build.
bloat:
    command -v cargo-bloat >/dev/null || { echo "install: cargo install cargo-bloat" >&2; exit 1; }
    cargo bloat --release -p plak --crates -n 40

# Production LOC density report (informational, not in any gate tier).
density:
    python3 tools/checks/check_prod_loc.py
