#!/usr/bin/env bash
# Maintainer Harbor / Terminal-Bench 4.0 entrypoint. Never part of just check.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
# shellcheck source=eval/scripts/arch.sh
source "$root/eval/scripts/arch.sh"
dataset='terminal-bench/terminal-bench@4.0.0'
jobs_dir="$root/.plak/harbor/jobs"
config="$root/eval/terminal-bench.yaml"

require_uv() {
  command -v uv >/dev/null || { echo "eval: install uv" >&2; exit 2; }
}

harbor_bin() {
  require_uv
  uv run --project "$root/eval" harbor "$@" </dev/null
}

docker_ok() {
  python3 -c '
import subprocess, sys
try:
    result = subprocess.run(
        ["docker", "info"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=15,
    )
except (FileNotFoundError, subprocess.TimeoutExpired):
    sys.exit(1)
sys.exit(0 if result.returncode == 0 else 1)
'
}

# Only the build commands need an architecture; bootstrap and check must keep
# working with no Docker daemon to ask.
select_arch() {
  arch="$(harbor_arch)"
  platform="$(harbor_platform "$arch")"
  installed="$(harbor_installed_root "$root" "$arch")"
  job_suffix="$(harbor_job_suffix "$arch")"
  echo "eval: HARBOR_ARCH=$arch ($platform), artifacts in $installed" >&2
}

cmd="${1:-}"
shift || true

case "$cmd" in
  bootstrap)
    require_uv
    uv sync --project eval --frozen
    ;;
  check)
    python3 -m unittest discover -s eval/tests -t eval/tests
    harbor_bin --version
    harbor_bin run --config "$config" --print-config >/dev/null
    harbor_bin run --config "$root/eval/rung-a-plumbing.yaml" --print-config >/dev/null
    harbor_bin run --config "$root/eval/rung-a-ab.yaml" --print-config >/dev/null
    harbor_bin run --config "$root/eval/rung-b-wire.yaml" --print-config >/dev/null
    ;;
  build-agent)
    select_arch
    mkdir -p "$installed"
    if ! docker_ok; then
      echo "eval: Docker is required to build the $platform Harbor agent on $(uname -s)" >&2
      exit 2
    fi
    # Always bookworm. Host Ubuntu 24.04 bins need GLIBC 2.38/2.39; Harbor
    # ubuntu cells do not have that. Same rule as just fetch-harbor-pmux.
    docker buildx build \
      --platform "$platform" \
      -f eval/harbor_adapter/Dockerfile \
      --target artifact \
      --output "type=local,dest=$installed" \
      "$root"
    ;;
  fetch-vendor-clis)
    select_arch
    if ! docker_ok; then
      echo "eval: Docker is required to fetch $platform vendor CLIs on $(uname -s)" >&2
      exit 2
    fi
    require_uv
    mkdir -p "$installed/vendor"
    args=()
    while IFS='=' read -r key value; do
      [[ -n "$key" ]] || continue
      args+=(--build-arg "$key=$value")
    done < <(HARBOR_ARCH="$arch" uv run --project "$root/eval" python -c 'from harbor_adapter.vendor_pins import docker_build_args
for k, v in docker_build_args().items():
    print(f"{k}={v}")')
    docker buildx build \
      --platform "$platform" \
      -f eval/harbor_adapter/vendor.Dockerfile \
      --target artifact \
      "${args[@]}" \
      --output "type=local,dest=$installed/vendor" \
      "$root/eval/harbor_adapter"
    HARBOR_ARCH="$arch" uv run --project "$root/eval" python -c 'from harbor_adapter.vendor_pins import ALL_CLIS, require_frozen_artifacts
for cli in ALL_CLIS:
    artifacts = require_frozen_artifacts(cli)
    print("eval: frozen", cli, "for linux/" + artifacts.arch)
'
    ;;
  fetch-pmux)
    select_arch
    if ! docker_ok; then
      echo "eval: Docker is required to build $platform pmux on $(uname -s)" >&2
      exit 2
    fi
    pmux_src="${PMUX_SRC:-$root/../pseudomux}"
    if [[ ! -f "$pmux_src/Cargo.toml" ]]; then
      echo "eval: missing pseudomux checkout at $pmux_src (set PMUX_SRC)" >&2
      exit 2
    fi
    mkdir -p "$installed/pmux"
    require_uv
    # buildx takes the checkout as a directory context, which carries no git
    # history, so the commit is resolved here and recorded beside the
    # binaries. Resolve before building: a dirty checkout produces artifacts
    # no commit describes, and learning that after a ten-minute cargo build
    # helps nobody. PMUX_ALLOW_DIRTY=1 is the named override.
    pmux_commit="$(PMUX_SRC="$pmux_src" uv run --project "$root/eval" python -c '
import os, sys
from pathlib import Path
from harbor_adapter.pmux_provenance import resolve_source_commit
try:
    print(resolve_source_commit(Path(os.environ["PMUX_SRC"])).commit)
except Exception as exc:
    print(f"eval: {exc}", file=sys.stderr)
    raise SystemExit(2)
')" || exit 2
    echo "eval: building pmux from pseudomux ${pmux_commit:0:7}" >&2
    docker buildx build \
      --platform "$platform" \
      -f "$root/eval/harbor_adapter/pmux.Dockerfile" \
      --target artifact \
      --output "type=local,dest=$installed/pmux" \
      "$pmux_src"
    HARBOR_ARCH="$arch" PMUX_SRC="$pmux_src" PMUX_DEST="$installed/pmux" \
      uv run --project "$root/eval" python -c '
import os
from pathlib import Path
from harbor_adapter.pmux_pins import FULL_CELL_BINS
from harbor_adapter.pmux_provenance import record_provenance, resolve_source_commit
dest = Path(os.environ["PMUX_DEST"])
source = resolve_source_commit(Path(os.environ["PMUX_SRC"]))
record_provenance(dest, arch=os.environ["HARBOR_ARCH"], source=source, bins=FULL_CELL_BINS)
print("eval: recorded pseudomux " + source.short + (" (dirty)" if source.dirty else ""))
'
    HARBOR_ARCH="$arch" uv run --project "$root/eval" python -c 'from harbor_adapter.pmux_pins import require_frozen_pmux
bins = require_frozen_pmux()
print("eval: frozen pmux for linux/" + bins.arch + " from pseudomux " + bins.commit[:7])
'
    ;;
  eval)
    require_uv
    select_arch
    export HARBOR_ARCH="$arch"
    # Task cells with no docker_image are built by `docker compose build`, and
    # neither Harbor's build compose template nor the task Dockerfiles name a
    # platform. DOCKER_DEFAULT_PLATFORM is the only thing that makes those cells
    # the selected architecture instead of the daemon's; compose honours it for
    # `build` and `up` alike, so it also covers the --force-build rebuild path.
    export DOCKER_DEFAULT_PLATFORM="$platform"
    # Prebuilt TB 4.0 task images are published for linux/amd64 only. Off
    # amd64, --force-build makes Harbor rebuild each cell from the task's own
    # Dockerfile on the daemon platform.
    force_build=()
    if [[ "$arch" != "amd64" ]]; then
      force_build=(--force-build)
    fi
    # ARM is a separate lane: the suffix goes on the job name inside jobs/, the
    # way rung-a and rung-b spell it. amd64 keeps its existing bare jobs/ path
    # so archived runs and `harbor.sh view` stay valid.
    jobs_dir="$root/.plak/harbor/jobs${job_suffix:+/eval$job_suffix}"
    mkdir -p "$jobs_dir"
    # Without --include-task-name this is the full 66-task TB 4.0 campaign.
    # Weekly signal is just harbor-rung-a; one-task wire is just harbor-rung-b.
    has_include=0
    for arg in "$@"; do
      case "$arg" in
        --include-task-name|--include-task-name=*|-i|-i=*)
          has_include=1
          break
          ;;
      esac
    done
    if [[ "$has_include" -eq 0 ]]; then
      echo "eval: harbor.sh eval with no --include-task-name runs all 66 TB 4.0 tasks" >&2
    fi
    exec uv run --project "$root/eval" harbor run --config "$config" --jobs-dir "$jobs_dir" \
      ${force_build[@]+"${force_build[@]}"} \
      --dataset "$dataset" "$@" </dev/null
    ;;
  view)
    require_uv
    exec uv run --project "$root/eval" harbor view "$jobs_dir"
    ;;
  ""|-h|--help)
    cat <<'EOF'
usage: eval/scripts/harbor.sh <bootstrap|check|build-agent|fetch-vendor-clis|fetch-pmux|eval|view> [harbor args]

eval is opt-in and paid. It is not part of just check.
Without --include-task-name, eval runs the full TB 4.0 dataset (66 tasks).
Weekly A/B is eval/scripts/rung-a.sh; one TB 4.0 task is eval/scripts/rung-b.sh.
fetch-pmux builds Full-cell pmux companions from PMUX_SRC (default ../pseudomux).

HARBOR_ARCH selects the cell architecture (amd64|arm64); unset it follows the
Docker daemon. Artifacts land in .plak/installed/<arch>/ and never mix.
eval exports DOCKER_DEFAULT_PLATFORM so compose-built cells are that
architecture, passes --force-build off amd64, and suffixes the arm64 job name.
EOF
    ;;
  *)
    echo "eval: unknown command $cmd" >&2
    exit 2
    ;;
esac
