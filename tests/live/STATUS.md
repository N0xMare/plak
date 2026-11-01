# Live status

The current qualification state of the opt-in real-provider runs. Scenario
definitions and oracles are in [`README.md`](README.md); what a qualification
run binds is spec 05 §3.4, and a pass there covers only the tuple it names.
Raw reports stay private under the ignored `target/live/`.

## Qualification

`25ea8365` (tag `v0.0.2`) passed the spec 05 §3.4 pre-release bar:

- the integrated gate on both supported platforms: `just full` on Apple
  Silicon and on Linux x86_64, the native musl tier and the pinned musl
  container gate, and the strict crash-point sweep over all seventeen shapes
  with no unclassified, divergent or refused cut;
- the 0.0.2 live matrix (History, below);
- a maintainer's review of the `25ea8365` maintainer build in the TUI.

`efe23aba`, the 0.0.3 release candidate, has passed part one: the integrated
gate on both supported platforms (`just full` on Apple Silicon and on Linux
x86_64, the native musl tier and the pinned musl container gate), the strict
crash-point sweep over all twenty-seven shapes with no unclassified, divergent
or refused cut, and the publish dry run. Its live matrix (History, below) has
run every non-sparing row. The maintainer's review of its build in the TUI is
pending, so it is not yet qualified.

## Live matrix

The rows are the owner's catalog, the `ROUTES` keys in `live.py`, in ladder
order (spec 05 §3.4). A row shows its latest result, from the 0.0.3 candidate
matrix where it ran there and from the 0.0.2 matrix otherwise; whether it
carries to a later revision is the §3.4 re-confirmation rule. A new row, or a
row whose model or effort changed, has no result yet.
Rung 3 rows are sparing and run only for a targeted confirmation.

| Route | Class | Rung | Last result |
| --- | --- | --- | --- |
| `or-nemotron-free` | informational | 1 | smoke pass at `bce238af` (Apple Silicon) |
| `or-nemotron-vl-free` | informational | 1 | smoke pass at `bce238af` (Apple Silicon) |
| `or-inkling-free` | informational | 1 | provider, recorded at `0df98a47` (Apple Silicon): OpenRouter refuses the model outside agentic harnesses |
| `or-flint-free` | informational | 1 | provider, recorded at `0df98a47` (Apple Silicon): no upstreams available on both runs |
| `or-lfm-free` | informational | 1 | unqualified: not planned in 0.0.2 (upstream at about 7 tokens per second with in-band 504s on every 0.0.2 run) |
| `codex` | light | 1 | smoke pass at `bce238af` (Apple Silicon) |
| `claude-haiku55-low` | light | 1 | smoke pass at `bce238af` (Apple Silicon) |
| `grok-46` | light | 1 | smoke pass at `bce238af` (Apple Silicon) |
| `api-openai` | light | 1 | smoke pass at `bce238af` (Apple Silicon, Linux x86_64 musl) |
| `api-anthropic` | light | 1 | smoke pass at `bce238af` (Apple Silicon, Linux x86_64 musl) |
| `api-xai` | light | 1 | smoke pass at `bce238af` (Apple Silicon, Linux x86_64 musl) |
| `codex-sol` | full | 2 | full cell pass at `bce238af` (Apple Silicon); Linux x86_64 musl smoke pass |
| `claude-sonnet55-low` | full | 2 | full cell pass at `bce238af` (Apple Silicon); Linux x86_64 musl smoke pass |
| `grok` | full | 2 | full cell and one-hour headless soak pass at `bce238af` (Apple Silicon); Linux x86_64 musl smoke pass |
| `deepseek` | full; long-session informational | 2 | full cell pass at `e180b55b` (Apple Silicon; the informational long-session lane passed after its MODEL rerun); Linux x86_64 musl smoke pass |
| `deepseek-messages` | full; long-session informational | 2 | smoke and `crash_jobs` x3 pass at `e180b55b` (Apple Silicon); jobs model, recorded (`wake_while_idle`: the model kept the root busy while the job ran); informational long-session pass at `b2755c04` after its MODEL rerun; Linux x86_64 musl smoke pass |
| `or-glm` | light | 2 | smoke pass at `bce238af` (Apple Silicon) |
| `codex-luna-medium` | light | 2 | smoke pass at `bce238af` (Apple Silicon) |
| `claude-haiku55-medium` | light | 2 | smoke pass at `bce238af` (Apple Silicon) |
| `claude-sonnet55-medium` | light | 2 | smoke pass at `bce238af` (Apple Silicon) |
| `claude-opus-low` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-opus-medium` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `codex-astra` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-fable` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |

## History

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
