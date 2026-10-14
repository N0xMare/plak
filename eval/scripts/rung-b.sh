#!/usr/bin/env bash
# Rung B: Terminal-Bench 4.0 wiring. One-task proof only; never the 66-task campaign.
# Opt-in, paid, never just check.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
# shellcheck source=eval/scripts/arch.sh
source "$root/eval/scripts/arch.sh"
arch="$(harbor_arch)"
platform="$(harbor_platform "$arch")"
installed="$(harbor_installed_root "$root" "$arch")"
job_suffix="$(harbor_job_suffix "$arch")"
export HARBOR_ARCH="$arch"
# Task cells with no docker_image are built by `docker compose build`, and
# neither Harbor's build compose template nor the task Dockerfiles name a
# platform. DOCKER_DEFAULT_PLATFORM is the only thing that makes those cells the
# selected architecture instead of the daemon's; compose honours it for `build`
# and `up` alike, so it also covers the --force-build rebuild path.
export DOCKER_DEFAULT_PLATFORM="$platform"
# Prebuilt Terminal-Bench task images are published for linux/amd64 only.
# Off amd64, --force-build makes Harbor rebuild each cell from the task's own
# Dockerfile on the daemon platform.
force_build=()
if [[ "$arch" != "amd64" ]]; then
  force_build=(--force-build)
fi
config="$root/eval/rung-b-wire.yaml"
dataset='terminal-bench/terminal-bench@4.0.0'
cmd="${1:-wire}"
arm="${2:-plak-codex}"
task="${3:-terminal-bench/foodstuff-beta-activity}"
n_concurrent="${HARBOR_N_CONCURRENT:-1}"
jobs_dir="$root/.plak/harbor/jobs/rung-b-${arm}${job_suffix}"

harbor_bin() {
  command -v uv >/dev/null || { echo "eval rung-b: install uv" >&2; exit 2; }
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

usage() {
  cat <<'EOF'
usage: eval/scripts/rung-b.sh wire [plak-codex|codex] [terminal-bench/<task>]

One TB 4.0 task. Does not run the 66-task dataset. Prefer plumbing/Rung A for
weekly capability signal.

HARBOR_ARCH selects the cell architecture (amd64|arm64); unset it follows the
Docker daemon. arm64 reads .plak/installed/arm64/, passes --force-build and
suffixes the job name with -arm64.
EOF
}

host_refresh_plak() {
  local account="$1"
  if [[ "${PLAK_HOST_REFRESH:-1}" != "1" ]]; then
    return 0
  fi
  if ! command -v plak >/dev/null; then
    echo "eval rung-b: plak is not on PATH; cannot refresh $account before Harbor" >&2
    exit 2
  fi
  plak refresh "$account"
}

case "$cmd" in
  -h|--help)
    usage
    exit 0
    ;;
  wire)
    ;;
  *)
    echo "eval rung-b: unknown command $cmd (only wire)" >&2
    usage >&2
    exit 2
    ;;
esac

if ! docker_ok; then
  echo "eval rung-b: Docker is not available; cannot execute Terminal-Bench trials" >&2
  exit 2
fi

if [[ "$task" == *'*'* || "$task" == all ]]; then
  echo "eval rung-b: refusing dataset-wide run; pass one terminal-bench/<task>" >&2
  exit 2
fi

case "$arm" in
  plak-codex)
    host_refresh_plak codex
    harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
      ${force_build[@]+"${force_build[@]}"} \
      --dataset "$dataset" \
      --agent harbor_adapter.agent:PlakAgent \
      --model gpt-5.6-luna \
      --agent-kwarg "account=codex" \
      --agent-kwarg "model=gpt-5.6-luna" \
      --agent-kwarg "effort=low" \
      --agent-kwarg "arch=$arch" \
      --agent-kwarg "binary_path=$installed/plak" \
      --agent-kwarg "js_host_path=$installed/plakous-js-host" \
      --n-attempts 1 --n-concurrent "$n_concurrent" \
      --include-task-name "$task"
    ;;
  codex)
    PYTHONPATH="$root/eval" python3 -c \
      'from harbor_adapter.vendor_pins import require_frozen_artifacts
require_frozen_artifacts("codex")' || {
      echo "eval rung-b: missing frozen codex CLI; run \`just fetch-harbor-vendor-clis\`" >&2
      exit 2
    }
    # Not `--ae CODEX_FORCE_AUTH_JSON=true`. Harbor treats every `--ae` value
    # as a secret and string-replaces it out of the whole trial directory, so
    # the literal `true` turns every JSON boolean in result.json into the bare
    # token `[REDACTED]` and the file stops parsing. The exported process
    # environment reaches the agent through Harbor's own env fallback.
    export CODEX_FORCE_AUTH_JSON=true
    # Stock vendor agents validate their kwargs, so arch travels in HARBOR_ARCH.
    harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
      ${force_build[@]+"${force_build[@]}"} \
      --dataset "$dataset" \
      --agent harbor_adapter.vendor_agents:FrozenCodex \
      --model openai/gpt-5.6-luna \
      --n-attempts 1 --n-concurrent "$n_concurrent" --env docker \
      --ak version="$(PYTHONPATH="$root/eval" python3 -c 'from harbor_adapter.vendor_pins import CLI_PINS; print(CLI_PINS["codex"])')" \
      --ak reasoning_effort=low \
      --include-task-name "$task"
    ;;
  *)
    echo "eval rung-b: wire arm must be plak-codex or codex" >&2
    exit 2
    ;;
esac
