#!/usr/bin/env bash
# Rung A: plumbing (local tiny tasks) then cheap TB 2.1 A/B.
# Reviewed campaigns only; serial native-home pairs. OpenRouter is disabled.
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
  PYTHONPATH="$root/eval" python3 -m harbor_adapter.campaign
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

# Bound image startup may still encounter transient compose setup faults.
export COMPOSE_HTTP_TIMEOUT="${COMPOSE_HTTP_TIMEOUT:-300}"
export DOCKER_CLIENT_TIMEOUT="${DOCKER_CLIENT_TIMEOUT:-300}"
HARBOR_DOCKER_ENV="${HARBOR_DOCKER_ENV:-harbor_adapter.docker_retry:RetryingDocker}"


vendor_ak() {
  local agent="$1" effort="$2"
  PYTHONPATH="$root/eval" python3 -c \
    'import sys
from harbor_adapter.controls import harbor_ak_flags
from harbor_adapter.vendor_pins import CLI_PINS
agent, effort = sys.argv[1], sys.argv[2]
cli = {"codex": "codex", "grok-build": "grok", "pi": "pi"}[agent]
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
    require_ready_pi_auth,
)
kind, provider = sys.argv[1], sys.argv[2]
if kind == "grok":
    require_ready_grok_auth()
elif kind == "pi":
    require_ready_pi_auth(provider)
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
    --attempts "1" --concurrency "$concurrent" \
    --prompt-variant "$variant"
}

run_plak() {
  local account="$1" model="$2" effort="$3" variant="${4:-}"
  local config concurrent
  config="$(slice_config)"
  concurrent=1
  local include=() source_flags=() variant_flags=()
  load_slice_cli_flags
  while IFS= read -r flag; do variant_flags+=("$flag"); done < <(prompt_variant_kwargs "$variant")
  record_comparison_context "$account" "$model" "$effort" "$concurrent" "$variant"
  harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
    ${variant_flags[@]+"${variant_flags[@]}"} \
    --agent harbor_adapter.agent:PlakAgent \
    --model "$model" \
    --agent-kwarg "account=$account" \
    --agent-kwarg "model=$model" \
    --agent-kwarg "effort=$effort" \
    --agent-kwarg "arch=$arch" \
    --agent-kwarg "binary_path=$installed/plak" \
    --agent-kwarg "js_host_path=$installed/plakous-js-host" \
    --n-attempts "1" --n-concurrent "$concurrent" \
    --env "$HARBOR_DOCKER_ENV" \
    "${source_flags[@]}" \
    "${include[@]}"
}

run_pmux() {
  local model="$1" effort="$2"
  local config concurrent
  config="$(slice_config)"
  concurrent=1
  local include=() source_flags=()
  load_slice_cli_flags
  record_comparison_context claude "$model" "$effort" "$concurrent"
  harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
    --agent harbor_adapter.pmux_agent:FrozenPmux \
    --model "anthropic/$model" \
    --agent-kwarg "arch=$arch" \
    --agent-kwarg "model=$model" \
    --agent-kwarg "effort=$effort" \
    --n-attempts "1" --n-concurrent 1 \
    --env "$HARBOR_DOCKER_ENV" \
    "${source_flags[@]}" \
    "${include[@]}"
}

run_vendor() {
  local agent="$1" model="$2" vendor_name="$3" effort="$4"
  local config concurrent
  config="$(slice_config)"
  concurrent=1
  shift 4
  local aks=()
  while IFS= read -r value; do
    aks+=(--ak "$value")
  done < <(vendor_ak "$vendor_name" "$effort")
  local include=() source_flags=()
  load_slice_cli_flags
  local account="$vendor_name"
  case "$vendor_name" in
    grok-build) account=grok ;;
    pi) account=codex ;;
  esac
  record_comparison_context "$account" "$model" "$effort" "$concurrent"
  # Stock vendor agents validate their kwargs, so arch travels in HARBOR_ARCH.
  harbor_bin run --config "$config" --jobs-dir "$jobs_dir" \
    --agent "$agent" --model "$model" \
    --n-attempts "1" --n-concurrent "$concurrent" \
    --env "$HARBOR_DOCKER_ENV" \
    "${aks[@]}" \
    "$@" \
    "${source_flags[@]}" \
    "${include[@]}"
}

run_named_arm() (
  local named="$1"
  export HARBOR_ARM="$named"
  case "$named" in
    *openrouter*) echo "eval: OpenRouter comparisons disabled: no explicit shared wire effort" >&2; return 2 ;;
  esac
  # One run per directory lets the summary gate inspect this invocation only.
  jobs_dir="${HARBOR_INVOCATION_DIR:-$root/.plak/harbor/jobs/${slice}-${named}${job_suffix}-$(date -u +%Y%m%dT%H%M%S)-$$}"
  mkdir -p "$jobs_dir"
  # EXIT runs even when errexit aborts a failed Harbor invocation. Keep
  # the original failure while still recording partial or missing trials.
  trap 'arm_status=$?; trap - EXIT; if summarize_jobs "$jobs_dir"; then :; else
    summary_status=$?; if [[ "$arm_status" == 0 ]]; then arm_status=$summary_status; fi
  fi; exit "$arm_status"' EXIT
  case "$named" in
    plak-codex) run_plak codex gpt-6-luna low ;;
    plak-codex-legacy) run_plak codex gpt-6-luna low legacy ;;
    plak-codex-parity) run_plak codex gpt-6-luna low parity ;;
    plak-grok) run_plak grok grok-4.6 low ;;
    plak-grok-legacy) run_plak grok grok-4.6 low legacy ;;
    plak-grok-parity) run_plak grok grok-4.6 low parity ;;
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
  local tasks=3
  [[ "$slice" == "ab" ]] && tasks=8
  [[ "$slice" == "expanded" ]] && tasks=16
  [[ -n "${HARBOR_INCLUDE_TASK:-}" ]] && tasks=1
  python3 "$root/eval/scripts/summarize.py" --write --require-readable \
    --report "$dir/comparison-report" \
    --expect-trials "$tasks" "$dir"
}

usage() {
  cat <<'EOF'
usage: eval/scripts/rung-a.sh <plumbing|ab|expanded> <arm>

arms: plak-codex|codex|pi-codex-oauth|plak-grok|grok-build|plak-claude|claude-code|all

prompt-variant ablation arms (plak only, same pair, same tasks):
  plak-codex-legacy|plak-codex-parity|plak-grok-legacy|plak-grok-parity|
  plak-claude-legacy|plak-claude-parity

Each passes prompt_variant= to PlakAgent, which sets PLAK_PROMPT_VARIANT for
the plak binary in the cell only. The variant is part of the job name, so
`ab-plak-codex-parity` never shares a job tree with `ab-plak-codex`.

Native-home pairs only (Codex vs Codex CLI and Pi Codex OAuth, Grok vs Grok
Build, Claude vs Claude Code via FrozenPmux pmux run). OpenRouter is disabled.
All launches require a reviewed digest-bound campaign, serial single-attempt
slots and locally frozen task image IDs. Use all for the campaign scheduler.

Every arm writes termination-summary.json beside its trials; read it with
eval/scripts/summarize.py (or `just harbor-summarize`) to see which reward-0
trials were harness deaths rather than model misses.

HARBOR_N_ATTEMPTS and HARBOR_N_CONCURRENT must both be 1. Campaign blocks
(minimum 3) supply repeat slots. HARBOR_INCLUDE_TASK is the scheduler's slot
task selector (one already-listed task), not an independent scored run.
Images must already be frozen locally; no per-trial pre-pull or force-build.
Compose setup retries once on a Hub timeout via RetryingDocker.

HARBOR_ARCH selects amd64|arm64 (unset follows the Docker daemon). Artifacts
come from .plak/installed/<arch>/; arm64 job names carry -arm64. Architecture
lanes and their timings are not comparable.

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
    exec python3 "$root/eval/scripts/campaign.py" "$slice"
    ;;
  *)
    echo "eval rung-a: unknown arm $arm" >&2
    usage >&2
    exit 2
    ;;
esac
