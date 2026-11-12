# Changelog

All notable changes to Plak and Plakous are recorded here. Every workspace
package shares one version. `0.0.x` releases carry no API, wire or
persisted-format compatibility promise; the session export document is the
one cross-build carrier (a build imports its own and the previous version).

## Unreleased

- Fix continuation command collisions between distinct sessions at equal feed
  positions, and namespace child loss continuations by their tree. Reopen keeps
  recorded decision identities, including older pending and consumed decisions.
  A proven foreign-submission collision gets one durable replacement decision;
  the other session's submission and accepted work remain untouched.

- Extend the test-hooks-only pending-draft producer to capture the first native
  sender's original UUID without choosing it at host startup. A content-free
  barrier reports that UUID; real pending checks, release, durable duplicate
  proof and cancellation retain their existing authority. Default and
  maintainer builds are unchanged; no persisted or wire format changes.

- Fixed a busy root's graceful host handoff being lost after an accepted
  send-now steer; restarting the host continues the handed-off turn once.

- Keep process executor contracts stable across shell-file replacement, while
  refusing replacement or retarget during a runtime enrollment. Fresh runtime
  enrollment accepts the new shell without changing recorded IDs.
- Preserve frozen tool presentations during manual compaction and inherited
  skills and pending epochs during fresh binding workflows. Recovery remedies
  respect the session's presented schema.
- Refuse host upgrades only on live binding regressions, showing actionable
  rows; report inactive branches and externally owned MCP contracts separately.

- Preserve each session's frozen tool presentation on exact reopen across
  release changes, while checking current executor compatibility separately.
- Check retained bindings read-only before a same-key host upgrade surrenders
  ownership, including branches belonging to children.
- Pin release binding facts across the offline route matrix and require exact
  reopen of predecessor-authored native root and child sessions.

- Add an integration map and a stdlib Python socket client checked against
  a real offline host in the tooling gate.

- Publish `plak wire schema` as a supported command and attach target-specific
  JSON Schemas to releases.

- Add `plak runner connect --stdio --binary <id>` to select and verify an
  already installed immutable runner image without moving command links.
  Unsafe, missing or mismatched snapshots refuse with an install remedy;
  ordinary exact-triple and pairing admission remain unchanged.

- Add macOS LaunchAgent installation for `plak host install`, with private
  log directories and explicit load, start and removal instructions. Installation
  never loads or starts the service.

- Report issued job output containing NUL as binary during literal or regex
  search instead of returning unmarked binary text. Context beyond an early
  match stop marks binary lines explicitly and keeps later context. Invalid
  UTF-8 still searches lossily with its existing warning and later matches.

- Fixed runner calls being queued on an already cancelled link; calls refused
  before dispatch remain provably not sent, without a replay journal claim.

- Ride out multi-minute provider outages when transport evidence proves a
  request write never completed, without spending ordinary retries or replaying
  an ambiguously dispatched session workflow.
- Clarify and test that Responses host-behavior revisions guard prepared
  attempts, not frozen session bindings: existing sessions reopen exactly
  across that revision change without a carry or rebind.

- Let a parent re-task a child held for recovery through
  `send_agent_message` with `purpose=followup` and `acknowledge_recovery` set
  to the child's current attempt ID. The host settles the old session without
  replaying uncertain work before admitting a fresh attempt. The receipt's
  `recovery_warning` preserves that a tool may have changed the workspace;
  ownership, effect-evidence and workspace refusals still block admission.
  The host-layer `ChildController` gains `recover_followup`, and the typed send
  request gains `acknowledge_recovery`, and the stored send receipt gains an
  optional `recovery_warning` field; no table, export-document or frame change.

- Speed up host startup continuation discovery by reading handoff candidates
  without reopening every persisted tree; full eligibility checks are unchanged.

- Continue recorded child handoffs at host startup even when their parent
  root was idle, without waiting for a client to attach. Unhanded-off children
  do not cause an eager open.

- Fixed unavailable process-group scans classifying promoted jobs as leftovers
  and allowing a run to exit before their completion notice.

- Fixed workspace reads returning stale cached bytes after a same-size rewrite.
- Fixed pending child decisions not being offered when the child's read model
  first attaches.
- Fixed due idle retention being skipped when an idle driver stops.

- Fixed duplicate topology refreshes from split binding/attach observations and
  made topology precede the terminal's initial promptable notification.

- Fixed a redundant topology snapshot request while resuming a session; the
  terminal now waits for the successful target transition before refreshing.

- Add explicit test-hooks-only abandoned-open recovery seeding and a real
  draft-reservation hold for deterministic host-client recovery and same-ID
  pending-admission tests. Default and maintainer behavior is unchanged.

- Restore live terminal pins and residency from same-instance inventory. Permit
  explicit lost-runner acknowledgment after a continuously observed namespace
  deadline or proven configuration removal, resetting loss witnesses on rebind.
  Open-rate backpressure now carries the remaining retry delay; other terminal
  backpressure explicitly reports no timed estimate. Browser rendering remains
  private client work. Product storage and state key are unchanged.

- Add client protocol 9 operator-terminal requests, fresh action-bound consent,
  independent credited stream frames, confirmed controller takeover and explicit
  residency/loss outcomes. Credit batches validate atomically; bounded per-stream
  and aggregate output budgets preserve control/transcript progress. The default-off
  terminal switch enables the strict-CSP WASM seam without permitting script eval.
  Product DTOs own terminal events; generic host streams remain attachment-free.
  Runner protocol 6 becomes 7 for OperatorControl and paired binding-free
  qualification in one batch, preserving the preceding Changes contract.
  Terminal state remains process-local; product schema and state key are unchanged.

- Preserve terminal controller authority across same-instance renewals. Bound slow-reader
  queues by decoded bytes, coalesce adjacent data, retain terminal end notices and
  close command admission before draining. Enforce host-wide quotas and six opens
  per principal per minute; collect completed host receipts behind principal-local
  fences without blocking runner inventory. Isolate operation locks and shutdown
  work across runners, and retain pins during same-instance disconnects.

- Collect refused terminal opens behind compact exact-outcome fences. Reserve
  bulk-channel capacity for terminal control responses and gate reads without
  losing the read deadline. Clean independent validated process groups even when
  another group is uncertain, report live session survivors, and back off periodic
  proof scans while retaining the explicit-close proof window.

- Anchor successful Darwin PTY spawns through the owned, unreaped child handle,
  including children that exit before their first observation. No Darwin waitid
  result is required; Linux retains full-scan identity checks. Retire the shared
  anchor before reaping and verify live members before every group signal.

- Keep operator-terminal authority process-local on the runner and home-local
  daemon. The private host registry discovers surviving pin inventory on first
  bind, retains resident holds and authenticated controllers, and joins shutdown.
  Scope reconciliation is independent of terminal namespace expiry and quota.
  Runner-owned leases guard workspace removal/trim; precise lost-instance pins
  remain while host memory survives. Simultaneous host/runner loss is unlocalized
  unknown loss, not cleanup proof. Product schema stays 37 and the state key is
  unchanged. Client protocol 9 exposes the private registry without adding persisted state.

- Keep settled remote upgrade retries on the serving registry connection, so
  importing another root cannot cancel live services. Retry with status-change
  wakeups and capped exponential backoff. Add `plak host upgrade --abandon` for
  permanently lost pending remote successors; reservations remain unusable,
  while retained exports can be copied and normally imported elsewhere.
  Missing rollback records count as restored; archived heads are excluded from
  settlement and the carried-root cap. The unshipped ownership receipt remains
  version 1 with explicit placement and abandonment; export v12 is unchanged.

- Fixed native-client exits losing `bye`: last-handle drop now waits up to two
  seconds for the sole writer to drain and finish, even after explicit goodbye.

- Wait up to two seconds when a root reopen races this process's runner
  readoption or lease-only stop, without weakening another process's lease
  fence. A local timeout now says that this host is still reconciling the
  session's runner and asks for a short retry, rather than claiming another
  process holds it. Skipped readoption is retried after a failed local open;
  a successful open suppresses the retry to preserve runner readiness. Local
  contention makes dormant stops explicitly retryable.
  The host-layer `readopt_lines` and `finish_recorded_stop` functions now take
  `ProductStateRoots` instead of a standalone session service, so their
  coordination identity comes from the actual runtime database.
  `try_lease_only` and `ReconciliationPermit` expose the same scheduling gate
  to product-owned stop retries. No persisted, wire or binding format changes.

- Harden workspace inspection audit coverage, deadline-bounded placement probes,
  UTF-8 result filtering and retryable continuation authority; retain full bounded
  Changes snapshots independently of client-response truncation.

- Harden Changes execution against repository-local executable search and Git
  index refresh writes. Require Git 2.44 with a no-lazy-fetch capability probe,
  enforce repository ownership, and tighten metadata redirect checks.
  Keep the 2,000-entry internal status budget separate from the bounded client
  projection, so full execution identities survive client-side truncation.

- Add consented host-console writes for named accounts, masked write-only keys,
  cancellable subscription login, logout and explicit refresh, plus etag-checked
  home configuration, template and profile publication. Client protocol 6 becomes
  7 (runner protocol unchanged); welcome advertises `max_request_bytes: 278528`,
  with the enlarged request allowance restricted to template/profile puts.
  Remote `SaveSettings`, `ApplyProfile {apply:default}` and
  `RefreshUsageCredential` are refused; the public socket TUI marks these
  controls unavailable, names host CLI remedies and visibly handles late
  refusals. Terminal-local preferences remain editable. Public API additions:
  `plakous-hosted` exposes `HostSessions::{host_request_timeout,
  host_request_refused, host_disconnect}` and `HostRequestContext.request_id`;
  `plakous-host-protocol` adds `ProtocolExtension::host_request_size_allowed`,
  `ConsoleCapability::{CredentialsWrite, ConfigWrite}` and a redacted
  `LineReader` Debug implementation. No persisted format changes.

- Fixed reattaching the original session after Fork now: the source and fork
  remain independently attachable at their directory addresses with their own
  transcripts.

- Add an explicit detach-only goodbye to the native wire client: send `bye`
  once and half-close writes while retaining the receiver to drain replies and
  exact delivery dispositions, without stopping session work.

- Add fixed, bounded read-only Changes execution: contained Git status and
  worktree-versus-HEAD diff, revision preconditions, disabled repository helpers
  and network/lazy fetch, and explicit refusals on unsafe repositories or limits.
  Paired runner protocol 6 adds the ownerless BuildRead `workspace.changes`
  status/diff verb; home-local daemons remain process-only.
  Placed environments route the same executor through their frozen backend
  identity, with matching local and paired-runner results and no local fallback.

- Add protocol-8 saved-workspace Files and Changes inspection with exact byte pages, contained execution and bounded operator admission.
- Include exact-account last settled credential-refresh outcome and time in protocol-8 account status reads.

- Add a separate operator-terminal runner protocol with validated namespace,
  terminal identity, dimensions and raw-byte stream vocabulary. Runner protocol
  7 is an exact-version cut, independent of client protocol 9.
  Runner and home-local daemon PTYs use principal-private, bounded in-memory
  tables and output rings, independent quotas and awake deadlines, single
  controllers and session-wide identity-checked cleanup. Unproven cleanup keeps
  workspace pins and blocks idle exit; agent lines, jobs and accounting are
  unchanged. Browser rendering remains private client work.
  Session-id isolation is probed before enabling PTYs, and transient observation
  failures never close live shells. Unknown termination supports fresh absence
  proof or explicit acknowledged loss with surviving identities. Completed
  receipts and retired namespaces are collected behind bounded replay fences.
  Darwin retains an owned zombie's unreaped-child anchor without treating it as
  live session membership; fast-exiting children retain cleanup authority.

- Add settled-root `plak host upgrade --expect-binary <id>`.
  It preflights the destination, optionally waits with a bounded admission
  pause, and carries settled roots across state keys through unchanged export
  v12, with durable successor mappings and source-surrender/retry receipts.
  Named state and paused text queues survive; carried roots start new lines.
  Children remain inert references and cannot be revived. Running/held work,
  live or sealing children and nonterminal daemon jobs refuse before cross-key
  drain. Paired remote roots require advancing the runner's dedicated link to
  the exact matching snapshot first; read-only qualification checks identity,
  revision/flavor/protocol, configuration and capacity before drain. After
  surrender the source exits; the destination host imports under its own
  custody before Ready and recovers partial imports from receipts. Surrendered
  roots cannot reopen on the old build. Same-key upgrade remains planned
  handoff. This is not live tree or job migration. Adds ownership receipt
  format 1 with source archival rollback and per-successor Ready acknowledgements,
  and private coordinator protocol 1, not a new session export or state-key
  format; runner protocol 8 adds paired read-only qualification.

- Recover Prepared host upgrades through plain host startup as well as explicit
  retry; withdraw non-crash preparation failures, make Aborted attempts inert,
  and archive surrendered source records for older builds' ordinary-reopen
  protection. A post-surrender runner outage withholds only its own successors
  while ready roots serve; host-owned retries finish them without duplicate
  import or renewed strict capacity/credential probes for completed imports.

- Reopen compaction and profile successors on runner-placed and home-daemon
  sessions through a fresh process port scoped to the current line owner.
  Profile switches keep their recorded attachable root address, and a late
  `ProfileApplied` confirmation reaches only the requesting client's
  replacement attachment on the same live connection.

- Keep saved history readable when a session cannot open: root attach answers
  `Attached` with a read-only, `not_ready` view instead of refusing it. Plain
  attach retries a failed open once its cause is fixed, except while the startup
  scan owns the retry; existing read-only links are told to reattach when it
  succeeds. Startup continuation retries transient pre-admission failures with
  backoff: 1 s doubling to 30 s, unbounded attempts while eligible. Interrupted
  runner line inspection is retryable, but explicit refusals are not. The public
  `plakous::agent::SessionStore` gains a required `branch_line` method, breaking
  embedder implementations. `plakous-hosted` renames `ArchivedSessionView` to
  `StoredSessionView` and `archived_view` to `stored_view`; the latter takes an
  `include_unarchived` flag to avoid projecting live sessions on normal attach.
  No frame-schema or persisted-format change; export document unchanged.

- Allow repeated reads within one model tool call on a remote runner. Claude
  `Read`, sliced text and image reads, multi-file patches, and search context
  reads no longer collide under the tool call's effect key. Reads retain their
  owner fence and placement hold but are neither recorded nor acknowledged;
  keyed effects retain their at-most-once behavior. Runner protocol 5 separates
  these semantics from older home-local runners, which can bind across source
  revisions; frame schemas and persisted formats are unchanged.
  Settle home-local background jobs before upgrading: a protocol-5 host does
  not see a live protocol-4 home-local daemon or its jobs, including in the
  embedded startup check. Finish or stop them with the old build before
  installing and using `/reload`.

- Open sessions placed on a remote runner with skills on: such a node
  discovers `<workspace>/.agents/skills` and then the runner process's
  `~/.agents/skills`, both on the runner, never the host's home. User-skill file
  reads use the runner's read-only exception, allow links only within that root,
  and never fall back to a same-spelled host file. Stop cancels a stalled read;
  a file identity change between slices refuses the whole page. A runner reader's
  failure now names the runner's reason (`SourceReadError::UnavailableBecause`), so
  `SourceReadError` is no longer `Copy`; discovery still fails closed when the
  runner cannot answer.

- Hold a live session's runner work while its placement is unreachable instead
  of failing it: unsent tool effects and model reads wait for the reconnect
  barrier and are sent once afterwards under their own key, a foreground
  command's observation reads again at its cursor, an effect already sent is
  observed past its window under its original key, and no new paid model
  request is sent until release. Stop and refusal end the wait; the directory
  shows the waiting root turn as needing the operator. No wire or format
  change.

- Bind a paired runner whose OS differs from the host's: a runner descriptor's
  containment is checked against its own `linux` or `macos` tag, not the
  decoding process's OS, and a runner naming any other OS is refused.

- Name the runner's OS in the prompt's environment sentence for a session whose
  tools run on a remote runner. `plakous::prompts` takes a `PromptWorkspace`
  (root, cwd, backend OS) in place of the root and cwd pair.

- Compile a session's explicit plan against the backend its tools run on, so a
  fresh session on a remote runner opens instead of being refused as an
  execution-environment change. `compile_explicit_runtime_profile` takes the
  backend's `ExecutionBackendDescriptor`, now on the facade.

- Move image sniffing and attachment/tool-result normalization into the internal
  `plakous-image` crate, so a client without the facade shares the one
  implementation. Outputs are byte-identical; no wire or format change.

- Share serialized product wire types through the internal `plakd-wire-types`
  crate without changing the wire schema or resetting terminal-local preferences.

- Include host/web serving in default builds and qualify a transparent
  `host connect --stdio` bridge. Explicit local auto-start uses a curated
  environment, detached bounded logging and five-minute empty-service exit;
  cross-key home guards prevent sibling hosts. Bare terminal launch is unchanged.

- Let the serving host announce and refuse requested reloads without a
  connected terminal; a client never consumes the host's request channel.

- Preserve current terminal preferences when a stale product configuration
  snapshot is saved; explicit terminal-key edits still apply under the lock.

- Refresh hosted-terminal preferences on every UI relaunch, including resume
  and rebind.

- Persist terminal preference resets locally and serialize configuration
  publication so independent terminal and host settings edits are merged.

- Preserve terminal-local theme and reading preferences across wire settings
  saves and reconnects. Automatic reattachment also keeps an unresolved
  new-session request's identity until the operator retries or selects a session.

- Add the portable internal runner dial core: Noise IK, pairing proofs, offers,
  fingerprint confirmation and bounded encrypted transport framing, without a product caller.

- Keep host startup tolerant of brief discovery lock probes and retire a
  recovered creation request identity on explicit session selection. Pin
  terminal restart pilots after stopped-tree settlement, and verify live
  streaming after reattachment.

- Keep held child jobs alive across successor attachment. Start managed local
  runners once per dial without ambiguous detached fallback, probe socket locks
  before spawning, and reap detached children. Hosts reconnect lost daemon links
  and isolate recovery failures per item. Foreground diagnostics drain on implicit
  transfers and retire with their owner; embedded opens refuse live daemon work
  anywhere in their tree without altering job outcomes. Session formats and frozen bindings are unchanged.

- Runner protocol 3 ends foreground authority at a newer owner transfer or
  instance-lease expiry, not at a transient disconnect. Transfers report stopped
  foreground processes for structured logging only; existing call recovery
  settles their unanswered tool calls once. Promoted jobs survive and are
  re-adopted. Queued starts and promotions recheck line and instance authority
  when execution begins. Home-local daemons use `host/runner/v3/`; obsolete protocols are
  inspected and reported unknown, never re-adopted. Session formats are unchanged.

### Format cut

This release changes every persisted store at once (the 0.0.4 format cut), so
a 0.0.4 build opens no 0.0.3 state; 0.0.3 sessions cross through `/reload` or
an export.

- State key `r27.e37.p37.t10` (runtime SQLite schema 27, runtime format epoch
  37, product schema 37, agent tree schema 10; 0.0.3 used `r26.e34.p35.t9`).
- Product schema 37 adds operator titles and archival state. Client protocol
  5 adds explicit runner placement and directory lifecycle; runner protocol
  4 adds workspace catalogs and per-session execution services.
- Epoch 36: a prepared-call record no longer embeds its request's wire-input
  item list (about 166 bytes per item, every round); it stores the list's
  identity digest, so a long turn's records stop growing as rounds times
  history (56% of a measured 3.4 GB store). The request anchor in context
  state stays the one persisted copy. Measurement, the token-projection,
  scope, status and admission identities, and the 4 MiB record bound (which
  still counts the list as if embedded) are unchanged; a decoded record
  refuses to be measured again.
- Runtime schema 27 records agent lines: each line's head, every branch's line
  membership and `line_job_terminals`. Epoch 35 adds line membership and
  generation to the checkpoint topology, line-head job owners and the line
  feed facts; the execution-environment digest moves to format 5 and the
  run-stable workspace facts to `workspace-environment-v2`, so every
  environment descriptor and plan digest changes.
- Product schema 36 adds the write-ahead `carry_intent` of a same-store carry,
  the session's node, the virtual root's profile, the runner display copy,
  frozen MCP placement (`host` or `runner`), the `switch` workflow kind and the root
  interrupt cause and continuation decision intents. The clear workflow kind is
  now `new` with `plak:new:` operation ids, and the legacy `workflows` table is
  gone.
- Agent tree schema 10 adds the node's line, epoch and runner placement,
  successor epochs, fork provenance, runner ancestors and the tree custody
  record (one live `plak` process holds a tree; see Plak below).
- Holder claims are `plak-holder-v3`: one lock per process and one record per
  grant, with no 128-claim cap. Tree custody proves a holder gone by its own
  host-lifetime lock in `custody-hosts/`, beside `subagents.sqlite3`.
- Session export version 12 adds the session's runner (today always the local
  runner) and its profile (root template and working set). It imports versions
  12 and 11 and refuses 10; `plak session --help` names those versions from the
  build's own constants.
- Telemetry store schema 21 (`telemetry-v21.sqlite3`, disposable).
- Upgrading from 0.0.3: a 0.0.3 session carries forward through `/reload` from
  a running 0.0.3 TUI, or through `plak session export` and `plak session
  import`. Version 11 documents import onto the local runner under the home
  default profile with one notice and an identical conversation. Agent trees
  never cross a state-key change. A 0.0.3 build refuses a `config.toml` that
  sets any of the new keys below.
- `/new` is stored under its own name: the session workflow it records is
  `new` (it was `clear`) with `plak:new:` operation ids. The JSONL `clear`
  command and the `clear` host phase keep their names.
- Configuration compatibility: `config.toml` accepts `[host]
  resume_after_loss` (`hold` or `continue`, default `hold`), `[mcp_servers.<id>]
  placement` (`host` or `runner`, default `host`), `[runners.<name>]` and
  `[web] allow` / `[web] origin` (read by `plak host --web`, below);
  `resume_after_loss` applies to the root and sub-agents a lost host
  interrupted. `plak` writes them only when you set them, and `plak doctor` warns `config uses
  0.0.4-only keys` while one is set.
- Plakous embedder break (agent lines): a head-advancing rebind's source
  becomes read-only, a non-head branch; opening it for work, committing on
  it, rebinding or pruning it is refused `NotLineHead`. An embedder that
  compares two bindings on one history forks, or uses
  `RebindSessionRequest::starting_line`.
- Plakous embedder break (agent lines): `JobOwner` is `{ line, generation,
  session_id, branch_id, fence }`, and `JobHost::service` takes a `LineId`
  (`service_at` takes a session and branch). A `JobHost` transfers a line's
  service only to an owner that `JobOwner::succeeds` its holder.
- Plakous embedder break (agent lines): the per-construction `AgentId` is
  renamed `DriverInstanceId`.
- Plakous embedder break (agent lines): a head-advancing rebind or carry whose
  source head has running jobs is refused (`RebindRefused::RunningJobsWithoutJobHost`,
  or `UnsettledProcesses` for a carry) unless the target binding installs a
  `JobHost`; `plakous::tools::LocalJobHost` is the in-process one. Without a
  `JobHost`, `OpenedSession::release_for_transfer` is a terminal release that
  stops the jobs.
- Plakous embedder break (agent lines): a same-store carry
  (`CreateSessionRequest::continuing_line`) takes the source head's lease, so
  it is refused as `Ownership` while the source is attached or held lease-only
  and as `NotQuiescent` while its head has an unfinished turn.
- Plakous embedder break (agent lines): history pruning (`gc`) keeps a line's
  predecessor branches whole (`PruneHistoryOutcome::Superseded`, or `LineEnded`
  once the session holding the head was deleted); deleting the session is the
  reclaim.
- Plakous embedder break (agent lines): a fork or new-line rebind copies no
  running job rows; it keeps the job numbers and the rows of jobs with
  terminal evidence, under its own owner.
- Plakous embedder break (agent lines): `JobView::owner` names the owning line
  (`LineId`) for a durable job row and the driver instance
  (`DriverInstanceId`) for a live process snapshot. It is display attribution,
  never authority.
- Plakous embedder break (runners): `ExecutionOperationControl` is renamed
  `OperationContext` (adding `owner`, `class` and `budget`) and
  `plakous_tools::operation_control` is `operation_context`;
  `RetrievalPort::retrieve` and `ImageGenPort::generate` take an
  `OperationContext`.
- Plakous embedder break (runners): `ExecutionBackendKind::Host` is gone. The
  in-process backend reports `ExecutionBackendKind::Runner` on
  `RunnerId::local()`, and `ExecutionBackendDescriptor::current_host` is
  renamed `local_runner`.
- Plakous embedder break (runners): the descriptor wire encoding gains
  `runner_id` (also in its `Debug` output), so every environment descriptor,
  plan digest and runtime environment component changes.
- Plakous embedder break (runners): a rebind or fork whose target names
  another runner is refused (`InvalidInput`, `BindingChange::Runner`).

### Plak

- When this home's `plak host` answers, the TUI uses its local socket.
  Terminal and web attachments share a session; `/new` switches only the
  terminal, quit detaches, `/stop` stops the attached session, and `/reload`
  replaces only the client. With no host it still embeds; a wire-version
  mismatch refuses with a stop-host/run-its-build remedy.

- Host-mode attachment refusals preserve the previous attachment and show
  the holder's remedy. Discovery enforces same-user socket checks and waits
  through host start/stop windows. Requested reloads settle even across an
  embedded-to-host transition or with two terminal faces, and an unanswered
  `/new` reuses its identity on the next explicit retry after reconnect.

- `plak host` runs root and child process sessions on the home-local runner:
  jobs survive a host crash or signal handoff, the next host sweeps and
  re-adopts their lines before recovering its journal, and recorded stops
  finish before it serves. Starts carry that host's captured login-shell
  PATH and HOME; files, bindings and embedded-mode exit behavior are unchanged.
  Daemon teardown races do not strand startup, including an obsolete daemon
  closing before its inspection Hello. An idle handoff does not revive a
  cancelled turn from a previous host.

- The TUI moves a focus marker through transcript items with `Alt+Up` and
  `Alt+Down` (then `Up` and `Down`). `Enter` or `Space` folds the focused
  block, `Alt+C` copies that item, `Esc` leaves the marker without stopping a
  turn, and typing returns to the composer.
- The TUI speaks wire v2 to its session: each round's supervisor serves a
  local-face connection of the session's host service over a socket pair
  in the process, and the TUI is a `plakous_host_protocol::client` of it.
  Only two process-local signals stay beside the frames: the
  abandon-and-release request on the session's mailbox, set once the
  session has its verdict on everything the TUI sent, and the launcher's
  `/new` and resume publication cells. The in-process face is gone. The
  TUI's sends never wait for the session: a refusal comes back later, and
  the TUI restores the draft and names the reason (the role-table row of a
  role refusal, or that no answer came and the session may still run it).
  A round whose connection fails releases its session and joins it.
- A tree's genesis roster reads the session's own workspace: a trusted
  project's templates in its `[roster] working_set` are on the root's spawn
  menu. Before, the roster read the home alone, so a trusted project's
  templates never reached a tree.
- `plak host` (hidden, maintainer builds): the per-user host process,
  serving every session of the home. One per home and state key
  (`state/<key>/host.lock`, a second one refuses naming the first); a control
  socket `host/run/<key>.sock` for this user only, whose clients use the wire
  v2 session directory: they list and watch the home's sessions, create one
  in any directory with `new_session` (a retry of the same request id
  answers the same session), and attach any, which opens a dormant one in its
  own workspace. An idle session releases after 300 s. A row names a tree's
  other custody holder only when it is not proven gone, so a crashed host's
  or TUI's stale custody reads free. Its custody identity and the login
  shell's `PATH` are captured once at start (not yet given to its sessions'
  tools); a `SIGTERM` hands every resident session off as a planned handoff
  and stops it within 60 s, and the next host's start scan opens each
  session whose continuation runs by itself (a handoff, a pending decision,
  a held loss under `resume_after_loss = "continue"`), so it continues with
  no client attached. `release_session` detaches every attachment of a
  session and releases it now unless something else keeps it resident, so a
  desk TUI can take it; a session already opening or releasing settles
  first, so `released` means another face can open it. `stop_session` stops
  the session the connection is attached to, as on a connection that serves
  one session, and is answered even when that attachment ends first. A
  session another live process holds (its tree's
  custody, or its runtime lease's holder record for any session) is named in
  its row, and its attach is refused `tree_hosted_elsewhere` naming that
  process; a working face whose open is refused for a lease another live
  process holds (the host included) is refused the same way, typed, naming
  it, and told how to release a host's, while a holder whose lease lapsed
  is still taken over. A stopping host opens and creates nothing more.
  A session no face has opened yet (the host's `new_session`) opens in any
  working face first. Spec 02 §4.6 carries the systemd unit.
- `plak host --web`: the host's sessions' wire v2 over
  a WebSocket at `/wire`, and a web UI's static build, on a private unix
  socket that `tailscale serve --bg unix:<path>` publishes on the tailnet,
  from a tagged node. Access is the tailnet login Serve stamps, checked
  against the new home-scoped `[web] allow`; Serve's forwarded shape for the
  exact `[web] origin` from a source that is not the serving machine; and on
  the upgrade the exact `Origin` and an HttpOnly `__Host-` session cookie,
  bound to the login and origin and signed with a key in `secrets/web/`. A
  loopback listener is dev mode and needs `--dev-login`. Each connection is a
  client of the host's session directory, as one on the control socket is;
  one `/wire` serves every session, chosen by id. There is no `plak web`.
  Spec 04 §2.10 states
  which check stops which attacker and the residual risks. A 0.0.3 build refuses a
  `config.toml` that sets `[web]`, so `plak` writes it only when you set it,
  and `plak doctor` lists it with the other 0.0.4-only keys.
- `plak wire schema` (hidden) prints this build's wire v2 JSON Schema with its
  protocol version.
- Wire v2 is protocol 4: a welcome names its session as `session`, and a host
  of many sessions names none. The session directory's requests
  (`list_sessions`, `watch_sessions`, `unwatch_sessions`, `new_session`,
  `release_session`), their answers, the `directory` and `detached` host
  frames and the `unknown_session` and `tree_hosted_elsewhere` refusals are
  new; a connection that serves one session refuses the requests
  `unsupported`. A protocol 3 client is refused `version_mismatch`. The typed
  client's `connect` refuses a welcome that names no session
  (`ConnectError::NoSession`), and `connect_to` attaches a root the caller
  names from the directory.
- The wire v2 connection moved into `plakous-hosted`
  (`host_service::wire::WireConnection`), shared by `plak agent --wire v2` and
  `plak host`; `WireSession` serves one session to many connections. A switch
  ends every attachment of a shared service (`HostService::move_attachments`,
  which queues the new `Outbound::Moved`). Embedder break: the new
  `Outbound` variant.
- Supply chain: `tokio-tungstenite` and `tungstenite` 0.29.0 (web endpoint,
  maintainer builds only); `plakous-host-protocol` now depends on `schemars`,
  which was already locked, so it reaches the minimal and headless closures.

- `/fork` makes a new node in an agent tree of its own. The fork's tree starts
  from the source tree's current epoch and carries none of its children, mail,
  workspaces or candidates. A fork is refused while the source's tree has a
  live sub-agent or a spawn in progress (`cannot fork while this session's
  sub-agents run`).
- Agent tree custody: one live `plak` process holds a tree, and every tree
  write proves that claim. An attach, or a later tree write, while another live
  process holds the tree is refused with `tree_hosted_elsewhere (<build>)`
  naming that process's host, machine and instance: exit code 7, and in `plak
  agent` a JSONL `error` with `code: "tree_hosted_elsewhere"`. A holder whose
  exit cannot be proven (another machine sharing the home) is released only by
  `plak tree release --force <tree-id>`, which refuses a holder still running
  on this machine.
- `plak runner`: a persistent runner daemon per machine user (`serve`,
  `connect --stdio`, which a host runs over ssh with fixed strict options and
  which starts the daemon when none answers), host pairing through an
  allowlist in `<home>/runner/runner.toml` (`allow`, `pair <name>`), and
  `ps` for the lines and jobs a runner holds, orphaned ones marked. Runners
  keep each line's newest owner across restarts, journal a host's keyed calls
  so a returning host replays them, and let the same host re-adopt its lines
  after a lost link. The ssh link requires a known host key
  (`StrictHostKeyChecking=yes`), a runner refuses roots in or around its
  product home, and a host's `ps <name>` sees only its own lines. No session
  is placed on a runner yet. `connect --stdio` delivers every frame the daemon
  sent, so a refused bind reaches the host as a refusal, and then ends at once.
  `serve`, `connect` and `ps` refuse a socket path past the platform's
  `sockaddr_un` limit by name before doing anything else.
- MCP servers can run on the node's runner: `[mcp_servers.<id>] placement =
  "runner"` (stdio only) starts the server there, one per runner, workspace
  and definition, with its `secret_env` names resolved from the runner's own
  secrets (`plak runner secret set|rm|list`); host secret values never cross
  to a runner. Without a remote runner the server runs here and reads this
  home's runner secrets. Sessions freeze the placement each server ran under.
  `plak doctor` adds a `runner_ssh` row per `[runners.<name>]` from `ssh -G`.
- Claude `Read` returns PNG, JPEG, GIF and WebP as an image when the model
  accepts image input. Codex Direct adds `view_image` for one workspace image.
  Other packs stay text. A model that cannot accept the image is refused with
  the route and model named. New sessions record a registry descriptor that
  includes `view_image`; no schema revision or state-key change. Codex native
  Code Mode does not offer `view_image`.
- The Agents tab's profile dialog offers **Switch now**: this session's root
  continues as the profile on its own line while its sub-agents and background
  processes keep running; future root spawns use the profile's cards and every
  running sub-agent keeps its own. It needs an idle root and an empty queue.
  **Fork now** is unchanged and now comes second.
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
  the reopen is answered after it.
- A sub-agent a lost host interrupted is held for your Resume: `r` on it in
  the Agents tab, or JSONL `resume` (optionally with `agent_id`), records your
  consent and continues it once with a new instruction turn that never
  replays the lost one. `[host] resume_after_loss = "continue"` now takes
  effect for sub-agents: each held one continues once by itself. Tree rows
  carry the interrupt `cause` and the `continuation` state (`held`, `pending`,
  `running`), and a live tree notice names the held agents after recovery.
- A root turn a lost host interrupted is held the same way: the root stays
  promptable (a new prompt ends the hold), and the one Resume (`Ctrl+R` on the
  paused queue, `/recover`, or JSONL `resume`/`recover`) continues it once with
  a new instruction turn, resumes every held agent and lets the restored queue
  drain. `resume_after_loss = "continue"` does that by itself at open.
- A planned handoff (`/reload`) records each node with work in flight as
  handed off before its host releases it, and the next host continues each
  once by itself, whatever `resume_after_loss` says; a handoff it could not
  record holds like a loss. A handoff waits up to 5 s for the running turn to
  settle (a quit waits 300 ms); a turn still running then is released as a
  held loss, with a warning rather than an error. `/reload` no longer refuses while a turn or a
  sub-agent runs: it hands them off, and the new build continues each once. It
  still refuses while a background process runs, and a reload that carries
  into a new session still refuses a running turn or sub-agent. The installed
  build cannot check a session whose turn is still running, so it is asked
  again once the handoff settled it; if it then would not reopen the session
  exactly, or cannot say, the session stays open in this build, which continues the
  handed-off work once. Interrupts now
  record why (`operator`, `parent`, `cascade`, `stopped`), and tree rows carry
  it. A stop is final: stopping a running root turn is recorded before its
  cancel, so a host lost before the turn ended never continues it, and
  `/stop-all` ends a root continuation held after a lost host; an idle
  interrupt (`Esc`) keeps it.
- Reopening a session no longer pauses the agent tree. A sub-agent whose turn
  lost its owner is interrupted (owner lost) on reopen, keeps its attempt and
  waits for mail or a followup; a lost turn the reopen finds still waiting
  (at an approval, admitted but not yet sent, or after a provider's server
  pause it already recorded) is closed as abandoned,
  never rejoined, and a pending wake's mail returns unread for the
  continuation. Mail sent to a sub-agent still being reopened
  waits as for a busy slot and stays durable until its recovery settles. `--resume-tree`, the TUI resume
  row and the `Resume agents first` copy are gone, and the background
  recovery pass no longer holds a tree-wide lock.
- Recovery is Plak's job, not the model's: `recover_agent` leaves every
  agent's tools. `list_agents` now says the host is recovering a child whose
  session recovery is pending, with no remedy to call. You retry one child
  with `u` in the Agents tab or, from a JSONL client, `invoke_tool` with
  `recover_agent`, which stays an operator-only name.
- `/reload` with a sub-agent whose request ignores its cancel no longer drops
  that sub-agent into a new session: it is handed off and the new build
  continues it once. The release names it instead of failing.
- After a lost host, a parent's `wait_agent` rows say why a child stopped
  (`cause`: `owner_lost`, `handoff`, `operator`, `parent`, `cascade` or
  `stopped`) and where its continuation stands (`continuation`: `held`,
  `pending` or `running`). The open sends one warning listing everything the
  lost host left held, the root's turn and each held agent with what it was
  doing, instead of one for the root and another counting the agents.
- After a lost host, a parent's next turn is told which of its sub-agents the
  loss interrupted, once per host run, in a `[harness]` note after its
  prompt and mail: each agent, and whether it waits for your resume or
  continues by itself. A handed-off agent is not named.
- `/reload` onto a build without an install manifest (a development or
  cargo-installed build) no longer falls back to an unverified resume on a
  busy machine; the reload probe answers without hashing the build.
- An explicit `recover` with nothing pending answers before the next command
  is handled, so a quit sent right after it no longer drops the answer. Urgent
  actions sent during an in-place root recovery are answered in the order they
  arrived.
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
- A carried session no longer flashes `invalid transcript presentation` above
  the chat after a send, a steer or a turn end. An imported row and a later
  tree result that name the same `child-result:` call stay two rows in key
  order; the result used to overwrite the imported row and break the view's
  key order. A repaired presentation now keeps one error notice per cause in
  that transcript and logs the cause to `plak.log`, and the TUI's note for a
  refused snapshot survives the resync that answers it.
- A reload request (`plak session reload`) belongs to its session. An in-place
  switch to another session (`/new`, `/resume`, a fork or a rebind) settles
  the requests the host holds for the session it leaves as `not taken: the
  session switched` before it completes; before, one the TUI had already taken
  could run `/reload` on the session it switched to.
- Rows that share a call id are kept apart everywhere: importing a document
  keeps every row, and a new tree result that reuses an imported result's call
  id shows as its own row while the turn runs, in the host and in the TUI,
  instead of briefly replacing the imported one. A carried session's first save
  keeps the notices recorded before it (such as the carried-forward note) as
  notices with their own identities, after the imported rows. A repair or
  refusal note stays where it happened in the transcript instead of trailing
  the newest turn.
- `Esc` right after `Enter`, before the turn shows as running, no longer
  stops the prompt silently. A sent prompt counts as busy for `Esc` and `Ctrl+C`
  until the host reports it busy, so either opens `Stop this turn?`; a host refusal before
  then closes the pop-up. A prompt stopped before its turn was admitted now
  ends with `⊘ interrupted` instead of a bare prompt row, and stays queued.

### Plakous

- Tree-store caches are scoped to each tree's revision and custody grant,
  with one shared writer per database file. Alternating resident trees no
  longer invalidate each other and continuously rewrite unchanged rows;
  stale baselines are rebuilt before writing row deltas.

- `plakous::sessions::holder::{sqlite_branch_holder, BranchHolder}` (feature
  `sessions-sqlite`): the process a runtime branch's holder record names,
  unless it is this process or its free lock proves it gone. Probes of
  another process's lock, there and in the gone-holder proof and tree
  custody's liveness, are serialized within the process and retried
  briefly, so a dead holder no longer reads alive under concurrent probes.
- A long turn no longer keeps every checkpoint it commits: between two model
  rounds the owner prunes superseded history at its Working head under the
  same 64-checkpoint minimum as the idle sweep, and that prune also keeps the
  head and the 64 checkpoints before it. A request whose horizon is at or
  below the one the branch already records removes nothing. A 200-round turn retains about 130 checkpoints
  instead of about 1,400. A reader that lags more than 64 commits inside the
  turn reads the head and shows `earlier output not retained`. Nothing is
  pruned while a compaction workflow is between its identity commit and its
  receipt. `PruneBranch::at_working_head` is the owner's request for it; an
  ordinary `PruneBranch::new` under a Working head is still
  `Invalid(PruneHeadNotQuiescent)`, and a released Working head is never
  pruned.
- Retention keeps the 4 most recent root terminal results (was 16): a
  duplicate of an older root command is `ControlErrorKind::Reclaimed`, and a
  bridge must settle a handed-off item within 4 further root turns.
- `TurnControl::abandon(cause)` cancels a turn because its owner was lost
  or let it go. The terminal is still `Cancelled`, the record carries an
  abandonment report (the same claim and note a reopen would write for that
  turn), and history gets the abandonment note instead of "the user
  cancelled the request above". A plain `cancel` is unchanged. If the report
  cannot be built, the turn still stops and `abandon` says so.
  A restored hosted tool-batch, or a server pause the lost owner never
  promoted, is closed that way: one terminal, and the baton is finalized
  in that same commit. The next host uses it when it does not continue a
  restored child turn (`ProcessLost`, or `Interrupted` after a handoff).
- An agent whose jobs run on a runner reads the line's jobs from the runner
  before it restores them and reports nothing until the restore is done, so
  a terminal an earlier host already reported is not reported again and a job
  the runner still lists is that live job. A runner that does not answer that
  read fails the agent's construction (`runner <id> did not answer the job
  read`). Embedder break: `JoinedSessionsPort::refresh_jobs` returns a
  `Result`.
- `plakous_hosted::host_service::HostService`: one session's wire v2 host
  side: attachments on the session's root, observations fanned out by class
  with addressed answers routed by the intent's origin attachment, an
  attach snapshot of every state, the root baseline, pending approvals and
  the holding credential wait, and bounded per-attachment queues that drop
  a slow attachment with `resync_required` instead of blocking the session.
  The TUI and `plak agent` JSONL v1 are its in-process faces
  (`host_service::face`, `FaceReceiver`): their intents go out on the
  session's own mailbox sender tagged with their attachment, and their
  observations pass through the service, so they receive exactly what they
  did before. A publisher answering after the host moved on
  (`Sender::answering`) reopens its requester's `IntentStarted` window
  around the answer. Embedder break: `IntentSender::tagged` and
  `IntentDelivery::correlated`; an intent's origin is part of the
  `IntentStarted` correlation, and only a client's own correlation makes an
  intent a command reply. In `plak agent` JSONL v1, a late answer (a
  profile apply's finish, a usage refresh) now carries the command id of
  the command it answers rather than the one running when it arrived.
- Wire v2 node attach: a host service attachment may target a child node of
  the session's tree (`HostService::with_nodes`, `ChildNodes`, `TreeNodes`).
  It holds that node's transcript only, and its intents are the node's
  controls: tree mail from the operator for prompts, follow-ups and steers
  (the new `SessionIntent::MailTopologyNode`), the tree's interrupt with
  the operator as its cause, its close and call detail as the node; a quit
  detaches it. It gets back only the answers its
  control's publisher marks (`Sender::answering` marks its windows
  `answer:`); the session serves one agent view, so a second agent's is
  refused while one is held. Approvals and tool calls are refused there for
  now. Operator mail is the root's send, reaching the root's message peers;
  its request is answered once the mail is written (`draft_accepted`, or
  `mail_refused`). A closed, retiring, out-of-reach or unknown node is
  refused with `node_closed`, `node_retiring`, `out_of_reach` or
  `unknown_node` (`RequestRefusal` gains all but the last, and
  `mail_refused`). The navigator's interrupt now records the operator as
  its cause (`interrupt_agent_for`). Embedder break: the new intent and
  refusals, an answering sender's marked windows, the required
  `ChildNodes::interrupt` with its `ControlDone` answer, and
  `ProductTopologyRuntime::executor`.
- Child processes and usage by node address: a child attachment's
  `background`, process stop and usage read become
  `SessionIntent::BackgroundTopologyNode`, `StopTopologyNodeProcesses` and
  `ReadTopologyNodeUsage`. Background reaches the node's running turn
  through its own control; a stop reaches only the processes the node's
  lines attached as that node; the usage view carries the node's own
  session ledger, kept from the child turn usage the tree projects. The
  intent x role table's B1 cells close. Embedder break: the new intents
  and `ProtocolExtension::reads_usage` (defaults to false).
- Embedder break: `plakous::sessions` (session workflows, the feed, portable
  histories and `SessionBackend::memory`) now needs the new facade feature
  `store`, and without it the facade links no session store.
  `sessions-sqlite` implies `store`, so an embedder that already selects it
  changes nothing; one that uses sessions with only the memory backend adds
  `store`. `plakous-store`'s `nix` and `rustix` are now `cfg(unix)` target
  dependencies, so its memory backend also builds for `wasm32-unknown-unknown`.
- An agent runs on a host `Runtime`: `AgentBuilder::runtime` supplies it, and
  without one the agent runs, as before, on the Tokio runtime current at
  build. The driver, the lease heartbeat, job-stop and cleanup blocking calls,
  every sleep and store or model timeout, and every monotonic reading go
  through it. `AgentBuilder::clock` replaces the wall clock alone.
  `Runtime::spawn_blocking` is new, with a default that runs the work as a
  task; `TokioRuntime` now lives in `plakous-agent`, still at
  `plakous::runtime::TokioRuntime`, and runs it on Tokio's blocking pool. The
  tool runtime keeps its Tokio-backed `ToolTimer` and `ToolTaskOwner`
  defaults.
- One drive per child turn: the child's turn waiter, run by its actor,
  reduces the turn's events into the read model every face reads beside the
  turn's result, so a follow-up admitted mid-turn no longer runs a reducer
  of its own. A wire v2 child attachment's interrupt now runs on the tree
  (`ProductTopologyRuntime::executor`), not behind the root session's
  mailbox, and is taken whatever the root runs.
- The TUI is an attachment of its session's host service: a face's
  intents pass through the service (`host_service::FaceSender`,
  `HostService::attach_face`), so its agent view is opened, held and
  released there as any attachment's and its queue holds the root and the
  agent transcript it opened. The faces' service holds the session's
  mailbox without being one of its senders (`HostService::for_faces`,
  `IntentSender::downgrade`, `WeakIntentSender`), so the session still ends when its last face sender
  goes, and it keeps the attach snapshot for later attachments. Embedder
  break: `face` returns a `FaceSender`, `IntentSendError::Refused` (an
  intent the service refused for the attachment's role; `plak agent` JSONL
  v1 answers it as that command's `error` record), `HostService` is
  `Clone` (its clones are the same service), and the new types.
- A wire v2 connection is a local or a remote face
  (`host_service::wire::Face`, `WireConnection::face`,
  `WireSession::connect` takes it); a remote face, the web endpoint's, is
  refused the local-face-only intents (`ProtocolExtension::local_only`:
  Plak's installed-build check and probe and the reload-request
  handover) as `refused_for_role` `local_face`. Embedder break: the
  `connect` signature and the new trait method (defaults to false).
- A remote face's attachment is never delivered, nor replayed on attach,
  an observation only a local face may receive
  (`ObservationExtension::local_only`: Plak's build status, reload
  probe answers, handed-over reload requests and lookup progress).
  Embedder break: `HostService::attach` takes the connection's `Face`.
- A wire v2 attach of the root a connection last knew, of a session that
  has since switched, is refused `moved` with the served session's root
  (`RequestRefusal::Moved`), not `unknown_node`: a connection welcomed
  before a switch learns where the session went.
- Wire v2 quit and stop: a `quit` intent on a root attachment of `plak host`
  (a host whose connections detach) detaches that attachment, answered
  `detached`, and the session runs on; on `plak agent --wire v2` it still
  quits the session. A child attachment's quit-as-detach now answers
  `detached` too, not `accepted`. The new `stop_session {session}` op
  (`Op::StopSession`, answered `stopped`, `Answer::Stopped`) stops the
  served session's work from any face: child wakes are suppressed until
  the next prompt's admission, running and held sub-agents are interrupted
  `stopped` (signalled, not joined, while a root turn is running), background
  processes stop, and the running root turn is then cancelled with
  `interrupt{stopped}` recorded before the cancel unless that turn already
  finished. A held or pending root continuation is recorded stopped last,
  so nothing continues by itself. A later handoff skips a stopped child
  only while that attempt has no later turn. It is answered only once that is durable; a session the host does
  not serve is refused `unknown_session` (`RequestRefusal::UnknownSession`),
  and each stop is logged with the connection's face and login. A dormant
  session is woken for it. In process the stop is
  `IntentSender::request_stop`, a host-only `SessionIntent::StopSession`
  that never crosses the wire, settled through `IntentReceiver::settle_stop`.
  The wire version is unchanged. Embedder break: the new `Op`, `Answer`,
  `RequestRefusal` and `SessionIntent` variants.
- Wire v2 session requests: `retry_attach`, `resume_session` and `rebind`
  ops (`Op::RetryAttach`, `Op::ResumeSession`, `Op::Rebind`), each naming
  its requester attachment and a client `request_id`, answered `moved`
  with the new root (`Answer::Moved`) or refused `session_unavailable`
  (`RequestRefusal::SessionUnavailable`). Every `moved` frame a session
  request causes names its `request_id`. The session host serves
  `resume_session`; `retry_attach` and `rebind` are refused there until a
  launcher serves them. A transcript the session's observation mailbox
  lost ends its wire holders with `resync_required` once per loss.
  Embedder break: `HostService::move_attachments` takes the request id and
  `Outbound::Moved` carries it.
- `plakous_host_protocol::client` is a typed wire v2 client over any byte
  transport: hello and welcome, the root attach, `moved` and
  `resync_required` followed on its own, intents refused as the in-process
  mailbox refuses them, session requests and their answers, and the
  session's observations in the in-process receiver's shape. Plak's
  `wire_transport::host_socket` connects it to the host's socket only in a
  directory private to the user. Wire fix: `ContextUsage::display_percent`
  is a `u64` (was `u128`, which no wire v2 reader could decode inside a
  tagged frame); embedder break for its readers.
- The session's background processes and the face's promptable mark are
  wire v2 traffic: the state observation `processes {views, running}`
  (`SessionObservation::Processes`), published when the rows change (a
  clear follows its `clear_view` with an empty list), each command and
  output tail redacted as a tool row's are, and
  the intent `promptable` (`SessionIntent::Promptable`), which sets the
  passive-maintenance flag without entering the queue. Embedder break:
  the mailbox mirror is gone (`IntentSender::processes`,
  `background_processes`, `mark_promptable`; `IntentReceiver::set_processes`,
  `set_background_processes`; the `FaceSender` forwards).
- The TUI's last direct product reads move behind the protocol: the
  reload target is the session host's `probe_reload` (answered by
  `reload_probed` with the target and the host build's refusal, or
  `reload_probe_failed`), and pre-bind lookup progress is the launch
  supervisor's `lookup_status` publication to the face on screen
  (`host_service::FacePublisher`, `FaceReceiver::publisher`).
- The interactive launch's round loop is the session supervisor
  (`tui_launch::supervisor`): it settles each round, applies the TUI's
  session requests (`SessionRequest`: retry attach, edit config, rebind,
  resume a session, reload release; `TuiOutcome` is now `Exited` or
  `Request`) and owns the launcher cells and the abandon. A wire v2
  connection whose session switches to another now sends `moved
  {attachment, node}` and keeps the connection for a fresh attach of the
  new root (`HostFrame::Moved`, `HostService::reroot`,
  `host_service::wire::session_root_of`; `WireConnection::serving` takes a
  root resolver). Embedder break: the new frame and the `serving`
  signature.
- Wire v2 for an embedded face: `HostService::publisher` and
  `WireConnection::publisher` publish to a service from outside its
  session (`FaceReceiver::publisher` is now that of its service), waking
  its connection; a connection whose session ends on its own sends `bye`
  while its client still reads; and the client's first attach reads no
  transcript as lost (it no longer starts with the root marked as needing
  a baseline). `WireConnection::serve`, `handshake` and the stdio and web
  acceptors take `wire::Lines` (`wire::lines`), one channel of the client's
  lines and the session mailbox's wakes, so a connection forwards what the
  session says at once instead of on a 20 ms poll.
  `WireConnection::session_baseline_follows` and
  `HostService::attach_asking` let an embedded face's first attach skip
  asking for a root baseline the session publishes anyway.
- `plakous_host_protocol::client`: `WireSender::send` no longer blocks;
  refusals and unknown outcomes come back through
  `WireReceiver::try_unsent`, and `WireSender::settle` waits for every
  verdict. `SendError::RefusedForRole` names the role-table row. What a
  face has not taken waits in an observation mailbox with the session
  mailbox's rules instead of a 4096-entry queue that dropped everything;
  a re-attach reads every held transcript as lost and opens the face's
  agent transcript again. Wire break: `hello` has no `locality`
  (`ClientLocality` is gone), and `WIRE_VERSION` is 3, so a peer of the
  previous version is refused `version_mismatch`.
- The TUI's installed-build check and its reload-request channel move to
  the session's host: every five seconds the TUI sends the product
  surface's `check_build`, and the host answers whether a newer build is
  installed (`build_status`), announces the session it serves on the
  reload-request channel and offers a waiting request (`reload_requested`)
  until the TUI acknowledges it (`accept_reload_request`); the TUI hands
  refusals back (`refuse_reload_request`) for the host to record, and the
  host refuses `not taken` a request no face took. One check is outstanding
  at a time. Only a session's UUID names a file in the channel. The
  announcement now lives with the session's host.
- Wire v2 handshake: `welcome` names the session id, tree id and root node,
  and the session opens before it is sent, so a failure before dispatch
  (`plak agent --wire v2` included) is a `refused` frame with the new
  `unavailable` refusal, never a JSONL record. A host service never reuses
  an attachment id, each attachment's sequence starts at 1, and no event
  precedes its `attached` answer. A client that sends no hello within 10
  seconds is refused, a terminating signal quits and joins the session, and
  a connection ends when its session switches to another. The shared
  connection code gains `host_service::wire::{session_root,
  session_welcome, refuse}`, `HELLO_DEADLINE`, `UNAVAILABLE_UNAUTHENTICATED`
  and `WireConnection::{with_stop, serving}`; `handshake` takes a stop
  probe. Embedder
  break: `Welcome` gains `session_id`, `tree_id` and `root`, and
  `HandshakeRefusal` gains `Unavailable`.
- Version control on a runner (unsupported; only the test route selects a
  runner): an orchestration tree on a selected runner drives its copies
  through `RemoteVcs` over the runner link instead of refusing every effect.
  Questions are `vcs.read` calls; every effect is a `vcs.effect` step of a
  tree operation (protocol class `TreeOperation`, keyed by tree and
  operation), recorded first as a `vcs_stage` tree intent and run at most
  once per key. The root and its children record the runner they run on; an
  `inherit` child on another runner is `runner_mismatch` and a copy on another
  runner `cross_runner_copy_unavailable`. Embedder break: `RunnerClass` is no
  longer `Copy` (`RunnerCall::class` returns a reference), and the VCS port's
  value types serialize. A stage left unfinished by a lost process reruns
  under its first keys, so a rerun never runs beside the step it repeats;
  the extension refuses a relative path or one with `..` before admission;
  a root stage of unknown outcome records a `vcs_fault`.
- Copies that cross runners (library only; no spawn argument names a runner
  yet): `spawn_reserve_on` places a `fork` or `snapshot` child on another
  runner the tree's port reaches (`routed_vcs::RoutedVcs`). Its parent's live
  state is pinned as a base snapshot, bundled leaving out what the target
  holds (`tree_runner_ancestors`, persisted per runner), relayed by the host
  in 1 MiB chunks it does not keep, and applied into the tree's mirror on the
  target, where the copy is minted; its candidate comes back by the same
  stages into the parent's checkout, the apply recorded as verifiable with
  the candidate reference and commit it leaves. `VcsOps` gains the transfer
  calls; `vcs_runner_extension` takes a copies root. Embedder break:
  `vcs_runner_extension` takes a second argument and `StageLedger` adds the
  stage record a carry runs through. `plak runner serve` registers the
  extension, admitting paths under `roots` and the new `runner.toml` `copies`
  root and never the product home; `copies` may not lie inside or around a
  root. Mirrors are keyed by runner (`<copies>/<runner id>/<tree id>`). A
  seal-back never moves a candidate reference that names another commit: it
  records a `vcs_fault` instead. A failed apply leaves no upload, and the
  reclamation pass sweeps transfer files older than a day.
- Wire v2 types in `plakous-host-protocol` (no frontend speaks it yet;
  JSONL v1 is unchanged): every observation has a class
  (`SessionObservation::class`: broadcast state, required or droppable
  event, or addressed to one attachment); intents and observations
  serialize as tagged objects that refuse unknown fields, decoding a
  snapshot or patch only when it validates and image bytes as base64; and
  the `wire` module holds the NDJSON frames (16 MiB lines), the
  exact-version handshake, requests with one typed response each, blob
  chunks for images and paged snapshots, with bounded blob and snapshot
  assembly and the event class derived at the receiver. The hidden
  `plak agent --wire v2` speaks it on stdio over the session's mailbox (root
  only, unsupported). Embedder break:
  `ObservationExtension` gains `wire_class` (with a default) and the
  protocol's view types serialize.
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

- Live sub-agents run on the same node actor as the root: one main loop and
  one turn loop serve both roles, the hub's follow-up, restart, release and
  cancel are messages to the child's actor, and a child's approval is decided
  once by its own actor (`LiveChildHandle::decide`). Behavior is unchanged.
- Runner protocol v1 (`plakous-runner-protocol`) and the runner library
  (`plakous-runner`): framed, versioned host-runner messages with line-keyed
  owner fences, at-most-once effect calls, process sessions, streams and
  liveness, behind an unsupported, hidden runner seam. Sessions select the
  local runner; a non-local runner is reachable only from tests.

### Security

- A background job's command is redacted for display with the runtime's
  escaped newlines undone, so a token a heredoc writes on its own line is
  redacted (process views, live and stored process transcript rows, process
  tool rows and the run's waiting notice, which showed the command
  unredacted), and so are wire v2's `job_started` command and
  `job_terminal` output for every face (JSONL v1 keeps them as the runtime
  displays them). A job's output tail now starts at a line's start, or with
  `…` (`plakous::tools::JOB_TAIL_FRAGMENT`) when it was cut inside its only
  line, which a face shows as the mark alone; a private key whose BEGIN the
  tail lost is redacted up to its END. The display redactor also redacts a
  private key without its END, to the end of the text. Every read of a
  stored transcript bounds its projector rows again, so rows an older build
  stored come out redacted too; the state key stays `r27.e35.p36.t10`.
- pulldown-cmark is 0.13.6 (RUSTSEC-2026-0433, quadratic parsing of nested
  image wikilinks). Plak never enables wikilinks, so it was not exposed.
- The build audit also waives RUSTSEC-2026-0429 for zip 8.6.0. Like
  RUSTSEC-2026-0367, -0368 and -0369 it concerns only `ZipArchive::extract`,
  which the Monty type checker's in-memory typeshed reader never calls.

### Development

- A traced gate build's SQLite actor runs deferred reclamation right after the
  job that queued it, so a sweep with `PLAK_TEST_WORKING_PRUNE_MINIMUM=1`
  and `PLAK_TEST_WORKING_PRUNE_MARGIN=1` (test-hooks knobs that prune between
  every pair of rounds, the margin one lowering the recency floor) keeps one
  commit order per twin pilot.
- A test's wall-clock wait that only guards against a hang is thirty seconds;
  a bound that tells prompt from late runs in the serial lane, on a paused
  clock or event-driven (spec 05 §2.3).
- The strict crash sweep covers a job on a child runner surviving host loss
  and a runner restart that loses its jobs; each fixture reaps every daemon of
  its runner home.
- The strict crash sweep runs its shapes on one shared worker pool and
  refreshes its progress tables every 30 seconds instead of after every cut,
  which makes it about a third faster; `timings.jsonl` records each task.
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
- `plakous-tools` joins the portable set. Its process, PTY and descriptor
  dependencies are `cfg(unix)` target dependencies; on `wasm32-unknown-unknown`
  (not a supported target) the library compiles without a host, so host
  environments, workspace files and job output files refuse there.
- `plakous-agent` joins the portable set; on `wasm32-unknown-unknown` only it
  enables `getrandom`'s `wasm_js` (WebCrypto) backend.
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
  through the same exact recovery path as the JSONL operator's
  `recover_agent`. A paused agent tree
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
