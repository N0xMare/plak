#!/usr/bin/env bash
# Run and validate the checked-in Plakous performance evidence workloads.

set -euo pipefail

PLAKOUS_PERF_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
PLAKOUS_PERF_MODE=${1:---quick}

case "$PLAKOUS_PERF_MODE" in
    --quick)
        PLAKOUS_PERF_BUILD_PROFILES=(minimal)
        PLAKOUS_PERF_BUILD_WARM_SAMPLES=0
        PLAKOUS_PERF_STORE_ARGS=(--small)
        ;;
    --full)
        PLAKOUS_PERF_BUILD_PROFILES=(minimal subscriptions full)
        PLAKOUS_PERF_BUILD_WARM_SAMPLES=2
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

export PLAKOUS_PERF_FEATURE_FLAGS=${PLAKOUS_PERF_FEATURE_FLAGS:-default-empty}
export PLAKOUS_PERF_RUSTFLAGS_ID=${PLAKOUS_PERF_RUSTFLAGS_ID:-unqualified-current-environment}

run_jsonl() {
    local name=$1
    shift
    local output="$PLAKOUS_PERF_RUN_DIR/$name.jsonl"
    printf 'running %s -> %s\n' "$name" "$output" >&2
    "$@" >"$output"
    python3 "$PLAKOUS_PERF_ROOT/tools/perf/validate_perf_jsonl.py" "$output"
}

cd -- "$PLAKOUS_PERF_ROOT"

for profile in "${PLAKOUS_PERF_BUILD_PROFILES[@]}"; do
    run_jsonl "perf-build-$profile" \
        python3 tools/perf/perf_build.py \
        --build-profile "$profile" \
        --cargo-profile dev \
        --operation check \
        --warm-samples "$PLAKOUS_PERF_BUILD_WARM_SAMPLES"
done

run_jsonl perf-core \
    cargo bench --locked --offline -p plakous --bench perf_core
run_jsonl perf-project \
    cargo bench --locked --offline -p plakous --bench perf_project
run_jsonl perf-store \
    cargo bench --locked --offline -p plakous-store --features sqlite,perf-instrumentation \
    --bench perf_store -- "${PLAKOUS_PERF_STORE_ARGS[@]}"

printf 'portable JSONL structure and invariants validated in %s\n' "$PLAKOUS_PERF_RUN_DIR"
printf '%s\n' \
    'latency, RSS, artifact-size, and database-growth values remain unratified raw host-sensitive observations'
