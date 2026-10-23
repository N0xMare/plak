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
- the live matrix below;
- a maintainer's review of the `25ea8365` maintainer build in the TUI.

## Live matrix

`plak/test-hooks` release builds for `aarch64-apple-darwin` and
`x86_64-unknown-linux-musl`, composition `plakous-default`. Routes are the
`ROUTES` keys in `live.py`.

At `25ea8365` the re-confirmation smoke on every `full` row passed on its
first attempt on both platforms. Since `f07e205e` the product changed Codex
native compaction successors (`3d6f915d`) and the TUI Usage tab's refresh
action and host-loop reads (`e873c6a8`, `a3106d2f`, `25ea8365`), and the live
drivers and matrix changed; no row's rendered-prompt digests or harness
footprints moved, so every other cell carries its earlier result.

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

The DeepSeek long-session lanes and `or-laguna-free` are informational; their
outcomes are recorded and do not block. `or-lfm-free` is no longer a planned
row (spec 05 §3.4).
