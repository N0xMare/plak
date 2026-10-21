#!/usr/bin/env bash
# Developer gate: check (default), fast iteration, platform, full.
# Usage: scripts/dev-check.sh [--fast [PACKAGE...] | --platform | --full |
#                            --steps GROUP[,GROUP...]]
# --steps selects exactly the named groups, in canonical order.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$root"

# Canonical group order; every tier runs its groups in this order.
all_groups=(static builds rustdoc tests-parallel tests-serial tests-durability contracts tooling package)
check_groups=static,builds,tests-parallel,tests-serial,contracts,tooling
platform_groups=static,builds,tests-parallel,tests-serial,tests-durability

tier=check
selection=$check_groups
packages=()
case "${1:-}" in
  --fast) tier=fast; selection=static,tests-parallel; shift; packages=("$@"); set -- ;;
  --platform) tier=platform; selection=$platform_groups; shift ;;
  --full) tier=full; selection="$(IFS=,; echo "${all_groups[*]}")"; shift ;;
  --steps)
    tier=steps; selection="${2:-}"; shift
    if [[ $# -gt 0 ]]; then shift; fi ;;
  -h|--help) sed -n '2,5p' "$0"; exit 0 ;;
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

# Linux budget tests need RAM-backed fsync under parallel gate load; macOS
# NOFOLLOW needs a physical path outside HOME. Placement changes, not deadlines.
TMPDIR="$(bash scripts/test-tmpdir.sh --fresh)"
export TMPDIR
# Nextest must not inherit the operator product home. A maintainer binary
# writes its diagnostics log as soon as it starts, and an unresolved
# PLAK_HOME falls back to $HOME/.plak. HOME itself stays the operator's:
# Cargo and git read it. Hand-run nextest needs the same export.
isolated_home="$TMPDIR/plak-isolated-home"
mkdir -p "$isolated_home"
chmod 700 "$isolated_home"

# Fast narrows only the parallel lane, to the touched packages and their
# reverse dependencies. Compilation stays workspace all-features everywhere:
# -p compilation would resolve a different feature graph.
scope=''
if [[ "$tier" == fast ]]; then
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
    for name in "${packages[@]}"; do scope+="${scope:+ | }rdeps(=$name)"; done
  fi
  echo "fast: ${packages[*]:-no Rust package touched}"
fi

# Bash 3.2 nounset rejects quoted empty arrays; guard their expansions.
target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then target_args=(--target "$CARGO_BUILD_TARGET"); fi
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$PWD/$target_dir" ;;
esac
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
  local key="" triple="${CARGO_BUILD_TARGET:-}"
  if [[ -z "$triple" ]]; then triple="$(rustc -vV | sed -n 's/^host: //p' || true)"; fi
  if [[ -n "$triple" ]]; then
    key="CARGO_TARGET_$(printf '%s' "$triple" | tr '[:lower:]-' '[:upper:]_')_RUSTFLAGS"
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

# One nextest lane: a profile from .config/nextest.toml plus extra arguments.
# Serial lanes pass --test-threads=1 explicitly so NEXTEST_TEST_THREADS in the
# environment cannot widen them.
run_lane() {
  local profile=$1
  shift
  prepare_tests
  PLAK_HOME="$isolated_home" PLAK_MONTY_WORKER="$monty_worker" cargo nextest run --locked --workspace --all-features \
    --profile "$profile" ${target_args[@]+"${target_args[@]}"} "$@"
}

run_group() {
  case "$1" in
    static)
      local failed_steps=""
      echo "== fmt =="
      cargo fmt --all --check || failed_steps="fmt"
      if [[ "$tier" == platform ]]; then
        echo "skip structure checkers: platform-independent (check/full on Linux)"
      else
        echo "== dependencies: fetch locked (offline first) =="
        cargo fetch --locked --offline || cargo fetch --locked \
          || failed_steps="${failed_steps:+$failed_steps }fetch"
        echo "== structure: workspace =="
        python3 tools/checks/check_workspace.py || failed_steps="${failed_steps:+$failed_steps }workspace"
        echo "== structure: features (static) =="
        python3 tools/checks/check_features.py || failed_steps="${failed_steps:+$failed_steps }features"
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
      echo "-- nextest: parallel lane (profile offline) --"
      if [[ -n "$scope" ]]; then
        run_lane offline --no-tests=pass -E "$scope"
      else
        run_lane offline
      fi ;;
    tests-serial)
      echo "-- nextest: serial lane (profile serial, wall-clock budgets, one thread) --"
      run_lane serial --test-threads=1 ;;
    tests-durability)
      echo "-- nextest: durability lane (profile durability, crash matrices, one thread) --"
      run_lane durability --test-threads=1 ;;
    contracts)
      echo "== positive/negative public compile contracts =="
      python3 tools/checks/check_public_compile.py ;;
    tooling)
      echo "== structure: checker unit tests =="
      python3 tools/checks/run_tooling_tests.py -v ;;
    package)
      # Publication packages a git tree; the musl container mounts an export.
      if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        echo "skip package: not a git work tree (the dry run stages tracked files)"
        return 0
      fi
      echo "== publish dry run: package and verify the cohort offline =="
      CARGO_NET_OFFLINE=true scripts/publish.sh --gate ;;
    *) echo "unimplemented step group: $1" >&2; exit 2 ;;
  esac
}

# A passing full tier on a clean tree records that tree as publication
# evidence (scripts/publish.sh refuses a tree without it).
evidence_tree=''
if [[ "$tier" == full ]] && git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    && [[ -z "$(git status --porcelain --untracked-files=normal)" ]]; then
  evidence_tree="$(git rev-parse 'HEAD^{tree}')"
fi

failed_groups=""
gate_status=0
run_timed() {
  local group=$1 started status worker_state
  echo "== group: $group =="
  started=$SECONDS
  # A subshell keeps set -e fail-fast inside the group; the parent records the
  # status and goes on to the next group. A prepared monty_worker survives a
  # failed lane so later lanes reuse the same build.
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
  echo "== time: $group $((SECONDS - started)) s =="
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
if [[ "$gate_status" -ne 0 ]]; then
  echo "== failed groups: $failed_groups =="
  exit "$gate_status"
fi
if [[ -n "$evidence_tree" ]]; then
  if [[ -z "$(git status --porcelain --untracked-files=normal)" && "$(git rev-parse 'HEAD^{tree}')" == "$evidence_tree" ]]; then
    mkdir -p "$target_dir/gate-evidence"
    printf '%s\n' "$evidence_tree" >"$target_dir/gate-evidence/full"
    echo "== evidence: full gate passed on tree $evidence_tree =="
  else
    echo "== no evidence: the tree changed during the run ==" >&2
  fi
elif [[ "$tier" == full ]]; then
  echo "== no evidence: not a clean git tree when the run started =="
fi
echo "== ok: $tier gate passed =="
