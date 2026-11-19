# Live status

The current qualification state of the opt-in real-provider runs. Scenario
definitions and oracles are in [`README.md`](README.md); what a qualification
run binds is spec 05 §3.4, and a pass there covers only the tuple it names.
Raw reports stay private under the ignored `target/live/`.

## Qualification

`080c15df` (0.0.4) passed the spec 05 §3.4 pre-release bar:

- the integrated gate on both supported platforms: `just full` on Apple
  Silicon and on Linux x86_64, the native musl tier and the pinned musl
  container gate, and the strict crash-point sweep over all fifty
  shapes (2,111 indexed and 187 wire-restart cuts) with no unclassified,
  divergent or refused cut, plus the publish dry
  run;
- the 0.0.4 live matrix at `86ddea1c`, re-confirmed at `080c15df`, and the new GPT-6.2 rows at
  `080c15df` (History, below);
- a maintainer's review of the `080c15df` maintainer build through an independently
  maintained client.

`efe23aba` (0.0.3) and `25ea8365` (tag `v0.0.2`) passed the same bar for
their releases.

No later revision is qualified.

## Live matrix

The rows are the owner's catalog, the `ROUTES` keys in `live.py`, in ladder
order (spec 05 §3.4). A row shows its latest result, from the 0.0.4 candidate
matrix where it ran there and from an earlier matrix otherwise; whether it
carries to a later revision is the §3.4 re-confirmation rule. A new row, or a
row whose model or effort changed, has no result yet.
Rung 3 rows are sparing and run only for a targeted confirmation.

| Route | Class | Rung | Last result |
| --- | --- | --- | --- |
| `or-nemotron-free` | informational | 1 | smoke pass at `86ddea1c` (Apple Silicon) |
| `or-nemotron-vl-free` | informational | 1 | smoke pass at `86ddea1c` (Apple Silicon) |
| `or-flint-free` | informational | 1 | smoke pass at `86ddea1c` (Apple Silicon) |
| `codex` | light | 1 | smoke pass at `86ddea1c` (Apple Silicon) |
| `claude-haiku55-low` | light | 1 | smoke pass at `86ddea1c` (Apple Silicon) |
| `grok-46` | light | 1 | smoke pass at `86ddea1c` (Apple Silicon) |
| `api-openai` | light | 1 | smoke pass at `86ddea1c` (Apple Silicon, Linux x86_64 musl) |
| `api-anthropic` | light | 1 | smoke pass at `86ddea1c` (Apple Silicon, Linux x86_64 musl) |
| `api-xai` | light | 1 | smoke pass at `86ddea1c` (Apple Silicon, Linux x86_64 musl) |
| `codex-sol` | full | 2 | full cell pass at `86ddea1c` (Apple Silicon); Linux x86_64 musl smoke pass; re-confirmation smoke pass at `080c15df` on both platforms |
| `claude-sonnet55-low` | full | 2 | full cell pass at `86ddea1c` (Apple Silicon); Linux x86_64 musl smoke pass; re-confirmation smoke pass at `080c15df` on both platforms |
| `grok` | full | 2 | full cell and one-hour headless soak pass at `86ddea1c` (Apple Silicon); Linux x86_64 musl smoke pass; re-confirmation smoke pass at `080c15df` on both platforms |
| `deepseek` | full; long-session informational | 2 | full cell pass at `86ddea1c` (Apple Silicon, the informational long-session lane included); Linux x86_64 musl smoke pass; re-confirmation smoke pass at `080c15df` on both platforms |
| `deepseek-messages` | full; long-session informational | 2 | full cell pass at `86ddea1c` (Apple Silicon, the informational long-session lane included); Linux x86_64 musl smoke pass; re-confirmation smoke pass at `080c15df` on both platforms |
| `or-glm` | light | 2 | smoke pass at `86ddea1c` (Apple Silicon) |
| `codex-luna-medium` | light | 2 | smoke pass at `86ddea1c` (Apple Silicon) |
| `claude-haiku55-medium` | light | 2 | smoke pass at `86ddea1c` (Apple Silicon) |
| `claude-sonnet55-medium` | light | 2 | smoke pass at `86ddea1c` (Apple Silicon) |
| `codex-sol62-low` | light | 2 | smoke pass at `080c15df` (Apple Silicon) |
| `codex-sol62-medium` | light | 2 | smoke pass at `080c15df` (Apple Silicon) |
| `api-openai-sol62-low` | light | 2 | smoke pass at `080c15df` (Apple Silicon, Linux x86_64 musl) |
| `claude-opus-low` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-opus-medium` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `codex-astra` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-fable` | light, sparing | 3 | smoke pass at `86ddea1c` (Apple Silicon), confirming the `claude-fable-5-2` tuple |
| `codex-astra62-low` | light, sparing | 3 | smoke pass at `080c15df` (Apple Silicon), confirming the `gpt-6.2-astra` tuple |
| `codex-astra62-medium` | light, sparing | 3 | smoke pass at `080c15df` (Apple Silicon), confirming the `gpt-6.2-astra` tuple |
| `api-openai-astra62-low` | light, sparing | 3 | smoke pass at `080c15df` (Apple Silicon, Linux x86_64 musl), confirming the `gpt-6.2-astra` tuple |

## Credential gateway

`just live gateway` prints the offline plan (seven routes, two topologies,
a reopen of every session across a gateway restart, a forced refresh of each
OAuth kind) and a `just live estimate` line, and calls no provider.
`just live gateway --paid` runs that plan: a host holding its logins and keys
in-process, then `plakd gateway serve` adopting that home and the machine's
subscription store in place, reached by a keyless host through `[gateway]
address` on its Unix socket. A binary without `plakd gateway serve` fails as a
harness error before any provider call. Tailnet TCP from a second machine,
remote login, the kill switch and `hop=gateway` faults are not driven.

`just live gateway --paid` passed at `53562def3` on Linux x86_64 (glibc)
with a `plakd/test-hooks` release build (binary ID `c1487bbb9a09`, unchanged
across the run). All 41 steps passed on the first attempt: both topologies
completed all seven turns and reopens and all three forced OAuth refreshes.
Every Unix-socket turn and reopen added gateway audit lines. With the
service holding the real logins and the host holding no keys, `plakd doctor`
reported `OK` and `ready · record held by the gateway` for Codex, Claude and
Grok, and `plakd status <account>` reported `credential=ready` for each.

`just live gateway --paid` passed at `bb356f99` on Linux x86_64 (glibc), a
`plakd/test-hooks` release build (`plakd` SHA-256 `c0bbfd4e…`, unchanged
across the run): all 37 steps on the first attempt, with no rerun.

| Topology | Steps | Outcome |
| --- | --- | --- |
| in-process | turn and reopen on `codex`, `claude-haiku55-low`, `grok`, `api-openai`, `api-anthropic`, `api-xai`, `or-nemotron-free`; forced refresh of codex, claude and grok | pass |
| unix-socket | gateway start; turn on each of the seven routes; restart onto a new instance id; reopen of every session; forced refresh of codex, claude and grok, each answering `credential=ready` | pass; the keyless host held no key file, and every turn and reopen added gateway audit lines |

## History

### 0.0.4 candidate matrix (`86ddea1c`, re-confirmed at `080c15df`)

`plakd/test-hooks` release builds, composition `plakous-default`: `86ddea1c` for
`aarch64-apple-darwin` (`plakd` SHA-256 `8b0214d7…`, workers `874decb0…` and
`0a3cf5e9…`) and `x86_64-unknown-linux-musl` (`plakd` SHA-256 `8b8038f6…`,
workers `990e829b…` and `830c36c8…`); `080c15df` for the final re-confirmation
smokes and the GPT-6.2 rows (`plakd` `ebecd5e1…` on Apple Silicon, `3a82d5ef…`
on Linux x86_64 musl). The revisions after `86ddea1c` change package-verification
tooling (`10dcfe0c`), same-key upgrade inspection of paired-runner sessions
(`054c2d1e`), paired-runner job retention across planned handoff (`a80ceae4`,
`7d2e9f36`) and enroll `gpt-6.2-sol` and `gpt-6.2-astra` on the Codex and
OpenAI API routes with GPT-6.2 shipped Codex defaults (`080c15df`). No existing
row's rendered-prompt pins move, so the `86ddea1c` cells carry with the
re-confirmation smoke on each `full` row, which also passed at `10dcfe0c`,
`054c2d1e` and `a80ceae4` on both platforms. The six GPT-6.2 rows are new tuples
and ran their class depth, the eleven-scenario smoke, at `080c15df`.

Commits after `382aa2ba` were re-recorded to correct three commits' author
metadata; every commit's tree is unchanged, so the gates and cells recorded
here ran on byte-identical sources under the commits' earlier identifiers.

Every cell below ran through the qualification cell driver. Before `86ddea1c`:

- The first Linux musl pass, at `c2e16bf9`, scored `api-anthropic` HARNESS:
  after compaction the request replayed a signed thinking block whose earlier
  history had been rewritten, and the Anthropic API refused it. `86ddea1c`
  invalidates native reasoning after history prefix rewrites. `api-openai`
  was provider on both attempts (the stored key was refused) and passed after
  the key was replaced.
- At `c2e16bf9` the rung-one `or-inkling-free` and `or-lfm-free` cells refused
  before a turn: OpenRouter withdrew both models. `ce2817fd` retired the rows.
- `c2e16bf9` moved the driver's scratch homes to a short private temporary
  root: under `target/live/` the host socket path exceeded the platform
  socket-path limit, so every resume scenario failed.

| Cells | Platform | Outcome |
| --- | --- | --- |
| Smoke on `codex`, `claude-haiku55-low`, `grok-46`, `api-openai`, `api-anthropic`, `api-xai`, `or-nemotron-free`, `or-nemotron-vl-free`, `or-flint-free` | Apple Silicon | pass |
| Smoke on `codex-luna-medium`, `claude-haiku55-medium`, `claude-sonnet55-medium`, `or-glm`, `claude-fable` | Apple Silicon | pass |
| Smoke, long-session `--all`, jobs and `crash_jobs` x3 on `claude-sonnet55-low` | Apple Silicon | pass |
| Smoke, jobs and `crash_jobs` x3 on `codex-sol` and `grok` | Apple Silicon | pass; jobs after its PROVIDER rerun (model lookup timeouts) |
| Smoke and long-session `--all` (informational) on `deepseek`; long-session `--all` (informational), jobs and `crash_jobs` x3 on `deepseek-messages` | Apple Silicon | pass |
| Long-session `--all` on `codex-sol` and `grok`; jobs and two further `crash_jobs` on `deepseek`; smoke on `deepseek-messages` | Apple Silicon | the first cells ran during a local network outage (DNS and connect failures on every provider, nothing sent) and scored harness or provider; fresh cells after the outage passed |
| Headless soak, one hour, `grok` | Apple Silicon | pass |
| Smoke on `codex-sol`, `claude-sonnet55-low`, `grok`, `deepseek`, `deepseek-messages`, `api-openai`, `api-anthropic`, `api-xai` | Linux x86_64 musl | pass, all on the first attempt |
| Re-confirmation smoke on `codex-sol`, `claude-sonnet55-low`, `grok`, `deepseek`, `deepseek-messages` at `080c15df` | Apple Silicon, Linux x86_64 musl | pass, all on the first attempt |
| Smoke on `codex-sol62-low`, `codex-sol62-medium`, `codex-astra62-low`, `codex-astra62-medium`, `api-openai-sol62-low`, `api-openai-astra62-low` at `080c15df` | Apple Silicon | pass, all on the first attempt |
| Smoke on `api-openai-sol62-low`, `api-openai-astra62-low` at `080c15df` | Linux x86_64 musl | pass, both on the first attempt |

### 0.0.3 candidate matrix (`bce238af` to `b2755c04`)

`plak/test-hooks` release builds, composition `plakous-default`: `bce238af` for
`aarch64-apple-darwin` (`plak` SHA-256 `90d2fc1b…`) and
`x86_64-unknown-linux-musl` (`plak` SHA-256 `9f5a277d…`); `e180b55b`
(`206773c1…`) and `b2755c04` (`4a5c4a0b…`) for the later Apple Silicon
DeepSeek cells. No row's rendered-prompt digests or harness footprints change
between `bce238af` and `efe23aba` (the later commits change TUI selection
painting, output-limit handling inside a tool call, tests and tooling), so
these results carry to the release candidate.

The first rung-one pass, at `0df98a47`, scored `fault_reset` HARNESS on every
route: the live driver's capture check predated the transport failure
record's `connect_attempts` field. `bce238af` corrected the driver, and every
rung-one row then passed on its first attempt.

| Cells | Platform | Outcome |
| --- | --- | --- |
| Smoke on `codex`, `claude-haiku55-low`, `grok-46`, `api-openai`, `api-anthropic`, `api-xai`, `or-nemotron-free`, `or-nemotron-vl-free` | Apple Silicon | pass |
| Smoke on `codex-luna-medium`, `claude-haiku55-medium`, `claude-sonnet55-medium`, `or-glm` | Apple Silicon | pass |
| Smoke, long-session `--all`, jobs and `crash_jobs` x3 on `codex-sol`, `claude-sonnet55-low`, `grok` | Apple Silicon | pass |
| Headless soak, one hour, `grok` | Apple Silicon | pass on its second run (131 of 131 tasks); both attempts of the first run were provider (model lookup timeouts during an upstream outage) |
| Smoke on `claude-sonnet55-low`, `api-openai`, `api-anthropic`, `api-xai` | Linux x86_64 musl | pass |
| Smoke on `codex-sol`, `grok`, `deepseek`, `deepseek-messages` | Linux x86_64 musl | pass |
| Smoke, jobs and `crash_jobs` x3 on `deepseek`; smoke and `crash_jobs` x3 on `deepseek-messages` | Apple Silicon | pass at `e180b55b` |
| Jobs on `deepseek-messages` | Apple Silicon | model, recorded at `e180b55b` after its MODEL rerun: `wake_while_idle`, the model kept the root busy while the job ran |
| Long-session `--all` on `deepseek` (informational) | Apple Silicon | pass at `e180b55b` after its MODEL rerun |
| Long-session `--all` on `deepseek-messages` (informational) | Apple Silicon | harness at `e180b55b`: a Messages `max_tokens` stop inside a tool call was rejected as a protocol error; fixed in `cbc135f2`, then pass at `b2755c04` after its MODEL rerun |
| Smoke on `or-inkling-free` | Apple Silicon | provider, recorded at `0df98a47`: OpenRouter refuses the model outside agentic harnesses |
| Smoke on `or-flint-free` | Apple Silicon | provider, recorded at `0df98a47`: no upstreams available on both runs |

### 0.0.2 matrix (`25ea8365`)

`plak/test-hooks` release builds for `aarch64-apple-darwin` and
`x86_64-unknown-linux-musl`, composition `plakous-default`. Route names are
the 0.0.2 `ROUTES` keys; `deepseek` and `deepseek-messages` were then
`deepseek/deepseek-v4.1-flash-1017`, `api-anthropic` was `claude-haiku-5-5` at
medium and `api-xai` was `grok-4.7` at low. `or-mimo`
(`xiaomi/mimo-v2.6-flash`), `or-qwen37` (`qwen/qwen3.7-flash`),
`api-openai-medium` (`gpt-6-luna` at medium), `or-laguna-free`
(`poolside/laguna-s-2.1:free`), `or-ling` (`inclusionai/ling-3.0-flash-vl`)
and `or-qwen38` (`qwen/qwen3.8-27b`) have since left the catalog.

At `25ea8365` the re-confirmation smoke on every `full` row passed on its
first attempt on both platforms. Since `f07e205e` the product changed Codex
native compaction successors (`3d6f915d`) and the TUI Usage tab's refresh
action and host-loop reads (`e873c6a8`, `a3106d2f`, `25ea8365`), and the live
drivers and matrix changed; no row's rendered-prompt digests or harness
footprints moved, so every other cell carried its earlier result.

| Cells | Platform | Outcome |
| --- | --- | --- |
| Smoke on `codex-sol`, `claude-sonnet55-low`, `grok`, `deepseek`, `deepseek-messages` | Apple Silicon, Linux x86_64 musl | pass at `25ea8365` |
| Jobs on `codex-sol` | Apple Silicon | pass at `6e8810a5` after its one MODEL rerun (`run_promoted`: the model waited for the command) |
| Long-session `--all` and two further `crash_jobs` on `codex-sol` | Apple Silicon | pass at `3d6f915d` |
| Long-session `--all`, jobs and `crash_jobs` x3 on `grok` and `claude-sonnet55-low` | Apple Silicon | pass at `f07e205e` |
| Jobs and `crash_jobs` x3 on `deepseek` and `deepseek-messages` | Apple Silicon | pass at `f07e205e` |
| Long-session `--all` on `deepseek` and `deepseek-messages` | Apple Silicon | model, recorded: the summary leaves out an earlier-turn fact |
| Headless soak, one hour, `deepseek` | Apple Silicon | pass at `f07e205e` |
| Smoke on the 17 `light` rows | Apple Silicon | pass at `f07e205e`; `or-mimo` after its PROVIDER rerun |
| Smoke on `api-openai`, `api-openai-medium`, `api-anthropic`, `api-xai` | Linux x86_64 musl | pass at `f07e205e` |
| Smoke on `or-nemotron-free`, `or-qwen38` | Apple Silicon | pass at `f07e205e` |
| Smoke on `or-ling` | Apple Silicon | pass at `f07e205e` after its PROVIDER rerun |
| Smoke on `or-laguna-free` | Apple Silicon | provider, recorded: upstream timeouts on both runs |

The 17 `light` rows were `codex`, `codex-luna-medium`, `codex-astra`,
`grok-46`, `claude-sonnet55-medium`, `claude-haiku55-low`,
`claude-haiku55-medium`, `claude-opus-low`, `claude-opus-medium`,
`claude-fable`, `or-mimo`, `or-glm`, `or-qwen37`, `api-openai`,
`api-openai-medium`, `api-anthropic` and `api-xai`. The DeepSeek long-session
lanes and `or-laguna-free` were informational; their outcomes were recorded
and did not block.
