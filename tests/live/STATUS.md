# Live status

The current qualification state of the opt-in real-provider runs. Scenario
definitions and oracles are in [`README.md`](README.md); what a qualification
run binds is spec 05 §3.4, and a pass there covers only the tuple it names.
Raw reports stay private under the ignored `target/live/`.

## Qualification

No revision has passed the spec 05 §3.4 pre-release bar. Nothing below
qualifies a tuple for publication.

## Latest install check

A pre-publication revision of this tree, `plak/test-hooks` release builds on
`aarch64-apple-darwin` and `x86_64-unknown-linux-musl`. This is an install
check, not the §3.4 bar.
Routes are the `ROUTES` keys in `live.py`, which fix account, model and effort.

| Matrix | Platform | Outcome |
| --- | --- | --- |
| `crash_jobs` x3 on every `full` row (`claude-sonnet55-low`, `codex`, `deepseek`, `deepseek-messages`, `grok`) | Apple Silicon | 3/3 pass on every row |
| Smoke subset (`tui`, `cli_resume`, `jsonl_resume`, `cancel`; `compaction` on `deepseek`) on four `full` rows | Apple Silicon | pass |
| Long-session `kill_reopen` on `claude-sonnet55-low` and `deepseek` | Apple Silicon | pass |
| `crash_jobs` x3, `jsonl_resume`, `compaction`, `cancel`, `tui`, `kill_reopen` on `claude-sonnet55-low` | Linux x86_64 musl | pass |

No outcome was a product harness failure. The `light` and `informational`
rows, the `jobs` and soak drivers and the full smoke set were not re-run at
that revision.
