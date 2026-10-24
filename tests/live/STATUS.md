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

No later revision is qualified.

## Live matrix

The rows are the owner's catalog, the `ROUTES` keys in `live.py`, in ladder
order (spec 05 §3.4). A row whose tuple passed in the 0.0.2 matrix shows that
result; whether it carries to a later revision is the §3.4 re-confirmation
rule. A new row, or a row whose model or effort changed, has no result yet.
Rung 3 rows are sparing and run only for a targeted confirmation.

| Route | Class | Rung | Last result |
| --- | --- | --- | --- |
| `or-nemotron-free` | informational | 1 | smoke pass at `f07e205e` (Apple Silicon) |
| `or-nemotron-vl-free` | informational | 1 | unqualified: new row |
| `or-inkling-free` | informational | 1 | unqualified: new row |
| `or-flint-free` | informational | 1 | unqualified: new row |
| `or-lfm-free` | informational | 1 | unqualified: not planned in 0.0.2 (upstream at about 7 tokens per second with in-band 504s on every 0.0.2 run) |
| `codex` | light | 1 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-haiku55-low` | light | 1 | smoke pass at `f07e205e` (Apple Silicon) |
| `grok-46` | light | 1 | smoke pass at `f07e205e` (Apple Silicon) |
| `api-openai` | light | 1 | smoke pass at `f07e205e` (Apple Silicon, Linux x86_64 musl) |
| `api-anthropic` | light | 1 | unqualified: effort changed to low (0.0.2 ran `claude-haiku-5-5` at medium) |
| `api-xai` | light | 1 | unqualified: model changed to `grok-4.6` (0.0.2 ran `grok-4.7`) |
| `codex-sol` | full | 2 | full cell pass (History) |
| `claude-sonnet55-low` | full | 2 | full cell pass (History) |
| `grok` | full | 2 | full cell pass (History) |
| `deepseek` | full; long-session informational | 2 | unqualified: model changed to `deepseek/deepseek-v4-flash-0731` |
| `deepseek-messages` | full; long-session informational | 2 | unqualified: model changed to `deepseek/deepseek-v4-flash-0731` |
| `or-glm` | light | 2 | smoke pass at `f07e205e` (Apple Silicon); no longer best-effort |
| `codex-luna-medium` | light | 2 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-haiku55-medium` | light | 2 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-sonnet55-medium` | light | 2 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-opus-low` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-opus-medium` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `codex-astra` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |
| `claude-fable` | light, sparing | 3 | smoke pass at `f07e205e` (Apple Silicon) |

## History

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
