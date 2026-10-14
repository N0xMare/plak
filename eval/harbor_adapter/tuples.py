"""Exact tuples the bakeoff may name. Neighbors are not admitted."""

from __future__ import annotations

ACCOUNTS = ("codex", "grok", "openrouter", "claude")

# Harbor container subscription dir names under XDG_STATE_HOME/plak/subscriptions/.
# OpenRouter is an API-key file under PLAK_HOME/secrets/, not OAuth.
ACCOUNT_SUBSCRIPTION_DIR = {
    "codex": "codex",
    "grok": "xai",
    "claude": "claude",
}

OPENROUTER_MODEL = "deepseek/deepseek-v4-flash"
# "none" means omit --effort / vendor thinking-variant. This Chat model attests
# no reasoning-effort enumeration; passing --effort medium fails closed.
OPENROUTER_EFFORT = "none"

# The Pi Codex arm is intentionally one strict same-model cell. Pi names the
# ChatGPT OAuth route `openai-codex`; Harbor's model prefix remains `openai`
# and FrozenPi performs that provider rewrite without copying Plak credentials.
PI_CODEX_MODEL = "gpt-6-luna"
PI_CODEX_EFFORT = "low"
PI_CODEX_PROVIDER = "openai-codex"

# §5.1 qualified native-home pairs used for v1 smoke, plus the OpenRouter
# API-key cell used only for the portable-CLI (Pi/OpenCode) vertical.
QUALIFIED = {
    ("codex", "gpt-6-luna", "low"),
    ("codex", "gpt-6-luna", "medium"),
    ("codex", "gpt-6-sol", "low"),
    ("codex", "gpt-6-sol", "medium"),
    ("claude", "claude-opus-5-5", "low"),
    ("claude", "claude-opus-5-5", "medium"),
    ("codex", "gpt-5.6-luna", "low"),
    ("codex", "gpt-5.6-luna", "medium"),
    ("codex", "gpt-5.6-sol", "low"),
    ("codex", "gpt-5.3-codex-spark", "low"),
    ("codex", "gpt-5.3-codex-spark", "medium"),
    ("grok", "grok-4.5", "low"),
    ("grok", "grok-4.5", "medium"),
    ("grok", "grok-4.6", "low"),
    ("grok", "grok-4.6", "medium"),
    ("claude", "claude-sonnet-5", "low"),
    ("claude", "claude-sonnet-5", "medium"),
    ("claude", "claude-opus-5", "low"),
    ("claude", "claude-opus-5", "medium"),
    ("openrouter", OPENROUTER_MODEL, OPENROUTER_EFFORT),
}

SMOKE_CODEX = ("codex", "gpt-5.6-luna", "low")
SMOKE_GROK = ("grok", "grok-4.6", "low")
SMOKE_CLAUDE = ("claude", "claude-sonnet-5", "low")
SMOKE_OPENROUTER = ("openrouter", OPENROUTER_MODEL, OPENROUTER_EFFORT)

# Follow-catalog: Codex CodeModeOnly → native-ptc; Grok/Claude/OpenRouter → direct.
DEFAULT_COMPOSITION = {
    "codex": "native-ptc",
    "grok": "direct",
    "claude": "direct",
    "openrouter": "direct",
}

# The Rust side reads exactly this variable to select the system-prompt
# shape. It is named once here so the ablation arms, the adapter, and the
# tests cannot drift apart.
PROMPT_VARIANT_ENV = "PLAK_PROMPT_VARIANT"
PROMPT_VARIANT_LEGACY = "legacy"
PROMPT_VARIANT_PARITY = "parity"
PROMPT_VARIANTS = (PROMPT_VARIANT_LEGACY, PROMPT_VARIANT_PARITY)
# Unset means the shipped default; an arm that names no variant passes none.
DEFAULT_PROMPT_VARIANT = None

STRIP_ENV = (
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "XAI_API_KEY",
    "OPENROUTER_API_KEY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
)

REMOTE_BINARY = "/installed-agent/plak"
REMOTE_JS_HOST = "/installed-agent/plakous-js-host"
REMOTE_HOME = "/logs/agent/plak-home"
REMOTE_XDG = "/logs/agent/xdg"
REMOTE_WORKSPACE = "/app"
REMOTE_OPENROUTER_KEY = f"{REMOTE_HOME}/secrets/openrouter.key"
REMOTE_PMUX = "/installed-agent/pmux"
REMOTE_PMUXD = "/installed-agent/pmuxd"
REMOTE_PMUX_RMUXD = "/installed-agent/pmux-rmuxd"
REMOTE_PMUX_LAUNCHER = "/installed-agent/pmux-launcher"
REMOTE_PMUX_HOOK = "/installed-agent/pmux-hook"
# Cell-local: Harbor bind-mounts /logs from the host; Darwin Docker Desktop
# cannot chmod Unix sockets on that volume (EINVAL). stdout/stderr stay
# under /logs/agent so Harbor still harvests them.
REMOTE_PMUX_RUNTIME = "/tmp/pmux"
REMOTE_PMUX_SOCKET = f"{REMOTE_PMUX_RUNTIME}/pmux.sock"
REMOTE_PMUX_PROMPT = f"{REMOTE_PMUX_RUNTIME}/prompt.txt"
REMOTE_PMUX_TRIAL = f"{REMOTE_PMUX_RUNTIME}/trial.sh"
# Overlay temp, not the pmuxd runtime and not Harbor /logs. Sharing TMPDIR
# with --runtime-parent makes Claude's file-store refresh NeedsLogin.
REMOTE_PMUX_TMPDIR = "/tmp/pmux-tmp"
REMOTE_CLAUDE_SECURESTORAGE = "/installed-agent/claude-securestorage"

DATASET_TB4 = "terminal-bench/terminal-bench@4.0.0"
DATASET_TB21 = "terminal-bench/terminal-bench-2-1@6"
DATASET_PLUMBING = "rung-a-plumbing"
DATASET = DATASET_TB4
DATASETS = (DATASET_TB4, DATASET_TB21, DATASET_PLUMBING)

SMOKE_TASKS = (
    "terminal-bench/photonic-waveguide-routing",
    "terminal-bench/music-harmony",
    "terminal-bench/bun-sourcemap-leak",
    "terminal-bench/foodstuff-beta-activity",
    "terminal-bench/interleaved-vigenere",
)

RUNG_B_WIRE_TASK = "terminal-bench/foodstuff-beta-activity"

# Local Harbor datasets name tasks by directory, not task.toml [task].name.
RUNG_A_PLUMBING_TASKS = ("write-hello", "sum-file", "fix-typo")

# Harbor PackageTaskId.get_name() is org/name. YAML task_names also accept
# globs; scripts pass the org-prefixed form to --include-task-name.
RUNG_A_AB_TASKS = (
    "terminal-bench/openssl-selfsigned-cert",
    "terminal-bench/fix-git",
    "terminal-bench/nginx-request-logging",
    "terminal-bench/sqlite-db-truncate",
    "terminal-bench/cancel-async-tasks",
    "terminal-bench/log-summary-date-ranges",
    "terminal-bench/regex-log",
    "terminal-bench/configure-git-webserver",
)

# Prebuilt Harbor task images (task.toml docker_image). Pre-pull before A/B
# so compose up does not depend on a live Docker Hub handshake.
RUNG_A_AB_DOCKER_IMAGES = (
    "alexgshaw/openssl-selfsigned-cert:20251031",
    "alexgshaw/fix-git:20260403",
    "alexgshaw/nginx-request-logging:20251031",
    "alexgshaw/sqlite-db-truncate:20251031",
    "alexgshaw/cancel-async-tasks:20251031",
    "alexgshaw/log-summary-date-ranges:20251031",
    "alexgshaw/regex-log:20251031",
    "alexgshaw/configure-git-webserver:20251031",
)

# A separate, broader frozen slice for less frequent measurements. It is a
# strict superset of cheap A/B so results can still be inspected by task.
RUNG_A_EXPANDED_TASKS = RUNG_A_AB_TASKS + (
    "terminal-bench/break-filter-js-from-html",
    "terminal-bench/build-cython-ext",
    "terminal-bench/cobol-modernization",
    "terminal-bench/count-dataset-tokens",
    "terminal-bench/db-wal-recovery",
    "terminal-bench/dna-insert",
    "terminal-bench/extract-elf",
    "terminal-bench/git-multibranch",
)

RUNG_A_EXPANDED_DOCKER_IMAGES = RUNG_A_AB_DOCKER_IMAGES + (
    "alexgshaw/break-filter-js-from-html:20251031",
    "alexgshaw/build-cython-ext:20251031",
    "alexgshaw/cobol-modernization:20251031",
    "alexgshaw/count-dataset-tokens:20251031",
    "alexgshaw/db-wal-recovery:20251031",
    "alexgshaw/dna-insert:20251031",
    "alexgshaw/extract-elf:20251031",
    "alexgshaw/git-multibranch:20251031",
)

HARBOR_OPENROUTER_MODEL = f"openrouter/{OPENROUTER_MODEL}"
