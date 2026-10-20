# Live status

The current qualification state of the opt-in real-provider runs. Scenario
definitions and oracles are in [`README.md`](README.md); what a qualification
run binds is spec 05 §3.4, and a pass there covers only the tuple it names.
Raw reports stay private under the ignored `target/live/`.

## Qualification

`79a385f8` (tag `v0.0.1`) passed the spec 05 §3.4 pre-release bar:

- the integrated gate on both supported platforms: `just full` on Apple
  Silicon and on Linux x86_64, the native musl tier and the pinned musl
  container gate, and the strict crash-point sweep over all fourteen shapes
  with no unclassified or divergent cut;
- the live matrix below;
- a maintainer's review of the `79a385f8` maintainer build in the TUI.

## Live matrix

`plak/test-hooks` release builds for `aarch64-apple-darwin` and
`x86_64-unknown-linux-musl`, composition `plakous-default`. Routes are the
`ROUTES` keys in `live.py`.

At `79a385f8`, the re-confirmation smoke on every `full` row passed on its
first attempt on both platforms. Every other cell carries its result from
`1a2938ce`: no row's rendered-prompt digests or harness footprints changed
between the two revisions.

| Cells | Platform | Outcome |
| --- | --- | --- |
| Smoke on `claude-sonnet55-low`, `codex`, `grok`, `deepseek`, `deepseek-messages` | Apple Silicon, Linux x86_64 musl | pass at `79a385f8` |
| Long-session `--all`, jobs and `crash_jobs` x3 on every `full` row | Apple Silicon | pass; `codex` long-session after its one MODEL rerun |
| Headless soak, one hour, `deepseek` | Apple Silicon | pass, 54/54 tasks across the intentional rebuild |
| Smoke on the 17 `light` rows | Apple Silicon | pass |
| Smoke on `api-openai`, `api-openai-medium`, `api-anthropic`, `api-xai` | Linux x86_64 musl | pass |
| Smoke on `or-nemotron-free`, `or-laguna-free`, `or-qwen38` | Apple Silicon | pass |
| Smoke on `or-lfm-free` | Apple Silicon | model, recorded: the root spawned fewer children than asked |
| Smoke on `or-ling` | Apple Silicon | provider, recorded: upstream HTTP 429 |

`or-lfm-free` and `or-ling` are informational rows; their outcomes are
recorded and do not block. The feature scenarios (`features.py`: image input
on every image-capable route, hosted retrieval, `fetch_url`, image
generation, MCP over stdio and HTTP, roles and fan-out) and the startup lookup
fault proofs (`lookup_retry.py`) also passed on this product code; they are
not cells of the bar.
