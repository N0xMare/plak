# Prompt renders

Prompt renders show what a model actually sees on each product route: the
harness costume, identity, enrolled `AGENTS.md`, every tool name and
description, and the first-turn HTTP request body.

They are **local review artifacts**, not committed goldens and not runtime
inputs. Correctness lives in code: `crates/hosting/hosted/tests/common/rendered_prompts.rs`
and `rendered_lifecycle.rs` pin a sha256 per route/surface, and the tests
assert the render's structure in memory without reading generated render
artifacts; instruction enrollment reads a temporary workspace fixture. A fresh
clone passes with no artifacts present.

## Render

```text
just prompts
```

Equivalent: `PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_`.

Output goes to `target/prompt-renders/` (git-ignored). Override the root with
`PLAK_PROMPT_ARTIFACT_DIR=/some/dir`.

Without `PLAK_UPDATE_GOLDENS`, the same `cargo test` invocation still
performs the full render and checks it against the in-code pins — it just
writes nothing.

## What the artifact contains

| Path | Contents |
| --- | --- |
| `target/prompt-renders/rendered/<route>.md` | One-file view of a first-turn agent: harness footprint (UTF-8 bytes and bytes÷4 tokens for costume, family/product/PTC instructions and compact `tools[]`, excluding enrolled `AGENTS.md` and the scenario user/assignment message), scenario summary, headers table (authorization `[redacted]`), and the pretty-printed request body in a `jsonc` fence |
| `target/prompt-renders/rendered/wire/<route>.request.json` | Strict, parseable first-turn body as the protocol kernel serializes it (Responses / Chat Completions / Messages) |
| `target/prompt-renders/rendered/wire/<route>.headers.json` | Dialect-pinned headers and approximate body size |
| `target/prompt-renders/rendered/lifecycle/*.md` | Post-first-turn surfaces: `tool-results`, `compaction`, `child-mail`, `mid-session` |
| the gitignored local capture directory | Gitignored vendor CLI captures (`claude` / `codex` / `grok` / `opencode`) from `just native-compare-prompts`; witnesses for comparison, not Plak runtime |

86 routes: 76 subscription and OpenRouter routes (providers × compositions ×
topology, minus `native-ptc` where no native PTC is enrolled) and 10 direct API routes, rendered as
the single-agent root only. The API routes go through the production API
profile and each route's Platform wire: the OpenAI API (`openai-api.*`) in
`direct`, `native-ptc`, `js-ptc` and `py-ptc`, and the xAI (`xai-api.*`) and
Anthropic (`anthropic-api.*`) APIs in `direct`, `js-ptc` and `py-ptc`.
`native-ptc` is the vertical's own PTC system; today only Codex Code Mode is
enrolled, on Codex and on the OpenAI API, whose models Plak enrolls as
`CodeModeOnly`. The Markdown `## Request`
block and the matching `wire/*.request.json` describe the same kernel-lowered
body; prefer the wire JSON when you need strict JSON.

## Diff a change

```text
just prompts-diff
```

That copies the current render to `target/prompt-renders.prev`, re-renders, and
prints `diff -ru` between them. Run it once before your prompt edit and once
after to see exactly which model-visible sentences moved.

## Update the pins

A prompt change makes the test fail with the list of routes whose digest
moved. That failure is the point: model-visible text changes deliberately.
After reviewing the diff, re-pin:

```text
just prompts-pins
```

That prints the new `PROMPT_RENDER_PINS` and `LIFECYCLE_RENDER_PINS` rows
(`PLAK_PRINT_PROMPT_PINS=1`); paste them over the tables in
`crates/hosting/hosted/tests/common/rendered_prompts.rs` and
`crates/hosting/hosted/tests/common/rendered_lifecycle.rs`, then re-run the tests.

## Where to change what the model sees

Never edit a rendered artifact — `just prompts` overwrites it from Rust.

| What the model sees | Where to change it |
| --- | --- |
| Family Direct base | `crates/harness/prompts/src/bases/{claude,grok,codex}.rs` |
| Pi base (OpenRouter Direct) | `crates/harness/prompts/src/bases/pi.rs` |
| JS/Python PTC harness | `crates/harness/prompts/src/ptc.rs` |
| Workspace / `You are {model}` / VCS / working agreements | `crates/harness/prompts/src/identity.rs` |
| Spawn, mail, wait, submit, merge, discard | `crates/orchestration/recursive-subagents/src/tree/text/mod.rs`, with the argument descriptions in `crates/orchestration/recursive-subagents/src/tools/registrations.rs` |
| Child Start / reminder / force | `crates/harness/prompts/src/child.rs` |
| Coding tool descriptions | each pack's `crates/harness/tool-{files,search,process,plan}/src/text.rs` and `crates/harness/compose/src/text.rs`; Claude Bash in `crates/harness/tool-process/src/projection.rs`; pi `read`/`edit`/`write` in `crates/harness/tool-files/src/lib.rs` and pi `bash`/`wait`/`stop` in `crates/harness/tool-process/src/text.rs` (schemas in `pi.rs`) |

## What the tests assert without files

- The route matrix is exactly 86 included routes plus two documented
  exclusions, and every API route is a composition the API table admits.
- Rendered wire JSON equals the kernel-lowered request, and the Markdown
  `## Request` fence equals the jsonc formatter's output.
- The harness footprint excludes enrolled `AGENTS.md` and the scenario
  assignment, subtracts enrolled source bytes, and keeps the Claude costume
  identity on Claude routes.
- Every product tool extension's **name and description** appear verbatim to
  the model, either as a lowered `tools[]` entry or in the harness instruction
  text (the code-mode presentations describe the same tools in prose).
- Each route's rendered Markdown, and each lifecycle surface, matches its
  pinned sha256.

## Native CLI captures

```text
just native-compare-prompts
```

Captures all four installed clients into the gitignored local capture directory. Pass vendor
names to select a subset, or `--output` to choose another local artifact directory:

```text
just native-compare-prompts claude
just native-compare-prompts codex grok
```

Each vendor directory contains `hello.md`, the strict `hello.request.json`,
redacted `hello.headers.json`, and `pin.json` identifying the installed binary
version and capture conditions. Plak's matching render remains under
`target/prompt-renders/rendered/`; run `just prompts` to regenerate it.
These are **private local request artifacts**, not shareable fixtures: even
with secret and identifier redaction, they contain full prompt and tool content,
local paths and host metadata. Never commit or attach them; manually review
only safe structural facts extracted from them before publication.

The clients use isolated configuration, synthetic credentials and local mock
responses. Your existing login is not used. Claude runs through the interactive
CLI with a synthetic OAuth token; `claude-1` is the account reference, not a
claim that the real account was exercised. The print/SDK entrypoint is not
substituted for the interactive request. An explicitly supplied `--from` file
must actually contain a CLI request body; ordinary debug logs may not contain
one.

These artifacts let you inspect request shapes and prompts. They do not prove
live authentication, successful provider execution or retry behavior, and
native capture is not part of `just check`. Generated files are never committed.
