#!/usr/bin/env bash
# Rung A: plumbing (local tiny tasks) then cheap TB 2.1 A/B.
# Native-home pairs only, plus OpenRouter Plak vs Pi/OpenCode.
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
# The plumbing tasks declare no docker_image, so Harbor always builds them with
# `docker compose build`, and neither Harbor's build compose template nor the
# task Dockerfiles name a platform. DOCKER_DEFAULT_PLATFORM is the only thing
# that makes those cells the selected architecture instead of the daemon's, and
# compose honours it for `build` and `up` alike.
export DOCKER_DEFAULT_PLATFORM="$platform"
jobs_dir="$root/.plak/harbor/jobs"

slice="${1:-}"
arm="${2:-}"

harbor_bin() {
  command -v uv >/dev/null || { echo "eval rung-a: install uv" >&2; exit 2; }
  if [[ "${openrouter_watch:-0}" == "1" && "${1:-}" == "run" ]]; then
    local baseline="${PLAK_OR_BUDGET_BASELINE:-}"
    if [[ "$baseline" != /* ]]; then
      echo "eval rung-a: PLAK_OR_BUDGET_BASELINE must name the existing absolute budget receipt" >&2
      return 2
    fi
    local key_file
    key_file="$(PYTHONPATH="$root/eval" python3 -c \
      'import sys; from harbor_adapter.credentials import require_watched_openrouter_key; print(require_watched_openrouter_key(sys.argv[1]))' "$named")"
    python3 "$root/eval/scripts/openrouter_watch.py" \
      --key-file "$key_file" --baseline "$baseline" \
      --held-usd "${PLAK_OR_HELD_USD-0}" \
      --report "$jobs_dir/openrouter-watch.json" run -- \
      uv run --project "$root/eval" harbor "$@" </dev/null
    return
  fi
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
  echo "eval rung-a: Docker is not available; cannot execute Harbor trials" >&2
  exit 2
fi

# Compose `up` can still handshake Docker Hub after a successful pre-pull.
export COMPOSE_HTTP_TIMEOUT="${COMPOSE_HTTP_TIMEOUT:-300}"
export DOCKER_CLIENT_TIMEOUT="${DOCKER_CLIENT_TIMEOUT:-300}"
HARBOR_DOCKER_ENV="${HARBOR_DOCKER_ENV:-harbor_adapter.docker_retry:RetryingDocker}"

host_refresh_plak() {
  local account="$1"
  if [[ "$account" == "openrouter" ]]; then
    return 0
  fi
  if [[ "${PLAK_HOST_REFRESH:-1}" != "1" ]]; then
    return 0
  fi
  if ! command -v plak >/dev/null; then
    echo "eval rung-a: plak is not on PATH; cannot refresh $account before Harbor" >&2
    exit 2
  fi
  plak refresh "$account"
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

require_frozen_cli() {
  local cli="$1"
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.vendor_pins import require_frozen_artifacts
require_frozen_artifacts(sys.argv[1])
' "$cli" || {
    echo "eval rung-a: missing frozen $cli CLI; run \`just fetch-harbor-vendor-clis\`" >&2
    exit 2
  }
}

require_frozen_pmux() {
  PYTHONPATH="$root/eval" python3 -c \
    'from harbor_adapter.pmux_pins import require_frozen_pmux
require_frozen_pmux()
' || {
    echo "eval rung-a: missing $platform pmux; run \`just fetch-harbor-pmux\` with HARBOR_ARCH=$arch" >&2
    exit 2
  }
}

require_host_auth() {
  local kind="$1"
  local provider="$2"
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.credentials import (
    require_ready_claude_code_store,
    require_ready_grok_auth,
    require_ready_opencode_auth,
    require_ready_openrouter_key,
    require_ready_pi_auth,
)
kind, provider = sys.argv[1], sys.argv[2]
if kind == "grok":
    require_ready_grok_auth()
elif kind == "pi":
    require_ready_pi_auth(provider)
elif kind == "opencode":
    require_ready_opencode_auth(provider)
elif kind == "openrouter":
    require_ready_openrouter_key()
elif kind == "claude-code":
    require_ready_claude_code_store()
else:
    raise SystemExit(f"unknown auth kind {kind}")
' "$kind" "$provider"
}

slice_config() {
  case "$slice" in
    plumbing)
      echo "$root/eval/rung-a-plumbing.yaml"
      ;;
    ab)
      echo "$root/eval/rung-a-ab.yaml"
      ;;
    expanded)
      echo "$root/eval/rung-a-expanded.yaml"
      ;;
    *)
      echo "eval rung-a: slice must be plumbing|ab|expanded" >&2
      exit 2
      ;;
  esac
}

default_concurrent() {
  echo "${HARBOR_N_CONCURRENT:-1}"
}

attempt_count() {
  echo "${HARBOR_N_ATTEMPTS:-1}"
}

# Prebuilt task images exist for amd64 only. --force-build makes Harbor ignore
# the task's docker_image and build the task Dockerfile on the daemon platform.
force_build_flags() {
  if [[ "$arch" != "amd64" ]]; then
    printf '%s\n' --force-build
  fi
}

pull_with_retry() {
  local image="$1"
  local attempt delay
  # The campaign binds cached image IDs before dispatch. Do not refresh a
  # present tag, which could move to a different image between paired arms.
  if docker image inspect "$image" >/dev/null 2>&1; then
    return 0
  fi
  for attempt in 1 2 3 4 5; do
    if docker pull --platform "$platform" "$image"; then
      if docker image inspect "$image" >/dev/null 2>&1; then
        return 0
      fi
    fi
    delay=$((attempt * 5))
    echo "eval rung-a: docker pull $image failed (attempt $attempt); retrying in ${delay}s" >&2
    sleep "$delay"
  done
  echo "eval rung-a: failed to pull $image after retries" >&2
  return 1
}

# The pinned TB 2.1 A/B images are published for linux/amd64 only. On any
# other architecture there is nothing to pre-pull: the run passes
# --force-build and Harbor builds each task's own Dockerfile on the daemon
# platform instead. Those bases are multi-arch with no per-arch steps.
prepull_ab_images() {
  if [[ "$slice" == "plumbing" ]]; then
    return 0
  fi
  if [[ "$arch" != "amd64" ]]; then
    echo "eval rung-a: $platform A/B cells are rebuilt from the task Dockerfiles; skipping the amd64 image pre-pull" >&2
    return 0
  fi
  local image
  while IFS= read -r image; do
    [[ -n "$image" ]] || continue
    pull_with_retry "$image"
  done < <(
    PYTHONPATH="$root/eval" python3 -c \
      'import sys
from harbor_adapter.tuples import RUNG_A_AB_DOCKER_IMAGES, RUNG_A_EXPANDED_DOCKER_IMAGES
images = RUNG_A_EXPANDED_DOCKER_IMAGES if sys.argv[1] == "expanded" else RUNG_A_AB_DOCKER_IMAGES
for image in images:
    print(image)
' "$slice"
  )
}

include_flags() {
  PYTHONPATH="$root/eval" python3 -c \
    'import os, sys
from harbor_adapter.tuples import RUNG_A_AB_TASKS, RUNG_A_EXPANDED_TASKS, RUNG_A_PLUMBING_TASKS
names = {"plumbing": RUNG_A_PLUMBING_TASKS, "ab": RUNG_A_AB_TASKS, "expanded": RUNG_A_EXPANDED_TASKS}[sys.argv[1]]
override = os.environ.get("HARBOR_INCLUDE_TASK", "").strip()
if override:
    if override not in names:
        raise SystemExit(
            f"HARBOR_INCLUDE_TASK={override!r} is not in the {sys.argv[1]} include list"
        )
    names = (override,)
for name in names:
    print(name)
' "$slice"
}

# Harbor refuses --include-task-name unless the CLI also names --dataset,
# --task, or --path. YAML datasets are not enough for that check.
slice_source_flags() {
  case "$slice" in
    plumbing)
      printf '%s\n' --path "$root/eval/datasets/rung-a-plumbing"
      ;;
    ab|expanded)
      printf '%s\n' --dataset "terminal-bench/terminal-bench-2-1@6"
      ;;
  esac
}

require_include_count() {
  local got="$1" want
  if [[ -n "${HARBOR_INCLUDE_TASK:-}" ]]; then
    if [[ "$got" -ne 1 ]]; then
      echo "eval rung-a: HARBOR_INCLUDE_TASK expects one include, got $got" >&2
      exit 2
    fi
    return 0
  fi
  case "$slice" in
    plumbing) want=3 ;;
    ab) want=8 ;;
    expanded) want=16 ;;
    *)
      echo "eval rung-a: unknown slice $slice" >&2
      exit 2
      ;;
  esac
  if [[ "$got" -ne "$want" ]]; then
    echo "eval rung-a: expected $want include task names for $slice, got $got" >&2
    exit 2
  fi
}

load_slice_cli_flags() {
  include=()
  source_flags=()
  local names=()
  while IFS= read -r name; do
    names+=("$name")
    include+=(--include-task-name "$name")
  done < <(include_flags)
  while IFS= read -r flag; do
    source_flags+=("$flag")
  done < <(slice_source_flags)
  require_include_count "${#names[@]}"
}

# The prompt-variant ablation reaches the plak binary only, as a kwarg the
# adapter turns into PLAK_PROMPT_VARIANT in the cell. Vendor arms run their
# own prompts and never see it. The name lives in harbor_adapter.tuples.
prompt_variant_kwargs() {
  local variant="$1"
  [[ -n "$variant" ]] || return 0
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.arguments import require_prompt_variant
print(require_prompt_variant(sys.argv[1]))
' "$variant" >/dev/null || {
    echo "eval rung-a: unknown prompt variant $variant" >&2
    exit 2
  }
  printf '%s\n' --agent-kwarg "prompt_variant=$variant"
}

record_comparison_context() {
  local account="$1" model="$2" effort="$3" concurrent="$4" variant="${5:-}"
  PYTHONPATH="$root/eval" python3 -m harbor_adapter.context \
    --root "$root" --output "$jobs_dir/comparison-context.json" \
    --arm "$named" --slice "$slice" --account "$account" \
    --model "$model" --effort "$effort" --arch "$arch" \
    --attempts "$(attempt_count)" --concurrency "$concurrent" \
    --prompt-variant "$variant"
}

run_plak() {
  local account="$1" model="$2" effort="$3" variant="${4:-}"
  local config concurrent
  config="$(slice_config)"
  concurrent="$(default_concurrent)"
  local include=() source_flags=() force=() variant_flags=()
  load_slice_cli_flags
  while IFS= read -r flag; do force+=("$flag"); done < <(force_build_flags)
  while IFS= read -r flag; do variant_flags+=("$flag"); done < <(prompt_variant_kwargs "$variant")
  record_comparison_context "$account" "$model" "$effort" "$concurrent" "$variant"
  host_refresh_plak "$account"
  harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
    ${force[@]+"${force[@]}"} \
    ${variant_flags[@]+"${variant_flags[@]}"} \
    --agent harbor_adapter.agent:PlakAgent \
    --model "$model" \
    --agent-kwarg "account=$account" \
    --agent-kwarg "model=$model" \
    --agent-kwarg "effort=$effort" \
    --agent-kwarg "arch=$arch" \
    --agent-kwarg "binary_path=$installed/plak" \
    --agent-kwarg "js_host_path=$installed/plakous-js-host" \
    --n-attempts "$(attempt_count)" --n-concurrent "$concurrent" \
    --env "$HARBOR_DOCKER_ENV" \
    "${source_flags[@]}" \
    "${include[@]}"
}

# Every FrozenPmux cell is handed the same host .credentials.json and each
# in-cell Claude Code refreshes it on start, but the Claude refresh token is
# single-use: concurrent cells race, the first to redeem it invalidates the
# rest, and those die NeedsLogin before any turn. The write-back of the
# rotated credentials to the host pin races on one file for the same reason.
# One trial at a time, whatever HARBOR_N_CONCURRENT says.
pmux_concurrent() {
  echo 1
}

run_pmux() {
  local model="$1" effort="$2"
  local config concurrent
  config="$(slice_config)"
  concurrent="$(pmux_concurrent)"
  local include=() source_flags=() force=()
  load_slice_cli_flags
  while IFS= read -r flag; do force+=("$flag"); done < <(force_build_flags)
  record_comparison_context claude "$model" "$effort" "$concurrent"
  harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
    ${force[@]+"${force[@]}"} \
    --agent harbor_adapter.pmux_agent:FrozenPmux \
    --model "anthropic/$model" \
    --agent-kwarg "arch=$arch" \
    --agent-kwarg "model=$model" \
    --agent-kwarg "effort=$effort" \
    --n-attempts "$(attempt_count)" --n-concurrent 1 \
    --env "$HARBOR_DOCKER_ENV" \
    "${source_flags[@]}" \
    "${include[@]}"
}

run_vendor() {
  local agent="$1" model="$2" vendor_name="$3" effort="$4"
  local config concurrent
  config="$(slice_config)"
  concurrent="$(default_concurrent)"
  shift 4
  local aks=()
  while IFS= read -r value; do
    aks+=(--ak "$value")
  done < <(vendor_ak "$vendor_name" "$effort")
  local include=() source_flags=() force=()
  load_slice_cli_flags
  while IFS= read -r flag; do force+=("$flag"); done < <(force_build_flags)
  local account="$vendor_name"
  case "$vendor_name" in
    grok-build) account=grok ;;
    pi) if [[ "$model" == openai/* ]]; then account=codex; else account=openrouter; fi ;;
    opencode) account=openrouter ;;
  esac
  record_comparison_context "$account" "$model" "$effort" "$concurrent"
  # Stock vendor agents validate their kwargs, so arch travels in HARBOR_ARCH.
  harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
    ${force[@]+"${force[@]}"} \
    --agent "$agent" --model "$model" \
    --n-attempts "$(attempt_count)" --n-concurrent "$concurrent" \
    --env "$HARBOR_DOCKER_ENV" \
    "${aks[@]}" \
    "$@" \
    "${source_flags[@]}" \
    "${include[@]}"
}

run_named_arm() (
  local named="$1"
  local openrouter_watch=0
  case "$named" in
    plak-openrouter|pi-openrouter|opencode-openrouter) openrouter_watch=1 ;;
  esac
  # One run per directory lets the summary gate inspect this invocation only.
  jobs_dir="$root/.plak/harbor/jobs/${slice}-${named}${job_suffix}-$(date -u +%Y%m%dT%H%M%S)-$$"
  mkdir -p "$jobs_dir"
  # EXIT runs even when errexit aborts a failed Harbor/watch invocation. Keep
  # the original failure while still recording partial or missing trials.
  trap 'arm_status=$?; trap - EXIT; if summarize_jobs "$jobs_dir"; then :; else
    summary_status=$?; if [[ "$arm_status" == 0 ]]; then arm_status=$summary_status; fi
  fi; exit "$arm_status"' EXIT
  prepull_ab_images
  case "$named" in
    plak-codex) run_plak codex gpt-6-luna low ;;
    plak-codex-legacy) run_plak codex gpt-6-luna low legacy ;;
    plak-codex-parity) run_plak codex gpt-6-luna low parity ;;
    plak-grok) run_plak grok grok-4.6 low ;;
    plak-grok-legacy) run_plak grok grok-4.6 low legacy ;;
    plak-grok-parity) run_plak grok grok-4.6 low parity ;;
    plak-openrouter)
      require_host_auth openrouter openrouter
      unset OPENROUTER_API_KEY OPENAI_API_KEY XAI_API_KEY
      HARBOR_N_CONCURRENT=1
      run_plak openrouter deepseek/deepseek-v4-flash none
      ;;
    plak-claude)
      unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL
      run_plak claude claude-opus-5-5 low
      ;;
    plak-claude-legacy)
      unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL
      run_plak claude claude-opus-5-5 low legacy
      ;;
    plak-claude-parity)
      unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL
      run_plak claude claude-opus-5-5 low parity
      ;;
    claude-code)
      require_frozen_cli claude
      require_frozen_pmux
      require_host_auth claude-code anthropic
      unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL
      run_pmux claude-opus-5-5 low
      ;;
    codex)
      require_frozen_cli codex
      # Not `--ae CODEX_FORCE_AUTH_JSON=true`. Harbor treats every `--ae`
      # value as a secret and string-replaces it out of the whole trial
      # directory, so the literal `true` turns every JSON boolean in
      # result.json into the bare token `[REDACTED]` and the file stops
      # parsing. The exported process environment reaches the agent through
      # Harbor's own env fallback, which reads os.environ.
      export CODEX_FORCE_AUTH_JSON=true
      run_vendor harbor_adapter.vendor_agents:FrozenCodex openai/gpt-6-luna \
        codex low
      ;;
    pi-codex-oauth)
      require_frozen_cli pi
      # Pi owns a separate OAuth store. Never derive or copy this credential
      # from Plak's subscription directory or Codex CLI's auth.json.
      require_host_auth pi openai-codex
      unset OPENAI_API_KEY XAI_API_KEY OPENROUTER_API_KEY
      # Cells share Pi's refresh token; serialize to prevent refresh races.
      HARBOR_N_CONCURRENT=1
      run_vendor harbor_adapter.vendor_agents:FrozenPi openai/gpt-6-luna \
        pi low
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
    pi-openrouter)
      require_frozen_cli pi
      require_host_auth pi openrouter
      unset OPENAI_API_KEY XAI_API_KEY OPENROUTER_API_KEY
      HARBOR_N_CONCURRENT=1
      run_vendor harbor_adapter.vendor_agents:FrozenPi \
        openrouter/deepseek/deepseek-v4-flash \
        pi none
      ;;
    opencode-openrouter)
      require_frozen_cli opencode
      require_host_auth opencode openrouter
      unset OPENAI_API_KEY XAI_API_KEY OPENROUTER_API_KEY
      HARBOR_N_CONCURRENT=1
      run_vendor harbor_adapter.vendor_agents:FrozenOpenCode \
        openrouter/deepseek/deepseek-v4-flash \
        opencode none
      ;;
    *)
      echo "eval rung-a: unknown arm $named" >&2
      return 2
      ;;
  esac
)

# Reward alone cannot separate a model miss from a harness death, so every
# arm leaves a termination summary beside its trials.
summarize_jobs() {
  local dir="$1"
  local tasks=3 attempts
  [[ "$slice" == "ab" ]] && tasks=8
  [[ "$slice" == "expanded" ]] && tasks=16
  [[ -n "${HARBOR_INCLUDE_TASK:-}" ]] && tasks=1
  attempts="$(attempt_count)"
  if [[ ! "$attempts" =~ ^[1-9][0-9]*$ ]]; then
    echo "eval rung-a: HARBOR_N_ATTEMPTS must be a positive integer" >&2
    return 2
  fi
  python3 "$root/eval/scripts/summarize.py" --write --require-readable \
    --report "$dir/comparison-report" \
    --expect-trials "$((tasks * attempts))" "$dir"
}

usage() {
  cat <<'EOF'
usage: eval/scripts/rung-a.sh <plumbing|ab|expanded> <arm>

arms: plak-codex|codex|pi-codex-oauth|plak-grok|grok-build|plak-openrouter|pi-openrouter|opencode-openrouter|plak-claude|claude-code|all

prompt-variant ablation arms (plak only, same pair, same tasks):
  plak-codex-legacy|plak-codex-parity|plak-grok-legacy|plak-grok-parity|
  plak-claude-legacy|plak-claude-parity

Each passes prompt_variant= to PlakAgent, which sets PLAK_PROMPT_VARIANT for
the plak binary in the cell only. The variant is part of the job name, so
`ab-plak-codex-parity` never shares a job tree with `ab-plak-codex`.

Native-home pairs only (Codex vs Codex CLI and Pi Codex OAuth, Grok vs Grok
Build, Claude vs Claude Code via FrozenPmux pmux run). OpenRouter is Plak vs
Pi/OpenCode.
Do not overlap two arms that share one OAuth refresh token. claude-code is
pmux run (Full cell), never pmux ask and never Harbor claude --print. That arm
also runs one trial at a time regardless of HARBOR_N_CONCURRENT: its cells
share a single-use Claude refresh token and concurrent ones die NeedsLogin.

Every arm writes termination-summary.json beside its trials; read it with
eval/scripts/summarize.py (or `just harbor-summarize`) to see which reward-0
trials were harness deaths rather than model misses.

HARBOR_N_ATTEMPTS defaults to 1. Set 3 for an independent-trial A/B.
HARBOR_INCLUDE_TASK names one already-listed task for a debug trial.
TB 2.1 slices pre-pull pinned alexgshaw task images with Docker Hub retries.
Compose `up` retries once on a Hub timeout via RetryingDocker.

HARBOR_ARCH selects the cell architecture (amd64|arm64); unset it follows the
Docker daemon. arm64 skips the amd64 pre-pull, passes --force-build so Harbor
rebuilds each task cell from its own Dockerfile, reads artifacts from
.plak/installed/arm64/ and suffixes job names with -arm64. ARM results are a
separate lane and their timings are not comparable to amd64.
EOF
}

case "$slice" in
  -h|--help|"")
    usage
    exit 0
    ;;
  plumbing|ab|expanded)
    ;;
  *)
    echo "eval rung-a: slice must be plumbing|ab|expanded" >&2
    usage >&2
    exit 2
    ;;
esac

case "$arm" in
  plak-codex|plak-grok|plak-openrouter|plak-claude|codex|pi-codex-oauth|grok-build|pi-openrouter|opencode-openrouter|claude-code)
    run_named_arm "$arm"
    ;;
  plak-codex-legacy|plak-codex-parity|plak-grok-legacy|plak-grok-parity|plak-claude-legacy|plak-claude-parity)
    run_named_arm "$arm"
    ;;
  ablation)
    echo "eval rung-a: running $slice prompt-variant arms sequentially" >&2
    for named in plak-codex-legacy plak-codex-parity plak-grok-legacy plak-grok-parity plak-claude-legacy plak-claude-parity; do
      # Preserve the child's status: negating it before reading $? would
      # turn a failed arm into success. set -e stops an incomplete sweep.
      "$0" "$slice" "$named"
    done
    ;;
  all)
    echo "eval rung-a: running $slice arms sequentially" >&2
    for named in plak-codex codex pi-codex-oauth plak-grok grok-build plak-openrouter pi-openrouter opencode-openrouter plak-claude claude-code; do
      # Preserve the child's status: negating it before reading $? would
      # turn a failed arm into success. set -e stops an incomplete sweep.
      "$0" "$slice" "$named"
    done
    ;;
  *)
    echo "eval rung-a: unknown arm $arm" >&2
    usage >&2
    exit 2
    ;;
esac
