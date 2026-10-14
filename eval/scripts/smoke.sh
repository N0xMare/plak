#!/usr/bin/env bash
# Five-task TB 4.0 smoke across v1 arms. Opt-in, paid, never just check.
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
dataset='terminal-bench/terminal-bench@4.0.0'
# ARM is a separate lane, so the suffix goes on the job name inside jobs/ the
# way rung-a and rung-b spell it. amd64 keeps its existing bare jobs/ path so
# archived runs stay where they are.
jobs_dir="$root/.plak/harbor/jobs${job_suffix:+/smoke$job_suffix}"
# Five-task slice: run the tasks at once. Independent provider arms are
# separate `harbor run` jobs and may overlap. Do not overlap two arms that
# share one OAuth refresh token (Plak Codex vs vendor Codex; Plak Grok vs
# grok-build only if a cell refreshes).
n_concurrent="${HARBOR_N_CONCURRENT:-5}"
tasks=(
  terminal-bench/photonic-waveguide-routing
  terminal-bench/music-harmony
  terminal-bench/bun-sourcemap-leak
  terminal-bench/foodstuff-beta-activity
  terminal-bench/interleaved-vigenere
)
include=()
for task in "${tasks[@]}"; do
  include+=(--include-task-name "$task")
done

harbor_bin() {
  command -v uv >/dev/null || { echo "eval smoke: install uv" >&2; exit 2; }
  # Harbor's TUI restores the terminal on exit. With no TTY, leftover stdin
  # is executed by this shell (`opencode-grok: command not found`, exit 127).
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

if ! docker_ok; then
  echo "eval smoke: Docker is not available; cannot execute Terminal-Bench trials" >&2
  exit 2
fi

arm="${1:-all}"

host_refresh_plak() {
  local account="$1"
  if [[ "${PLAK_HOST_REFRESH:-1}" != "1" ]]; then
    return 0
  fi
  if ! command -v plak >/dev/null; then
    echo "eval smoke: plak is not on PATH; cannot refresh $account before Harbor" >&2
    exit 2
  fi
  plak refresh "$account"
}

run_plak() {
  local account="$1" model="$2" effort="$3"
  host_refresh_plak "$account"
  harbor_bin run --config "$root/eval/terminal-bench.yaml" --jobs-dir "$jobs_dir" \
    ${force_build[@]+"${force_build[@]}"} \
    --dataset "$dataset" \
    --agent harbor_adapter.agent:PlakAgent \
    --model "$model" \
    --agent-kwarg "account=$account" \
    --agent-kwarg "model=$model" \
    --agent-kwarg "effort=$effort" \
    --agent-kwarg "arch=$arch" \
    --agent-kwarg "binary_path=$installed/plak" \
    --agent-kwarg "js_host_path=$installed/plakous-js-host" \
    --n-attempts 1 --n-concurrent "$n_concurrent" \
    "${include[@]}"
}

vendor_ak() {
  local agent="$1" effort="$2"
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.controls import harbor_ak_flags
from harbor_adapter.vendor_pins import CLI_PINS
agent, effort = sys.argv[1], sys.argv[2]
cli = {"codex": "codex", "grok-build": "grok", "pi": "pi", "opencode": "opencode"}[agent]
for value in harbor_ak_flags(agent, effort=effort, version=CLI_PINS[cli]):
    print(value)
' "$agent" "$effort"
}

run_vendor() {
  local agent="$1" model="$2" named="$3" effort="$4"
  shift 4
  local aks=()
  while IFS= read -r value; do
    aks+=(--ak "$value")
  done < <(vendor_ak "$named" "$effort")
  # Stock vendor agents validate their kwargs, so arch travels in HARBOR_ARCH.
  harbor_bin run --jobs-dir "$jobs_dir" --dataset "$dataset" \
    ${force_build[@]+"${force_build[@]}"} \
    --agent "$agent" --model "$model" \
    --n-attempts 1 --n-concurrent "$n_concurrent" --env docker \
    "${aks[@]}" \
    "$@" \
    "${include[@]}"
}

require_frozen_cli() {
  local cli="$1"
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.vendor_pins import require_frozen_artifacts
require_frozen_artifacts(sys.argv[1])
' "$cli" || {
    echo "eval smoke: missing frozen $cli CLI; run \`just fetch-harbor-vendor-clis\`" >&2
    exit 2
  }
}

require_host_auth() {
  local kind="$1"
  local provider="$2"
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.credentials import (
    require_ready_grok_auth,
    require_ready_opencode_auth,
    require_ready_pi_auth,
)
kind, provider = sys.argv[1], sys.argv[2]
if kind == "grok":
    require_ready_grok_auth()
elif kind == "pi":
    require_ready_pi_auth(provider)
elif kind == "opencode":
    require_ready_opencode_auth(provider)
else:
    raise SystemExit(f"unknown auth kind {kind}")
' "$kind" "$provider"
}

run_named_arm() {
  local named="$1"
  case "$named" in
    plak-codex) run_plak codex gpt-5.6-luna low ;;
    plak-grok) run_plak grok grok-4.6 low ;;
    codex)
      require_frozen_cli codex
      # Not `--ae CODEX_FORCE_AUTH_JSON=true`. Harbor treats every `--ae`
      # value as a secret and string-replaces it out of the whole trial
      # directory, so the literal `true` turns every JSON boolean in
      # result.json into the bare token `[REDACTED]` and the file stops
      # parsing. The exported process environment reaches the agent through
      # Harbor's own env fallback, which reads os.environ.
      export CODEX_FORCE_AUTH_JSON=true
      run_vendor harbor_adapter.vendor_agents:FrozenCodex openai/gpt-5.6-luna \
        codex low
      ;;
    grok-build)
      require_frozen_cli grok
      require_host_auth grok xai
      unset XAI_API_KEY OPENAI_API_KEY
      # See the codex arm: an `--ae` value of `true` corrupts result.json.
      export GROK_FORCE_AUTH_JSON=true
      run_vendor harbor_adapter.vendor_agents:FrozenGrokBuild xai/grok-4.6 \
        grok-build low
      ;;
    pi-codex)
      require_frozen_cli pi
      require_host_auth pi openai-codex
      unset OPENAI_API_KEY XAI_API_KEY
      run_vendor harbor_adapter.vendor_agents:FrozenPi openai/gpt-5.6-luna \
        pi low
      ;;
    pi-grok)
      require_frozen_cli pi
      require_host_auth pi xai
      unset OPENAI_API_KEY XAI_API_KEY
      run_vendor harbor_adapter.vendor_agents:FrozenPi xai/grok-4.6 \
        pi low
      ;;
    opencode-codex)
      require_frozen_cli opencode
      require_host_auth opencode openai
      unset OPENAI_API_KEY XAI_API_KEY
      run_vendor harbor_adapter.vendor_agents:FrozenOpenCode openai/gpt-5.6-luna \
        opencode low
      ;;
    opencode-grok)
      require_frozen_cli opencode
      require_host_auth opencode xai
      unset OPENAI_API_KEY XAI_API_KEY
      run_vendor harbor_adapter.vendor_agents:FrozenOpenCode xai/grok-4.6 \
        opencode low
      ;;
    *)
      echo "eval smoke: unknown arm $named" >&2
      return 2
      ;;
  esac
}

case "$arm" in
  plak-codex|plak-grok|codex|grok-build|pi-codex|pi-grok|opencode-codex|opencode-grok)
    run_named_arm "$arm"
    ;;
  all)
    echo "eval smoke: running each v1 arm sequentially" >&2
    for named in plak-codex codex plak-grok grok-build pi-codex pi-grok opencode-codex opencode-grok; do
      if ! "$0" "$named"; then
        status=$?
        if [[ "$status" -eq 2 ]]; then
          echo "eval smoke: skipping $named (fail-closed preflight)" >&2
          continue
        fi
        exit "$status"
      fi
    done
    ;;
  -h|--help)
    cat <<'EOF'
usage: eval/scripts/smoke.sh [plak-codex|codex|plak-grok|grok-build|pi-codex|pi-grok|opencode-codex|opencode-grok|all]

HARBOR_ARCH selects the cell architecture (amd64|arm64); unset it follows the
Docker daemon. arm64 reads .plak/installed/arm64/ and passes --force-build.
EOF
    ;;
  *)
    echo "eval smoke: unknown arm $arm" >&2
    exit 2
    ;;
esac
