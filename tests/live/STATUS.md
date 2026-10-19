# Live status

The current qualification state of the opt-in real-provider runs. Scenario
definitions and oracles are in [`README.md`](README.md); what a qualification
run binds is spec 05 §3.4, and a pass there covers only the tuple it names.
Raw reports stay private under the ignored `target/live/`.

## Qualification

No revision has passed the spec 05 §3.4 pre-release bar. Nothing below
qualifies a tuple for publication.

## Latest matrix

The §3.4 matrix on `plak/test-hooks` release builds for `aarch64-apple-darwin`
and `x86_64-unknown-linux-musl`, composition `plakous-default`, at revisions
`1a2938ce` and `e6d13d15`. The second changes only the live drivers' outcome
classification (spec 05 §3.2), so its `plak`, `plakous-js-host` and `monty`
builds are byte-identical to the first's and every row carries its earlier
result. Routes are the `ROUTES` keys in `live.py`.

| Cells | Platform | Outcome |
| --- | --- | --- |
| Long-session `--all`, jobs and `crash_jobs` x3 on every `full` row | Apple Silicon | pass; `codex` long-session after its one MODEL rerun |
| Headless soak, one hour, `deepseek` | Apple Silicon | pass, 54/54 tasks across the intentional rebuild |
| Smoke on `claude-sonnet55-low`, `codex`, `grok`, `deepseek-messages` | Apple Silicon, Linux x86_64 musl | pass |
| Smoke on `deepseek` | Apple Silicon, Linux x86_64 musl | blocked: OpenRouter refused twice with HTTP 429 (`temporarily rate-limited upstream`); on Apple Silicon the rerun's startup discovery then failed to connect (`EHOSTUNREACH`, nothing sent), which scores harness |
| Smoke on the 17 `light` rows | Apple Silicon | pass |
| Smoke on `api-openai`, `api-openai-medium`, `api-anthropic`, `api-xai` | Linux x86_64 musl | pass |
| Smoke on `or-nemotron-free`, `or-laguna-free`, `or-qwen38` | Apple Silicon | pass |
| Smoke on `or-lfm-free` | Apple Silicon | model, recorded: the root spawned fewer children than asked |
| Smoke on `or-ling` | Apple Silicon | provider, recorded: upstream HTTP 429 |

The `deepseek` smoke cell blocks these revisions. A later revision is
re-confirmed by a new smoke on each `full` row on both platforms.
