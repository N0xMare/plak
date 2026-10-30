# Changelog

All notable changes to Plak and Plakous are recorded here. Every workspace
package shares one version. `0.0.x` releases carry no API, wire or
persisted-format compatibility promise; the session export document is the
one cross-build carrier (a build imports its own and the previous version).

## Unreleased

### Plak

- Agent trees use schema 9 and state key `r26.e33.p34.t9`. Carry sessions by
  `/reload` or export/import. Forks use short
  `plak/<tree8>/<agent_id>` branches and `<tree8>-<agent_id>` copy directories,
  with a full-digest prefix collision guard. Base, candidate and landing
  provenance pins retain work independently of checkout removal. Submitted
  output is stored in its intent, not duplicated in attempt rows or submit receipts.
  Schema 9 also records textual Git conflict candidates, their target and
  resolver lineage, and `adopted_via`. A conflict leaves the checkout untouched;
  resolve it in a fork over the named conflict candidate or explicitly land
  markers with `adopt_candidate {accept_conflicts:true}`. Non-textual conflicts
  and JJ retain plain conflict refusals.
  Resolver copies persist their source, target and marker paths independently
  of conflict records. Live inputs protect those records and pins; late
  resolutions are superseded at seal time and spent conflict inputs refuse
  another resolver. Inconsistent resolver lineage fails closed rather than
  becoming work; ordinary stacked copies and accepted resolver follow-ups
  keep sealing after their input records are collected. Resolver copies retain
  their mint-time conflict base independently of those records.
  The same schema records reversal provenance: `adopt_candidate {revert:true}`
  undoes one adopted delta while preserving later edits; conflicting reversals
  change nothing. Reverted Git candidates remain re-adoptable after copy removal.
  Follow-ups cleanly merge the parent's current state into live-fork copies;
  `refresh:false` opts out, and candidate-minted copies keep their pinned bases.
  Conflicts leave the old copy unchanged and report paths to both agents.

- Session export v10 retains Messages native citation replay, including protected
  citation tokens. Versions 10 and 9 import; version 8 is refused before session
  creation. Runtime epoch, product/tree schemas and state key are unchanged.

- Fast children retain their start and finished transcript milestones even
  when they complete before the host's first tree observation. Durable notice
  identities prevent duplicates after reattachment or transcript truncation.
  Reversals use an ordinary tree notice rather than a second adopted milestone;
  conflicts deduplicate by topology operation, even without a display call id.

- All seven native/API routes are independently selectable. Session
  show/log/export still reads an omitted route; attach/resume refuses before
  writes. Full builds qualify headless, API and minimal variants; releases
  continue to ship the default artifact only.
- Source builds can omit the TUI, clipboard paste, image decoding, Python
  composition, or MCP with Cargo features. Defaults retain the shipped shape.
  Omitted capabilities refuse explicitly (exit 2), never select another route;
  doctor, non-default version labels and JSONL hello report build capabilities.

- Harbor supports declared `py-ptc` versus `py-sdk` A/B arms, with verified
  observed selection and the Python worker in the frozen upload cohort.
  Adapter support is covered without inference; no paid trial is a gate.

- `py-sdk` is selectable for a single-agent root through settings, CLI and
  JSONL. Its typed Python base replaces the family base; existing composition
  text stays unchanged. This root opens no topology; child templates and
  optional tools refuse this root-only surface before inference.
- Session exports carry the selected composition and committed named state.
  Product schema 34 admits `py-sdk`; state key `r26.e33.p34.t9`.
  `[root] composition` overrides the root template's composition without
  granting that selection to children.

- Settings persist composition in `[root] composition` rather than changing
  the root template, so children inherit the template's own composition.
  Changing the root template or applying a profile clears that override.

- Contract drift is reported, not mistaken for a credential problem: the model
  picker keeps a drifted row visible but disabled with its reason, `plak run`
  and `plak models` show the reason, and a drifted model is refused before
  effort checks with an install-newer-build remedy rather than a re-login hint.
  Turn-time contract rejections render distinctly from refused credentials.
- Under account load the Codex subscription backend can answer HTTP 401
  "authentication failed (status=503): Too many concurrent requests". That
  exact answer is retried as a temporary overload under the normal retry
  budget, honoring Retry-After, with no credential refresh and no re-login
  advice; an exhausted budget ends the turn as temporarily unavailable. Every
  other 401, and every other route, is still a refused credential.

### Plakous

- `plakous-responses-dialect` is folded into `plakous-responses` and retired;
  its last published version stays 0.0.2 (not yanked). The workspace has 37
  packages. Host, error and usage hooks become concrete family code and the
  internal projection and parsing surfaces narrow; route policy, digest
  domains and semantic revision pins are unchanged.
- `ResponsesProjector::start_tool_block`, `ResponsesProjector::terminal_response`
  and `MessagesRequestPolicy::new` take typed parameter structs instead of long
  argument lists; validation order and request fields are unchanged.
- Native subscription auth shares one set of primitives and loopback fixtures.
  Codex catalog contract and minimum-client drift, login-time OIDC endpoint
  drift, a Claude grant without the required inference scope, and the absence
  of an RS256 signing key at login or refresh are refused with an
  install-newer-build remedy. Grok grants are not gated on scopes; extra
  Claude scopes and mixed signing algorithms are accepted. Only the proven
  Grok 426 client-version and documented Claude 400 sent-beta rejection shapes
  are classified as permanent build-contract failures, never as a refused
  credential; served pins are never adopted and removed betas are never retried.
  `plakous::xai::SUPERGROK_REFERENCE_CLIENT_VERSION` is public.

- Messages accepts initial and streamed text citations, preserving source
  associations and exact native replay without duplicating answer text.
  Unfamiliar informational blocks produce a counted, content-free notice;
  complete unfamiliar signed blocks retain opaque replay. Unknown tool lineage
  and signed deltas that cannot be reconstructed still fail closed.

- Messages artifact decoding preserves raw Thinking and RedactedThinking blocks
  instead of refusing their signed or redacted replay payloads.

- Offline folds require at least a 5 % fresh-input reduction. A summary that
  overruns its output bound selects the Tight instruction for the next permitted
  fold; response-free failures do not reset it. Idle `/compact` and scheduled
  passes take the free placeholder rung after a definite paid failure, and a
  free commit after that failure cannot start a paid scheduled follow-on.

- Atomic attachment now exposes only the cursor, source/turn lifecycles and
  pending decisions. Presentation counters, usage, conversation summaries and
  `ToolActivityView` are removed from the snapshot API. `AgentViewReducer`
  advances only attach state; hosted `TurnProjection` owns live presentation.
  Committed history still uses the facade's atomic transcript attachment.
  `TurnEvents::recv_delivery` reports turn-filtered lost ranges, and
  `SubscriptionEnd` and `SubscriptionLimits` are nameable through the facade.

- Native credential failures before send park the turn for login rather than
  failing it. Same-account readiness resumes the captured request once;
  cancellation, shutdown and account drift still settle through the ordinary
  terminal paths. Waiting releases shared request capacity and does not spend
  the operation deadline. The HUD and child activity show the login wait. This
  is in-memory state only; persisted and export encodings are unchanged.
- Native subscription builds select `codex`, `grok` and `claude` independently,
  without pulling in other providers or evaluators. Frozen subscription
  vocabulary remains readable; omitted operations refuse before state writes.
- JS and Python evaluator choices forward independently from the product;
  Python profiles require explicit `composition-python`. MCP client omission
  unlinks its transports while preserving frozen MCP records for read/export.
- `auth` no longer enables provider crates. `web-fetch` independently selects
  the local page fetcher; provider, subscription and MCP features do not enable
  it. Product defaults retain local fetch, and omitted fetch selections refuse
  explicitly while frozen bounds remain readable.
- Feature validation uses bounded 2n+3 closure witnesses for every package
  instead of enumerating the library feature power set.
- JWT identity verification shares ring with TLS instead of linking aws-lc.
  Only RS256 and ES256 verification is enabled; signing refuses. Embedders
  needing another JWT provider must explicitly install it before the first
  Plakous identity verification; later installation and crate-feature defaults
  cannot replace the lazily installed process provider.
- All local cell profiles use `LocalCompositionProfile::cell(&CellDescriptor)`.
  The four per-engine profile constructors and Python-only component identity
  accessors are removed. `CellDescriptor` is available at the compose crate
  root and through `plakous::tools`.
- Tools can return ephemeral typed execution values alongside text when an
  in-process consumer requests them. The demand flag defaults off; model
  results, recorded evidence, and existing prompt pins are unchanged.
- `plakous-code-mode` is folded into `plakous-compose`'s `cell::codex_exec`
  implementation and removed from the published cohort. Compose exposes the inert `HostApi`, `python_bootstrap`, its
  interpreter-neutral `PythonBootstrap` data, and `codex_exec` at its crate
  root. Presentation building blocks and fixture constructors are private.

- Local composition has session-owned named state. The driver stages bounded
  canonical JSON writes per cell and publishes them only with a successful
  terminal result. Exact reopen, compaction, fork and rebind preserve saved
  values independently of interpreter lifetime. The harness lists keys and
  sizes, not values, before the next prompt. This is the persistence seam;
  Python SDK selection and export carriage are separate changes.

- The harness compaction instruction asks for fixed sections (Requests,
  Constraints, Values, Work done, Decisions, Open errors, Plan, Next step),
  each written once. Standing user constraints from any request stay quoted
  exactly. Only the three newest requests not in progress are quoted, up to
  about 150 words each, keeping every requirement in the user's own words for
  longer requests; earlier ones get one line, finished ones merge into at most
  10 lines, and a finished request keeps the values it produced, moving them
  to Values rather than dropping them to meet the line cap. An earlier summary
  is merged rather than carried forward whole, and the summary aims under
  about 2,000 words to curb growth across folds. The live matrix has not been
  rerun on it.
- `plakous-js-eval` is renamed `plakous-cell` and replaces its JSONL sidecar
  protocol with the cell protocol: length-prefixed JSON frames, a worker
  `Hello` checked before the one `Configure`, and per-cell nested-result
  delivery verdicts. `SidecarHost` is replaced by `CellClient` and
  `CellSession`. The `plakous-js-host` worker speaks only the new protocol, so
  a stale worker is refused at its hello.
- Python PTC runs on the same cell protocol. `plakous-py-eval` and the
  `monty-pool` dependency are removed, and with them the WebSocket stack
  (`tungstenite`, `tokio-tungstenite`, and the duplicate `webpki-roots` 0.26)
  and the `cap-std`
  graph. `plakous-py-worker` builds `plakous-py-host`, which drives the pinned
  Monty 1.2.0 child state machine in process; the binary `monty` is gone, and
  `PLAK_MONTY_WORKER` is replaced by `PLAK_PY_HOST` (else a sibling
  `plakous-py-host`).

- The context meter counts each image on Claude routes at 4,784 tokens,
  Anthropic's current high-resolution maximum (`ceil(w/28) * ceil(h/28)`
  after resizing), instead of 3,279 from the retired `w * h / 750` rule.
  Requests are unchanged.

- JavaScript cell output fits one tool result's 128 parts and 1 MiB of
  JSON-encoded parts: over the limits, nonempty adjacent text items join with
  newlines, media give way to a counted notice, and oversized text is cut in
  the middle. Script errors survive media omission. The driver validates the
  outcome before committing the bounded, presented parts as cell evidence;
  live delivery and interrupted-batch recovery apply the same observation cut
  once. Invalid media become per-item notices without losing status or valid
  text; projection and presentation refusals retain the status header. Within
  both limits output bytes and request pins are unchanged.

### Persisted formats

- Runtime format epoch 33: the cell compiler uses one v3 descriptor identity
  recipe for product JavaScript, Python PTC (structured and freeform), and
  Codex Code Mode (native and minimal). Their descriptor identities, all
  product-extension receipts and attachment bindings rotate in this epoch;
  the Python profile, presentation, result, catalog and limits component
  identities are removed. Python product evidence now binds the exact outer
  and nested projection digests instead of the removed component identities.
  The typed Python SDK adds a `PythonHarnessSdkV1` v3 cell descriptor and
  a driver-only `run_py` marker; SDK attachments and product-extension
  receipts bind that descriptor and its exact outer/nested projections.
  The Monty adapter is `plakous.monty-python-adapter.v9`; the shared cell
  protocol is `plakous-cell-protocol.v2` (Reset, SDK configuration and typed
  stopped outcomes), refusing stale workers at hello. Both revisions ride
  this unreleased epoch. Existing non-SDK model-visible text is unchanged.
  Unreleased t9 maintainer sessions whose older notices have UUID or
  `imported-N` ids and still resolve to their tree may re-publish their child
  milestones once on the first new host. New durable milestone ids prevent
  repetition thereafter; no migration is applied to those prototype notices.
  The Codex reference measurement revision also rotates the descriptor's
  semantic binding: same-epoch sessions need an explicit rebind. This rides
  unreleased epoch 33; installed epoch-32 sessions are not reopened by this
  build and use the cross-build conversation carrier instead.
  Runtime SQLite schema 26 admits `NamedStateValue` objects (kind 20). The
  manifest gains a sorted named-state key/address group; checkpoint integrity
  and commit identities bind keys and immutable value addresses.
  State key `r26.e33.p34.t9`; carry conversation history and named state with
  `/reload` or export/import through document version 9; version 8 imports
  with empty named state.
  Claude OAuth session authority now binds the canonical provider account UUID
  instead of a grant lineage, so a same-account re-login can continue and
  reopen. This identity change rides unreleased epoch 33 without changing
  the embedded history or binding encodings.
  Unreleased epoch 33 also accepts the exact Tight compaction instruction in
  recovery, additively, without changing serialized fields or state keys.
  Older builds refuse that request as a non-source-derived compaction input;
  rollback to an older epoch-33 build is not supported.
  Carrying a Claude session from an epoch-32 build preserves visible history
  but drops provider replay once: the account digest changes from grant lineage
  to the provider UUID, so old opaque replay is not retagged to the new scope.
- Runtime format epoch 32: Python PTC records the Monty cell-worker engine
  contract and adapter `plakous.monty-python-adapter.v8` instead of the
  monty-pool contract and adapter v7. Its worker artifact, host and containment
  identities also change, so sessions created by an epoch-31 build are not
  opened in place. The JavaScript adapter becomes
  `plakous-js-host-bootstrap.v7`, binding the stored-value depth limit.
  State key `r25.e32.p33.t8`; carry a session with `/reload`
  or export and import. Export document version 8 is unchanged.
- Runtime format epoch 31: the JavaScript worker speaks the cell protocol
  (`plakous-cell-protocol.v1`, length-prefixed frames checked in the worker's
  hello) instead of JSONL, and the recorded engine contract and Code Mode
  descriptor identities name the cell worker revision. A session recorded
  under the previous engine contract is refused as incompatible before any
  inference. State key `r25.e31.p33.t8`; carry a session with `/reload` or
  export and import. Export document version 8 is unchanged.
- Runtime format epoch 30: inline image bytes are stored as canonical padded
  base64 instead of an array of JSON numbers, so a stored image is about 1.33
  times its size instead of about 3.6. Request-plan digests bind the request
  identity instead of re-serializing and hashing the whole request on each of
  the five validations per turn. State key `r25.e30.p33.t8`: 0.0.2 state is
  not opened or migrated; carry a session with `/reload` or export and import.
  Export document version 8 is unchanged.

### Pins

- Review windows. The subscription profiles (Codex, Grok, Claude) expire
  exactly 90 days after their review, at 00:00 UTC; Claude moves from 365
  days to 90 and Codex drops its end-of-day expiry.
- The documented API routes (OpenRouter Chat, OpenAI Platform, xAI Platform,
  Anthropic API) never refuse on the calendar. Each keeps a review-due date 90
  days after its review that only drives `plak doctor` and `plak status`
  notes. The facade constants are renamed to
  `*_REVIEW_DUE_AT*`, and `OpenRouterChatBuildError::ProfileNotCurrent`,
  `XaiPlatformBuildError::Stale` and the OpenAI
  `ConfigError::PlatformManifestEvidenceExpired` are removed.
  `XaiPlatformModel::preflight_configuration` no longer takes a clock.
- Consent binds the contract, not the review: a re-review that changes no
  profile, source, notice or kill-switch revision keeps every acceptance
  current, so nobody accepts the notice again. A changed notice asks once and
  keeps the login; a changed OAuth contract carries a new profile id and needs
  `plak login`. A Codex login stored under another profile now reads as
  written under another profile, as on Grok and Claude, instead of as unsafe
  storage, so `plak login` replaces it (new
  `NativeCodexAuthError::ProfileChanged`).
- `plak doctor` and `plak status` print a `Warning:` for a subscription
  profile within 14 days of its expiry and a `Note:` for API facts within 14
  days of their review-due date, or past either.
- Publication dry runs and uploads require more than a third of every review
  window remaining (more than 30 days of 90) instead of more than 21 days.
- Fresh review dated 2026-10-30 on all seven surfaces. Subscription expiries
  (Codex, Grok, Claude) and API review-due dates (OpenRouter Chat, OpenAI,
  xAI, Anthropic) are all 2027-01-28T00:00:00Z. Native references are Codex
  0.166.0, Grok Build 1.0.70 (`x-grok-client-version` 1.0.70) and Claude Code
  2.1.314. Claude beta order, runtime costume and billing vectors are
  refreshed with the existing cch algorithm; 14 Claude and 13 Grok render
  hashes move accordingly. OAuth consent identities are unchanged.
- Anthropic API model table `anthropic.api-models.2026-10-24.v1`: Claude
  Sonnet 4.5 (`claude-sonnet-4-5-20250929`) is deprecated and retires
  2026-11-30 (replacement `claude-sonnet-5-5`); the bundled table now records
  each row's `deprecated_at` and `retires_at`, so the offline fallback lists
  it as retired from that date. The Haiku 5.5 100,000-token price threshold
  is unchanged.

### Compatibility

- The subscription compatibility receipt stays schema 2 and still records
  the review and expiry dates the notice showed, which this build no longer
  compares. A 0.0.2 receipt for the same profile is current here. 0.0.2
  still compares those dates, so it asks once to accept the notice again for
  a receipt this build wrote; the receipt it then writes stays current for
  this build, and the login is kept.

### Known limitations

- One message is capped at a 16 MiB per-message budget, encoded, and images
  encode as base64: a prompt holds about 12 MiB of images, which is three
  maximum-size (3.75 MiB) images or eight typical screenshots (up to about
  1.4 MiB each). Up to 8 images are accepted; a fourth maximum-size image is
  refused with a message naming the budget.

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
  list and detail pane at 140+ columns; Usage offers `[refresh]` beside a
  Codex, SuperGrok or Claude account whose token needs a refresh, which
  refreshes it (as `plak refresh` does) and re-reads its usage; the label
  highlights only under the pointer or after Up/Down; Usage and Providers read
  credential state off the host loop, so a prompt sent while another process
  holds a subscription lock is accepted at once; streaming code fences
  highlight incrementally; long inline code and URLs wrap. A scrolled-up
  transcript stays anchored while output arrives, and a send-now steer shows
  its row immediately.
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
- `NativeSubscriptions::access_token_expired` reports, from local state only,
  whether a `Ready` subscription credential's stored access token has expired.
- Long Responses sessions keep working past 4096 history items or 3 MiB of
  replay: per-response limits no longer apply to accumulated history.
  Provider replay is bound to the account that produced it, so changing
  accounts never sends one account's encrypted reasoning under another.
- Hosted runtime construction goes through one sealed `RuntimeFactory` port.
- Oversized user messages are refused at admission.
- A Codex native compaction mid-turn keeps the current round's tool batch
  verbatim after the compaction item, so the model still sees the output it
  asked for; when that successor would not fit or shrink, the tail-less
  successor from the same compaction item is published instead. A native
  compaction no longer keeps earlier continuation, plan restoration,
  placeholder or replay-omission notes above its compaction item; process
  notices stay. The plan restoration and continuation notes are appended after
  the batch, so a second native compaction in a turn no longer leaves the
  earlier note above the new summary.
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
- Compaction summaries are written by the model. DeepSeek V4.1 Flash
  (`deepseek/deepseek-v4.1-flash-1017`) on OpenRouter leaves out earlier
  details once large tool outputs follow them, so after a compaction it may
  re-read files; see
  [known limits](docs/user-guide/known-limits-and-security-model.md).
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
