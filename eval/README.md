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
exist for diagnostic builds, and both are recorded. Generated campaigns still
require the exact clean pin and refuse both overrides:

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

1. Record `schemaVersion: 1`, `phase` (`calibration`, `null` or `scored`),
   `source_commit`, `arch`, `concurrency: 1` and `blocks` (at least three for
   the paired estimator; exactly one for calibration).
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
   calibration. Comparator arms now run a cell-local HTTPS recording proxy;
   Plak uses its own private capture, never a changed subscription URL.

Claude-only instruction comparison has one reviewed projection. The exact
`instructions_sha256` and full `request_sha256` are still recorded for every
trial. `claude_instruction_projection_sha256` normalizes only three known
per-request values on the first `x-anthropic-billing-header` system line:
`cc_prompt_id` UUID, the five-hex body-derived `cch`, and the three-hex
first-user-text suffix of `cc_version`. Their first-party definitions are
`crates/providers/anthropic/src/cch.rs:41` (version suffix),
`crates/providers/anthropic/src/cch.rs:78` (prompt UUID), and
`crates/providers/anthropic/src/cch.rs:142` (body attestation). The base client
version, entrypoint, all other fields (including unknown new fields), all
other instruction text and every other wire control stay pinned. Only Claude
arms match this projection at the trial gate; Codex/Grok remain exact, and
missing or malformed projections never authorize an exception. No request
body is rewritten and no projected digest is mislabeled as a wire digest.

The package/tool/cgroup probe digest is recorded as `cell-preflight.json`.
Plak refreshes short-lived OAuth on the host under an upload lock. Native
stores must already cover the task and verifier budgets plus five minutes;
missing expiry or insufficient lifetime refuses dispatch. Refresh those
stores serially on the host before retrying, not in a sibling trial.
Only one campaign process may use an account/store at a time. The scheduler
holds host-local account leases across all its serial slots, including retries.
Do not use the same subscription account concurrently on another host.
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

Cells must be frozen with the common root/package policy below. Every arm
refuses missing prerequisites instead of independently installing packages.
Claude Code runs as root through pmux with `IS_SANDBOX=1`, and its first-party
launcher denies WebSearch/WebFetch; Codex explicitly disables web search.
Calibration must confirm those settings from bytes. A flag is never evidence.
`READINESS.md` records every D1-D32 disposition and the deliberately deferred
OpenRouter and exact native-retry questions.

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

## 0.0.2 Operator Sequence

All paths below are absolute private paths outside the repository. Do not put
credentials, raw requests or campaign files in the tree. Run from a clean
checkout on one native Docker host (for example an x86_64 Linux host with a native Docker daemon);
never mix native architectures in a campaign. Worker Rust builds/gates use
the assigned wtools lane, not the Mac. Python-only adapter tests can use the
Mac eval venv. None of the preparation commands below sends model requests.

First integrate the current GPT-6 Codex prompt and the amp-jobs change, then
build **that commit**. `plak run --leave-jobs-running` must appear in
`run_arguments`; the in-cell `run --help` probe refuses older builds before
credential upload. This eval change does not duplicate that product flag.
An old Plak manifest, old CLI version, dirty source or replacement archive
cannot be approved by editing its hash. Rebuild/refetch instead.

```sh
just harbor-bootstrap
PATH="$PWD/eval/.venv/bin:$PATH" just harbor-test
export HARBOR_ARCH=amd64                 # arm64 only on a native arm64 daemon
just build-harbor-agent                  # worker protocol: assigned Linux lane
just fetch-harbor-vendor-clis
just fetch-harbor-tools
# Claude only: pinned clean PMUX_SRC, plus a Linux OAuth file store
just fetch-harbor-pmux
```

The preparation commands below export `terminal-bench/terminal-bench-2-1@6`
to a private task directory (`TB21`, one version only). Refresh or
log in to each selected subscription store serially on the host. A native
host `plak` must be on PATH for `plak refresh ACCOUNT`. Codex, Pi, Grok and
Claude stores must cover agent + verifier budgets + 300 seconds. Do not copy
one arm's OAuth document into another arm's different native store format.
No API key is needed for the Codex, Grok or Claude verticals. OpenRouter stays
disabled regardless of the presence of a key.

### Prepare Phase 0

`harbor-inventory` discovers task paths and names the frozen artifacts; it does
not fabricate hashes or calibrations. `harbor-freeze` builds/pulls every task
once, then bakes the **same** `ca-certificates coreutils procps curl git
python3 openssl bash` package policy as root into it. These are derived eval
images; each resulting immutable image ID is shared by every arm. There is
no inference and no per-trial apt install. Compose/multi-image tasks are refused.
For native arm64, or when intentionally rebuilding the task source, pass
`--build-tasks` to `harbor-freeze`; this explicitly builds each task Dockerfile
once instead of pulling a possibly amd64-only prebuilt tag. There is no silent
architecture fallback or per-trial force-build. BuildKit cannot use a bare
image ID in `FROM`; freezing creates a unique base tag for those already
resolved bytes and records `base_image`/`base_tag` in the private inventory.
Those pins are retained deliberately: removing the last tag could delete an
image ID another campaign still references. Pruning is an explicit operator
action after all campaigns using those images have ended.
`harbor-campaign` inspects image architecture, probes the real cell (network
none), hashes source task contents, archives, archive binary members and the
clean Plak build, and writes a mode-0600 JSON plus its SHA-256. Outputs never
overwrite an existing reviewed file.

```sh
export PRIVATE=/absolute/private/harbor-v002
mkdir -m 700 "$PRIVATE"
uv run --project eval harbor datasets download terminal-bench/terminal-bench-2-1@6 \
  --export --output-dir "$PRIVATE/tasks"
export TB21="$PRIVATE/tasks"
just harbor-inventory --arch "$HARBOR_ARCH" --slice plumbing \
  --task-root "$PWD/eval/datasets/rung-a-plumbing" \
  --arms plak-codex codex pi-codex-oauth opencode-codex-oauth plak-grok grok-build plak-claude claude-code \
  --output "$PRIVATE/plumbing-inputs.json"
just harbor-freeze --inventory "$PRIVATE/plumbing-inputs.json" \
  --output "$PRIVATE/plumbing-frozen.json"
just harbor-campaign --inventory "$PRIVATE/plumbing-frozen.json" \
  --phase calibration --slice plumbing --blocks 1 --output "$PRIVATE/phase0.json"
```

The calibration plan selects `write-hello` only: **one trial per selected
arm**, not three plumbing tasks times three scored blocks. Unlike a scored
plan it admits absent calibration pins and different models across verticals.
It makes no wire/policy parity claim, is explicitly unscored, and cannot
satisfy a scored trial gate. Review its
one JSON and the printed SHA-256 before granting paid approval.

```sh
export HARBOR_CAMPAIGN="$PRIVATE/phase0.json"
export HARBOR_CAMPAIGN_SHA256=<reviewed-sha256>
just harbor-phase0                       # PAID: 8 trials for the eight arms above
```

The scheduler prints its output directory. Only reduced structural evidence
is copied into source/platform/binary-bound `calibrations/ARM.json`; the raw
Plak captures, ephemeral proxy
CA/key, request/response buffers and credentials never leave the cell. A
calibration succeeds only with actual wire evidence **and** a passing plumbing
verifier. A failed calibration is diagnostic, never a license to infer an
observation from CLI flags. Set `CAL` to that output's `calibrations` directory.

The proxy is stdlib Python + OpenSSL, bound only to cell loopback. It records
HTTPS CONNECT to exact allowlisted DNS hosts on 443, verifies upstream TLS,
streams SSE, and writes only `observed_config.py` structural fields/digests.
It never logs headers, URLs, exceptions or content. Its buffers are bounded.
Comparator model traffic is decrypted only for the exact listed provider hosts;
other HTTPS (including model-launched tool traffic) is blind-tunnelled without
observation, and ordinary HTTP is not proxied. Recorders remain alive until
Harbor tears down the cell, so a surviving service does not inherit a dead
proxy at the agent/verifier boundary.
The current transport is HTTP/1.1, not WebSocket; a CLI bypassing the proxy or
requiring a different transport fails closed. Confirm the specific frozen
CLI's first-party proxy/debug settings during Phase 0 before any scored run.
The utility can also calibrate OpenCode manually inside a disposable cell
without enabling an OpenRouter comparison. `opencode-codex-oauth` is a
calibration-only arm using OpenCode's own `openai` OAuth entry; it cannot be
included in a scored comparison. For a separate manual subscription capture,
upload the `harbor_adapter` package's `recording_proxy.py`, `observed_config.py`
and empty `__init__.py` under `/tmp/harbor-wire-code/harbor_adapter`, then:

```sh
# Inside the cell only; never apply these variables to Plak.
PYTHONPATH=/tmp/harbor-wire-code python3 -m harbor_adapter.recording_proxy \
  --directory /tmp/opencode-wire --host chatgpt.com --host api.openai.com --port 18080
# In a second cell shell, for the comparator process only:
HTTPS_PROXY=http://127.0.0.1:18080 NODE_EXTRA_CA_CERTS=/tmp/opencode-wire/ca.pem \
  opencode --model=openai/gpt-6-luna run --variant=low <reviewed plumbing prompt>
```

This manual invocation is also paid and needs approval. Export only
`/tmp/opencode-wire/observed-config.json`, then stop that recorder. No raw
request, response or proxy CA/key may be exported. Plak's automatic private
capture remains the preferred exact-URL path. A rotated/incomplete Plak
capture is not a verified whole-run observation.

### A/A Null Test

Create/freeze the eight-task inventory once, preserving the same reviewed
binary artifacts and source. Replace `TB21` and `CAL` with absolute paths.
`--phase null` automatically creates `plak-codex` and `plak-codex-aa` with
identical binaries, tuple, composition and calibration, not prompt ablations.

```sh
just harbor-inventory --arch "$HARBOR_ARCH" --slice ab --task-root "$TB21" \
  --arms plak-codex --calibrations "$CAL" --output "$PRIVATE/null-inputs.json"
just harbor-freeze --inventory "$PRIVATE/null-inputs.json" --output "$PRIVATE/null-frozen.json"
just harbor-campaign --inventory "$PRIVATE/null-frozen.json" --phase null --slice ab \
  --blocks 3 --output "$PRIVATE/null.json"
# Review null.json and its SHA-256, then obtain paid approval.
export HARBOR_CAMPAIGN="$PRIVATE/null.json"
export HARBOR_CAMPAIGN_SHA256=<reviewed-sha256>
just harbor-null                         # PAID: 8 x 3 x 2 = 48 trials
```

`null-receipt.json` must say `passed: true`: all eight tasks retained with
three scored trials per label, paired 95% CI covering zero and infra strictly
below 5% per arm. Failed receipts block the Codex recipe; never rerun a null
until favorable without documenting the failure and the changed pipeline.
Set `NULL` to the successful receipt path.

### Codex Pair

Freeze the expanded slice once (16 tasks), generate a fresh scored plan with
both calibration files and the reviewed A/A receipt, then review its JSON and
printed SHA-256. The generator binds the receipt's file digest; the launch
recipe additionally checks its successful verdict, source, architecture and
Plak binary hashes. Preparation itself does not authorize inference.

```sh
just harbor-inventory --arch "$HARBOR_ARCH" --slice expanded --task-root "$TB21" \
  --arms plak-codex codex --calibrations "$CAL" --output "$PRIVATE/codex-inputs.json"
just harbor-freeze --inventory "$PRIVATE/codex-inputs.json" --output "$PRIVATE/codex-frozen.json"
just harbor-campaign --inventory "$PRIVATE/codex-frozen.json" --phase scored --slice expanded \
  --blocks 5 --null-receipt "$NULL" --output "$PRIVATE/codex.json"
export HARBOR_CAMPAIGN="$PRIVATE/codex.json"
export HARBOR_CAMPAIGN_SHA256=<reviewed-sha256>
just harbor-codex                        # PAID: 16 x 5 x 2 = 160 trials
```

All launches are serial. Alternating AB/BA blocks yield the preregistered
ABBA order across blocks. Infra slots may add at most two retries each;
exhausted slots are dropped symmetrically. More than 10% infra or 2% unknown
per arm stops at the block boundary. A wire/artifact validity failure stops
immediately rather than spending the remaining campaign budget.

Subscription costs are **tokens/quota, not USD estimates**. Planned usage:
Phase 0 = 1 task x 1 block x N arms (8 above); A/A = 48; Codex = 160;
subsequent Grok/Claude pairs = 160 each. Retry ceilings are 3 times the planned
scored slots, but health thresholds normally stop bad plumbing far earlier.
The 40-task generalization slice is 40 x 3 x 2 = 240 per pair, separately
preregistered. OpenRouter has no authorized phase or spend here. Do not extend
a nonsignificant 16 x 5 result: its fixed-task MDE is about 15 percentage
points and its task-population MDE about 22-32 points, not 3-5 points.

### Refusals

The preflight rejects changed/unreviewed plans, dirty/mismatched source or
Plak builds, missing/wrong bytes, nonnative architecture, moving image tags,
absent tools/CA/cgroup probes, mismatched package/resource manifests,
nonserial scheduling, unsafe credential lifetime and a leased account.
Scored plans additionally reject absent/unverified wire calibration,
model/effort/thinking/tier/policy differences, enabled web tools, secrets in
harvests, served-identity gaps, changed composition, or changed executed
artifacts. Calibration cannot be scored. Null designs require exactly eight
tasks, three blocks and two identical labels. `harbor-codex` requires exactly
the expanded Codex pair, five blocks and the matching successful A/A receipt.
Do not weaken any gate to make a historical job or unobserved arm pass.
Before paid dispatch, test Docker create/start/stop/cleanup on an owned,
credential-free disposable cell. A Linux Docker-host probe under concurrent gate load
needed about **9 minutes** to finish Compose cleanup, and a bounded kill API
call timed out before cleanup eventually completed. All probe resources were
removed and verified absent. Do not interpret `docker info` alone as cleanup
health; establish acceptable start/stop latency before spending subscription
quota, and classify daemon/cleanup stalls as infrastructure.

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
