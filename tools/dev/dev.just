# Maintainer playground, coverage and opt-in sweeps. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set working-directory := '../..'

[private]
default:
    @just --justfile "{{justfile()}}" --list dev

# Symlink ~/.local/bin/plak-dev (repairing a stale one) and seed the isolated
# ~/plak-dev-home playground.
[doc("symlink ~/.local/bin/plak-dev and seed the isolated playground")]
home-install:
    tools/dev/plak-dev.sh --dev-install

# Wipe ~/plak-dev-home sqlite/sessions; keep config.toml.
home-reset:
    tools/dev/plak-dev.sh --dev-reset-home

# Offline preflight on a private backup. A changed state key rehearses
# carry-forward in disposable homes. --wait-idle retries three idle export refusals at
# 15 s intervals (--interval S overrides), avoiding large-store backup thrash;
# --detach --log <path> survives the caller's turn; --install installs on success.
[doc("offline reload preflight on a private backup")]
reload-rehearsal uuid *ARGS:
    tools/build/reload-rehearsal.sh {{quote(uuid)}} {{ARGS}}

# Line/region/function coverage for the parallel and serial lanes, merged.
# Needs: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov
[doc("line coverage for the parallel and serial lanes, in its own target dir")]
coverage:
    tools/dev/coverage.sh

# Slowest tests, per-binary and per-package totals and each lane's test span,
# from the JUnit reports the last nextest lanes wrote. Informational.
[doc("per-test timing from the last lanes' nextest JUnit reports")]
test-timing *ARGS:
    tools/perf/nextest_timing.py {{ARGS}}

# Opt-in indexed SQLite crash-point sweep of the real offline binary.
[doc("opt-in indexed SQLite crash-point sweep of the real offline binary")]
crash-sweep *ARGS:
    tools/gate/test-env.sh -- python3 tools/gate/test-processes.py -- tools/dev/crash-sweep.py {{ARGS}}

[doc("compare two crash-sweep reports at equal cuts")]
crash-compare *ARGS:
    tools/dev/compare_crash_sweeps.py {{ARGS}}

# Opt-in external jj writer compatibility; not part of a gate tier.
[doc("opt-in external jj writer compatibility; not a gate")]
jj-compat:
    tools/dev/jj-compat.sh
