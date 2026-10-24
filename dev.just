# Maintainer playground, coverage and opt-in sweeps. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

[private]
default:
    @just --justfile "{{justfile()}}" --list dev

# Symlink ~/.local/bin/plak-dev (repairing a stale one) and seed the isolated
# ~/plak-dev-home playground.
[doc("symlink ~/.local/bin/plak-dev and seed the isolated playground")]
home-install:
    scripts/plak-dev.sh --dev-install

# Wipe ~/plak-dev-home sqlite/sessions; keep config.toml.
home-reset:
    scripts/plak-dev.sh --dev-reset-home

# Offline preflight on a private backup. A changed state key rehearses
# carry-forward in disposable homes. --wait-idle retries three idle export refusals at
# 15 s intervals (--interval S overrides), avoiding large-store backup thrash;
# --detach --log <path> survives the caller's turn; --install installs on success.
[doc("offline reload preflight on a private backup")]
reload-rehearsal uuid *ARGS:
    bash scripts/reload-rehearsal.sh {{quote(uuid)}} {{ARGS}}

# Line/region/function coverage for the parallel and serial lanes, merged.
# Needs: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov
# Writes target/llvm-cov/html/html/index.html and target/llvm-cov/coverage.json.
# Its nextest runs also overwrite target/nextest/{offline,serial}/junit.xml.
# Uses its own CARGO_TARGET_DIR so it never invalidates the normal build.
[doc("line coverage for the parallel and serial lanes, in its own target dir")]
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
    cargo llvm-cov clean --workspace
    cargo llvm-cov --no-report --no-cfg-coverage nextest --locked --workspace --all-features \
      --profile offline
    cargo llvm-cov --no-report --no-cfg-coverage nextest --locked --workspace --all-features \
      --profile serial --test-threads=1
    cargo llvm-cov report --html --output-dir "$CARGO_TARGET_DIR/html"
    cargo llvm-cov report --json --output-path "$CARGO_TARGET_DIR/coverage.json"
    cargo llvm-cov report --summary-only

# Slowest tests, per-binary and per-package totals and each lane's test span,
# from the JUnit reports the last nextest lanes wrote. Informational.
[doc("per-test timing from the last lanes' nextest JUnit reports")]
test-timing *ARGS:
    python3 tools/perf/nextest_timing.py {{ARGS}}

# Opt-in indexed SQLite crash-point sweep of the real offline binary.
crash-sweep *ARGS:
    python3 scripts/crash-sweep.py {{ARGS}}

# Opt-in external jj writer compatibility; not part of a gate tier.
jj-compat:
    bash -c 'source scripts/dev-tools-prefix.sh; python3 scripts/jj-tools.py compat "$(plak_dev_tools_prefix "$PWD")"'
