#!/usr/bin/env bash
# Run the checked-in Plakous offline benchmark workloads.

set -euo pipefail

PLAKOUS_PERF_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
PLAKOUS_PERF_MODE=${1:---quick}

case "$PLAKOUS_PERF_MODE" in
    --quick)
        PLAKOUS_PERF_STORE_ARGS=(--small)
        ;;
    --full)
        PLAKOUS_PERF_STORE_ARGS=()
        ;;
    *)
        printf 'usage: %s [--quick|--full]\n' "$0" >&2
        exit 2
        ;;
esac

if [[ -n "${PLAKOUS_PERF_OUTPUT_DIR:-}" ]]; then
    PLAKOUS_PERF_RUN_DIR=$PLAKOUS_PERF_OUTPUT_DIR
    mkdir -p -- "$PLAKOUS_PERF_RUN_DIR"
else
    mkdir -p -- "$PLAKOUS_PERF_ROOT/target"
    PLAKOUS_PERF_RUN_DIR=$(mktemp -d "$PLAKOUS_PERF_ROOT/target/perf-check.XXXXXX")
fi

run_jsonl() {
    local name=$1
    shift
    local output="$PLAKOUS_PERF_RUN_DIR/$name.jsonl"
    printf 'running %s -> %s\n' "$name" "$output" >&2
    "$@" >"$output"
}

cd -- "$PLAKOUS_PERF_ROOT"

run_jsonl perf-core \
    cargo bench --locked --offline -p plakous --bench perf_core
run_jsonl perf-project \
    cargo bench --locked --offline -p plakous --bench perf_project
run_jsonl perf-store \
    cargo bench --locked --offline -p plakous-store --features sqlite,test-support \
    --bench perf_store -- "${PLAKOUS_PERF_STORE_ARGS[@]}"

printf 'offline benchmark outputs written to %s\n' "$PLAKOUS_PERF_RUN_DIR"
printf '%s\n' \
    'latency, RSS, artifact-size, and database-growth values remain unratified raw host-sensitive observations'
