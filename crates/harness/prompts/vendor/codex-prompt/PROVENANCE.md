# Provenance: `codex-prompt` vendor snapshot

This directory is a reviewed, license-preserving copy of part of the base
instruction text the OpenAI Codex CLI sends its models. It exists so that the
Plakous Codex recommended profiles can carry Codex's own wording, byte for
byte, instead of a paraphrase: a frontier model tuned against these
instructions behaves measurably worse under a short substitute.

It is data, not a crate. Nothing here is compiled as a package; the Plakous
prompts crate reads the files with `include_str!` and pins their digests.

## Upstream identity

| Fact | Value |
| --- | --- |
| Repository | `https://github.com/openai/codex` |
| Revision | `b1a547b1f73ce86205d9222ac19cff334b3b7a2e` |
| Package version | `0.153.3` |
| License | Apache-2.0 (`LICENSE`, with upstream `NOTICE`) |

The revision is the same one `vendor/codex-code-mode-protocol` pins, so the
model-visible instruction text and the model-visible `exec`/`wait` tool
descriptions come from one upstream snapshot.

## Files copied

| This package | Upstream path | Lines copied | Bytes | sha256 of the whole upstream file | sha256 of the vendored file |
| --- | --- | --- | --- | --- | --- |
| `prompt/01-identity.md` | `codex-rs/core/templates/model_instructions/gpt-5.2-codex_instructions_template.md` | 1-1 | 133 | `492a212d8a23be8b03c488177d8986f4db4ee54a34b2e8a60779e5e5c89a1b63` | `cbed1057f7d16e4d4865df0bee21f9bee892e65a4ed5a5202d5203b78458928a` |
| `prompt/02-technical-communication.md` | `codex-rs/core/gpt_5_2_prompt.md` | 13-15 | 404 | `415333a461404a790c981fb00a7b9dc73b1906d3f87133dc8afe7ed77e92bd67` | `9cfa98bb59a0cd783883f288f2d4abc7081266e5019ea8ec5ab5af5a6e59d610` |
| `prompt/03-working-with-the-user.md` | `codex-rs/core/templates/model_instructions/gpt-5.2-codex_instructions_template.md` | 5-36 | 3180 | `492a212d8a23be8b03c488177d8986f4db4ee54a34b2e8a60779e5e5c89a1b63` | `2f6714fe9712b63280d0835880662b1272674c84b304b6a705d627a6ba90c311` |
| `prompt/04-autonomy-and-persistence.md` | `codex-rs/core/gpt_5_2_prompt.md` | 29-32 | 795 | `415333a461404a790c981fb00a7b9dc73b1906d3f87133dc8afe7ed77e92bd67` | `5abf7c0ad0c8b34130c4ca8b292a130406bf463f268b9c080d4c1f7c2241fa3e` |
| `prompt/05-general-and-editing-constraints.md` | `codex-rs/core/templates/model_instructions/gpt-5.2-codex_instructions_template.md` | 38-55 | 2148 | `492a212d8a23be8b03c488177d8986f4db4ee54a34b2e8a60779e5e5c89a1b63` | `ab6934a2bbfae3990ef72aa7b888d4f387e3d760aa9f454d2f583ea8a31ce4c5` |

Each file is one contiguous upstream line range, copied with no edit inside
it: the copy keeps upstream's headings, its curly quotes, its non-breaking
hyphens and its trailing blank lines. The file names are this package's, and
the numeric prefix is the order the sections are joined in. 6660 bytes in
total, about 1665 tokens at the renderer's bytes/4 approximation.

The vendored digest is `sha256sum` of the file as it sits here. It is what a
re-vendor is checked against when the upstream checkout is not at hand: a
whole-file upstream digest cannot be reproduced from an excerpt, the vendored
digest can.

## Sections dropped, and why

| Upstream section | Where | Why it is not here |
| --- | --- | --- |
| `{{ personality }}` expansion (`templates/personalities/*.md`) | template line 3 | Persona prose — values, tone, escalation. Plakous ships no personality selector; the one paragraph that is about technical communication is copied instead, from `gpt_5_2_prompt.md`. |
| `## AGENTS.md spec` | `gpt_5_2_prompt.md` 17-27 | Plakous discovers and enrols instruction files itself (`prompts::instructions`) and states the rule in its own working agreements. |
| `## Responsiveness`, `## Planning`, `### Examples`, `## Task execution`, `## Validating your work`, `## Ambition vs. precision` | `gpt_5_2_prompt.md` 34-159 | Preamble-message and plan-narration mechanics tied to the Codex CLI's own channels and its `update_plan` cadence. Roughly 9 KB, and the footprint budget does not have room for them. |
| `## Plan tool` | template 57-63 | Plakous's plan tool owns its own description (spec 02 §6.3); a second, differently worded cadence would contradict it. |
| `## Special user requests` | template 64-68 | Names Codex CLI review affordances Plakous does not present. |
| `## Frontend tasks` | template 69-80 | Visual-design guidance with no bearing on the measured failure, and 1.1 KB of budget. |
| `# Tool Guidelines`, `## Shell commands`, `## apply_patch`, `## update_plan` | `gpt_5_2_prompt.md` 244-end | Tool descriptions. In Plakous the pack that owns a capability owns the text that explains it, and the Code Mode `exec`/`wait` descriptions already come from the same upstream revision. |

There is no `Using skills` section, no channel (`commentary` / `final`)
mechanics and no `Visualizations` section at this revision; nothing had to be
excised from inside a copied range to leave them out.

## Where the copy is not the truth

`05-general-and-editing-constraints.md` and `04-autonomy-and-persistence.md`
were written for a harness with a sandbox and an approval flow. Plakous has
neither, on any platform. The copied text is
not edited to say so. Instead the Plakous environment block, assembled after
the base in `prompts::plan::PromptPlan`, states the truth in Plakous's own
words (`identity::PLAK_PRODUCT_EXECUTION_POLICY`).

## Updating

Re-copy the same ranges from the new revision, recompute both digest columns
with `sha256sum`, update the table above, re-pin
`bases::codex_vendor::CODEX_VENDOR_PROMPT_PINS`, and regenerate the rendered
prompt goldens with
`PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_`. Any
movement in those pins is a change to model-visible wording and needs its own
review.

## GPT-6 family snapshot

The GPT-6 parity base is separately pinned to tag `rust-v0.163.0`
(annotated tag object `129b8ec7ee6f4511557a4856279f529025c25af4`), peeled
commit `83086188ae0614bd882818ff1aa23f3194962012` of `openai/codex`.
Source: `codex-rs/models-manager/models.json`, entry `slug == "gpt-6.1-sol"`,
`model_messages.instructions_template`. Apache-2.0; the LICENSE and NOTICE
in this directory apply. This does not move the Code Mode protocol pin.

Verified directly against the commit URL and the upstream tag reference.
The decoded UTF-8 template is **21,779 bytes**, not 21,769: the latter count
in the review proposal was inaccurate. The source snapshot preserves every
byte, including trailing spaces and the final newline.

| File | Bytes | sha256 |
| --- | ---: | --- |
| `gpt6/direct.md` | 11218 | `c59094ea776a953a05b0cde13880fa72bde1264ec970ed371039b8fb464e0c42` |
| `gpt6/instructions-template.md` | 21779 | `e1bdd4f8f0df4b20f4a0ffc8a861ce819df45325d8cecdfb92e80379cf8d142e` |
| `gpt6/native-ptc.md` | 11863 | `bbdcae63bfcdb39b9a1a20d2aa4d2259c976124b20c325191a79321f8ff7e120` |

`native-ptc.md` is the approved Code Mode base; `direct.md` is its Direct
delta. Both trim upstream line-end spaces. The full source snapshot is not
sent to the model. Line numbers below refer to that snapshot.

### Recorded CHANGED edits

- L7: replace "skills" with "project instruction files".
- L11: retain the no-messaging rule; remove skill/plugin authorization clauses.
- L13: retain the confirmation explanation, with "AGENTS.md rule" as its only
  example; remove automatic approval-review reporting.
- L19: remove "creating draft PRs" from the reversible-work examples.
- L23: ask in the final message after independent work, with stated assumptions.
- L25: remove "and skill" from local-file exceptions.
- L67: replace the async-question-tool paragraph with two sentences explaining
  that questions end the turn after useful independent work.
- L81: remove the claim that commentary is collapsed after the final answer.
- L89-L100: describe terminal Markdown; replace clickable Markdown file-link
  instructions with standalone backticked paths and optional 1-based lines;
  omit L97, retaining the URI, range and grouping rules.
- L117-L118 (Direct only): replace both Code Mode parallelism bullets with
  independent tool calls in one response, keeping dependent work sequential.
- L123: remove `$CODEX_HOME` from the reserved-variable examples.

### Omitted and authored text

Omit L37, L41-L45, L49, L55-L59, L83, L97, L104-L112, L121, L126,
and L130-L172 (skills, Apps, Plugins). Retain L47's heading. Direct additionally
omits L124's JavaScript shell-escaping bullet. Other upstream wording is kept.

Append two authored editing/worktree bullets: use apply_patch except for generated
or multi-file replacements, preserve others' edits, prefer non-interactive git.
The authored Tools suffix names exec/wait and the compiled nested catalog on
Code Mode; on Direct it retains the first and last sentences of the existing
Direct tool paragraph (the apply_patch rule is already in the editing bullet).
The assembler omits the editing bullet when apply_patch is absent.

The Codex-family working agreements omit the no-narration and generic
parallelism sentences; the bases own that guidance. The Codex wake paragraph
omits "Never sleep-poll"; the wait paragraph is unchanged. Identity, topology,
assignment and enrolled project instructions remain separately owned.

Re-vendor by decoding that exact JSON member, checking the source digest,
reviewing the recorded edits, then updating the source/base pins in
`src/bases/codex.rs` and the rendered prompt pins. GPT-5.6 and other models
retain the earlier base, and the legacy variant retains its control base.
