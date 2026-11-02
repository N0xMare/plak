#!/usr/bin/env bash
# Line/region/function coverage for the parallel and serial lanes, merged.
# Needs: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov
# Writes target/llvm-cov/html/html/index.html and target/llvm-cov/coverage.json.
# Its nextest runs also overwrite target/nextest/{offline,serial}/junit.xml.
# Uses its own CARGO_TARGET_DIR so it never invalidates the normal build.
set -euo pipefail
cd "$(dirname "$0")/../.."
source tools/gate/dev-tools-prefix.sh
export PATH="$(plak_dev_tools_prefix "$PWD")/bin:$PATH"
tools/gate/dev-tools.sh check-nextest
command -v cargo-llvm-cov >/dev/null || { echo "install: cargo install cargo-llvm-cov" >&2; exit 1; }
mkdir -p target/llvm-cov
# Assign first. `eval "$(test-env.sh)"` ignores a failing helper under set -e.
test_env="$(tools/gate/test-env.sh)"
eval "$test_env"
export TMPDIR PLAK_HOME GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM
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
