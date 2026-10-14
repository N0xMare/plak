#!/usr/bin/env bash
# Developer gate: safe default check, iteration fast, platform builds, full.
# Usage: scripts/dev-check.sh [--fast [PACKAGE...] | --platform | --full |
#                            --steps GROUP[,GROUP...] | --changed [BASE]]
# --steps selects exactly the named groups, in the full tier's canonical order.
# --changed scopes both test lanes from a previously checked BASE.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$root"

# Timing ledger sits beside the target directory, outside the artifact sweep
# and the lane's git clean. Captured before this run creates target/.
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$PWD/$target_dir" ;;
esac
if [[ -d "$target_dir" ]]; then cache_hint=warm; else cache_hint=cold; fi
ledger="${target_dir%/*}/.gate-times.jsonl"
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
  platform_triple="$CARGO_BUILD_TARGET"
else
  platform_triple="$(rustc -vV | sed -n 's/^host: //p' || true)"
fi
if [[ -z "$platform_triple" ]]; then platform_triple=unknown; fi
commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
recorded=""
failed_groups=""
gate_status=0

# One ordered inventory drives full, explicit groups and the change planner.
all_groups=()
while IFS= read -r group; do all_groups+=("$group"); done <<<"$(python3 tools/checks/gate_plan.py --groups)"
tier=check
packages=()
selection="$(python3 tools/checks/gate_plan.py --tier check)"
scope=''
workspace_check=true
features_check=true
case "${1:-}" in
  --fast) tier=fast; selection=static,tests-parallel; shift; packages=("$@"); set -- ;;
  --platform)
    tier=platform; selection="$(python3 tools/checks/gate_plan.py --tier platform)"
    workspace_check=false; features_check=false; shift ;;
  --full) tier=full; selection="$(IFS=,; echo "${all_groups[*]}")"; shift ;;
  --steps)
    tier=steps; selection="${2:-}"; shift
    if [[ $# -gt 0 ]]; then shift; fi ;;
  --changed)
    tier=changed; shift
    base="${1:-}"
    if [[ $# -gt 0 ]]; then shift; fi
    # A real temporary file propagates planner failure (process substitution
    # would silently discard it). No generated shell code is evaluated.
    plan_file="$(mktemp)"
    if ! python3 tools/checks/gate_plan.py --shell "$base" >"$plan_file"; then
      rm -f "$plan_file"; exit 2
    fi
    { IFS= read -r selection; IFS= read -r scope
      IFS= read -r workspace_check; IFS= read -r features_check; } <"$plan_file"
    rm -f "$plan_file" ;;
  -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
esac
if [[ $# -gt 0 ]]; then echo "unknown argument: $1" >&2; exit 2; fi
if [[ -z "$selection" || "$selection" == ,* || "$selection" == *, || "$selection" == *,,* ]]; then
  echo "expected non-empty comma-separated step groups" >&2; exit 2
fi
IFS=, read -r -a requested <<<"$selection"
for group in "${requested[@]}"; do
  known=false
  for candidate in "${all_groups[@]}"; do
    if [[ "$group" == "$candidate" ]]; then known=true; break; fi
  done
  if [[ "$known" != true ]]; then echo "unknown step group: $group" >&2; exit 2; fi
done

export PATH="$root/.dev-tools/bin:$PATH"
bash scripts/dev-tools.sh check

# macOS NOFOLLOW needs a real workspace path; Linux budget tests need RAM-backed
# fsync under parallel gate load. Placement changes, not deadline assertions.
if [[ -z "${PLAK_TEST_TMPDIR:-}" && "$(uname -s)" == Linux && -d /dev/shm && -w /dev/shm ]]; then
  test_tmpdir="/dev/shm/plak-test-$(id -u)-$(printf '%s' "$PWD" | cksum | cut -d' ' -f1)"
  rm -rf "$test_tmpdir"
else
  test_tmpdir="${PLAK_TEST_TMPDIR:-$PWD/target/plak-test-tmp}"
fi
mkdir -p "$test_tmpdir"
TMPDIR="$(cd "$test_tmpdir" && pwd -P)"
export TMPDIR
# Nextest must not inherit the operator product home. A maintainer binary
# writes its diagnostics log as soon as it starts, and an unresolved
# PLAK_HOME falls back to $HOME/.plak. HOME itself stays the operator's:
# Cargo and git read it. Hand-run nextest needs the same export.
isolated_home="$TMPDIR/plak-isolated-home"
mkdir -p "$isolated_home"
chmod 700 "$isolated_home"

if [[ "$tier" == fast ]]; then
  checker_file="$(mktemp "$TMPDIR/plak-checkers.XXXXXX")"
  if ! python3 tools/checks/gate_plan.py --checkers >"$checker_file"; then
    rm -f "$checker_file"; exit 2
  fi
  { IFS= read -r workspace_check; IFS= read -r features_check; } <"$checker_file"
  rm -f "$checker_file"
  if [[ ${#packages[@]} -eq 0 ]]; then
    package_file="$(mktemp "$TMPDIR/plak-packages.XXXXXX")"
    if ! python3 tools/checks/changed_packages.py >"$package_file"; then
      rm -f "$package_file"; exit 2
    fi
    while IFS= read -r name; do packages+=("$name"); done <"$package_file"
    rm -f "$package_file"
  fi
  if [[ ${#packages[@]} -eq 0 ]]; then
    selection=static
  elif [[ "${packages[*]}" != --workspace ]]; then
    rdeps=""
    for name in "${packages[@]}"; do rdeps+="${rdeps:+ | }rdeps(=$name)"; done
    scope="($rdeps)"
  fi
  echo "fast: ${packages[*]:-no Rust package touched}"
fi
for enabled in "$workspace_check" "$features_check"; do
  if [[ "$enabled" != true && "$enabled" != false ]]; then
    echo "invalid checker selection from planner" >&2; exit 2
  fi
done

# Workspace all-features preserves one artifact set: -p compilation would
# resolve a different feature graph. Package/rdeps scopes narrow tests only.
# Bash 3.2 nounset rejects quoted empty arrays; guard their expansions.
target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then target_args=(--target "$CARGO_BUILD_TARGET"); fi
scope_prefix="${scope:+$scope and }"
monty_worker=''
prepare_tests() {
  if [[ -n "$monty_worker" ]]; then return; fi
  worker_manifest="$(mktemp "$TMPDIR/plak-worker-build.XXXXXX")"
  trap 'rm -f "$worker_manifest"' EXIT
  if [[ "${PLAK_MONTY_WORKER+x}" == x ]]; then
    worker_mode=explicit
    worker_input="$PLAK_MONTY_WORKER"
  else
    worker_mode=built
    # The worker comes out of the test build itself. A `cargo build` would leave
    # dev-dependency features off and compile a second variant of the graph;
    # this way nextest finds everything built and the tests run this worker.
    cargo test --no-run --locked --workspace --all-features --message-format=json ${target_args[@]+"${target_args[@]}"} >"$worker_manifest"
    worker_input="$worker_manifest"
  fi
  monty_worker="$(python3 - "$worker_mode" "$worker_input" <<'PY_WORKER'
import json
import os
from pathlib import Path
import sys

mode, value = sys.argv[1:]
if mode == "built":
    records = [json.loads(line) for line in Path(value).read_text().splitlines() if line.strip()]
    paths = {row["executable"] for row in records
             if row.get("reason") == "compiler-artifact"
             and row.get("target", {}).get("name") == "monty"
             and "bin" in row.get("target", {}).get("kind", []) and row.get("executable")
             and not row.get("profile", {}).get("test")}
    if len(paths) != 1:
        raise SystemExit("gate requires exactly one built monty executable")
    value = paths.pop()
if not value:
    raise SystemExit("PLAK_MONTY_WORKER must name an executable file")
path = Path(value).resolve(strict=True)
if not path.is_file() or not os.access(path, os.X_OK):
    raise SystemExit("gate requires an executable Monty worker")
print(path)
PY_WORKER
  )"
  rm -f "$worker_manifest"
  trap - EXIT
}

# Deny rustc warnings on one build. Cargo ignores RUSTFLAGS when
# CARGO_TARGET_<triple>_RUSTFLAGS is set, so -D warnings is appended to
# whichever variable this invocation actually passes through. The addition is
# process-local: the all-features test build keeps its own artifact set.
# Registry crates are capped at allow; only workspace crates fail closed.
run_build_denying_warnings() {
  local key=""
  if [[ -n "${platform_triple:-}" && "$platform_triple" != unknown ]]; then
    key="CARGO_TARGET_$(printf '%s' "$platform_triple" | tr '[:lower:]-' '[:upper:]_')_RUSTFLAGS"
  fi
  if [[ -n "$key" && -n "${!key+x}" ]]; then
    local current="${!key}"
    if [[ -n "$current" ]]; then
      env "$key=$current -D warnings" "$@"
    else
      env "$key=-D warnings" "$@"
    fi
  elif [[ -n "${RUSTFLAGS:-}" ]]; then
    env "RUSTFLAGS=${RUSTFLAGS} -D warnings" "$@"
  else
    env RUSTFLAGS="-D warnings" "$@"
  fi
}

run_group() {
  case "$1" in
    static)
      local failed_steps=""
      echo "== fmt =="
      cargo fmt --all --check || failed_steps="fmt"
      if [[ "$workspace_check" == true ]]; then
        echo "== structure: workspace =="
        python3 tools/checks/check_workspace.py || failed_steps="${failed_steps:+$failed_steps }workspace"
      else
        if [[ "$tier" == platform ]]; then
          echo "skip workspace checker: platform-independent (check/full on Linux)"
        else
          echo "skip workspace checker: no Rust source, manifest/lockfile, checker or unknown input changed"
        fi
      fi
      if [[ "$features_check" == true ]]; then
        echo "== structure: features (static) =="
        python3 tools/checks/check_features.py || failed_steps="${failed_steps:+$failed_steps }features"
      else
        if [[ "$tier" == platform ]]; then
          echo "skip features checker: platform-independent (check/full on Linux)"
        else
          echo "skip features checker: no Rust source, manifest/lockfile, toolchain, checker or unknown input changed"
        fi
      fi
      echo "== clippy =="
      cargo clippy --locked --workspace --all-features --all-targets ${target_args[@]+"${target_args[@]}"} -- -D warnings \
        || failed_steps="${failed_steps:+$failed_steps }clippy"
      if [[ -n "$failed_steps" ]]; then
        echo "== failed static steps: $failed_steps =="
        return 1
      fi ;;
    builds)
      echo "== default features =="
      run_build_denying_warnings cargo build --locked --workspace --lib --bins ${target_args[@]+"${target_args[@]}"}
      echo "== maintainer features =="
      run_build_denying_warnings cargo build --locked -p plak --features maintainer --bins ${target_args[@]+"${target_args[@]}"}
      python3 tools/build/check-artifact-linkage.py "$target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug" ;;
    rustdoc)
      echo "== rustdoc =="
      RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked ${target_args[@]+"${target_args[@]}"} ;;
    tests-parallel)
      prepare_tests
      not_budget_tests="$(python3 tools/checks/budget_filterset.py --exclude)"
      echo "-- nextest: parallel lane (everything but budget and durability) --"
      PLAK_HOME="$isolated_home" PLAK_MONTY_WORKER="$monty_worker" cargo nextest run --locked --workspace --all-features --profile offline \
        ${target_args[@]+"${target_args[@]}"} --no-tests=pass -E "${scope_prefix}($not_budget_tests)" ;;
    tests-serial)
      prepare_tests
      budget_tests="$(python3 tools/checks/budget_filterset.py)"
      serial_args=()
      if [[ -n "$scope" ]]; then serial_args=(--no-tests=pass); fi
      echo "-- nextest: serial lane (wall-clock budgets, --test-threads=1) --"
      PLAK_HOME="$isolated_home" PLAK_MONTY_WORKER="$monty_worker" cargo nextest run --locked --workspace --all-features --profile offline \
        ${target_args[@]+"${target_args[@]}"} ${serial_args[@]+"${serial_args[@]}"} --test-threads=1 -E "${scope_prefix}($budget_tests)" ;;
    tests-durability)
      prepare_tests
      durability_tests="$(python3 tools/checks/budget_filterset.py --durability)"
      serial_args=()
      if [[ -n "$scope" ]]; then serial_args=(--no-tests=pass); fi
      echo "-- nextest: durability lane (crash matrices and sqlite SIGKILL campaigns, --test-threads=1) --"
      PLAK_HOME="$isolated_home" PLAK_MONTY_WORKER="$monty_worker" cargo nextest run --locked --workspace --all-features --profile offline \
        ${target_args[@]+"${target_args[@]}"} ${serial_args[@]+"${serial_args[@]}"} --test-threads=1 -E "${scope_prefix}($durability_tests)" ;;
    contracts)
      echo "== positive/negative public compile contracts =="
      python3 tools/checks/check_public_compile.py ;;
    tooling)
      echo "== structure: checker unit tests =="
      python3 tools/checks/run_tooling_tests.py -v ;;
    perf)
      echo "== offline performance smoke (informational) =="
      if ! bash scripts/perf-smoke.sh --check; then
        # Non-zero here is a gated-metric regression *or* a run that could not
        # complete. Saying "regression" for both sends the reader hunting a slowdown
        # that never happened, which is what a missing BSD/GNU `time` used to do on
        # Linux.
        echo "perf smoke: did not pass (gated regression or incomplete run); informational only"
      fi
      ;;
    loc)
      echo "== source size (informational) =="
      python3 tools/checks/check_prod_loc.py ;;
    *) echo "unimplemented step group: $1" >&2; exit 2 ;;
  esac
}

report_warnings() {
  if [[ -z "$recorded" ]]; then return 0; fi
  python3 "$root/tools/checks/gate_times.py" warn \
    --ledger "$ledger" --platform "$platform_triple" --groups "$recorded" \
    || echo "gate timing ledger: warning check failed" >&2
}

run_timed() {
  local group=$1 started status elapsed worker_state
  echo "== group: $group =="
  started=$SECONDS
  # A subshell keeps set -e fail-fast inside the group. The parent records the
  # status instead of aborting before the ledger line. Preserve a prepared
  # monty_worker even if nextest fails, so later lanes reuse the same build.
  worker_state="$(mktemp)"
  printf '%s\n' "$monty_worker" >"$worker_state"
  set +e
  (
    set -Ee
    monty_worker="$(head -n 1 "$worker_state")"
    trap 'printf "%s\n" "$monty_worker" >"$worker_state"' ERR
    run_group "$group"
    printf '%s\n' "$monty_worker" >"$worker_state"
  )
  status=$?
  set -e
  monty_worker="$(head -n 1 "$worker_state")"
  rm -f "$worker_state"
  elapsed=$((SECONDS - started))
  echo "== time: $group $elapsed s =="
  python3 "$root/tools/checks/gate_times.py" append \
    --ledger "$ledger" \
    --commit "$commit" \
    --tier "$tier" \
    --group "$group" \
    --seconds "$elapsed" \
    --exit-status "$status" \
    --platform "$platform_triple" \
    --cache "$cache_hint" \
    || echo "gate timing ledger: append failed" >&2
  recorded="${recorded:+$recorded,}$group"
  if [[ "$status" -ne 0 ]]; then
    failed_groups="${failed_groups:+$failed_groups }$group"
    if [[ "$gate_status" -eq 0 ]]; then gate_status=$status; fi
  fi
}

for group in "${all_groups[@]}"; do
  if [[ ",$selection," == *",$group,"* ]]; then
    run_timed "$group"
  fi
done
report_warnings
if [[ "$gate_status" -ne 0 ]]; then
  echo "== failed groups: $failed_groups =="
  exit "$gate_status"
fi
echo "== ok: $tier gate passed =="
