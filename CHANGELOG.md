# Changelog

All notable changes to Plak and Plakous are recorded here. Every workspace
package shares one version. `0.0.x` releases carry no API, wire or
persisted-format compatibility promise; the session export document is the
one cross-build carrier (a build imports its own and the previous version).

## 0.0.2

### Plak

- Conversations carry across builds and configuration changes: `/reload` and
  `session import` move the full neutral conversation through export
  document version 8 (imports 8 and 7). `/reload` strips provider replay that
  the destination cannot validate, with a one-line notice instead of failing;
  file `session import` always strips provider replay.
- Background work: shell groups left running after a command are promoted to
  owned background jobs, with `plak run --leave-jobs-running`; repeated quiet
  process polls are held and released on host notices.
- MCP: OAuth for HTTP servers (`auth = "oauth"` with `oauth_issuer`,
  `oauth_client_id`, `oauth_client_secret` and `oauth_callback_port`;
  `plak mcp add --oauth`; `plak mcp login|logout`), with the home-scoped
  `mcp.client_id_metadata_url` for Client ID Metadata Document registration;
  resources (`resources = true`, `plak mcp add --resources`, a `read_resource`
  tool); and prompts (`plak mcp prompts`).
- TUI: delivered child mail renders as compact expandable groups; Menu uses a
  list and detail pane at 140+ columns; Usage offers `[refresh]` for
  credentials that need it; streaming code fences highlight incrementally;
  long inline code and URLs wrap. A scrolled-up transcript stays anchored
  while output arrives, and a send-now steer shows its row immediately.
- `plak doctor` and `plak status` warn when a compiled provider review expires
  within 14 days or has expired, naming the surface and UTC date.
- Startup: store integrity scans moved to `plak doctor`; the binary identity
  comes from an install-time manifest. Resume and fork open faster, and the
  projector no longer copies the transcript on every commit.
- Truncated final answers are reported instead of shown as complete:
  `plak run` prints the answer but exits 5 (`turn_failed:output-limit`), not
  0. Empty output-limit responses retry once.
- Telemetry records retries once per attempt and keeps failed wire exchanges
  individually; refused workspace reclamation backs off and trims proven
  disposable caches.
- jj support is opt-in at build time (`--features jj`); release tarballs and
  default builds are git-only. Copies use a command allowlist, a repository
  discovery ceiling and a frozen base; `plak doctor` reports jj readiness.
  The optional integration uses jj-lib 0.46.
- Release binaries are stripped and built with fat LTO. The release
  tarball's `THIRD-PARTY` lists exactly the packages the release build links
  (no jj-lib, gix or test-support entries from the workspace-wide resolve).
- OpenRouter: an error the provider reports inside an already accepted stream
  reads `OpenRouter returned an in-band error after accepting the request:
  HTTP <status> (<class>) from <upstream>`; `stream validation failed` now
  means a real protocol violation. Chat is the recommended OpenRouter protocol
  for non-Anthropic models.

### Plakous

- Codex uses the GPT-6 base prompt for GPT-6 models.
- Code Mode `exec_command`/`write_stdin` return parsed objects.
- Codex cache affinity and request IDs match its backend expectations;
  Anthropic API-key turns use prompt-cache breakpoints.
- Request uploads time out on idle progress instead of total duration.
- A failed request's attempt summary (`[N requests; last: ...]`) names the
  provider's refusal class (`rate limited`, `unavailable`, `timeout`) on every
  provider, and OpenRouter failures carry the provider's HTTP status, including
  an in-band one, on the model error and the JSONL `turn_lifecycle`.
- OpenRouter derives its input capacity from the catalog.
- Long Responses sessions keep working past 4096 history items or 3 MiB of
  replay: per-response limits no longer apply to accumulated history.
  Provider replay is bound to the account that produced it, so changing
  accounts never sends one account's encrypted reasoning under another.
- Hosted runtime construction goes through one sealed `RuntimeFactory` port.
- Oversized user messages are refused at admission.
- The compaction summary lists each user request in order with its status,
  keeps the active request verbatim after the summary, and states completed
  work as done so a model does not redo it after a mid-task compaction. The
  mid-turn continuation note says the earlier work is summarized above.
- A Codex native compaction mid-turn keeps the current round's tool batch
  verbatim after the compaction item, so the model still sees the output it
  asked for; when that successor would not fit or shrink, the tail-less
  successor from the same compaction item is published instead.
- Re-reading a compaction's receipt (recovery, or `/compact` repeated with the
  same operation) no longer reports the session as corrupt when the successor
  carries a post-compaction note, and a compaction completed during recovery
  keeps its plan restoration note, as the live fold does.
- Python Code Mode (`py-ptc`) runs Monty 1.2.0 under adapter
  `plakous.monty-python-adapter.v7`. A refused `Path.scan` (glob, rglob, walk)
  raises `RuntimeError` instead of returning an empty result; `bisect`, `csv`,
  `hashlib`, `os.path` and `tomllib` are importable.

### Persisted formats

- Export document version 8; imports versions 8 and 7. State key
  `r25.e29.p33.t8`: 0.0.1 state is not opened or migrated. To carry a 0.0.1
  session, use `/reload` from the running 0.0.1 build, or export with that
  binary and import the v7 document with 0.0.2. New exports use v8.
- Runtime format epoch 29: native compaction provenance records how many
  retained segments follow the compaction item.

### Security

- rustls-webpki 0.103.16 and webpki-roots 1.0.10 fix RUSTSEC-2026-0382
  (distrusted intermediates accepted) and RUSTSEC-2026-0381 (distrusted
  roots).

### Pins

- No-change release-candidate review dated 2026-10-22. Review expiries (UTC):
  Codex subscription profile, 2026-11-21T23:59:59Z; OpenRouter Chat dialect,
  2026-11-21T00:00:00Z; xAI and OpenAI API profiles, 2027-01-20T00:00:00Z;
  Grok subscription profile, 2027-01-31T00:00:00Z; Claude subscription and
  Anthropic API profiles, 2027-09-22T00:00:00Z.
- When a newer build renews a review, accept the renewed notice with
  `plak models <account>` (this release asks again for Codex), which keeps the
  login; `plak login` runs a full new login.

### Known limitations

- No OS sandbox and no tool-approval prompts: process tools run with your full
  host authority. Credentials are plaintext `0600` files; there is no OS
  keychain. See [known limits](docs/user-guide/known-limits-and-security-model.md).
- The `claude` route presents the Claude Code client identity. Plak is not an
  official Anthropic client; use it at your own risk.
- Routes with compiled review windows refuse after their expiry until a
  newer build renews them; see the current dates above.
- API-key routes compact locally only: no provider compaction, token counting,
  user-configurable cache settings or hosted programmatic tool calling yet.
- xAI sometimes refuses a follow-up turn on content-policy grounds in long Grok
  sessions; Plak reports it as a provider content-policy refusal.
- Image input on OpenRouter's free `gemma-4-31b-it:free` is unproven.
- One message is capped at 16 MiB encoded, and that encoding stores each image
  byte as a JSON number (about 3.6 bytes for compressed image data). A prompt
  therefore holds about 4.5 MiB of images in total, one maximum-size
  (3.75 MiB) image, although up to 8 images are accepted.
- Starting a new TUI session whose `[root] agent` template names a model that
  is not in its account's catalog clears `[root] agent` from `config.toml`;
  choose the model again.
- Template `[env]` reaches only the `on_mint` hook, not tool calls.
- Dormant jj copies are retained because coverage inspection is unavailable;
  close or discard them explicitly.
- An adopted candidate reports how many files landed, not which.
- Responses prefix drift is not persisted and the producer is unnamed after
  reopen. Telemetry does not yet record wire bytes or per-kind event counts.
- Not yet available: skills, tool search, a browser tool, agents as tools, MCP
  elicitation, and keyboard navigation between transcript items.
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
