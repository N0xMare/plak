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

- Python 3.12 or newer on `PATH`, a local Docker daemon with buildx, and [uv](https://docs.astral.sh/uv/).
  Harbor is pinned at 0.23.0 in `eval/pyproject.toml` and always runs through
  `uv run --project eval`, never a global `harbor`.
- Host credentials for the arms you run:
  - Plak OAuth: `plak login codex`, `plak login grok`, `plak login claude`;
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
just fetch-harbor-vendor-clis  # versioned prefixes; review and freeze archive bytes below
just fetch-harbor-tools        # pinned public ripgrep release for every arm
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

## Validity Gate

Historical jobs and `comparisons/*.json` drafts are diagnostic only. No scored
campaign is enabled by default. All run entry points require `HARBOR_CAMPAIGN`
and `HARBOR_CAMPAIGN_SHA256`; a changed file, dirty eval source, absent
calibration or missing pin fails closed before dispatch. `--print-config` and
the offline tests need neither credentials nor a campaign.

Prepare a private campaign JSON (schema in `harbor_adapter/campaign.py`):

1. Record `schemaVersion: 1`, `source_commit`, `arch`, `concurrency: 1` and
   `blocks` (at least three for the paired estimator).
2. Freeze each task image once on the selected architecture. Inspect it with
   `docker image inspect --format '{{.Id}}' IMAGE`. The `tasks` object maps the
   exact task names to `image` (`sha256:...`, not a tag), task `checksum`,
   `agent_timeout`, `verifier_timeout` and `cell_manifest_sha256`. Compose
   tasks with multiple images are not supported by this binding yet.
3. Build Plak with `just build-harbor-agent`. Its `plak-build.json` binds the
   source commit/dirty state, maintainer feature, toolchain and binary hashes.
   Dirty or source-less builds are refused. This is a glibc, maintainer-featured
   eval build, not the supported musl release artifact.
4. Review the complete frozen vendor archives, not just version strings.
   `artifact_sha256` maps `.plak/installed/<arch>/...` paths to SHA-256 values.
   This campaign pin table is mandatory at launch and enforced again on vendor
   uploads. Fetching a new prefix does not approve its bytes: npm dependency
   resolution at fetch time is not a reproducible source build. Never rewrite
   a campaign pin to accept replacement bytes mid-campaign.
5. Each `arms` entry contains `account`, `binaries` (every executed/archived
   artifact name from the adapter identity record to its hash),
   `observed_config`, `calibration_file`, `calibration_sha256` and `policy`.
   Policy records `user`, `web_tools`, `leftover_processes: "preserve"` and
   `cell_prep`. The run user, package/probe manifest, effort, thinking and
   requested service tier must match across the pair. A claimed policy is not
   enough: the actual cell manifest and binaries are checked before dispatch.
6. Calibrate the first wire request for each arm and reduce it with
   `harbor_adapter/observed_config.py`. The reducer accepts `.request` JSON and
   matching `.body` JSON/SSE files; only structural fields and digests leave
   the cell. Pin that reduced document's file hash as `calibration_sha256` and
   its `first_request` as `observed_config`. Pin the resolved shipped
   composition for Plak as `resolved_composition` as well. Codex rollout and Pi
   session declarations are extracted but deliberately cannot satisfy wire
   calibration. A proxy/first-party wire capture for Grok, Claude Code and
   OpenCode remains required; this change does not supply that recording proxy.

The package/tool/cgroup probe digest is recorded as `cell-preflight.json`.
Plak refreshes short-lived OAuth on the host under an upload lock. Native
stores must already cover the task and verifier budgets plus five minutes;
missing expiry or insufficient lifetime refuses dispatch. Refresh those
stores serially on the host before retrying, not in a sibling trial.
Only one campaign process may use an account/store at a time. Serial slots
are enforced within an invocation; a cross-campaign credential lease is not
implemented.
All arms get the same pinned `rg` 15.1.0 at `/usr/local/bin/rg`; amd64 uses the
official static musl release, arm64 the official GNU release (no static arm64
release exists at this pin). Both archive and executable hashes are pinned;
the actual cell must start it. The arm64 GNU loader requirement remains an
explicit limitation, not silent apt installation.

After reviewing the private plan's SHA-256, run a vertical with:

```sh
export HARBOR_CAMPAIGN=/absolute/private/campaign.json
export HARBOR_CAMPAIGN_SHA256=<reviewed-file-sha256>
just harbor-rung-a ab all
```

`all` uses alternating AB/BA replicate blocks, one attempt per task/arm slot,
and writes a hash-bound `selection.json` under the new `campaign-*` directory.
Infra slots get at most two reruns; exhausted slots are dropped for both arms.
More than 10% infra or 2% unknown for an arm halts at the block boundary.
An agent trial failing the post-run wire/served-identity/artifact gate stops
the campaign rather than silently entering the estimator. Report the selected
slots with `summarize.py --selection .../selection.json --report DIR .../campaign-*`.

Current blockers are intentional: the adapters still have different package
and user policies (notably pmux), and vendor declarations alone are not wire
evidence. Such campaigns must remain blocked until equal cell policy and
per-trial wire evidence are available. Do not weaken a pin or synthesize an
observation from requested flags to make the gate pass.

Plak's live home, OAuth state and exchange captures stay under `/tmp/plak-cell`.
The reducer reads automatic, non-required captures from
`/tmp/plak-cell/home/wire-captures`, outside both harvested subtrees.
Only `state/` and `logs/` are copied to the harvested `plak-home`; secret roots,
external symlinks and exact known secret values are removed and cause a refusal.
Generic secret-pattern hits preserve task content and flag the trial unknown.
`harvest.json` certifies the final scan. Old private job trees are not modified
automatically: they may still contain historical credential copies and must
not be shared. Rotating/deleting those private historical secrets is an
operator action, not part of a scored run or this offline test suite.

## Running

| Rung | Dataset | Recipe |
| --- | --- | --- |
| A plumbing | three local tasks in `datasets/rung-a-plumbing` (`write-hello`, `sum-file`, `fix-typo`) | `just harbor-rung-a plumbing <arm>\|all` |
| A cheap A/B | eight TB 2.1 tasks from `terminal-bench/terminal-bench-2-1@6` | `just harbor-rung-a ab <arm>\|all` |
| A expanded | those eight plus eight more TB 2.1 tasks (16) | `just harbor-rung-a expanded <arm>\|all` |
| B wire | one task of `terminal-bench/terminal-bench@4.0.0` | `just harbor-rung-b wire [plak-codex\|codex] [terminal-bench/<task>]` |
| Smoke | five hard TB 4.0 tasks, serial | `just harbor-smoke <arm>\|all` |

The task sets are frozen in `rung-a-*.yaml`, `harbor_adapter/tuples.py` and
`harbor_adapter/verifier_evidence.py` (task refs and checksums). TB 2.1 and
TB 4.0 scores are not comparable, and neither are amd64 and arm64 timings:
arm64 job names carry an `-arm64` suffix. Build or pull each task image once
before the campaign (no per-trial pre-pull); every trial uses its recorded
image ID, never `--force-build`. All launches require a reviewed campaign.
`rung-a.sh` refuses to launch unless the include list is
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
| OpenRouter arms | disabled | no admitted explicit shared wire effort; `none`/omit is not parity |
| `plak-claude` / `claude-code` | `plak run` / FrozenPmux `pmux run` | claude-opus-5-5, low |
| `all` | the reviewed campaign's arms, interleaved | one vertical/model/effort per campaign |

Prompt ablation arms compare two prompt variants of the same `plak` binary:
`plak-codex-legacy` / `plak-codex-parity`, and the same pairs for `grok` and
`claude`; `ablation` dispatches all six only when admitted by the reviewed
campaign. The arm sets `PLAK_PROMPT_VARIANT`
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
| `HARBOR_N_ATTEMPTS` | must be 1; campaign `blocks` supplies repeat slots |
| `HARBOR_N_CONCURRENT` | must be 1 for every arm; no refresh-token races |
| `HARBOR_INCLUDE_TASK` | scheduler slot selector: one already-listed task |
| `HARBOR_COMPARISON_ID`, `HARBOR_REPEAT_BLOCK` | label a repeat campaign and its round |
| `HARBOR_CAMPAIGN`, `HARBOR_CAMPAIGN_SHA256` | private campaign JSON and its independently reviewed file SHA-256 |
| `HARBOR_ARM` | arm key for direct `harbor-eval`; Rung A/B and smoke set it |

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

The primary `paired_estimates` use only validity-qualified, aligned scored
slots: task-weighted differences, at least three scored trials per task/arm,
10,000 deterministic two-stage bootstrap replicates and fixed-task intervals.
A sensitivity estimate drops provider-after-retry slots symmetrically. Holm,
permutation tests, TOST and secondary-outcome intervals are not implemented;
these intervals alone do not authorize a multiplicity-adjusted ranking claim.
Per-arm Wilson/operational rates remain diagnostic; `primary_pass_rate` is
null when no trial passes the validity gate.

Outcome attribution is symmetric: `infra` and `unknown` are unscored; `agent`
uses assertion-verified reward even after timeout, context/tool failure or
provider-after-retry failure. Harbor's regex exception subtype is never native
provider evidence. A shared provider incident needs reviewed block evidence;
the reporter does not infer one from tool chatter or two unrelated errors.

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

Agent timeouts and agent-attributable nonzero exits retain the verifier reward.
A verifier timeout is unscored, not reward 0. On the TB 2.1 slices a reward without assertion-bearing
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
  Harbor environment. Cells never `plak refresh`: before each Plak trial the host checks access expiry
  under a lock and refreshes if it cannot cover the task agent timeout, verifier
  timeout and five-minute margin. Insufficient lifetime after refresh refuses dispatch. Never overlap two arms that share one OAuth
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
  an adapter-owned retry; repeats use campaign `blocks`, never
  `HARBOR_N_ATTEMPTS`. Rung A
  fails unless Harbor produced the expected number of readable trial results
  and a termination summary.
- **Cost.** Implied USD applies `harbor_adapter/rate_card.json` (public list
  prices) to canonical token buckets. It is an estimate, not a bill; missing
  usage stays unknown, never `$0`, and subscription arms report tokens.
- **OpenRouter.** Comparison arms are disabled until explicit shared wire effort
  is admitted; the standalone budget watcher is not a campaign launch path.
