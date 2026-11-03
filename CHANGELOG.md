# Changelog

All notable changes to Plak and Plakous are recorded here. Every workspace
package shares one version. `0.0.x` releases carry no API, wire or
persisted-format compatibility promise; the session export document is the
one cross-build carrier (a build imports its own and the previous version).

## Unreleased

### Plak

- Configuration compatibility: `config.toml` accepts `[host]
  resume_after_loss`, `[mcp_servers.<id>] placement` and `[runners.<name>]`,
  validated and inert until node recovery and runner placement land. A 0.0.3
  build refuses a `config.toml` that sets any of them, so `plak` writes them
  only when you set them and `plak doctor` warns `config uses 0.0.4-only keys`
  while one is set.
- Claude `Read` returns PNG, JPEG, GIF and WebP as an image when the model
  accepts image input. Codex Direct adds `view_image` for one workspace image.
  Other packs stay text. A model that cannot accept the image is refused with
  the route and model named. New sessions record a registry descriptor that
  includes `view_image`; no schema revision or state-key change. Codex native
  Code Mode does not offer `view_image`.
- `/clear` is replaced by `/new`, which starts a new session in the same TUI; the
  previous session is untouched and resumable. With running agents or background
  processes, `/new` asks first and offers to stop that work before switching.
- Claude `Read` and pi `read` results cut to fit the context keep whole leading
  lines and say where to continue, like Grok's read.
- Agent Skills: `SKILL.md` folders under `<workspace>/.agents/skills` and
  `~/.agents/skills` are listed in each family's native wording (the project
  wins a shared name) and frozen with the session like `AGENTS.md`. Claude
  sessions get Claude Code's `Skill` tool. `skills.enabled` (default on)
  applies to new sessions and spawns; a fork, rebind or switch keeps its
  source's setting, and sessions created before this build never gain `Skill`.
  Skills that are skipped are reported once as a session notice. A project
  skills folder that resolves outside the workspace is refused; only the user
  folder is readable outside the workspace, and only regular files.
- Grok `read_file` returns the file text verbatim plus one metadata line
  instead of JSON inside JSON; program cells keep the JSON form. A read cut to
  fit the context keeps whole leading lines and continues after them. Grok
  `grep` and `search_repository` accept the agent's own issued `full output:`
  paths and search job logs up to 64 MiB. Context rows are bounded like match
  rows, and an empty search says so. Tool path wording matches what the tools
  accept, the process lifetime sentence appears only on the starting tool, and
  `list_dir` names a missing path.
- Existing sessions on routes whose tools changed (Grok, Codex, Claude, pi,
  Python cells and the typed SDK) carry on their next reopen.
- `plak session --help` names the export versions it writes (12) and imports
  (12 and 11); a test pins that text to the build's export constants.
- Session export version 12 names the session's runner (today always the local
  runner) and its profile (root template and working set), and an imported
  session keeps that profile. Version 11 documents from the 0.0.3 release import onto the
  local runner under the home default profile with one notice and an identical
  conversation; version 10 is refused.
- Text and thinking streamed by a model request that is retried, refused, cut
  by the output limit or folded for context are dimmed or dropped from the live
  transcript back to the start of that request. Earlier requests of the same
  turn keep their text.
- Copying a thinking lead copies its rail, glyph, summary and meta, and a
  selection edge on part of a wide grapheme takes the whole grapheme.
- A full observation mailbox keeps a credential wait's park and clear notices;
  it drops streaming text or transcript content that the next resnapshot
  repairs instead.
- A root turn that loses its owner mid-tool now recovers in place as soon as
  the session is idle: the pill reads `needs recovery · reopening`, the
  interrupted tool is named once in the transcript (also after a crash during
  the reopen), and no `/recover` or prompt is needed first. A quit sent during
  the reopen is answered after it. Sub-agents keep their existing recovery.
- `/reload` onto a build without an install manifest (a development or
  cargo-installed build) no longer falls back to an unverified resume on a
  busy machine; the reload probe answers without hashing the build.
- An orchestration workspace path that cannot be inspected (permission or I/O
  error) now refuses the spawn, adopt, reclaim or restore instead of being
  treated as absent.
- Telemetry records content-free measurement facts: each child's template,
  salted per-session digests of read, search and list targets with the page
  returned, a program label for shell calls (credential-shaped words become
  `other`), wait targets and outcomes, the context window beside prepared
  input, compaction trigger facts, the first request's footprint, the turn's
  prepared-input high water and idle gaps. `plak telemetry status --views`
  lists queries for them.

### Plakous

- A Messages `model_context_window_exceeded` stop fails the stream and takes
  the context-limit overflow fold when nothing hosted ran, on Anthropic and
  OpenRouter.
- A programmatic tool call cut by `max_tokens` ends the request as an effect
  that needs reconciliation; deferred programmatic calls flush only once the
  stop reason is known.
- Subscription side requests (SuperGrok web and X search and images, Claude
  OAuth web search and page fetch) report a reviewed version gate or sent-beta
  rejection as contract drift with the newer-build remedy. A 401 or 403 keeps
  the credential path.
- Portable runtime interfaces (`plakous::model::runtime`: spawn, sleep, a
  monotonic clock and an HTTP transport) with native implementations in
  `plakous::runtime`; host task sets are built on them.
- Tree mutations run through an async per-tree writer that keeps commit order;
  a storage-full wait no longer blocks a worker thread, owed releases and effect
  completions never give up, and host shutdown is a latched signal.
- Storage-wait interrupts are bound per handle for the runtime and tree stores.
- Runner ports: instruction and skill discovery read through
  `InstructionSourceReader`, workspace probes through `WorkspaceProbe`, and job
  services through `JoinedSessionsPort`; answers distinguish unavailable from
  absent and discovery fails closed. `JobHost`, `discover_instruction_sources`,
  `discover_skills`, `discover_skill_roots` and `Skill::load` signatures change.
- The execution backend descriptor names its runner
  (`ExecutionBackendKind::Runner` with a `RunnerId`; the in-process backend is
  the reserved local runner `local`), and a rebind or fork onto another runner
  is refused with `BindingChange::Runner`. `ExecutionBackendKind::Host` and
  `ExecutionBackendDescriptor::current_host` are gone (`local_runner`), and the
  descriptor's `Debug` output gains `runner_id`. `ExecutionOperationControl`
  becomes `OperationContext`, which also carries the line owner, the request
  class and key, and the caller's deadline; `operation_control` becomes
  `operation_context`, and `RetrievalPort::retrieve` and
  `ImageGenPort::generate` take an `OperationContext`.
- Hosted and orchestration tasks have explicit owners: a session's tasks are
  aborted and joined at release, a tree's at its last root lease, and tasks
  that own a child's session or runtime authority run until they return.
  Operator intents are classified in one table, and each session's product
  writes have their own storage interrupt.
- `AgentSessions::created_tool_registration_and_sources` reads a checkpoint's
  frozen tool registration and instruction sources without opening it.
  `AgentBuilder::image_normalizer` installs the image decoder tool reads use.
- Tool measurement uses typed execution values and the new `WaitFact` only,
  and does no work when telemetry is disabled.

- The attached root runs as one async node actor (biased urgent-first idle
  wait, workflows awaited inline with urgent-only observation, the session's
  task set closed on release); root tree passes run only at the four points
  spec 03 §4.4 names.
- The orchestration VCS port (`occupancy_vcs::VcsOps`) is async, and the tree
  asks it every workspace question before a writer turn; settle and spawn
  mints run as their own release-owned tasks.

### Security

- pulldown-cmark is 0.13.6 (RUSTSEC-2026-0433, quadratic parsing of nested
  image wikilinks). Plak never enables wikilinks, so it was not exposed.
- The build audit also waives RUSTSEC-2026-0429 for zip 8.6.0. Like
  RUSTSEC-2026-0367, -0368 and -0369 it concerns only `ZipArchive::extract`,
  which the Monty type checker's in-memory typeshed reader never calls.

### Persisted formats

- Telemetry store schema 21 (`telemetry-v21.sqlite3`, disposable). The state
  key (`r26.e34.p35.t9`) and the export document (v11) are unchanged.

### Development

- Development tooling lives under `tools/<area>/` beside the recipe modules
  that run it. Reference configs moved to `docs/reference-config/` and config
  scenarios to `tests/fixtures/config-scenarios/`; CONTRIBUTING and SECURITY
  moved to `.github/`; formatter, lint and audit configs use `.rustfmt.toml`,
  `.clippy.toml` and `.config/deny.toml`.
- The static group compiles seven library feature profiles instead of every
  singleton, still validating every feature closure, and checks maintainer
  Python with ruff 0.17.3.
- The crash sweep's twin-pilot determinism check covers every strict shape in
  the durability lane.
- A portability fence keeps `plakous-model`'s dependency closure free of
  native-only crates for `wasm32-unknown-unknown`, and the builds group
  compiles it for that target. `just dev-tools install` adds the target.
- One test-environment helper and one build entry point serve every recipe;
  `just nextest` runs cargo-nextest under the gate's environment.

## 0.0.3

### Plak

- Agent trees use schema 9. Forks use short
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

- Session exports retain Messages native citation replay, including protected
  citation tokens.

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
  The product store admits `py-sdk`.
  `[root] composition` overrides the root template's composition for the root
  only; children keep their template's composition, and changing the root
  template or applying a profile clears the override.

- Contract drift is reported, not mistaken for a credential problem: the model
  picker keeps a drifted row visible but disabled with its reason, `plak run`
  and `plak models` show the reason, and a drifted model is refused before
  effort checks with an install-newer-build remedy rather than a re-login hint.
  Turn-time contract rejections render distinctly from refused credentials.
- Under account load the Codex subscription backend can answer HTTP 401
  "authentication failed (status=503): Too many concurrent requests". That
  exact answer is retried as a temporary overload under the normal retry
  budget, honoring Retry-After, with no credential refresh and no re-login
  advice, including web search and image generation side requests; an
  exhausted budget ends the turn as temporarily unavailable. Every other 401,
  and every other route, is still a refused credential.

- `/reload` asks the installed build, before teardown, whether the session
  reopens exactly. When a frozen binding input changed (an adapter revision or
  a tool description) and the session is quiescent, it carries the session into
  a new linked session in one step; the note names the change, says the provider
  cache rebuilds, and counts the finished sub-agents that are not restored. With
  a running turn, live sub-agents or background processes it refuses and stays
  attached. The check writes nothing, never renews a login and never starts an
  MCP server; when it cannot answer, `/reload` resumes as before. Exact-reopen
  refusals name the changed input. If the installed build cannot import this
  build's export version, the session stays open (`the new build cannot import
  this build's version N export`). `just dev reload-rehearsal` reports
  `reopen=exact|carries|unknown|refused`.
- Each session keeps its own profile. Fork changes only that session;
  `Set as default` in the Agents tab sets the profile new sessions start with.
  `/reload`, import-resume and `/rebind` keep the session's root template and
  working set. The profile chip names the nearest profile and the values that
  differ, e.g. `claude-default · root effort xhigh (profile: high)`.

- Agents list rows show the status glyph, elapsed time, `#id`, role and one
  live fact; model, turns, tokens and process count move to the inspector. A
  narrow row drops the role before the id.
- Dormant children (parked, retired conversation, reclaimed checkout, closed)
  are dimmed with the reason. JSONL `tree` rows carry an optional `dormancy`
  object with only the true facts.
- `t` opens a history of settled agents grouped by local day, with search
  (`/`) and a failures filter (`!`).
- `u` recovers a failed or recovery-required child after a confirmation,
  through the same exact recovery path as `recover_agent`. A paused agent tree
  shows a clickable resume row at the top of the list and of the history.
- When `config.toml` fails to load, the Agents and profile surfaces show one
  diagnostic naming the config error instead of a missing-template error per
  profile, and the profile chip reads `unavailable`.
- `plak session reload <session> --expect-binary <id>` lets the same user's
  processes, including an agent's shell, ask a running interactive session to
  reload onto the installed build at its next idle boundary. When that build
  imports this build's export, a state-format change carries the session and
  wakes the root with one host-authored continuation; when it does not, the
  session stays open. It is off unless the home sets `tui.reload_requests = true`.
- Following the live transcript stays on the last row after block heights are
  corrected and after the sticky prompt bar grows, so the jump-to-bottom arrow no
  longer sticks over a clipped final paragraph. On a light theme the caret uses
  the theme's text color; while Plak owns the terminal the caret is a steady
  block, and leaving restores the terminal's cursor color and shape.
- OpenRouter Chat, Messages and Responses refuse to send a key that was rotated
  after the model was admitted, before any bearer is read. Invalid xAI product
  keys are refused before admission.
- Operator help is chosen by the frontend. Hosted and library refusal text is
  neutral; Plak renders TUI, `plak run` and JSONL help from a closed set of
  typed remedies stored on notice rows, never from diagnostic prose, and a row
  without a remedy (including an imported version-10 row) displays verbatim.
  An exact reopen refused for a changed frozen binding leads with the carry
  (`/rebind`) and names restoring the original account as the exact-reopen
  alternative; a missing login is never a binding change, and contract drift
  carries no login remedy. A missing MCP secret names `plak secret set <name>`,
  low storage names `plak doctor`, and a reasoning downgrade is classified from
  the requested and resolved reasoning state.
- Write refuses to overwrite an existing file the agent has not explicitly read
  in its current form in this attachment; reads are remembered in memory only,
  so a reopened session reads again. Edit, Write, patch and SDK results name
  each changed path and its action, binary overwrites are refused with a
  delete-then-create remedy, and unreadable or directory targets and paths
  beneath a file fail before any effect with a remedy.
- A background process whose ownership was explicitly released shows
  `released`: its notice, trailer and write refusal say the process may still
  be running, never that it exited.
- `/rebind` carries the full history into a new session under the current
  login, keeping the source account, model, effort, composition and target
  when they still resolve. A removed account or a model missing from a Ready
  catalog falls back to the configured root binding, with a notice. A
  binding-refused `/reload` takes this path.
- `sh install.sh uninstall` removes Plak-owned links and snapshot roots and
  keeps the home and credentials. `--dry-run` prints the removal and changes
  nothing.
- A git adopt receipt lists up to 64 changed files (`A`/`M`/`D`/`R`/`T`, and
  `files_truncated` when the list is cut). Adopting a candidate stacked on an
  unadopted input is refused, naming the input and its adopter. Git
  ahead/behind counts are measured from the recorded mint base. On git, adopt
  no longer refuses because a dependent copy is still running; jj still
  refuses, and a jj adopt receipt lists no changed files. A merge-conflict
  refusal names each path's conflict kind and cuts the list at whole entries
  (`+N more`).
- An invalid or incomplete `[root] agent` binding is refused with a named
  remedy. It is not cleared from `config.toml`.
- `plak doctor` and idle chrome warn below 5 GiB free on the product volume.
  New root turns and spawns are refused below 512 MiB (`storage_low`). When
  storage refuses an automatic continuation during `plak run`, the run prints
  `plak: run stopped at storage unavailable`, exits 4 (interrupted) after the
  usual job and child cleanup, and does not retry.
- `plak doctor` points at `plak state prune-legacy` for older telemetry and
  never prunes by itself. `plak status` and `plak doctor` show native refresh
  evidence, and an oversized Claude login response is distinguished from a
  provider rejection.
- JSONL v1 accepts `resolve_approval` (an opaque handle, allow or deny) for a
  live or restored approval.
- The TUI requests keyboard disambiguation, ignores bare modifier keys and
  preserves shifted chords.
- A sent draft shows at once as a dim `sending…` row. A sent steer shows its
  text (`sent · <text>`); steers carry no send time. When provisional rows do
  not fit the shelf or queue panel, the last row reads `+N sending`. A steer's
  transcript row sits where the model received it, not where it was accepted,
  and later steers stay provisional until their own acknowledgement.
- Repeated reopen and cleanup cause chains are collapsed in error text; each
  leaf cause is kept once.
- The composer HUD shows `retrying Xs`; the attempt and cause stay on the
  transcript retry row (`retry n/N · cause`).
- Over-wide fenced and indented code lines soft-wrap under the code rail with
  a `↳` marker, and copy back as the original source line.
- The Context tab keeps the last request's tool names after the turn, in
  memory only.
- While the Usage tab is open during a turn, allowances re-read every 60
  seconds and keep the previous figures on screen. Codex, SuperGrok and Claude
  usage reads run concurrently under one 5-second deadline, so a stalled
  provider does not blank the others.
- Slash palette rows show argument hints (`<id>`, `<profile>`, `[resume]`,
  `<path>...`); `/resume` with extra words is refused and does nothing.
- `just dev reload-rehearsal` reports a carried native-subscription session it
  cannot import without credentials as `import=not rehearsed: credentials
  unavailable`.
- Large composer drafts redraw and scroll without recomputing wrap layout.

### Plakous

- API keys are one shared zeroizing secret type with typed generation-bound
  sources in `plakous-auth`; raw key text and bearer request constructors stay
  inside the integrations, outside the supported facade. API catalogs share one
  bounded transport and row projection in `plakous-exchange`.
- Claimed refresh and revoke share one execution ladder; a revoke whose accept
  fails leaves the credential quarantined. Chat and Messages share one attempt
  host and one HTTP/SSE host, Chat seals a successful terminal before publishing
  it, and Chat, Messages, Responses and image side requests share one bounded
  executor. OpenAI Platform, xAI Platform and OpenRouter Responses share one
  API-key preparation and dispatch skeleton in `plakous-responses`.
- `plakous::agent::BindingChange` names the frozen binding input an exact reopen
  found changed; `AgentSessions::verify_open` answers whether an exact quiescent
  head would attach, with no lease, driver, recovery or write.
  `TopologyHubAttach::Inspect` and `McpToolPack::unbound` compile tree and MCP
  tools without attaching, leasing or starting anything.
- Image residency: the driver replaces the oldest resident images with
  placeholders, unpaid and deterministically, at admission, at tool-batch and
  continue fences and before every model round, keeping image counts and bytes
  within the adapter's budgets (75 % is the batching target). A candidate that
  does not fit defers to the ordinary compaction ladder; a round refuses with
  `RequestBudget` only while images still exceed a budget after it. Selection,
  audits and reconstruction bind one unpaid commit and both recovery consumers.
  `ResidentImageBudget::new` returns a typed error.
- A definite turn refusal (model, request budget, round limit, invalid input)
  at a round start or tool-batch fence first finalizes any open server-pause or
  provider-programmatic continuation, so the turn settles with one terminal.
- Explicit job release is durable: a restored released job reports `released`
  in the process tool output schemas and raised results without claiming exit
  or stop authority.
- Whole-file overwrites require the last explicit text read of the target in
  the same execution environment; mutations revalidate content or absence in
  backend preflight and consume touched observations. SDK operation indexes
  stay on ordinary policy refusals. A linked path component is a pre-effect
  `SymlinkEscape` on Linux and macOS alike, and a component that is not a
  directory is the new pre-effect `ExecutionBackendErrorKind::NotADirectory`
  rather than a missing path.
- `plakous-host-protocol` carries typed host phases, pauses, failures and the
  closed `NoticeRemedy` vocabulary; tool operations are derived by
  `ToolOperation::classify` rather than stored; hosted launch and startup
  timing names no longer name a frontend. `AgentError::reresolution_error`
  keeps a live re-resolution refusal in memory (never in canonical records),
  and MCP errors carry a typed missing-secret requirement.

- `plakous-responses-dialect` is folded into `plakous-responses`. Host, error
  and usage hooks become concrete family code and the internal projection and
  parsing surfaces narrow; route policy, digest domains and semantic revision
  pins are unchanged.
- Retired crates stop at 0.0.2 on crates.io and are not yanked:
  `plakous-responses-dialect`, `plakous-code-mode`, `plakous-js-eval` and
  `plakous-py-eval`. The workspace has 37 packages.
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

- A Messages response that stops at `max_tokens` or
  `model_context_window_exceeded` inside a client tool call, and a Chat
  Completions `finish_reason: length` with tool calls, no longer fail the turn
  as a protocol error. The whole response is discarded without running or
  recording the call; the turn retries once with the output-limit note, and a
  second consecutive output limit fails as an output limit. A cut after
  possible hosted effects still requires reconciliation.

- Offline folds require at least a 5 % fresh-input reduction. A summary that
  overruns its output bound selects the Tight instruction for the next permitted
  fold; response-free failures do not reset it. Idle `/compact` and scheduled
  passes take the free placeholder rung after a definite paid failure, and a
  free commit after that failure cannot start a paid scheduled follow-on.

- Atomic attachment exposes only the cursor, source/turn lifecycles and
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
- Frozen subscription vocabulary remains readable; omitted operations refuse
  before state writes.
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
  implementation. Compose exposes the inert `HostApi`, `python_bootstrap`, its
  interpreter-neutral `PythonBootstrap` data, and `codex_exec` at its crate
  root. Presentation building blocks and fixture constructors are private.

- Local composition has session-owned named state. The driver stages bounded
  canonical JSON writes per cell and publishes them only with a successful
  terminal result. Exact reopen, compaction, fork and rebind preserve saved
  values independently of interpreter lifetime. The harness lists keys and
  sizes, not values, before the next prompt.

- The harness compaction instruction asks for fixed sections (Requests,
  Constraints, Values, Work done, Decisions, Open errors, Plan, Next step),
  each written once. Standing user constraints from any request stay quoted
  exactly. Only the three newest requests not in progress are quoted, up to
  about 150 words each, keeping every requirement in the user's own words for
  longer requests; earlier ones get one line, finished ones merge into at most
  10 lines, and a finished request keeps the values it produced, moving them
  to Values rather than dropping them to meet the line cap. An earlier summary
  is merged rather than carried forward whole, and the summary aims under
  about 2,000 words to curb growth across folds.
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
- Compaction is scheduled when checkpoint history approaches the segment
  bound, including a session of many short turns, and before admission
  exhausts input headroom. A native compaction that refuses its artifact falls
  back to harness compaction.
- The Codex context meter applies the pinned reasoning-length estimate to
  encrypted reasoning items.
- Codex streams that label a just-added assistant item `completed` are
  accepted; completion is still decided when the item is done.
- Unsigned historical Messages thinking is recorded in the log only, not as a
  transcript diagnostic.
- Connects race all resolved addresses, staggered by 250 ms. Codex
  `usage_limit_reached` is terminal and carries validated reset advice.
  OpenRouter Chat replays one in-band 408 or 504 when the replay is
  effect-safe, within the existing retry budget, discarding the interrupted
  output.
- A process cwd beneath a non-directory is the pre-effect `NotADirectory`
  refusal; `image_gen` refuses an output path beneath a non-directory and
  treats an edit input beneath one as missing.

### Persisted formats

- State key `r26.e34.p35.t9` (runtime SQLite schema 26, runtime format epoch
  34, product schema 35, agent tree schema 9; 0.0.2 used `r25.e29.p33.t8`).
  0.0.2 state is not opened or migrated, and its directory is left for the
  0.0.2 binary.
- Export document version 11; imports versions 11 and 10. 0.0.2 wrote version
  8, which this release refuses, so 0.0.2 sessions cannot be carried into
  0.0.3: `/reload` from a running 0.0.2 TUI keeps the session open (`the new
  build cannot import this build's version 8 export`), and `plak session
  import` of a 0.0.2 export is refused. Finish those sessions on 0.0.2, or
  start new ones. Agent trees never cross a state-key change.
- Inline image bytes are stored as canonical padded base64 (about 1.33x the
  image size instead of about 3.6x).
- JavaScript and Python cells run on `plakous-cell-protocol.v2`
  (length-prefixed frames, worker hello checked before Configure); the Monty
  adapter is `plakous.monty-python-adapter.v9` and the JavaScript adapter
  `plakous-js-host-bootstrap.v7`; one v3 descriptor identity covers product
  JavaScript, Python PTC, the Python SDK (`PythonHarnessSdkV1`) and Codex
  Code Mode.
- Runtime schema 26 stores session-owned named state (object kind 20) bound
  into checkpoint and commit identities.
- Claude OAuth session authority binds the provider account UUID, so a
  same-account re-login continues and reopens.
- Recovery accepts the Tight compaction instruction.
- Epoch 34 records compaction strategy tag 5 with v3 image-residency requests
  and evidence, and an explicit Released job terminal (tag 3). Runtime schema
  26 checks codec 34. Process tool output schemas (`released`) and the
  file-mutation tool descriptions rotate the description-derived tool,
  projection, plan and Code Mode cell identities; read observations are not
  persisted. Product schema 35 and export version 11 store the typed remedy
  on notice rows. Version-10 notices import verbatim, without a remedy.
- Telemetry schema 20 (`telemetry/telemetry-v20.sqlite3`). Older telemetry
  databases, including `telemetry-v19.sqlite3` and unversioned
  `telemetry.sqlite3`, are not read; `plak state prune-legacy` lists and
  removes them.
- Home config key `tui.reload_requests` (bool, default false). Reload handoff
  sidecars are private transient files under `carry/`; reload requests,
  listeners and outcomes are private transient files under
  `run/reload-requests/`.

### Security

- pulldown-cmark 0.13.5 fixes RUSTSEC-2026-0414 (Plak did not enable the
  affected wikilinks option).
- The configured editor (`plak config edit`, `/config`) runs as a direct
  process with shell-word parsing; substitutions are never executed and
  unmatched quotes are refused.
- Claude OAuth login state is independent of the PKCE verifier and compared
  in constant time; grant and request secrets stay in zeroizing redacted
  containers.
- Inside a child copy, the git shim refuses writes to coordination state:
  pins, other copies' branches, the stash and worktree-qualified refs,
  including through fetch/push mappings, stdin transactions and
  rebase.updateRefs.
- Reload requests are same-user only, off by default, and home-scoped
  (`tui.reload_requests`).
- The Linux beneath-open classifies `ENOTDIR` by stating only the final
  component relative to its O_PATH parent, so classification never follows an
  intermediate link outside the workspace.

### Pins

- Process tool output schemas gain `released` and the file-mutation tool
  descriptions are rewritten, so the eight durable descriptor registry and
  eight projection pins, five Code Mode cell identities, the Python SDK
  identity, four tool-text digests, 48 prompt-render rows, eight
  compaction-wire rows, the tool-results lifecycle row, and the epoch-34
  publication and checkpoint-identity goldens move. The files pack gains the
  `files.write_parent_not_directory` refusal pin.

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

- No OS sandbox and no tool-approval prompts: process tools run with your full
  host authority. Credentials are plaintext `0600` files; there is no OS
  keychain. See [known limits](docs/user-guide/known-limits-and-security-model.md).
- The `claude` route presents the Claude Code client identity. Plak is not an
  official Anthropic client; use it at your own risk.
- The Codex, Grok and Claude subscription routes refuse after their compiled
  expiry (2027-01-28T00:00:00Z for this build) until a newer build renews them;
  API routes never refuse on the calendar.
- Compaction summaries are written by the model. DeepSeek V4.1 Flash
  (`deepseek/deepseek-v4.1-flash-1017`) on OpenRouter leaves out earlier
  details once large tool outputs follow them, so after a compaction it may
  re-read files; see
  [known limits](docs/user-guide/known-limits-and-security-model.md).
- The new compaction instruction has not been through the live matrix.
- API-key routes compact locally only: no provider compaction, token counting,
  user-configurable cache settings or hosted programmatic tool calling yet.
- xAI sometimes refuses a follow-up turn on content-policy grounds in long Grok
  sessions; Plak reports it as a provider content-policy refusal.
- Image input on OpenRouter's free `gemma-4-31b-it:free` is unproven.
- One message is capped at a 16 MiB per-message budget, encoded, and images
  encode as base64: a prompt holds about 12 MiB of images, which is three
  maximum-size (3.75 MiB) images or eight typical screenshots (up to about
  1.4 MiB each). Up to 8 images are accepted; a fourth maximum-size image is
  refused with a message naming the budget.
- Template `[env]` reaches only the `on_mint` hook, not tool calls.
- Dormant jj copies are retained because coverage inspection is unavailable;
  close or discard them explicitly.
- jj adopt receipts list no changed files.
- Responses prefix drift is not persisted and the producer is unnamed after
  reopen. Telemetry does not yet record wire bytes or per-kind event counts.
- Not yet available: skills, tool search, a browser tool, agents as tools, MCP
  elicitation, and keyboard navigation between transcript items.
- The build audit waives three zip extraction advisories
  (RUSTSEC-2026-0367, -0368, -0369): the affected zip 8.6.0, pulled in by the
  Monty type checker, only reads its embedded typeshed archive in memory and
  never extracts files. `cargo install` of `plakous-py-worker` cannot apply the
  repository's `monty-typeshed` patch, so it builds the upstream crate, which
  depends on the advisory-affected zip 0.6.6 and whose type-checker archive is
  not byte-reproducible; release tarballs and source builds use the reviewed
  snapshot.

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
