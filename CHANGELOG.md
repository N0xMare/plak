# Changelog

All notable changes to Plak and Plakous are recorded here. Every workspace
package shares one version. `0.0.x` releases carry no API, wire or
persisted-format compatibility promise; the session export document is the
one cross-build carrier (a build imports its own and the previous version).

## 0.0.1

First public release.

### Plak

- `plak`, an interactive TUI; `plak run` for one non-interactive task;
  `plak agent`, a long-lived JSONL host for programmatic control.
- Seven accounts: Codex, Grok and Claude subscription OAuth, and the
  separately billed OpenAI, xAI, Anthropic and OpenRouter API keys.
- Durable SQLite sessions that recover after a crash; resume, fork,
  compaction, and session export/import between builds.
- `/reload` resumes a running TUI session on a newly installed build.
- Agent templates and a default role set (root, worker, reviewer,
  researcher); spawned children work in git or jj copies whose candidates the
  parent reviews and adopts.
- Image input from the TUI (drop, paste, `Ctrl+V`, `/image`) and
  `plak run --image`.
- Optional tools: provider-hosted `web_search`, `web_fetch` and `x_search`,
  Plak-run `fetch_url`, `image_gen`, and MCP servers.
- Tool compositions: direct tools, JavaScript (`js-ptc`) and Python (`py-ptc`)
  Code Mode in worker processes, and native Codex Code Mode.

### Plakous

- The embeddable single-agent library behind Plak: the `plakous` facade with
  Responses, Messages and Chat Completions protocol kernels, provider
  integrations, harness profiles and SQLite session stores, all behind opt-in
  features. See [embedding](docs/user-guide/embedding.md).

### Install

- Release tarballs for `aarch64-apple-darwin` and `x86_64-unknown-linux-musl`
  with `SHA256SUMS` and build attestations, installed by `install.sh`.
- `cargo install --locked plak plakous-js-runtime plakous-py-worker`, or a build
  from source. Other platforms may compile but are not supported.

### Known limitations

- No OS sandbox and no tool-approval prompts: process tools run with your full
  host authority. Credentials are plaintext `0600` files; there is no OS
  keychain. See [known limits](docs/user-guide/known-limits-and-security-model.md).
- The `claude` route presents the Claude Code client identity. Plak is not an
  official Anthropic client; use it at your own risk.
- The Codex subscription profile and the OpenRouter Chat dialect expire
  2026-11-13 (UTC); after that a build refuses those routes until a newer
  build renews them.
- API-key routes compact locally only: no provider compaction, token counting,
  cache settings or hosted programmatic tool calling yet.
- xAI sometimes refuses a follow-up turn on content-policy grounds in long Grok
  sessions; Plak reports it as a provider content-policy refusal.
- Image input on OpenRouter's free `gemma-4-31b-it:free` is unproven.
- Starting a new TUI session whose `[root] agent` template names a model that
  is not in its account's catalog clears `[root] agent` from `config.toml`;
  choose the model again.
- Template `[env]` reaches only the `on_mint` hook, not tool calls. Dormant jj
  copies are never reclaimed; remove them by hand.
- An adopted candidate reports how many files landed, not which.
- Responses prefix drift is not persisted and the producer is unnamed after
  reopen. Telemetry does not yet record wire bytes or per-kind event counts.
- Not yet available: skills, tool search, a browser tool, agents as tools, MCP
  prompts, resources, elicitation and OAuth, and keyboard navigation between
  transcript items.
- There is no uninstall command; remove `~/.local/lib/plak` and its links in
  `~/.local/bin` by hand.
- The build audit waives three zip extraction advisories
  (RUSTSEC-2026-0367, -0368, -0369): the affected zip 8.6.0, pulled in by the
  Monty type checker, only reads its embedded typeshed archive in memory and
  never extracts files. `cargo install` of `plakous-py-worker` cannot apply the
  repository's `monty-typeshed` patch, so it builds the upstream crate, which
  depends on the advisory-affected zip 0.6.6 and whose type-checker archive is
  not byte-reproducible; release tarballs and source builds use the reviewed
  snapshot.
