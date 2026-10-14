# Harness bakeoff (`eval/`)

Maintainer tooling that runs the real `plak run` binary (one non-interactive
root turn, full tools, no approval gates) against Codex CLI, Claude Code, Grok
Build, Pi and OpenCode on Terminal-Bench tasks through
[Harbor](https://github.com/harbor-framework/harbor), and compares capability,
cost, tokens and wall time on the same model and effort.

It is not product support and not qualification: a Terminal-Bench pass
qualifies no tuple. Paid runs are opt-in and in no gate tier.
`just harbor-test` runs the zero-inference adapter tests in `eval/tests/`;
they are not in any gate tier either.

## Prerequisites

- A local Docker daemon with buildx, and [uv](https://docs.astral.sh/uv/).
  Harbor is pinned at 0.23.0 in `eval/pyproject.toml` and always runs through
  `uv run --project eval`, never a global `harbor`.
- Host credentials for the arms you run:
  - Plak OAuth: `plak login codex`, `plak login grok`, `plak login claude`;
  - Plak OpenRouter: `secrets/openrouter.key` in the Plak home (`plak key set
    openrouter`);
  - Codex CLI: `~/.codex/auth.json`; Grok Build: `~/.grok/auth.json`;
  - Pi and OpenCode: their own `auth.json` (`~/.pi/agent/auth.json`,
    `~/.local/share/opencode/auth.json`), or private copies named by
    `PI_AUTH_JSON_PATH` and `OPENCODE_AUTH_JSON_PATH`;
  - Claude Code: a Linux `.credentials.json` file store named by
    `PLAK_CLAUDE_CODE_CREDENTIALS_DIR` (on macOS the default `~/.claude`
    keychain store is refused).
- For the Claude Code arm, a clean checkout of
  [pseudomux](https://github.com/N0xMare/pseudomux) at the pinned commit
  (below), at `../pseudomux` or named by `PMUX_SRC`.

## Setup

```text
just harbor-bootstrap          # uv sync --project eval
just check-harbor              # zero-inference adapter tests (+ harbor --print-config)
just build-harbor-agent        # Linux plak + js-host into .plak/installed/<arch>
just fetch-harbor-vendor-clis  # pinned Codex/Grok/Pi/OpenCode/Claude prefixes
just fetch-harbor-pmux         # Full-cell pmux companions from PMUX_SRC
```

`HARBOR_ARCH` is `amd64` or `arm64`; unset, it follows the Docker daemon's
architecture. Artifacts live under `.plak/installed/<arch>/`, one tree per
architecture, and every frozen binary's ELF machine is checked against it.
Build the agent and companions with these recipes (Debian bookworm images):
host-built `plak`, `plakous-js-host` or `pmux*` binaries need a newer glibc
and fail in Harbor cells. The `linux/aarch64` `plak` is an eval build target,
not a supported product platform.

Vendor CLIs are frozen per-architecture prefixes (Node 22.20.0, Codex 0.159.3,
Grok 1.0.30, Pi 0.87.1, OpenCode 1.18.30, Claude Code 2.1.280). Arms upload
them into the cell; they never `nvm install`, `npm i -g …@latest` or
`curl | bash` per trial.

### pmux companions

`just fetch-harbor-pmux` builds `pmux`, `pmuxd`, `pmux-rmuxd`,
`pmux-launcher` and `pmux-hook` from the pseudomux checkout and writes
`.plak/installed/<arch>/pmux/manifest.json` with the source commit and dirty
state. The pinned commit is **`fc0e8cb`**; it is defined once, in
`harbor_adapter.pmux_provenance.PSEUDOMUX_PIN`. An arm refuses companions with
no manifest, a different commit or a dirty build. Two explicit overrides
exist, and both are recorded:

- `PMUX_ALLOW_DIRTY=1` lets the fetch build from a dirty worktree; the
  manifest then records `"dirty": true`.
- `PMUX_ALLOW_UNPINNED=1` lets an arm run companions that do not name the pin,
  for deliberately testing an off-pin pseudomux.

## Running

| Rung | Dataset | Recipe |
| --- | --- | --- |
| A plumbing | three local tasks in `datasets/rung-a-plumbing` (`write-hello`, `sum-file`, `fix-typo`) | `just harbor-rung-a plumbing <arm>\|all` |
| A cheap A/B | eight TB 2.1 tasks from `terminal-bench/terminal-bench-2-1@6` | `just harbor-rung-a ab <arm>\|all` |
| A expanded | those eight plus eight more TB 2.1 tasks (16) | `just harbor-rung-a expanded <arm>\|all` |
| B wire | one task of `terminal-bench/terminal-bench@4.0.0` | `just harbor-rung-b wire [plak-codex\|codex] [terminal-bench/<task>]` |
| Smoke | five hard TB 4.0 tasks, concurrent | `just harbor-smoke <arm>\|all` |

The task sets are frozen in `rung-a-*.yaml`, `harbor_adapter/tuples.py` and
`harbor_adapter/verifier_evidence.py` (task refs and checksums). TB 2.1 and
TB 4.0 scores are not comparable, and neither are amd64 and arm64 timings:
arm64 job names carry an `-arm64` suffix, and arm64 cells are rebuilt from the
task Dockerfiles. `rung-a.sh` refuses to launch unless the include list is
exactly three plumbing directory names, eight prefixed A/B names, or 16
prefixed expanded names. Rung B refuses a dataset-wide run.
`just harbor-eval …` passes its arguments to `harbor run`; without
`--include-task-name` it is the full 66-task TB 4.0 campaign and warns.

Rung A arms:

| Arm | Harness | Tuple |
| --- | --- | --- |
| `plak-codex` / `codex` | `plak run` / Codex CLI `codex exec` | gpt-6-luna, low |
| `pi-codex-oauth` | Pi CLI, its own OAuth store | openai-codex/gpt-6-luna, low |
| `plak-grok` / `grok-build` | `plak run` / Grok Build | grok-4.6, low |
| `plak-openrouter` / `pi-openrouter` / `opencode-openrouter` | `plak run` / Pi / OpenCode | deepseek/deepseek-v4-flash, effort `none` |
| `plak-claude` / `claude-code` | `plak run` / FrozenPmux `pmux run` | claude-opus-5-5, low |
| `all` | the ten above, sequentially | |

Prompt ablation arms compare two prompt variants of the same `plak` binary:
`plak-codex-legacy` / `plak-codex-parity`, and the same pairs for `grok` and
`claude`; `ablation` runs all six. The arm sets `PLAK_PROMPT_VARIANT`
(`legacy` or `parity`; anything else fails before Harbor starts) for the
`plak` binary only; an arm without a variant gets the shipped default,
`parity`. Compare archived pairs with:

```sh
python3 eval/scripts/summarize.py --report target/prompt-ablation \
  --prompt-ablation <legacy-job-dir> <parity-job-dir>
```

Environment:

| Variable | Effect |
| --- | --- |
| `HARBOR_ARCH` | `amd64` or `arm64` cells |
| `HARBOR_N_ATTEMPTS` | trials per task (default 1) |
| `HARBOR_N_CONCURRENT` | concurrency for Plak and non-Claude vendor Rung A arms (default 1); record it with any comparison |
| `HARBOR_INCLUDE_TASK` | limit Rung A to one listed task, for debugging |
| `HARBOR_COMPARISON_ID`, `HARBOR_REPEAT_BLOCK` | label a repeat campaign and its round |
| `PLAK_HOST_REFRESH=0` | skip the host refresh before a Plak OAuth arm |
| `PLAK_OR_BUDGET_BASELINE`, `PLAK_OR_HELD_USD` | OpenRouter budget watcher inputs (below) |

`comparisons/*.json` are digest-locked A/B plan fixtures: they freeze dataset,
task names, model, effort and named arms, and `validate_plan` checks the shape
of their `sha256:` digests and job ids, not that the digests describe any
bytes. `candidateProvenanceDigest` stays null, so no plan there is a
publishable result.

Jobs, binaries and traces stay untracked: `.plak/harbor/jobs/` holds the
Harbor job trees and `.plak/installed/<arch>/` the frozen artifacts. Traces,
transcripts and credentials are private; shareable summaries use structural
fields and reason codes only.

## Reading results

```text
just harbor-summarize .plak/harbor/jobs/ab-plak-openrouter
just harbor-summarize --json .plak/harbor/jobs
just harbor-summarize --report target/eval-report .plak/harbor/jobs/ab-plak-codex
```

`eval/scripts/summarize.py` reads only harvested files (no Docker, Harbor or
model calls) and prints, per job, task, reward, termination class, tool
rejections, tokens and agent execution time, plus the `plak` and `pmuxd`
digests with trial counts (`<-- MORE THAN ONE` when a job ran two builds).
`--report DIR` writes `eval-report.json` and `eval-report.md`; Rung A writes
one automatically under each invocation's `comparison-report/`. `--selection
INDEX.json` selects archived results by path and hash.

Every trial carries a termination class beside its reward, from
`/logs/agent/plak-termination.json` and the job's `termination-summary.json`:

| Class | Meaning |
| --- | --- |
| `completed` | The agent exited 0; the reward is a real score |
| `turn_failed:<kind>` | The turn died with a typed diagnostic |
| `agent_init_failed:<Kind>` | `plak run` refused before opening a session (`plak: agent could not be initialized (<Kind>): …`); no turn was dispatched |
| `agent_exit_nonzero` | Nonzero exit with no recognised diagnostic |
| `harness_exception:<Type>` | Harbor itself raised |
| `result_unparseable` | The trial's `result.json` could not be read; nothing may be concluded |
| `unknown` | No exit code, diagnostic or exception |

| Kind | Read from |
| --- | --- |
| `context-capacity` | `context admission denied`, `cannot be compacted` |
| `round-limit` | `exceeded its model-operation limit` |
| `credential` | `native subscription credential is unavailable`, HTTP 401 |
| `provider-rate-limit` | HTTP 429, `rate limit exceeded`, `ApiRateLimitError` |
| `provider-rejection` | `server_is_overloaded`, a typed terminal provider error |
| `tool-failure` | `mutating tool outcome`, `local composition host`, journal reconciliation |
| `other` | A turn failure with no recognised marker |

A Harbor `AgentTimeoutError` is `agent_timeout`; a verifier timeout is an
error, not reward 0. On the TB 2.1 slices a reward without assertion-bearing
`verifier/ctrf.json` is unverified.

## Rules

- **Native-home pairs.** A Plak arm is compared only with the vendor CLI for
  the same account, model and effort. Each CLI gets the documented effort flag
  (Plak `--effort`, Codex `-c model_reasoning_effort=…`, Grok
  `--reasoning-effort`, Pi `--thinking`, OpenCode `--variant`); a CLI with no
  documented equivalent is skipped, never defaulted.
- **Claude Code is FrozenPmux `pmux run`** in a Full cell under subscription
  OAuth, never Harbor's stock `claude-code` agent (`claude --print` with
  `ANTHROPIC_API_KEY`) and never `pmux ask`. The cell drops to an unprivileged
  `pmux` user with passwordless sudo, since Claude Code refuses
  skip-permissions as root.
- **Credentials.** Cells upload only their vertical's host credential and
  refuse `OPENAI_API_KEY`, `XAI_API_KEY` and `OPENROUTER_API_KEY` from the
  Harbor environment. Cells never `plak refresh`: the host refreshes once
  before each Plak OAuth arm. Never overlap two arms that share one OAuth
  refresh token.
- **One FrozenPmux trial at a time.** The Claude refresh token is
  **single-use**: concurrent cells invalidate each other's copies and die with
  `NeedsLogin` before any turn, so `claude-code` always runs with
  `--n-concurrent 1`.
- **Never pass `--ae NAME=true`.** Harbor redacts every `--ae` value from
  every text file in the trial, so `true` becomes `[REDACTED]` and
  `result.json` stops parsing. Vendor flags go through the Harbor process
  environment instead. Old trees are repaired on read and marked
  `[repaired]`; repaired results are diagnostic only.
- **One task run per attempt.** Provider overload or HTTP 429 never triggers
  an adapter-owned retry; extra attempts need `HARBOR_N_ATTEMPTS`. Rung A
  fails unless Harbor produced the expected number of readable trial results
  and a termination summary.
- **Cost.** Implied USD applies `harbor_adapter/rate_card.json` (public list
  prices) to canonical token buckets. It is an estimate, not a bill; missing
  usage stays unknown, never `$0`, and subscription arms report tokens.
- **OpenRouter budget.** Paid OpenRouter arms run serially under
  `eval/scripts/openrouter_watch.py`, a soft stop on the key's reported usage.
  Record a private baseline once:

  ```sh
  python3 eval/scripts/openrouter_watch.py --key-file <openrouter.key> \
    --baseline <private-dir>/openrouter-key-baseline.json init
  ```

  then set `PLAK_OR_BUDGET_BASELINE` to that absolute path for every
  OpenRouter arm, and `PLAK_OR_HELD_USD` to spending held by other runs on the
  same key. The watcher stops at $8 spent or $2 remaining of the $10 ceiling.
  Pi and OpenCode must use the same key as the watcher. Never reset the
  baseline or switch keys to reset the budget.
