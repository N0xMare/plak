# Testing and evidence

This chapter states what the repository verifies, what it deliberately does not
verify, and what each kind of evidence is allowed to claim. Two ideas run
through all of it.

The first is that **evidence binds an exact tuple**. A passing run qualifies the
exact source, binary, host, procedure, account, provider, model, effort,
composition, and protocol it named — nothing adjacent. The second is that
**reachability is not qualification**. A model that appears in a catalog, a
symbol that is nameable, a command that parses, and a route that renders are all
accessibility facts. None of them is a support or correctness promise.

Direct Anthropic API refusal qualification follows Anthropic's
[refusals and fallback documentation](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback),
which treats partial refused output as incomplete and instructs consumers to
discard it. Its [streaming-refusal guidance](https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals)
also warns that unchanged context may produce another refusal. These statements
do not qualify an unchanged-context task retry or authorize automatic fallback.
Claude OAuth qualification instead requires the pinned native interactive
Claude Code client and route-specific fixtures; API documentation alone is
not authority for subscription behavior.
The Sonnet 5.5 Platform model page and migration guide alone establish no
Claude OAuth enrollment. The pinned Claude Code 2.1.284 macOS arm64
executable (SHA-256
`50a14c2f50f56668380fdda490167f1d3630d5cc18fb8aed3073c2c7ea7314fe`)
lists the model and low–max effort. Synthetic interactive low, medium and max
request captures establish the first-turn effort shape; short authenticated
interactive turns at those efforts reached a response. The server does not echo
the effective effort in these observations. Multi-turn thinking replay and
native PTC remain unqualified.

The [tool-input accumulation contract](https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming)
requires a physical block stop but warns that accumulated JSON can be incomplete.
Refusal tests therefore cover wire-closed malformed/truncated client inputs;
no evidence here permits terminal metadata before a physical block stop.

Offline refusal regressions cover closed client-tool proposals, text and refusal
blocks across strict Messages, Claude OAuth and OpenRouter stream endings. They
require empty canonical refusal output, retained usage/identity/stop details,
no local dispatch, no invented results, and durable reopen followed by a new
explicit turn without replaying discarded blocks. Physically open Messages blocks, open non-client canonical blocks, non-refusal
output mismatches, identity/usage contradictions, and possible hosted effects
remain rejected. The canonical assembler also rejects revocation of `Pending`,
`Completed`, and `Failed` tool calls for both local and MCP ownership, while
allowing their `Requested` proposals and rejecting every provider-hosted state.
A live refusal demonstrates terminal handling, never successful
completion of the requested file work; a live scenario must check file content
independently of a clean protocol terminal.

Offline idle-continuation evidence covers notices without a tree runtime,
content-free trigger receipts, `turn.auto_continue` home-only immediate policy,
obsolete-key rejection, Esc suppression, and RFC 3339 UTC notice envelopes.

Run-until-idle regressions cover continuation admission and terminal status,
turn-free waiting diagnostics, admitted-turn and timeout bounds, signal
interruption, and exactly-once stdout projection.
Spawned-binary offline loopback fixtures promote a real OS process, synchronize
on the exact stderr waiting line, release the process, and require a second
admitted turn with only its authoritative answer on stdout. Separate real-job
fixtures require max-turns and timeout cleanup with exit 6 and a real SIGINT
while waiting with exit 4; a TERM-trap marker proves the process was stopped.
Child-owned promoted-process fixtures cover timeout and SIGINT, interrupted
tree status, stopped process groups, durable terminal attribution and exact
child reopen, including a live attempt with a Quiescent runtime head. A real
js-ptc sidecar cell promotes nested `exec_command` and
requires `JobPromoted { nested: true }` telemetry. Process-watch regressions
cover output/exit wakeups and await notification waiting that leaves the
blocking pool available; child-live
attachment regressions enforce one-second retry spacing without snapshot floods.
Terminal observations with more than 256 unread records drain before their hint
deadline. A contended stop/poll on a one-worker Tokio runtime leaves another
async task responsive. Behavioural monitor tests require unobserved short-job
retirement and owner-drop wakeup even without terminal publication. Run cleanup
tests distinguish closed/backpressured child ingress from fatal publication
failures.
Preparation regressions require an expired deadline to precede authentication,
cancel pending preparation, forbid later request dispatch, and remain expired
when the hosted drive starts. All use bounded waits with gate-load slack.

### Process lifetime and recovery regressions

Memory and SQLite cleanup conformance advances lease time without waiting for
a heartbeat: exact expired grants release singly and in batches, including
idempotent repetition. Renewed, reacquired and foreign-domain grants refuse;
SQLite rolls back a stale batch without releasing its valid expired peer.

Offline orchestration regressions distinguish watched urgent mail from steer
wakes, require edge-triggered owned-process wakes without durable non-mail
receipts, reject foreign process ids, and refuse submission with live child
attempts or jobs. Attempt-end and close tests fence descendant work and process
stops before sealing; interrupt preserves promoted jobs. Adoption with running
processes succeeds with an explicit warning rather than a refusal.
Retirement regressions require unrelated settlement not to stop historical
attempt jobs, a job promoted after closure but before cancellation to stop with
`attempt ended`, and complete descendant and own-owner retirement despite one
release failure. Hosted run-bound tests use the child-runtime factory to admit
a real live child, trigger timeout without a wall-clock wait, join max-turns,
timeout and interruption cleanup, and reopen the released child canonically.
These hosted tests do not establish CLI signal delivery for a real live child;
the CLI real-child resume e2e remains separate evidence.
Offline background regressions cover v1 JSONL command admission, a background
edge waking an in-flight top-level dispatch without cancellation, later-batch
and next-turn reset, Ctrl+B foreground-only routing, and process-start-only
preemption (polls keep waiting). Job protocol fixtures check ordered promotion
and terminal transitions, repeated-snapshot suppression and uncorrelated v1
wire identity pins. Route schema fixtures accept oversized start hints before
lowering clamps them; no paid inference qualifies these checks. Full-ledger
regressions cover Background and Steer, including a command that outlives the
original foreground hint. Registry tests consume a terminal before the next host
poll, verify ordered lifecycle publication, retry a saturated observation channel,
and reserve terminal capacity before promotion. Channel fixtures protect job
lifecycle receipts from eviction and cap/coalesce child traffic with a gap. Child
fixtures pin stable selection beyond eight children and finished-tail reattachment.
The background driver fixture rendezvous on dispatch readiness rather than a
two-second wall-clock timeout. Neutral telemetry fixtures exercise adapter labels
without tool-name or argument-key classification in L0.

Hosted registry fixtures pin foreground-first same-branch transfer, branch
isolation, running-only transition refusals without side effects, and host-wide
stops across registered slices. A durable fast-exit
fixture checks attributed process stop, a history-unchanged terminal checkpoint,
and lease release in that order. Portable export refuses missing job-terminal
evidence without changing its document version. These tests are deterministic
and use local pipes, never provider inference or persisted PIDs.
Dedicated carry fixtures pin fork/rebind branch seeding and crash/recovery
release, including abandonment of a Working provider turn, with terminal rows
and the number counter preserved. A release receipt alone still yields an
unknown process outcome. Reload admission reads a process-local host count and
refuses without closing the terminal.
Terminal-mode regressions cover hosted-owner shutdown, direct attachment
release, and retirement of an interrupted child with a running job, then reopen
with a fresh registry to prove terminal evidence was committed before release.
Explicit transfer reopens the same branch with its live job and number intact.
Admission-gate fixtures refuse promotion after terminal release starts and fence
stale-owner cleanup after a replacement binds the retained service.
Quit-grace fixtures distinguish a settled interrupted terminal from an
unsettled 300 ms cancel, and check post-abandon exact-locator publication.
Cancel refusal text retains millisecond precision for subsecond deadlines.

The job release review regressions pin RecoveryRequired followed by an owner
transfer, stop and record failures that still release the exact lease, hidden
unmatched terminal snapshots, and registry pruning that retains live jobs and
pending notices. Notice envelope tests inject control characters and check
entity-aligned escaped tails; export refusal tests distinguish unsettled process
rows from unfinished turns. The real promoted-process quit/resume fixture also
asserts the one-second elapsed bound.

Interactive exit has spawned-PTY wall-clock assertions for idle quit,
mid-turn quit against a cancellation-deaf offline provider, `/reload`, and the
real-process quit/resume fixtures (TUI, JSONL and promoted job).
Local launch measures first-frame and typing budgets separately for fresh and
resumed sessions. These and the hosted real-root/two-child batched-release
regression run in the serial budget lane. The hosted test checks one SQLite
mutation, released holders, stopped admission, aborted hub recovery and no
late actor publication; the store test explicitly rejects an old commit both
before and after a successor acquires the branch. Fault environment settings
in the offline launch fixture do not make paid transport calls.

`tui_local_launch_budget_new_and_resume_with_network_faults` keeps fixed
process-local limits of 200 ms to first frame and 500 ms to typing, and checks
visible key echo as well as both sessions' timing rows. Each scenario uses three
separate process launches, prints every sample, and asserts the minimum first
frame, ready-to-type, and frame-plus-key-echo measurements independently. This
best-of-three rule tolerates a transient host stall without accepting a
regression that makes every launch slow. The offline model prepares its host on the
same deferred path a provider launch takes (it is eligible on its explicit
binding, since no Ready catalog lists it), so the measured first frame excludes
host preparation exactly as a product launch does. The launch banner's
pure renderer test `the_banner_paints_while_binary_identity_is_pending` covers
the pending and completed executable identities without hashing in a draw.
`launch_phases_wait_for_the_background_telemetry_writer` pins deferred emission
with the original measurements, so a fast frame cannot lose its process-start
or frame timings before writer registration.
`an_exit_imported_before_writer_registration_is_emitted_on_registration` pins
the same buffer for the previous exit's timings, which an open imports at its
first frame, before host preparation registers the writer.

Job fixtures pin per-id joining and snapshot reads, cancellation detachment,
64-per-runtime and 128-per-host pre-spawn refusals, attributed stops, foreground
hint promotion, output-woken empty polls and non-PTY Ctrl-C. The durable driver
also pins steer delivery to the top-level executor without cancellation,
reset after the steering commit with a second normally dispatched batch in the
same turn, mid-response source-ordered superseded closes with durable
`ProvenNotDispatched` transitions and no `Dispatched` transition,
and a nested terminal read that preserves the unconsumed row through its
enclosing composition close. Projection fixtures distinguish empty-poll
5..300-second hints from nonempty-input 250 ms..30-second hints and verify
that zero selects the 250 ms first observation on every process vocabulary.
Cell wait contract fixtures pin the shared 600-second schema ceiling and
pre-validation hint clamp without changing cell-id normalization or invalid
hint shapes. Driver fixtures steer during the initial cell observation and
an outer wait, then resume the same un-joined cell to completion. A nested
mutation fixture proves steer leaves its preempt carrier unset, waits for
settlement and one accepted result delivery, then yields; the exact yielded
checkpoint is checked by the Code Cell abandonment fold. A process-local crash
snapshot reopens or reconciles that exact prefix without replaying the mutation.
Real nested process fixtures prove steer neither promotes a start nor shortens
an empty poll; a top-level read-only group fixture pins whole-group closes or
whole-group dispatch.
Mixed-mode fixtures keep a live cell's lineage marker while a top-level process
observes steer and a prepared top-level read-only group closes without dispatch.
Live and restored operator-approval fixtures prove an approval after steer
cannot execute the superseded effect. Full-row-ledger process fixtures prove
steer retains foreground ownership through normal terminal observation.
A virtual-time regression expires the outer observation deadline inside a
nested mutation and requires the later steer yield to remain successful rather
than cancelling and joining the cell. A nested read-only regression likewise
drains its running execution and delivery before yielding.
Grok 1.0.46 fixtures cover process-only multi-await under one deadline,
immediate and repeated output-so-far snapshots, terminal-only consumption,
steer without stop, idempotent stop, preserved natural exits, identifier
mapping and the 600-second foreground/one-hour await clamps. The trio's
schemas come from a sanitized Grok Build 1.0.46 native capture; draft,
format/default annotations and nullable optional hints are narrowed to the
strict portable schema subset, with the changed budgets described explicitly.
Claude Code 2.1.289's TaskStop takes a strict object with one string task id;
Plak presents that shape and does not expose the deprecated shell-id alias.
The output file is named by the pack-neutral `full output: <path>` line rather
than native `Output is being written to:` wording; tests pin that the Claude
`Bash` description names that line and `Read`, that the `Read` description
names the issued-path exception, its earlier-process refusal and the
write-failure line, and that Claude `Read` reads an issued file outside the
workspace. Claude Code 2.1.289
and later present no TaskOutput (a background task's output file is read with
`Read`), so the Claude pack presents no output tool. Its five-tool surface
(`Bash`, `Edit`, `Read`, `TaskStop`, `Write`) is pinned by exact exposed-name
tests on every catalog path, and the retired `BashOutput`, `Grep`, `Glob` and
`TodoWrite` names are tested to fail to lower. The Claude Code 2.1.295 native
interactive captures (mock origin, synthetic OAuth) are the costume evidence for
the user agent and billing `cc_version` pin; the capture tool's scripted
multi-turn mode (`--claude-scenario multiturn`: two `tool_use` rounds, one 529
retry, a `--continue` reopen) is the evidence for `diagnostics` and the turn
indices, pinned offline by sanitized facts in the Anthropic protocol tests. Native Bash/Read/Edit/Write
schemas and descriptions remain those compared with native 2.1.289 (Plak's
presented descriptions are its own truthful wording in that vocabulary, not
the native text). The version suffix test pins `hello` to the captured `05c`;
`2.1.293` stays `72d`. Occupancy fixtures
keep the stop name process-allowlist gated without changing its effect tag.
The job-ledger fixture carries a promoted row across later turns and their operation-only and
terminal commits, copies terminal evidence in a history-unchanged Quiescent
checkpoint, and proves repeated recording is a no-op. Manifest round trips pin
the row/terminal bounds, same-batch consumption, clear counter and epoch 26.
Fresh-service reopen retains exit-code and output-tail answers until consumption;
stop on an already terminal snapshot preserves its original outcome. Row-cap
fixtures keep foreground retention separate from promoted, unconsumed jobs.
The top-level terminal-result commit removes the row and snapshot together;
repeated terminal recording after that commit is a no-op.
Compaction and its verified re-fold preserve the pending terminal and counter.
Review regressions cover CRLF/lone-CR normalization and NUL/C0/C1/DEL terminal
output, including a real PTY tail, followed by successful recording
and later commits, defensive tail bounds, monotonic counter-only extensions,
missing stopper attribution, consumption before retirement and cancellation
between the running observation and promotion. Lost-owner reopen fixtures poll
and consume formerly running rows as outcome-unknown snapshots; hydrated rows
count toward the 128-row cap while a full ledger still permits foreground starts.
Code Cell recovery rejects crossed dispatched root-command request bytes.
External consumer fixtures name the job registry, views, terminal-recording
workflow and the preempt receiver through supported surfaces.

Process-notice regressions pin atomic prompt admission (the notice precedes
`RecentIntent` and its rows disappear in that same checkpoint), one notice-only
boundary commit, and delivery riding a pending steering commit. Held model
responses deterministically finish jobs before a continuing boundary or before
a final answer: notices affect the next continuing request but never extend an
answered turn. A host-instruction admission covers the L0 path used by child
start, follow-up and nudge turns. A one-shot ordinary-capacity refusal leaves
the row pending until the next safe boundary. A blocked notice publication
models a crash before commit and requires redelivery after reopen; a crash
after publication preserves the notice and empty ledger without redelivery.
Lost-owner reopen pins the exact `outcome_unknown` warning. Terminal-reporting
top-level tool results suppress later notices; nested terminal reads retain
their pending rows. Envelope tests cover integer versus string handles, every
stop attribution, escaped/sanitized 40-line/4-KiB tails, and oldest-first
coalescing under the 16-KiB batch bound. Coalesced rows use the same atomic
consumption path as full envelopes. No host idle-wake policy is tested here.

The deterministic full-driver sequential-process fixture exceeds both the live
handle retention and retirement-tombstone bounds with 136 exited children. It
checks a successful terminal, finalized durable process lifetimes and a usable
following turn. Retained-child superset tests and genuinely missing-child tests
keep their separate roles: completion of exited work cannot excuse lost proof.
JSONL recovery tests require explicit `resume_queue` before draining paused work;
queue resumption and persisted-tree resumption are distinct controls.

In-process fences recover in place. The hosted and spawned-binary suites drive a
scripted turn whose stream reports an unobservable outcome after two deltas and
require: one `recovery_required` terminal per turn, one `recovered` record with
the `model_request` claim and `interrupted` cause (from JSONL `recover`, from the
next prompt, from `plak run --session`, and from `plak session recover`), exactly
one harness note, no second provider dispatch, and no text naming `/clear`.
Unit tests pin the abandonment fold per obligation shape, the claim precedence
and each claim × cause note, the process-session and Code Cell abandonments at
reopen, Esc inside a paid fold ending `Cancelled` with the fold finalized, a
failed paid fold falling to placeholders with one fold dispatched, lease
revival under the same fence (and its refusal after an acquire or a store
reopen), and a pre-dispatch lease refusal being definite.
Offline compaction regressions cover artifact-gated replay across twelve tool
rounds, pressure preparation refusal leaving the paid guard unspent, occupancy
folds after that guard is spent, strict shrink against fresh source lowering,
and stable placeholder prefix/cache affinity across passes. Production Codex
lowering pins byte-identical serialized bodies for fresh encrypted-reasoning
history and its portable projection, including reasoning-only and mixed items.
A deterministic 64-round fixture bounds
compaction count and requires eventual paid dispatch without ContextLimit.
Code Cell reopen fixtures also pin accepted pending and completed same-turn
steering, and the exact Prepared-root, Completed-provider,
`EvaluationAbandonmentRequired` shape: no invented cell result, preserved
completed evidence, and exactly one root terminal. A real-driver crash matrix
(`code_cell_crash`) cuts the store inside a native Code Mode, JavaScript and
Python cell — the round landed but the cell not yet prepared (claim
`NothingStarted`), source admitted with no nested call, a nested tool or
`exec_command` running, a nested call returned or yielded while the cell runs,
and a later cell beside an earlier cell's live job — and requires the head to be
closable by the runtime, the reopen to settle every in-flight cell, child, effect and process
operation outcome-unknown with claim `RunningTool` (cause `ProcessLost`), no
source, nested call or provider request replayed, the job kept as
outcome-unknown, and the next prompt to complete. Host refusal classification
distinguishes local abandonment/finalization debt from external effect evidence.

### Declarative TUI stories and the semantic pilot

Processes stories cover 48-, 80- and 120-column TestBackend frames in both
default theme classes: idle HUD count, Processes list, live output, the busy
`Esc` stop pop-up and the quit pop-up. `activity-processes.frame` pins these
surfaces; pilot journeys cover confirmed `s` and `S` intents and the turn
stop's `[ Stop turn + processes ]` button. The typing-safety tests type drafts,
editing keys and pastes into every view (Activity tabs, the agent view, the
root view, the queue, approvals, the slash palette, quit and settings reset).
Over the composer a printable key closes any pop-up into the draft and the
following `Enter` sends that draft, never the pop-up's action; in full-page
views a mapped key acts and `Enter` presses the pop-up's focused button, and
an unmapped key sends nothing. A real offline PTY
journey promotes a process, opens `/ps`, confirms its stop and observes process
cleanup. Registry tests cover exact-id stop attribution, recent-terminal
retention across consumption, the pre-consumption callback without any product
poll, and the five-minute expiry. These are offline evidence, not live provider
qualification.

`just tui-story` drives a versioned JSON story through the real Plak binary on
a PTY against the repository-only scripted model. A story contains exact input,
deterministic waits and assertions, and hand-labeled choices over a closed
action table. Its private trace records each normalized before/after screen,
cursor visibility and position, compact style runs, transition latency, and
terminal-restoration evidence. The checked-in first story covers launch,
settings, modal dismissal, prompt execution, rendered completion, and graceful
quit. Story traces live under `target/` and may contain prompt or model content;
they are private evidence and MUST NOT be published.

This is the black-box acceptance lane, not the semantic automation control
plane. It proves terminal encoding, paint, binary wiring and mode restoration.
Agent-guided TUI control instead projects typed TUI view state, derives
mode-aware actions from the product keymap and slash registries, and dispatches
typed UI intent through the existing driver. A VT100 screen or hand-copied key
sequence MUST NOT become that control plane or a second product runtime.

The semantic TUI snapshot is owned beside the TUI as
`plak.tui-snapshot.v1`. It projects stable focus and nested modal mode, phase,
binding, redacted composer shape, revisioned queue identity, transcript shape,
Menu state, registry-keyed Settings values, picker shape and canvas
identity while excluding prompt, queue, transcript and overlay text, geometry,
clocks, animation and caches. Its typed driver rejects actions absent from the
current mode-aware legal table. Local movement is lowered through the product's
real input router; host operations remain the same `UserAction` values a human
gesture emits. Slash choices, key descriptions and Settings rows come from the
product registries rather than a second action catalog.
The Menu's read-only tabs use owner `menu`; `open-menu` reopens the last-used
tab and `all-keybinds` is legal only on the While typing Keybinds page.


Offline contracts pin Settings persistence, composer send and busy steer,
all slash choices, queue revision actions, picker and confirmation ownership,
canvas confirmation, execution of every action advertised by representative
owner states, and a 200-turn redaction/bounded-action soak.
This is internal test machinery, not an external pilot API.

The private pilot uses `plak.tui-pilot.v2`. Observations carry a conservative
monotonic revision and a digest of the revision, public snapshot and
legal-action table. Every mutating request names `if_revision`; the UI owner
checks it immediately before local dispatch and rejects stale requests without
a host action. The revision can advance for presentation invalidation and is
not a coverage identity. Responses distinguish `observed`, `applied`,
`stale_observation` and `rejected`; `host_action_pending` means the local
action has produced a host intent, not that the host admitted or completed it.
The snapshot's `turn` field projects correlated canonical turn outcomes; busy
and usage are never completion oracles. An unresolved observation reports
missing terminal evidence rather than inventing a result.

The `.frame` review utility writes deterministic SVG derivatives under
`target/`. It adds no render oracle: checked-in frames and Rust tests remain
the evidence, while SVG is only a human visual-review convenience.

### The external facade consumer

The external facade consumer also executes offline in the public-contract gate.
It uses public facade APIs for a custom model and tool, cancellation, durable
session reopening and shutdown, without repository test-support APIs. The
lifecycle has a 30-second bound. It proves the public workflow, not live
provider/auth admission, an OS sandbox, or any evaluator. The
`facade-consumer` fixture selects every feature and the JavaScript-only fixture
the `composition` workflow; neither needs a `[patch]` table (§06). Neither is a
registry-distribution proof.

### Claude continuity regressions

Offline Claude continuity regressions require previous-response attribution
after durable reopen with refreshed capability/context observation timestamps,
while retaining rejection of foreign configuration, account, credential authority,
and ambiguous or imported response provenance. Local TLS capture checks the HTTP
session header against the logical session UUID in the request body and keeps
the per-dispatch client request UUID distinct. These checks establish wire and
provenance behavior; they do not establish the cause of a live provider refusal.

### The Harbor bakeoff plane

`eval/` is a separate, opt-in and paid Harbor bakeoff plane that runs
the same `plak run` binary against other coding CLIs. It is maintainer tooling,
in no gate tier and not product qualification; its adapter tests run with
`just harbor-test`, and `eval/README.md` owns its datasets, arms,
evidence rules and recipes.

Test-only session-record faults use one complete forwarding store with per-method
policies; composition fixtures use one scripted integration, session and event
port. Refactoring these fixtures does not change their scenario assertions.
Memory conformance runs once, independently of the SQLite suite. The route
SIGKILL matrix owns the direct boundary cells; the commit-cut campaign remains
separate. Projection rebuild and plain-reopen laws share a lifecycle per shape,
and capacity and workflow cuts share immutable seed/pilot fixtures, never the
mutated cut workspace. Busy and quiescent child-timeout fixtures overlap their
unchanged real timeout windows but assert each terminal and exact reopen.

Debug TUI smoothness gates score 40 frames over 120 ordinary transcript blocks;
release retains 200 frames over 400 blocks. Windowed-layout and retention-cap
fixtures still cross the production bounds. Debug pilot paging uses sizes
1, 2 and the full feed with every replay depth; release checks every size.
The generated `rebuild_from_empty_equals_every_paging_and_replay` law covers
arbitrary page partitions in either build. Wall-clock performance claims still
require the release procedure, not a debug gate.

## 1. Gate layers

The authoritative gate is `scripts/dev-check.sh`, in four tiers plus explicit
step-group and change-scoped modes. It is Cargo tooling plus tests plus thin
structure checks. Process paperwork -- requirement digests, provenance ledgers,
documentation link matrices -- is not part of it.

```text
scripts/dev-check.sh --fast [PKG...]   # just fast: iteration, no serial budgets
scripts/dev-check.sh                   # just check: unconditional pre-commit gate
scripts/dev-check.sh --platform        # just platform: per-install platform evidence
scripts/dev-check.sh --full            # just full: all groups, gate changes and publication
scripts/dev-check.sh --steps GROUP,... # just steps GROUP,...: independent group lanes
scripts/dev-check.sh --changed [BASE]  # just changed [BASE]: conservative plan from a checked tree
```

| Tier | Groups |
| --- | --- |
| `--fast` | `static`; `tests-parallel` scoped to package reverse dependencies, if any |
| default | `static`, `builds`, `tests-parallel`, `tests-serial`, `contracts`, `tooling` |
| `--platform` | `static` (fmt/clippy only), `builds`, `tests-parallel`, `tests-serial`, `tests-durability` |
| `--full` | All groups in the canonical order below |

`static` runs fmt, workspace structure, static feature checks, then workspace
clippy. Check, full and explicit static groups run both Python
checkers unconditionally, immediately after fmt. Platform runs only fmt and
cfg-specific clippy in static, without either structure checker. Fast and changed select them
by their actual inputs, and print the reason for each skipped checker:

| Checker | Inputs in fast/changed |
| --- | --- |
| `check_workspace.py` | Any Rust source, Cargo.toml/Cargo.lock, tests/fixtures/** (including inventory and .cargo/config.toml), gate inputs; unknown paths or unresolved readers widen to it |
| `check_features.py` (static) | Any Rust source, Cargo.toml, Cargo.lock, .cargo/**, rust-toolchain*, deny.toml, gate inputs; unknown paths or unresolved readers widen to it |

Absent unresolved readers, docs-only and ordinary tooling-only diffs skip both.
Rust-only, manifest, gate-input and unknown diffs run both. Fmt and
workspace clippy remain mandatory regardless of diff size. Public compile contracts are cheap warm and run
in check and full, never platform; changed selects them when the rules require them.
Contracts isolate fixture feature graphs from workspace artifacts in a persistent
`.public-contracts/<target-dir-name>/<target-triple>` directory beside
`CARGO_TARGET_DIR`, defaulting to the checkout's `target` as the base.
The triple is `CARGO_BUILD_TARGET`, or rustc's host triple when unset.
The sibling placement avoids workspace artifact sweeps (which also descend
into nested target directories); the gitignore rule preserves it across lane
checkout cleanup. Switching workspace target triples does not evict another
triple's contracts. No diagnostic pin is relaxed to keep builds warm.
`PLAK_PUBLIC_CONTRACTS_DIR` overrides the cache root (the triple is still appended).
The pinned musl container sets it to `/qual/target/public-contracts-<image-tag>`
under the persistent target mount, not an ephemeral container sibling, so each
image/triple pair remains warm across container runs.

The canonical `GROUPS` list in `tools/checks/gate_plan.py` drives full and explicit
groups. Ordered `PLATFORM_GROUPS` and `CHECK_GROUPS` derive from it. These tiers
are not nested: check is the Linux pre-commit gate, with structure checkers,
feature builds, the parallel and serial budget lanes, contracts and tooling, but no
durability lane; platform is the per-install, per-platform gate, with
fmt/clippy, feature builds, both budget lanes and the durability lane, but no
platform-independent checks. Full is their integrated union plus the remaining
full groups. Tooling tests assert that platform and check are subsets of full,
platform excludes both checkers and contracts and includes the durability lane,
check includes both checkers, feature builds, the parallel and serial lanes
and contracts and tooling and excludes the durability lane, and full equals the selection of every group, with
identical invocations and order. `--steps` runs exactly its named groups, in
canonical order, rejects unknown/empty groups, and does not imply static or
other groups. Independent runners must use separate checkout/target/temp
roots; test groups retain their real Monty-worker prerequisite.

| Group (canonical order) | Coverage |
| --- | --- |
| `static` | fmt, workspace structure, static feature closures/security hashes, workspace clippy |
| `builds` | Workspace default-feature build; Plak maintainer-feature build. Both deny rustc warnings |
| `rustdoc` | Workspace all-features rustdoc, denied warnings |
| `tests-parallel` | Parallel offline lane, excluding the budget and durability lanes |
| `tests-serial` | Serial offline budget lane, one test thread |
| `tests-durability` | Serial crash-matrix and sqlite SIGKILL campaign lane, one test thread. Platform and full only; not check |
| `contracts` | Positive/negative public compile contracts |
| `tooling` | Checker unit tests, including dev-check and gate-plan self-tests |
| `perf` | Offline performance smoke, informational |
| `loc` | Source-size report, informational |

Each group prints elapsed wall-clock seconds and appends one JSON object to
`.gate-times.jsonl` beside `CARGO_TARGET_DIR` (the checkout root when that
directory is `target/`). The file is gitignored and sits outside the workspace
artifact sweep, the same placement idea as the public-contracts cache. A line
records the git commit, the tier (`check`, `platform`, `full`, `fast`,
`changed` or `steps`), the group, elapsed seconds, the group's exit status,
the platform triple (`CARGO_BUILD_TARGET`, otherwise rustc's host triple; never
a machine name), and `warm` or `cold` according to whether the target directory
existed before the run. After the run, each successful group is compared with
the median of its last 10 successful samples for that same triple. Fewer than
3 successful samples, or a median of zero, produces no comparison. A group
that took more than twice that median prints a warning, for example
`WARNING: contracts 277 s vs median 5 s: cold cache or regression`. The warning
is informational and never fails the gate.

After valid planning and prerequisite checks, every selected group runs even
if an earlier group fails. The static group also runs every selected step
(fmt, each enabled structure checker, clippy), retaining and summarizing all
failures. Other groups may stop internally on a failed prerequisite or command;
later groups still run. Timing rows retain each group's exit status. The final
summary names all failed groups, exits with the first failed group's status,
and never reports success when any group failed. Invalid plans and missing
gate prerequisites still stop before group execution.

Moving structure checks earlier
does not remove any full-tier coverage; builds, rustdoc, the test lanes,
contracts, tooling, perf and LOC keep their relative order.

`--fast` maps tracked working-tree changes against HEAD plus untracked,
non-ignored files to their longest-prefix package owner. Named packages replace
the diff. Root Cargo inputs select every package. Its parallel lane uses
`rdeps(=<pkg>)`; an empty selection passes. Fast deliberately leaves serial
budgets to changed/check, and is not complete pre-commit evidence.

`--changed [BASE]` uses the same diff source against BASE,
including staged, unstaged, deleted and untracked non-ignored paths.
Gitignored files present under `**/proptest-regressions/**` are also inputs of
their owning packages. Untracked `.rs` files are covered through the diff,
even though the reader scan inventories tracked sources only. It prints
the BASE used. BASE must already have passed `just check` on this toolchain;
change-scoped verification cannot establish the correctness of that baseline.
The default is `git merge-base HEAD main`, using HEAD~ when on branch main
itself. Missing main or a merge-base equal to HEAD on another branch/detached
checkout fails with a request for an explicit BASE, as does branch main when
HEAD~ does not exist (a single-commit history). Forked runs pass an explicit
BASE (for example their checked fork point), rather than relying on the host's
main. An empty diff prints a loud warning without failing. It prints
the groups, package filter and the rule/path behind every selected group,
then runs them. All matching rules union. The static group (fmt/clippy plus input-selected checkers) is mandatory even
for an empty diff; a non-empty diff never yields an empty plan. When unsure, run
more, never less: every unmatched path widens to the entire check tier and
is named in the printed reason. Invalid diffs/planner failures stop the gate.

| Input rule | Required coverage beyond static |
| --- | --- |
| Any Rust source or Cargo manifest | Public compile contracts over the whole facade closure |
| Package Rust source (including test-only source) | Default-feature and maintainer-feature builds; both lanes filtered to that package and its reverse dependencies |
| Any file in a durability-owning package (`plak`, `plakous`, `plakous-store`, `plakous-offline-crash-harness`) | `tests-durability` as well, with that package and its reverse dependencies. A package that does not own one of those tests does not select the group, including when an artifact consumer does |
| Any non-Rust file inside a package | That package and reverse dependencies in both lanes; API coverage is purely additive |
| Root Cargo.toml/Cargo.lock, any crate Cargo.toml, .cargo/, rust-toolchain*, .config/nextest.toml, deny.toml, lint/format config | Whole workspace in both lanes, plus builds |
| Facade, supported core API crates, tests/fixtures/**, public compile checker | Contracts and rustdoc; package Rust rules also apply |
| Gate inputs: scripts/dev-check.sh, scripts/dev-tools.sh, Justfile, .config/nextest.toml; tools/checks/{gate_plan,changed_packages,budget_filterset,check_workspace,check_features,plakous_policy,check_public_compile}.py | Whole check tier (including tooling) and both structure checkers; planner self-edits cannot rely on weakened package rules |
| Other scripts/**, tools/**, tests/tooling/** | Tooling |
| docs/**, configs/** and other referenced inputs | Discover readers at plan time; their packages and reverse dependencies in both lanes; no-reader docs select only static if discovery has no unresolved inputs |
| crates/harness/prompts/** (sources or pins) | Default-feature and maintainer-feature builds; owning package and reverse dependencies in both lanes |
| Any planned package in the Python worker / JavaScript runtime workspace dependency closure | Also select artifact consumers: Python evaluator, hosted and Plak / JavaScript evaluator and Plak, respectively, with their reverse dependencies; closure includes optional, target-specific and dev dependencies from Cargo metadata |
| Anything unmatched | Whole check tier, both structure checkers, with unmatched path in the reason |

The planner scans tracked Rust files for `include_str!`, `include_bytes!`,
`#[path]` and string literals resolving to files or directories outside their
own package (including docs/configs and indirect path helpers), excluding
sources under package benches/ and examples/ (nextest does not run them;
workspace clippy still compiles them). Char and byte-char literals are masked
without hiding subsequent embedded inputs. Non-embedded literals resolving only
to the repository root (`.` or `alias/..`) are not repository readers; embedded
root targets remain unresolved. Interpolated prose (whitespace or sentence
punctuation in the literal portions), URLs and serialized data are not path
templates. A template with a literal leading directory records that directory
when it exists in the checkout or is under docs/configs (including deleted
inputs). A nonexistent prefix outside docs/configs is a runtime path, not a
repository reader. A dynamic root with literal path segments, or a template
prefix resolving to the checkout root, remains unresolved. The planner carries
an exact `(source file, literal)` inventory of reviewed non-reader runtime IDs,
URLs and private-home paths, with a rationale for each; tests reject stale
entries and prevent the exemptions from applying to embedded inputs or other
sources/literals. The current tracked-source unresolved residue is empty and
ordinary package diffs stay scoped. All remaining uncertainty and unsupported/
unresolved embedded targets widen to the whole check tier and both checkers; Cargo-generated
OUT_DIR targets use their tracked build inputs. This intentionally favors
false positives over missed inputs and can widen otherwise no-reader docs.
Completeness tests independently inventory existing quoted docs/config paths
in tracked non-bench/non-example Rust sources, plus known executable-discovery
markers and their consumers for every package in each artifact's metadata
dependency closure. Scratch-source tests cover parsing escapes, root-only
literals, prose, directory templates, exact exemptions and uncertainty; real-tree
regressions pin the unresolved inventory and package/core/doc-only scopes;
these are not a proof of arbitrary dynamic Rust reader discovery.
The feature checker reads workspace manifests and package Rust sources,
including test targets, to require locally declared UUID generation features.
Rust-source changes therefore select both structure checkers. Fixture
inventories and fixture .cargo/config.toml are not feature-checker inputs;
fixture manifests still conservatively select it.
A scoped serial lane
may be empty and passes; an unscoped serial lane still must contain tests.
The durability lane follows the same empty-scope rule.

Every tier's main compilation and lint use `--workspace --all-features`:
Cargo resolves features per selected package set, so a `-p` subset would build
a second copy of the dependency graph, while the whole workspace rebuilds only
the touched packages and their dependents. The package/rdeps plan narrows both
test lanes, not Cargo feature resolution. The check/platform/full tiers and cargo
input, Rust and prompt plans additionally build the workspace's default features and Plak's
maintainer feature set; an all-features build cannot prove those smaller
closures compile and link, so a feature-gated slip fails the per-change gate.
Those two builds deny rustc warnings. `-D warnings` is appended to the rustflags
Cargo will actually pass (`CARGO_TARGET_<triple>_RUSTFLAGS` when that variable
is set, otherwise `RUSTFLAGS`) and is not exported to later steps, so the
all-features test artifacts are not rebuilt. Registry dependencies stay
`--cap-lints allow`. A warning that exists only in a default or maintainer
closure, and is hidden from `--all-features` clippy, fails this group.

Use changed for minimal sufficient change verification, check for
unconditional whole-workspace evidence, and full when changing the gate itself
or at batch/publication boundaries. Platform-independent structure checks and
public contracts and Python tooling run once on Linux in check/full; rustdoc
runs once on Linux in full. Each install additionally
requires `just platform` on Apple Silicon macOS and `just musl-native` on Linux
x86_64. The native musl lane defaults to `--platform`; `just musl-check` runs
the full tier in the pinned offline musl container before publication (chapter 06 §6.1). Both musl scripts set `CARGO_BUILD_TARGET` to
`x86_64-unknown-linux-musl`; the gate passes an explicit `--target` to every
compile and nextest invocation, and subprocess Cargo checks inherit that
target. A GNU-host gate does not qualify the supported musl platform.

Every test entry point (`dev-check.sh`, `just test`, `just coverage`,
`perf-smoke.sh` and `crash-sweep.py`) exports `TMPDIR` from one selector,
`scripts/test-tmpdir.sh`, before anything runs. On Linux it is a per-checkout
directory under `/dev/shm`; on macOS a per-checkout directory under
`/private/tmp`, so neither lies under the operator's `HOME`, where a spawned
binary's project discovery could reach the real `~/.plak`. The directory is
named `plak-test-<uid>-<cksum of the checkout path>`, must be owned by the
caller and mode `0700`, and must resolve to itself with no symlinked
component; `dev-check.sh` empties it at the start of each run. When the shared
root or the directory fails those checks, or on another system, the selector
falls back to `target/plak-test-tmp` in the checkout. Linux `fsync` is a real
device flush that can exceed the stop, cancel and quit bounds under gate load.
macOS `fsync` does not flush the device, so the two platforms only agree when
the Linux scratch is RAM-backed. Maintainers may select another disposable
filesystem with `PLAK_TEST_TMPDIR`; the selector resolves its physical path
before `TMPDIR` is exported. This changes test storage placement, not
deadlines or assertions, and must be recorded when interpreting
filesystem-sensitive results. On macOS `/tmp` and `/var` are symlinks, the
SQLite stores open with `SQLITE_OPEN_NOFOLLOW`, and the credential store walks
refuse symlinked parents, so a default temporary root makes every host test
that opens `runtime.sqlite3` fail with `SQLITE_CANTOPEN_SYMLINK`. Anyone
invoking `cargo nextest` by hand MUST set a physical, non-symlinked `TMPDIR`
outside `HOME`; `TMPDIR="$(bash scripts/test-tmpdir.sh)"` selects the same one.

The same scripts export `PLAK_HOME` to `$TMPDIR/plak-isolated-home` (mode
`0700`) on every nextest lane. Offline tests must not create the operator's
`~/.plak`. Product home resolution consults `PLAK_HOME` before `$HOME/.plak`,
so a test, or a `plak` it spawns, that forgets to choose a home writes that
scratch directory instead of the real one. `HOME` stays the operator's, because
Cargo and git read it. A hand-run `cargo nextest` does not set `PLAK_HOME`;
export it to an absolute scratch directory first, the same way as `TMPDIR`.

| Layer | Command | What it proves |
| --- | --- | --- |
| Formatting | `cargo fmt --all --check` | The tree is byte-identical to `rustfmt` output under the pinned toolchain |
| Lints | `cargo clippy --locked --workspace --all-features --all-targets -- -D warnings` | Every target of every package, all features on, no warnings. The workspace lint table denies `missing_docs`, `unwrap_used`, `expect_used`, `dbg_macro`, `todo`, `unimplemented`, and broken intra-doc links, and forbids `unsafe_code` |
| Default feature builds | `cargo build --locked --workspace --lib --bins` and `cargo build --locked -p plak --features maintainer --bins`, each with `-D warnings` on the rustflags Cargo passes | Check, platform and full tiers, after clippy: default-feature libraries and binaries compile and link, as does the maintainer installer's Plak feature set, without test hooks or all-features unification hiding missing dependencies. Rustc warnings in those closures are errors; clippy remains the `--all-features` lint |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked` | Rustdoc builds every package's documentation with its own lints denied: broken intra-doc links, links from public documentation into private items, and bare URLs are errors. Runs after clippy and before the tests |
| Offline tests | `cargo nextest run --locked --workspace --all-features --profile offline` | The whole workspace's deterministic behavior, with no network and no credentials |
| Public compile contracts | `python3 tools/checks/check_public_compile.py` | Supported inventories still compile from outside, and withheld authority still fails to compile |
| Workspace structure | `python3 tools/checks/check_workspace.py` | Every package at the path its `[package.metadata.plakous] layer` prescribes, its support class legal for that layer, every dependency edge admitted by the layer matrix or an explicit exception, neutral packages separate from providers, the facade as the only Plakous surface of host/bin sources, the CLI ambient surface, the `plak`/`plakous-js-host` build split, and tracked-file hygiene: no tracked or untracked unignored text file carries an absolute home path outside a short list of generic placeholder users, or a private host or maintainer tool name listed one per line in the optional untracked `docs/local/hygiene-private-names.txt` (absent, that name check is skipped; the tree carries neither the names nor their digests); no per-file exemptions |
| Feature closures | `python3 tools/checks/check_features.py` | Per-package feature closures (2^n subsets of each package's own public features, never a cross-product; 514 for the facade), statically, plus reviewed manifest-surface digests, locally declared UUID generation features, and test-only dependency features (transport fault injection) absent from a default `plak` and present under `test-hooks` |
| Checker unit tests | `python3 tools/checks/run_tooling_tests.py` | Offline tests over the checker, build, and performance helpers, one `python3 -m unittest` process per `tests/tooling` module, four at a time |
| Performance smoke | `bash scripts/perf-smoke.sh --check` | Six offline measurements of the real binary against this machine's seed row (§2.7). Informational |
| Source size | `python3 tools/checks/check_prod_loc.py` | Reports per-scope LOC (L0, below-facade, host, product, test). Informational |

Nested-failure regressions cover exact successful string delivery, caught and
uncaught JavaScript rejection, `Promise.allSettled`, retained earlier output,
correlated acceptance, and joined shutdown through both embedded and sidecar
engines. The private-driver scheduler additionally exercises failed read-only
and partially effective mutating children, later successful mutation, rejected
invalid arguments, finalized child records, and joined cancellation. Canonical
projection tests distinguish ordinary tool failure from infrastructure failure.
Every tier that runs Rust tests takes the `monty` binary from the workspace test
build (`cargo test --no-run --workspace --all-features`) and obtains its
executable path from Cargo's artifact record, respecting Cargo target-directory
and build-target configuration. An explicit `PLAK_MONTY_WORKER` instead selects
an existing worker; missing, empty, directory, or non-executable paths fail the
gate. Both offline nextest lanes receive its absolute path, so real Monty tests
cannot silently take their missing-worker early return. A worker that cannot
run or speak the protocol fails those tests. Manually invoked Rust tests
without a worker retain their existing limited, potentially skipped coverage.

Python regressions distinguish feed-queue handoff from correlated worker
acceptance, lost receipts from successful delivery, and value-depth or full-frame
decoder refusal from a delivered Python exception. The full-frame regression
covers a nested null whose value depth is valid but protobuf recursion is not.
Real worker cases exercise out-of-order
`asyncio.gather` results and uncaught nested rejection. The private-driver
scheduler also withholds its first delivery receipt until the second result
arrives, while checking canonical child finalization and joined cancellation.
Owner-transfer regressions cover a committed tool terminal followed by refused
child-edge publication, preserving definite evidence without result delivery,
sibling effects, or replay. Injected panics before source admission, after
source admission, and after cell acknowledgement preserve explicit recovery
obligations when the extracted resource pair is lost. Nested read-only and
mutating permit waits recheck an expired store clock before executor entry,
including exact duplicate responders. Persistent-interpreter coverage runs a
successful compaction between cells, retaining the same generation and stored
value while joining final session cleanup once. These deterministic fixtures
complement the real process-nested stack and cancellation witnesses; they do
not claim real-engine injected lease timing.

The repository's `crates/plakous/tests/agent_scripted_quickstart.rs` is an offline
integration test:
it constructs through the curated facade, completes its scripted turn and joins
shutdown. This verifies the executable example, not only its imports or syntax;
it does not claim live-provider embedding qualification.

Lease-domain regressions exercise store authority time directly and refuse
equality, unavailable clocks and mismatched domains at final admission,
and preserve exact historical receipts after a simulated reboot while requiring
higher replacement fences. SQLite tests reject established same-boot foreign scopes without
changing database/WAL bytes and prevent an old handle from adopting the new
binding. Independent child processes exercise the default host clock against a
shared database. The lingering-owner process test excludes the product WAL
writer while suspending the owner and waits for its stopped acknowledgement
before releasing that exclusion: a stopped SQLite writer would retain its lock
indefinitely, which is not lease-fencing behavior. A separate contention test
requires binding writes to refuse without partial state and succeed after the
writer releases; no timeout or stale-owner oracle is relaxed.
These deterministic and process tests do not simulate a real
host suspend, privileged namespace transition or reboot; Apple Silicon execution
remains a separate platform qualification step.

`nested_delivery_panics_preserve_remaining_shutdown_stages_and_prior_failures`
queues one result and an exact duplicate. The original responder panics during
execution; the duplicate waits for integration shutdown before panicking during
final settlement. The fixture proves one recovery terminal and executor entry,
no accepted result or replay, later lease release, store flush and event closure,
and preservation of a prior integration-shutdown failure. Tool-task shutdown and
heartbeat-join ordering are source-audited on this path.

Runtime organization checks preserve public compile contracts, test basenames,
wire identities and golden assertions. Private durable fixtures live beneath
the durable owner so tests can inspect its nonce and compatibility state without
exposing live fields. The full agent suite and real executor-stack witness remain
required after runtime module moves.

`just fmt`, `just clippy`, `just doc`, and `just test` run individual layers.
`cargo-nextest` is required by every tier: if it is absent the gate exits 2 rather than falling back to `cargo test`, because a plain
workspace `cargo test` would skip the serial budget lane and the nextest test groups.

The file-read regression suite checks exact empty, LF, CRLF, unterminated,
Unicode, and metadata-looking content; selected-line pagination; Claude Direct
cat-n projection with byte-exact round trips for empty, LF, CRLF, no-final-LF,
mixed-ending, and Unicode content while nested JSON stays unchanged; and
structured observation-budget truncation. Local
composition must receive neutral JSON rather than a Direct display header.

Product-store ownership evidence drives the real session-list command and
attach configuration constructor across separate state-root requests and a
worker thread, checking that they use one lazily opened connection. Replacement
coverage drives the command after replacing the product home. SQLite contract
tests cover failed-open retry and rollback after interruption between branch
writes, including continued use of the same service. The identical Memory/SQLite
conformance cases also reject cross-session and retired queue identities,
require consecutive queue edits, and prove failed save/prepare and interrupted
branch publication reserve no identities and advance no caller revisions.
Shared load cases cover every delivery and workflow state, queue-only and empty
records, and both valid stored drain modes. They require paused recovery only for
outstanding work, stable repeated loads and listings, and an unchanged CAS token
until an explicit save. Terminal delivery tests persist nonempty command bodies
through both stores and compare their complete loaded records, reject a body
that contradicts its digest before compaction on save and both prepare paths,
including unchanged rows, revisions, and caller records after refusal and valid
retries by either the original owner or a different owner of the new identities, and prove exact terminal replay
survives receipt eviction while changed replay is refused.

Result-delivery tests cover quiet-window idle admission on results and questions,
coalescing, non-waking findings, disabled and interrupted hosts, queued-prompt
precedence, 30 consecutive wakes with fresh mail and process completions, and
post-terminal delivery of two jobs finishing 10 s apart followed by a child
result after 24 consecutive wakes, including quiet-window coalescing and no
leftovers for user admission. They cover Esc suppression until a user prompt,
run continuation beyond 24 host-authored turns, and exact-once delivery after
clean close and reopen. They also cover bounded
prompt backpressure with unread results retained, and cuts after journal
preparation and after accepted admission but before tree acknowledgement.
Nested-parent context replay includes newer mail arriving between preparation
and retry, exact prompt conflicts, blocked competing commands, reconstructed
tree state, and repeated acknowledgement. A real SQLite child reopen covers the cut
after durable turn binding but before result acknowledgement, verifies the production
recovery path consumes the exact batch, and proves provider evidence remains unchanged.
Real hosted one-shot and authorized submit-harness turns verify context reaches admitted
commands. SQLite product-store coverage binds
result batches into immutable delivery identity and refuses changed message ids.
Mailbox tests cover unread-only listing and bounded read history, full 2048-byte
steer and followup bodies, exact mail returned and acknowledged by waits,
non-repeating mail wakes, wake reasons, omitted running usage, and unread mail
retained after a failed receipt publish. They preserve numeric arrival order
across reopen and retain terminal attempt output when the global queued-mail
byte ceiling refuses a result push.

Account workflow tests drive command dispatch with a writer-free `AccountOps`
double and injected terminal-machine effects. Automatic completion leaves
stdin unread even when browser launch fails; paste completion follows the
challenge's exact mode. Failed, cancelled, timed-out, and SSH-refused attempts
never retry or switch routes. Owned-PTY subprocess tests verify hidden paste,
retained cooked editing and signals, and exact terminal/file-flag restoration
after success, oversized input, Ctrl-C, and panic. A refusing reader proves the
raw byte bound stops consumption even for endless whitespace; injected
clipboard doubles cover both `c` and `copy`, unavailable clipboard guidance,
and subsequent paste without launching a platform clipboard process. One-shot
execution has no fake method on the account port; its evidence comes from the
hosted driver and actual one-shot adapter.

Workspace-cleanup regressions withhold removal acknowledgement, reopen the
prepared `discarding` row, and refuse duplicate dispatch. Injected unmint and
unpin failures retain the copy or pin, publish recovery rather than discarded
success, and prevent disposal of unresolved seal/session work. Both memory and
SQLite persistence paths verify the pending row and exact terminal receipt.

Child-startup regressions exercise reservation before command acknowledgement,
close/shutdown joining, cancellation before and after live ownership transfer,
late release, a lost acknowledgement deadline, and failed release. They assert
that unresolved work remains reserved and unsealed, durable recovery survives
reload, and exact spawn replay never invokes the binder again. A composed
hosted regression creates a real SQLite-backed session, admits and physically
dispatches one offline provider request, withholds acknowledgement delivery,
and crosses the caller deadline. It then proves close cannot seal the active
copy, late acknowledgement releases the owned attachment, and exact reopen
admits no replacement dispatch. Caller deadlines bound observation; they do not
qualify termination of an uncooperative session operation.

Child-retirement regressions use deterministic release barriers through the
normal completion, concurrent cancel, navigator close, repeated close, stop-all,
and shutdown paths. They assert that cancellation of a caller does not abandon
its owned release, a panic after the caller is aborted still publishes a
recovery-required fault before completion, overlapping operations join the same retirement, no live
mutex is held during release, and no copy seals while cancellation is unsettled. Repeated close during an already
queued terminal seal preserves one ticket and one candidate without emitting a result for closed work,
and the existing completion retains the close intent. Composed retirement-fault
regressions hold an already dispatched effect across repeated close/stop, run one
VCS snapshot, and retain recovery evidence after both seal success and refusal.
A cloned old completion cannot consume a later same-attempt reseal; durable
reconstruction drops transient ownership and refuses the old completion.
Actual child-watcher fixtures drive canonical failed/recovery-required model
outcomes after submitted output, with and without a prior close; quota and
rate-limit classification stay intact, no submit harness runs, and exact release
still joins. A dispatched-provider barrier plus SQLite publication refusal covers
terminal recovery after failed checkpoint publication. Composed recovery tests retain existing candidates
through seal success/refusal, adoption, close/stop, binding replay, and repeated
release failure without losing the original canonical cause.
Failed normal release retains its exact-turn reservation and makes subsequent
close fail with recovery-required evidence. Shutdown tests join both existing
retirements and newly released active children concurrently, then cover the
bounded abort-and-join path and its retained fault. A two-child barrier proves
batch cancellation starts both releases before joining either, joins an overlapping
cancel, and preserves input-order reports despite reversed completion. Existing stale-turn tests
continue to exclude replacement retirement and terminal mutations. Exact-operation
replays are driven while release is blocked, after release success or refusal,
after caller cancellation, and with a replacement occupying the slot; none can
dispatch a second cancellation. Refusal to persist cancellation preparation is
also checked to leave both node state and the live canceller untouched.

Child-recovery regressions (`agent::recovery::tests` in `plakous-hosted`, the
tree's `tree::recovery` unit tests, the `recover_agent` tool tests, and one
host-loop `/recover` test) reproduce the stranding failure mode: a real
unknown-outcome child turn on a real runtime database, a fixture binding with
a real projected tool catalog, and a SQLite trigger that refuses the turn
waiter's own recycle reopen, so the node is left `recovery_required` and its
turn is never abandoned. Every settlement goes through the product reopen
(`reopen_spawned_child`) with the spawn's real create identity and a parked
rejoin spec. They prove that `recover_agent` (through the hub tool executor)
and the automatic pass each settle the child as `turn_abandoned` with no
additional provider dispatch, that a followup is admitted afterwards and close
frees the slot, and that `/recover` shows the user which child it recovered. A
lease another session holds is refused `recovery_lease_held` exactly, by
`recover_agent` after a short injected lease wait, with the reattach lock free
between its polls, and by the pass at once. A pass that cannot take the
reattach lock by its deadline reports every target as not attempted and
settles nothing. Hub unit tests prove that a start while a pass runs makes it
run once more, that a closed hub starts no pass, and that shutdown aborts a
pass that overruns its allowance and still returns. A
refused tree publish changes nothing and its retry still reports
`turn_abandoned`; a refusal records nothing on the node. Recovery racing the
turn waiter settles the attempt once, and a real turn waiter whose recycle is
waiting out a held lease makes `recover_agent` answer `recovery_owner_live`
and the pass skip the child at once, without either waiting behind it. A
terminal refusal lets `close_agent` settle the attempt with the refusal's code
and end the node. An interrupted adopt behind an unknown
outcome settles its session first and then closes to a free slot. Startup
uncertainty is settled by replaying the spawn's create, as
`recovered_without_turn` and never `completed`. Reopen errors map to exact
`recovery_*` codes from their typed kind, a live owner is never reopened
under, and a leaf caller is refused before any reopen because the tool is not
compiled for it.
The unit tests pin the settlement kinds, first-settlement-wins, attempt
fencing, close authority (`grant_denied` for a caller that does not manage the
node), that a failed copy removal is not a session recovery, that a
terminal refusal for a stale attempt is ignored, and that a
`recovery_required` node holds its tree node slot until it is closed. A
`plakous-agent` unit test pins that a turn result handle returns a published
terminal failure even when both of its channels close before the waiter reads
them.

### 1.1 The public compile contracts

`check_public_compile.py` is not a nextest target — it drives `cargo` itself,
because it must compile code that lives outside the workspace and must assert
that some code fails to compile.

`just dev-tools install` explicitly prefetches dependencies for all six locked
external fixtures with `check_public_compile.py --fetch` (`cargo fetch --locked`
per manifest), after installing the pinned tools. Prefetch never compiles a
fixture or rewrites its lockfile, is repeatable, and fails installation if any
fetch fails. Ordinary prerequisite checks and compile-contract gates remain
locked and offline; they never fetch dependencies implicitly.

Positive pins compile five external consumer fixtures. Four cover the supported
roots: `tests/fixtures/agent-consumer` (`plakous-agent`),
`tests/fixtures/facade-consumer` (`plakous`), `tests/fixtures/model-consumer`
(`plakous-model`), and `tests/fixtures/tools-consumer` (`plakous-tools`). The
fifth is `tests/fixtures/js-embedder-consumer`, the JavaScript-only workflow
that must carry no `[patch]` section. The first two are excluded from the
workspace precisely so they resolve as genuine downstream crates. These
fixtures are the inventory: a direct-package symbol outside them is nameable,
not supported.

Negative pins compile 26 single-import binaries under
`tests/fixtures/authority-negative` and require each one to fail with the exact
expected error code, message, and line. One locked, offline `cargo check
--keep-going --message-format=json` selects every requested negative binary;
each diagnostic must belong to that binary's Cargo target and still match its
pinned primary source span. A missing diagnostic (including an unexpectedly
compiling binary) fails the batch even when another binary fails as expected.
They cover the tool runtime and
scheduler (from `plakous-tools`, `plakous::tools`, and `plakous-agent`), the
internal process-dispatch seam, runtime checkpoints and codecs, the session
store, the durable session runtime and driver endpoint, opaque invocation and
turn-result construction, credential byte exposure, family transports, Standard
integration authority, family-artifact sealing, the raw Anthropic transport,
the OpenRouter model lane, xAI Platform authority, and the local composition
session. A twenty-seventh documentation pin runs the curated `plakous-agent` and
`plakous` doctests, which include compile-fail assertions for withdrawn
prototypes.

Identical positive compile/build and lifecycle commands for one fixture or
example feature set share the stronger `cargo run`; different fixtures and
feature sets never merge. All inventory and source-content assertions remain.
`--positive-only` and `--negative-only` select a half; repeatable `--only`
fragments select pins before batching. `--archive-dir PATH` retains one complete
log per pin and the negative batch log for an evidence archive. Direct nameability of
an implementation crate is never itself a support failure; only a supported
root exporting withheld authority is.

### 1.2 What is not gated, and why

These are real procedures with real evidence. They are kept off the daily gate
because they are slow, paid, host-specific, or measurement rather than
correctness.

| Not gated | Run it when |
| --- | --- |
| `cargo deny check advisories bans licenses sources` (`just deny`) | Dependencies change. It needs advisory acquisition, so it is not offline |
| `cargo check --workspace --all-features --all-targets --locked` as an MSRV probe | The MSRV is the toolchain pin, so the ordinary build already is the MSRV build |
| `python3 tools/checks/check_prod_loc.py` (`just density`) | A density question is live. It always exits 0 |
| `cargo bench` for `perf_core`, `perf_project`, `perf_store`, and `tools/perf/perf_build.py` | A performance claim needs numbers. There is no portable numeric baseline; every claim carries its emitted host, feature, and toolchain metadata |
| `cargo nextest run -p plakous-offline-crash-harness` | Process-crash matrices are wanted on demand. Unix only |
| `just native-compare-prompts` | Comparing Plak's prompts against installed vendor CLIs using isolated synthetic credentials and local mock responses |
| `eval/` Harbor two-rung bakeoff (Rung A weekly plumbing + TB 2.1 A/B; Rung B one-task TB 4.0 wire; `harbor-smoke` hard TB 4.0 slice) | Harness-comparison against vendor CLIs. Opt-in, paid, never a qualification row. `just harbor-test` runs its adapter tests. See `eval/README.md` |
| Live scenarios (§3.2); the pre-release bar (§3.4) | A claim needs live evidence. Paid and opt-in. The bar is not a gate tier |
| `just api-census` (`scripts/api_model_census.py`) | Before an install, to diff the bundled API model tables against the providers' lists and docs. Opt-in, networked, never a gate tier. See spec 01 §6.3 |

Local verification is the authoritative gate for every layer above; chapter 06
§8 owns the repository-settings rule.

### 1.3 Source-size reporting

`just density` and the daily gate run `tools/checks/check_prod_loc.py` as an
informational source-size report. There is no historical growth baseline,
strict density mode, or justification-file requirement.

`check_prod_loc.py` derives its scopes from each package's `layer`: `l0` is
core plus facade, `below_facade` the common, engine, protocol, provider, store, and
harness layers, `host` and `product` the packages above the facade, and `test` the
repository-only packages — reported separately, with `library` (`l0` + `below_facade`) kept for continuity. `--json` emits the path-rule responsibility
inventory, every source file with its LOC, and the reconciliation between the
subtotals.

The metric counts physical lines, including comments and blanks. It excludes
balanced inline `mod tests` bodies and named inline modules explicitly gated by
`#[cfg(test)]`, together with their attributes; production after a test module
still counts. Registered external test sources and test-file conventions are
excluded separately. Other conditional code (including platform branches,
`test-support`, and test-only instrumentation outside these modules) remains
conservatively counted. This is a source-size metric, not compiled reachability.
`python3 tools/checks/inventory_large_rust.py` emits a planning inventory of tracked
Rust sources under `crates/` and `bin/`: physical/test/candidate counts,
responsibility totals, remaining conditional sites, source/data inclusions,
declaration locations, and recent file touches. It does not expand generated
build output or resolve imports into a dependency graph. Accounting corrections
must be reported separately from production-code reductions.

## 2. Offline test conventions

Repository tooling tests live in `tests/tooling/`; opt-in live scenarios and
their status page live in `tests/live/`. Build helpers
and their authenticated inputs live in `tools/build/`, repository policy in
`tools/checks/`, and measurement helpers in `tools/perf/`. `scripts/` holds the
small maintainer entrypoint surface and delegates to these owners.

Doc comments are gated on the same footing as tests: the `== rustdoc ==` step
runs `cargo doc --workspace --no-deps --all-features --locked` under
`RUSTDOCFLAGS="-D warnings"` between clippy and nextest, so a stale intra-doc
link fails the gate instead of rotting unnoticed.

### 2.1 Placement

Test code has one home per kind, and production behavior is never moved into
test code to change density accounting.

| Kind | Home |
| --- | --- |
| Small private-invariant tests | Inline `#[cfg(test)]` in the crate |
| Public component contracts | That crate's `tests/` directory |
| Reusable deterministic doubles and fixtures | `tests/support/plakous-test-support` |
| Cross-crate, crash, product, protocol, and performance procedures | Workspace consumer packages under `tests/` |

The `tests/` tree holds the consumer packages:

| Path | Package | Role |
| --- | --- | --- |
| `tests/support/plakous-test-support` | `plakous-test-support` | Scripted models and tools, scenario and barrier helpers, conformance drivers. Reachable only through the public `plakous-model` and `plakous-tools` contracts; never reexported by the facade |
| `tests/harnesses/offline-crash` | `plakous-offline-crash-harness` | Public-facade SQLite `SIGKILL`/reopen matrices, and the declarative offline model script (§2.6). Depends on `plakous` with `sessions-sqlite` and exports narrow loopback builders to `plak` as a dev-dependency, and as a `test-hooks`-only optional dependency. Its finalization-debt fixture uses `plakous-agent` and `plakous-store` to commit a real debt-only checkpoint for hosted recovery tests without exposing checkpoint internals to the hosted crate |
| `tests/fixtures/{agent,facade,model,tools}-consumer` | not workspace members for the first two | The supported external workflow inventories driven by `check_public_compile.py` |
| `tests/fixtures/authority-negative` | — | The 26 compile-fail import pins |
| `tests/performance/baselines` | — | The perf-smoke seed rows and the allocation probe |
| `tests/live` | — | The live drivers, the status page, and the pre-release bar (§3.2, §3.4) |

The reference, provider, and scenario Plak home shapes live outside that tree,
in the top-level `configs/`, and are loaded by an offline `plak` test so
they cannot rot (§4).

Every path above is repository-only. Test-support, crash-harness, and fixture
packages are never publication candidates.

The offline crash harness production closure is public-facade-only: the
`sessions-sqlite` feature, a loopback `LanguageModel` and a loopback mutating
`Tool`, so it cannot reach a live or paid provider. Parent and child exchange
one versioned JSON-lines command stream, and the child fsyncs separate
provider and effect evidence before it reports each cut boundary. After
`SIGKILL` and reopen through `AgentSessions`, a cut accepts only the exact old
state, the exact new state, or a typed recovery-required classification, and
reopen or recovery never appends provider or effect dispatch evidence.
Recovery, context status, compaction, fork and rebind all return the same
recovery authority. The public facade has no hook at the private checkpoint
transition or compaction publication, so the provider-terminal to checkpoint
interval is labeled split evidence, closed by `OpenedSession::release`
returning its durable locator; the harness claims no direct injection at a
private publication fence and no process cuts of composition. Plak's own
product-journal and host `SIGKILL` matrices are private Plak tests.

### Model fuzz targets

The standalone nested workspace `crates/core/model/fuzz/` keeps fuzz-only
dependencies out of the production workspace and lockfile. Its targets cover
response assembly, semantic JSON canonical idempotence and serde reconstruction,
and portable-schema compilation, reconstruction and instance validation.
From that directory, use a pinned `cargo-fuzz` installation and an explicit
run bound, for example:

```sh
cargo fuzz run model_response_assembler -- -runs=10000
cargo fuzz run semantic_json -- -runs=10000
cargo fuzz run portable_schema -- -runs=10000
```

The local `.cargo/config.toml` puts build output under the repository's
`target/plakous-model-fuzz/`. These opt-in campaigns are not ordinary gate
evidence.

### 2.2 nextest profiles

`.config/nextest.toml` defines the store directory, two test groups, and the
profiles. There is exactly one profile beyond `default`: `offline`.

| Profile | Membership | Use |
| --- | --- | --- |
| `default` | Everything nextest selects | Ad-hoc single-package runs |
| `offline` | Everything nextest selects, with a 60-second slow timeout | The development bar. `just test`, `scripts/dev-check.sh` |

`offline` is hermetic: no test in the workspace reaches a network or reads a
credential. The gate and `just test` pass `-E` with the lane split of
§2.3. `just check` runs the parallel lane, the serial budget lane and the
Python tooling self-tests (including the gate planner and lane inventory); `just test`, platform and full also run the durability lane.

`default` and `offline` carry identical overrides, so a test behaves the same
whichever one selects it. `fail-fast` is off in both. The workspace slow timeout
is 60 seconds with termination after the second period; the budget lane raises
its own to 240 seconds (§2.3).

The `default` profile sets `leak-timeout = { period = "5s", result = "fail" }`;
`offline` inherits it, as do ad-hoc nextest runs using `default`.
After a test process exits, nextest waits up to five seconds to observe EOF on
its captured stdout and stderr, and fails if EOF has not been observed. A
shorter window false-failed in-process tests whenever the runner stalled under
gate load.
That result does not prove a handle physically remains open or count child
processes. Tests that own processes must still prove their own bounded, natural
or forced joins.
A prior LEAK observation under the runner's shorter passing default does not
by itself establish whether that test leaked a process.

The integration suites in `plak`, `plakous-agent`, `plakous-tools`,
`plakous-store`, `plakous-openai` and `plakous-compose` each link one
`integration` target rooted at `tests/integration/main.rs`. The former target
files are sibling modules, so their test names have the former target name as
a module prefix. Store's
`fixture.rs` is a library-test helper, not an integration target; the nested
SQLite conformance module retains its independently inventoried tests.
Product modules gate telemetry on `test-support`, offline end-to-end and TUI
tests on `test-hooks`, and reload rehearsal on `maintainer`; store gates its
SQLite module on `sqlite`. Default-feature `cargo test` compiles the ungated
modules, and the all-features gate includes every module. Relative fixture
includes follow the module location; manifest-root fixture paths do not move.

### 2.3 Serial groups and the budget lane

Three mechanisms keep wall-clock and resource assumptions honest, and none of
them relaxes an assertion to buy parallelism.

**Test groups** cap concurrency across the whole workspace:

- `native-auth-serial` (`max-threads = 1`) covers every `native_auth::` test and
  the `local_transport` binary. OAuth loopback and native subscription auth tests
  bind fixed ports and share process-local fixtures.
- `cli-contract-serial` (`max-threads = 1`) covers `cli_contract::` in Plak's
  `integration` binary, selected by package, binary and anchored module name,
  not the other modules sharing that binary.

**The serial budget lane** is one override block, repeated byte-identically in
`default` and `offline`, named BUDGET LANE in `.config/nextest.toml`. It carries
`threads-required = "num-test-threads"` and a 240-second slow timeout with
termination after the second period. Its members are the tests whose subject is
a wall-clock measurement rather than a scheduler outcome:

| Kind | Predicates |
| --- | --- |
| Interactive PTY budgets | `product_terminal_e2e_tests::` — the whole real-PTY end-to-end module, 20 tests: startup and first frame, ordered transitions and quiescent reopen, prompt exit, resume and restore, cancellation chords, draft editing, the control-center keys; `fresh_real_binary_shell_keeps_controls_and_draft_usable` covers fresh-shell controls, draft retention and exit |
| Observation parity | `observation_parity_tests::` — the whole module, 10 tests: plain turn, tool error, capacity block, clear, rebind, interrupt then second prompt, escape then quit, compaction, the cancelled-tool terminal, the JSONL terminal tool line |
| Process spawn, drain and cancellation windows | `exec_command_with_zero_yield_returns_the_joined_output`, `host_process_bounds_live_output_and_enforces_timeout`, `joined_process_pty_cancellation_and_timeout_join_the_group`, `cancel_during_a_cell_running_a_real_process_command_joins_inside_the_settle_window` |
| Python worker request deadline | `a_stuck_worker_request_is_killed_and_reported_as_session_loss` — isolates bundle setup from parallel load without weakening the stuck-request kill and fail-closed loss assertions |
| Nonblocking joined-proof probes | `terminal_status_without_joined_owner_is_not_durable_proof`, `contended_operation_does_not_stall_joined_proof_probe` — routine reconciliation must not spend the cleanup deadline on a busy owner |
| Auth deadlines | `cancelling_claimed_refresh_after_request_observation_restores_ready`, `local_browser_rejects_wrong_host_without_dispatch_and_returns_no_store` |
| Host quit, compaction and session round trips | `interactive_observation_timeout_releases_gate_and_joins_without_masking_failure`, `interrupt_then_quit_publishes_the_cancelled_terminal`, `quit_during_active_turn_waits_for_cancelled_terminal_projection`, `scheduled_compaction_failure_keeps_host_worker_alive`, `product_session_facade_create_turn_shutdown_and_reopen`, `retiring_a_live_child_advances_its_session_locator`, `real_session_late_start_ack_releases_after_deadline_without_redispatch` |
| A real race, listed not hidden | `completed_terminal_outcome_remains_authoritative_when_exit_was_requested` |
| Product durability scenarios | `durability::` — quit, resume and failure-wave tests. Not crash matrices; they stay in this lane |

`native_ready_lease_is_held_through_head_and_released_before_stream_body` runs
in the parallel lane. Its logout-start, response-head and response-body barriers
prove exclusive store-lock contention before the head, then await successful
logout while the body is still gated; there is no negative timed wait.

The generated-actor fault schedules and success permutations run with paused
Tokio time in the parallel lane. Their case and dispatch/join deadlines measure
idle scheduler progress, not host speed; dispatch polling sleeps so an absent
dispatch reaches its virtual deadline instead of yield-spinning. A negative
no-dispatch fixture pins the elapsed deadline error. Tokio inhibits automatic
time advance during blocking work, so joined blocking cleanup is not mistaken
for a lost wakeup. All joined-terminal assertions and fixed seeds remain intact.

The module predicates select whole suites. PTY and observation-parity tests
share the host turn machinery and its wall-clock budget, so the module is the
unit rather than an arbitrary subset of its tests.

`local_browser_rejects_wrong_host_without_dispatch_and_returns_no_store` is in
the lane despite running in 0.01 seconds. Its budget is internal: it races the
loopback callback against a `tokio::time::timeout(Duration::from_secs(2), …)`
on the login completion, and `??` turns an elapsed timeout into a failure. The
fixed callback port is already serialized by the test's own gate and by
`native-auth-serial`, so the port is not the race — the two seconds are.

Reserving the pool is not enough on a loaded machine: a reservation stops other
*tests* from running alongside, but nothing stops the reservations from
interleaving with each other, and each of these is then measured against
whatever the machine was doing. So the gate splits the suite instead.

**The durability lane** is a second override block, also repeated byte-identically
in `default` and `offline`, named DURABILITY LANE. It carries the same
`threads-required = "num-test-threads"` and 240-second slow timeout. Its members
are crash matrices and the sqlite SIGKILL commit campaign. They are slow because
they relaunch a process and fsync through a crash cut, and they depend on OS
signal and fsync behavior, so they run per platform (`just platform` and
`just full`), not in `just check`. `just changed` selects `tests-durability` when
a changed path is in a package that owns one of these tests (`plak`, `plakous`,
`plakous-store`, `plakous-offline-crash-harness`), scoped to that package and
its reverse dependencies.

| Kind | Predicates |
| --- | --- |
| Indexed crash-point sweep | `crash_sweep::durability_subset` — `final_only`, `direct_tool_then_final`, `promoted_background_job`, and `sealed_family_artifacts`, every indexed commit, sixteen workers |
| Real SIGKILL relaunch | `actual_host_sigkill_relaunch_matrix_uses_real_handlers_and_reconcilers` |
| GC after a compaction cut | `gc_after_a_compaction_cut_before_projection_skips_the_pending_workflow` |
| Product projection SIGKILL matrix | `plak_product_projection_sigkill_matrix_converges_from_the_feed` |
| Harness direct-route SIGKILL matrix | `direct_route_cuts_have_one_deterministic_reopen_contract_each` |
| Crash-matrix completion control | `complete_run_is_one_exact_new_state_and_probe_is_inert` — the same harness child and evidence probe, without SIGKILL; the bounded child lifetime is the load sensitivity |
| SQLite paid-compaction SIGKILL | `sqlite_paid_compaction_sigkill_reopens_exactly_once`, `sqlite_no_locator_paid_compaction_sigkill_is_never_redispatched` |
| SQLite SIGKILL commit campaign | `bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit` |

`durability::` stays in the budget lane, as that lane's table says.
`unsupported_runtime_schemas_refuse_without_changing_files` and
`real_binary_refuses_unsupported_runtime_without_moving_state` are not members
of either lane; they run in the parallel lane.

**Three filtersets.** `scripts/dev-check.sh` and `just test` never restate the
lists. `tools/checks/budget_filterset.py` reads both overrides (`--durability`
is the durability filter, `--exclude` is `not (<BUDGET> | <DURABILITY>)`,
`--names` is the budget names).

1. Parallel: `-E 'not (<BUDGET> | <DURABILITY>)'` at normal parallelism.
   `--fast` runs only this invocation, narrowed as §1 describes.
2. Budget: `-E '<BUDGET>'` with `--test-threads=1`. Check, coverage, platform and full run it.
3. Durability: `-E '<DURABILITY>'` with `--test-threads=1`. `just test`, platform and full run it. Check and coverage do not.

The three expressions partition the suite, so a run that includes all three
executes every test once. Check deliberately skips the durability expression.
Both overrides keep `threads-required = "num-test-threads"`, so an ad-hoc
`cargo nextest run` that does not split the suite still reserves the pool for
either family.

`tests/tooling/test_budget_filterset.py`, part of the structure checks, proves
that the three invocations partition the suite, that the budget lane union the
durability lane equals the pinned serial inventory (the previous serial set
plus enrolled tests such as `crash_sweep::durability_subset`), that neither lane is restated
inline, that `default` and `offline` carry both lanes, that the PTY module
predicate stays in the budget lane, and that every predicate resolves to a
definition under `crates/`, `bin/` or `tests/` — an `fn <name>` for a bare name,
a `mod <name>` for a `<module>::` predicate, or the named source module's
`fn <name>` for a qualified `<module>::<name>` predicate.

The lane makes
`completed_terminal_outcome_remains_authoritative_when_exit_was_requested` quiet,
not correct: it is a genuine race in `crates/hosting/hosted/src/runtime/turn.rs`
("authoritative result was not ready") that has no readiness gate today. It is
listed
here rather than buried in a retry.

### 2.4 Goldens live in the crate

The durable tool-descriptor golden checks each real process registration against
both enrolled shell-profile bindings, then substitutes a fixed test-only process
binding for serialization pins. Shell device/inode authority varies by host;
it is not a portable format constant. Actual IDs, definitions, other bindings
and nested eligibility remain unchanged in the fixture. No production binding
or containment descriptor is normalized.

Frame goldens for the TUI are the only oracle kept in files. They are
committed under `bin/plak/tests/goldens` as 39 `.frame` files, one per surface
and terminal size — chrome at two widths, each tab at 80x24 and 120x40, the
resume picker, the Agents list (live story in both themes at 120x40 and 40x40,
plus lifecycle sealing), control hover, the transcript frames
(compose collapsed/expanded/in-flight/cancelled, spacing, thoughts, tool rows)
and the offline usage-limits overlay. `PLAK_UPDATE_GOLDENS=1` rewrites them
from the render; without it the test compares and fails with the diff. They are
in-crate and committed because the TUI is inert and presentation-only: a frame
is a pure function of state, so a moved byte is a real behavior change.

Every other oracle is pinned in code, so a fresh clone passes with no artifacts
present. The full inventory:

| Oracle | Where | Re-pin with |
| --- | --- | --- |
| TUI frames | `bin/plak/tests/goldens/*.frame` | `PLAK_UPDATE_GOLDENS=1 cargo test -p plak` |
| Wire identity goldens (host-protocol intents, observations, views) | `crates/hosting/host-protocol/src/wire_identity_goldens.rs` | `PLAK_PRINT_WIRE_PINS=1 cargo test -p plakous-host-protocol wire_identity -- --nocapture` |
| Durable tool-descriptor golden | `crates/harness/compose/tests/integration/durable_descriptor_identity_golden.rs` | `PLAK_PRINT_DESCRIPTOR_PINS=1 cargo test -p plakous-compose --test integration durable_descriptor_identity_golden:: -- --nocapture` |
| Pack prompt pins (each pack's `pins()`, checked by `drifted_pins`) | `crates/core/tools/src/prompt_digest.rs`, each pack's `crates/harness/tool-{files,search,process,plan}/src/text.rs`, `crates/harness/compose/src/text.rs`, and `crates/orchestration/recursive-subagents/src/tree/text/mod.rs` | the failing assertion prints each drifted id with its new digest |
| Rendered route, lifecycle and wire pins | `crates/hosting/hosted/tests/common/{rendered_prompts,rendered_lifecycle,rendered_wire}.rs` | `PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_ -- --nocapture` (§2.5) |
| Observation parity goldens (the host loop and the JSONL projection emit the same sequence) | `bin/plak/tests/parity/*.jsonl` | `PLAK_UPDATE_GOLDENS=1 cargo test -p plak --lib observation_parity` |

### 2.5 Prompt renders are pinned in code, not in files

What a model sees on each product route — harness costume, identity, enrolled
`AGENTS.md`, every tool name and description, and the first-turn HTTP request
body — is pinned by sha256 tables in `crates/hosting/hosted/tests/common/rendered_prompts.rs`
and `rendered_lifecycle.rs`, with the wire lowering in `rendered_wire.rs`. They
are private fixture suites compiled into the `plakous-hosted` unit-test target,
the crate that builds a session
from route, composition, packs, topology and prompts together. The
tests render in memory and compare digests. They read no file, so a fresh clone
passes with no artifacts present.

The human-readable render is a local review artifact:

```text
just prompts        # PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_
just prompts-diff   # copy the previous render aside, re-render, diff -ru
just prompts-pins   # PLAK_PRINT_PROMPT_PINS=1: print the new pin tables
```

Output goes to the git-ignored `target/prompt-renders/`, overridable with
`PLAK_PROMPT_ARTIFACT_DIR`. Without `PLAK_UPDATE_GOLDENS` the same test still
performs the full render and checks the pins; it just writes nothing.

Beyond the digests, the tests assert file-free structural facts: the route
matrix is exactly 76 included routes plus one documented exclusion
(`native-ptc` is enrolled only as Codex Code Mode); the rendered wire JSON equals the
kernel-lowered request and the Markdown `## Request` fence equals the jsonc
formatter's output; the harness footprint excludes the enrolled `AGENTS.md` and
the scenario assignment, subtracts enrolled source bytes, and keeps the Claude
costume identity on Claude routes; and every product tool extension's name and
description reaches the model verbatim, either as a lowered `tools[]` entry or
in the harness instruction text.

The same suite holds the two normative harness-footprint budgets (chapter 02
§6.7): a basic single node under 4,000 approximate tokens, the fullest surface
at or under 8,000. `rendered_prompt_footprints_stay_within_budget` fails the
build for any route over its budget, naming the route and its instruction and
tool split, and prints a table of every route's footprint whether it passes or
not, so the cost of a wording change is read rather than re-derived.

A prompt change is meant to fail the test with the list of routes whose digest
moved. The workflow is: `just prompts-diff` to read the model-visible sentence
changes, then `just prompts-pins` and paste the new rows over the tables. A
rendered artifact is never edited — `just prompts` overwrites it from Rust.

Vendor CLI captures under a gitignored local capture directory come from
`just native-compare-prompts`. Installed binaries run with isolated configuration
and synthetic credentials against local mock responders. Claude is driven
through its interactive CLI, not the print/SDK entrypoint. Its synthetic OAuth
request retains the original API origin through a local CONNECT recorder.
Codex control-plane calls use `chatgpt_base_url` on the local mock. The
responses turn uses `openai_base_url` ending in `/backend-api/codex`: an unset
provider base stays on `https://chatgpt.com/backend-api/codex` and never
reaches the mock.
The recorder rejects unknown CONNECT destinations rather than forwarding them.
Captures redact authorization, cookies, API keys, OAuth secrets and e-mail
addresses. They record the installed version and capture conditions; synthetic
identity and mock responses do not qualify live authentication, provider
acceptance or refusal recovery. These are comparison witnesses, never runtime
inputs or a gate. An existing CLI request log may also be ingested explicitly;
ordinary native debug logs are not guaranteed to contain request bodies.
Explicit qualification/reference OAuth/API wire captures belong in gitignored
`tests/provider-authority/`; ordinary maintainer capture belongs in
`PLAK_HOME/wire-captures/` (prefer a private home outside the checkout);
§3.5 is the safe collection and sanitization procedure. Do not commit raw
traffic or treat a local structural capture as live provider qualification.

Plak's private inference observer is available in `maintainer` or `test-hooks`
builds. Maintainer builds select fresh private capture automatically in the
product home, including private scratch homes inside the checkout. Selection
is lazy: the first inference exchange creates and logs the run, shared by later
roots and reattachments in that process. Commands that send no inference
create no capture directories and log no capture-selection message. Explicit
capture validation likewise happens before that first exchange can dispatch.
Automatic capture is passive and never blocks inference on recording failures.
Maintainer startup enables safe `plak.capture=warn` file diagnostics
in `PLAK_HOME/logs/plak.log`, including alongside `PLAK_LOG` target selection,
not TUI stderr. The live smoke and soak drivers also require capture for every process and default to
release executables. Explicit `PLAK_TEST_EXCHANGE_CAPTURE_DIR` selection in either
feature build is instead required, fail-closed qualification capture.
Capture persists mode-0600 `.request` bodies and upfront
`.info` crash markers before dispatch, then `.body` consumer response bytes and
terminal metadata, syncing request and terminal evidence and the directory
links for newly created capture runs. Pending captures
remain incomplete. The required observer refuses dispatch on start failure
and reports response/terminal IO or capacity failure to the transport.
Request and response limits are 8 MiB each; neither mode has a 512-start lifetime
limit. Automatic capture rotates segments at 128 starts or earlier under byte
pressure, sealing a partially filled current segment, and budgets 1 GiB per
process run (request bytes, actual response bytes, 4 KiB metadata allowance per
exchange, maximum response reservation while in flight). Before declining a new
recording, it reclaims oldest closed successful segments in that active run:
every observation must have been dropped after `Complete` or validated typed-terminal
`ProtocolComplete` with `incomplete=false`, and no HTTP error status.
Arbitrary `ClosedEarly` does not qualify. Unsealed current segments, pending, abandoned, failed and active
observations are protected. A private, synced `process/retention.info` records
merged actual exchange ranges selected for removal and `full_run=false` before deletion;
an interrupted cleanup may retain some selected files. This summary prevents a
retained suffix from being mistaken for full-run evidence. If protected/current
evidence exhausts the unavoidable disk limit, it declines
recording, persists a sticky `process/incomplete.log` and diagnoses incomplete
evidence, without refusing inference. Failed starts conservatively retain their
reservation. Startup retention keeps eight
successful unlocked automatic-v1 runs plus all locked active runs and all runs
with pending, failed, non-success or incomplete `.info` metadata or `incomplete.log`.
Retention inspects only bounded private metadata, never raw bodies, and preserves
unsafe or unrecognized layouts. Locks remain held by
in-flight observations. Legacy/explicit/reference directories without the
automatic-v1 marker are untouched. Retention failures preserve remaining files
and diagnose failure; many active processes, preserved failure evidence, unmanaged evidence and failed
cleanup can exceed the target. Explicit qualification has no run budget or
automatic retention; its driver owns that lifecycle. Metadata correlates run
path, process ID and process-wide exchange number; a failed terminal carries the
transport's redacted failure facts (spec 01 §13.1: phase, transmission, kind, cause
category, HTTP/2 reason, OS error number, TLS cause and alert code, negotiated
ALPN, HTTP/2 I/O origin, connection use) and nothing else; every `.info` also
records `connection=` as `Some(Fresh)`, `Some(Reused)` or `Some(ReplacedStale)`,
or `None` when no response head arrived or a test fault injected the response,
and the capture warning names it; the body-only observer
boundary has no session/turn identity. Pending markers, failed exchanges and
run-level gap or retention markers prohibit a claim of complete wire evidence, regardless of
inference success. This is not a recording of every network byte or token
refresh; no HTTP/auth headers are captured. Bodies remain private task data.
Codex's typed terminal explicitly closes and joins its body without waiting
for network EOF; a pooled client keeps the connection only when that end was
already received (spec 01 §13.1); required terminal-recording failure prevents success. Required
capture failures on refusal bodies remain transport failures, not retryable
provider status responses.

### 2.6 Spawned-binary lifecycles

A class of defect lives only in the process: how a signal ends it, what its
writer does when the reader stops reading, whether a failed shutdown still
terminates the stream, and whether a session an earlier process wrote reopens.
No in-process test reaches any of it, and each of those was found live rather
than in the gate.

`bin/plak/tests/integration/offline_e2e.rs` spawns the shipped executable through
`env!("CARGO_BIN_EXE_plak")` and drives all three faces — `plak run`, the
`plak agent` JSONL stream, and the terminal application on a real pseudoterminal
— against a deterministic in-process model. There is no provider, no credential
and no network.

The model is selected by two environment variables the binary reads only under
the `test-hooks` feature: `PLAK_TEST_MODEL`, which must name
`offline-loopback-v1`, and `PLAK_TEST_SCRIPT`, which names a JSON document.
A hook that is requested and unusable refuses the launch; it never falls through
to a provider, because a gate run that silently tested nothing is worse than a
failing one.

The document is `plakous_offline_crash_harness::script::ScriptPlan`: a version,
a list of assistant steps, and the fixture dials the compaction ladder needs. A
step names its output (text, a tool call with arguments, a reasoning-only block,
or nothing), its stop reason, its usage figures, an optional stall, and an
optional provider fault — refused before the stream, reported in band after a
named number of events, or reported after a stall. The script's `fold` says
whether a semantic-summary request is answered or refused, and a refused fold is
what drops the compaction ladder onto its offline placeholder rung. This is the same
`LoopbackModel`, descriptor, runtime authority, tool registry and boundary
controller the Rust loopback builders use; only the round-by-round shape comes
from data.

`bin/plak/tests/integration/offline_e2e.rs` module `durability` chains those gestures in one
spawned session. Its tool/cancel/compact/resume chain uses an accepted bounded
summary: when retained tool output clips the next turn, it checks that turn
ends without replay, compacts for subsequent work, and answers a new explicit
prompt before quitting and reopening. A `tree` script may also set
`durable_tree` (publish through the product `subagents.sqlite3` so
`plak session show` reads the same rows),
`disk_full_at_child_locator` (the first bound child locator publish and all
later saves return the classified `disk_full` category and no backend text;
the child row lands before the fault, independent of projector save counts),
`child_hold_ms` / `child_hold_after` (later children stay running after the
locator is published), and `fail_child_after_successes` (a later bind returns
recovery-required). Those dials are refused unless `tree` is set, and
`disk_full_at_child_locator` requires `durable_tree`. They are fixture controls,
not product configuration. A scripted stall observes cancellation unless the
script sets `stall_ignores_cancellation`. On an attached offline face,
`PLAK_TEST_PROVIDER_RELEASE` selects a file gate at the first physical provider
dispatch: the operation waits, observing cancellation, until that file exists.
This repository-only hook uses the harness boundary controller, not a scripted
delay or a persisted-format change. With the offline model selected,
`PLAK_TEST_ASSISTANT_DONE_RELEASE` file-gates the face after writing
`assistant_done`, before checking stdin. Atomic release with `quit` waits for
the already-sent input frame before consuming another host event; release with
`release` does not require input. A gate containing `idle` drains host events
through the actual busy-false observation before checking stdin. This
deterministic scheduling hook never applies without the offline model. The
failure-wave chain checks the
failed child on both the JSONL face and the TUI, then sends another root prompt.
It does not require root refusal while a child locator is unavailable: child recovery
inspection and root admission are separate facts.

`test-hooks` is not in `default`, is never enabled in a release build, and
carries exactly three edges: the harness that holds the model,
`plakous-hosted/test-support`, the injection point that admits it, and
`plakous-recursive-subagents-store/commit-trace`, tracing the already-present
tree-store dependency. The direct tree feature avoids a harness-to-tree normal
edge cycling through the subagents crate's harness dev-dependency. All are
pinned — `check_workspace.py` refuses any other contents for the feature, and
`check_features.py` requires the forwarding edge, without which the injection is
compiled out and the binding would be inert. The default and release binaries
contain none of the selector's strings. The harness also carries
`plakous-http/fault-injection` (§2.8) unconditionally, so `test-hooks` is the
one product edge to transport fault injection as well.

**Crash-point sweep.** `just crash-sweep [--shapes SHAPES] [--jobs N]
[--repeat R] [--strict]` runs the ignored `crash_sweep::full_sweep`; the always-on
`crash_sweep::durability_subset` covers `final_only`, `direct_tool_then_final`,
`promoted_background_job`, and `sealed_family_artifacts` with sixteen workers.
The process pilots prove a real shell job was promoted and quit settled it;
the sealed pilot additionally proves promotion of managed native family bytes.
PID evidence is scoped to each fixture root, never a workspace shared by cuts.
Each shape starts from a clean
post-setup home. Twin pilots must agree on indexed (store, label, tables)
sequences independently per store and establish the live publication/tool/dispatch
facts. Completing shapes wait for the face's `busy:false` after their `assistant_done`
before sending terminal input; the compaction-workflow shape also waits for
`busy:false` after its compact tool terminal before quitting. A pre-marker idle
record or an idle status alone does not acknowledge that boundary. Both pilot
twins use the existing answer-release gate: the first exposes the answer marker
before face idle and consumes any prematurely sent quit before another event;
the second drains events through `busy:false` before checking stdin. This pins
the legal face schedules without adding an interrupt to the scripted inputs.
The strict corpus also includes `busy_quit_escalates`: a cancellation-deaf
provider emits a prefix and then stalls, so quit is sent while busy, exhausts
the cancellation grace and result-join bound, and releases only the lease
without inventing a terminal. Both pilots must have identical per-store traces;
the pilot proves the released head is still Working and requires abandonment
on reopen. Every indexed startup, admission, escalation and release commit is
cut, with the same outcome/verdict table and convergence checks. A prompt cut
before admission resumes normally from its restored queue, then completes once.
Cross-store interleaving is free. Every per-store indexed commit, including
startup and shutdown, is killed in a fresh home copy with `<store>:<k>`. The
child must exit with signal 9; its targeted store must emit exactly the pilot
prefix through k, and every other store must emit a prefix of its pilot sequence.
Each prefix compares both labels and tables. Non-target store progress and durable
post-state are not compared for equality across cuts or repeats: interleaving
can change how far those stores got before the targeted commit.
The trace global order and final global ordinal are recorded, informational only;
divergence is retried twice, then fails with its diff. One lifecycle step is
bounded to 45 seconds without observed stdout or durable commit-trace progress;
each new record or commit renews that stalled-step budget. Slow startup and
several individually progressing steps do not share one lifecycle wall-clock
deadline. Commit-prefix equality, outcome classification and convergence
assertions are unchanged.

Seeded shapes capture their post-setup durable summary once and classify only
new operation ids, commands, feed records, submissions and queue items beyond it.
Finalized seed commands are not evidence that the swept original was admitted;
an unchanged seeded baseline is `NOT_ADMITTED`, or `REQUEUED` for a new queue item.

Read-only summaries bind the runtime head, recovery phase and ordered
(operation, action) obligations, decoded ledger-state signature and durable
answer marker, promoted artifact family/schema/codec and native-byte digest,
content-free unconsumed job rows, the runtime feed tail and the decoded fact
kinds after the setup baseline, the product and tree cursors, the product head
view, submission, workflow-request and queue rows, transcript counts, tree nodes
and physical dispatch identities. Offline rehearsal must predict the actual
runtime closure and the number of records the projection is behind, and its
runtime/product/tree main and WAL bytes must be unchanged (SHM is transient
reader coordination). The session-workflow compaction cut has a compact request
with `WorkflowAdmitted` and no settlement; the runtime closes it on open by its
workflow row (chapter 01 §5.5) and the projector derives the settlement's rows
from its `WorkflowSettled` fact: for `Abandoned`, exactly one not-applied notice
naming proven-unsent summaries as not sent and otherwise preserving the
may-have-been-billed warning; for a Completed summary, the `Compacted`
settlement's rows. The cut must retain its answer, dispatch no
admitted fold again, and the second reopen must converge.
Relaunch one closes/reconciles, runs a follow-up, and quits; relaunch two must
converge without new dispatch or publication (idle attach/quit may advance the
CAS revision, which is recorded but not semantic turn authority).
Converged-state checks after each reopen first require the product cursor to
equal the runtime feed tail, without a harness projector pass. The subsequent
harness pass must leave the views unchanged; it cannot mask unpaid reopen debt.
Refused relaunches must preserve the entire decoded summary, including revision; WAL
checkpoint representation is not an authority change. Fixture PID files reap
orphans.

The table has five classes: `COMPLETED_ONCE` (exactly one expected terminal
publication: an answer proved by the original root command's completed terminal
and matching published assistant row, independent of fixture text, or, for
`capacity_overflow`, a capacity denial; shape markers remain content proofs),
`ABANDONED{claim}` (one honest typed interruption notice, never over a durable
answer), `REQUEUED` (the prompt is restored to a paused queue, its submission
pending with no feed fact), `REFUSED{name}` (typed exit 5 and a signature-keyed
allowlist name, with durable authority unchanged), and `NOT_ADMITTED` (no
submission and no runtime root command for the prompt, no answer, clean reopen,
exactly one follow-up dispatch). `UNCLASSIFIED`, invariant violations and
divergent traces are findings and fail the run.

**Product and tree cuts always converge.** A cut at any `product:<k>` or
`subagents:<k>` commit can only leave a projector cursor behind the feed or an
intent without a fact, so relaunch one MUST converge: it is never `REFUSED`, and
no product or tree signature may enter the allowlist. After relaunch one the
product and tree cursors equal their feeds' tails, the head view equals the
runtime head, every submission and workflow request either has a fact or is
pending behind a paused queue, and relaunch two changes no view row.
The signature-keyed REFUSED allowlist is empty and stays empty: recovery
replays recorded decisions (chapter 01 §5.5), every Working head this build
writes has a closure row, and an uncertain external effect is closed as outcome
unknown rather than refused. A `REFUSED` row in any shape, subset or opt-in, is
a finding. A refusal may only be accepted by adding a row to this chapter that
names the signature and the genuinely uncertain external effect no closure can
honestly settle; there is no such row.

The signatures below close as follows. A final
answer's or capacity failure's `Completed` provider is the decision commit
of chapter 01 §3.5 item 1: it carries the recorded `Terminal` or `FailTurn`
decision, and the reopen finishes the turn from it with the live consequence
commit, so the original answer or capacity denial is published once.

| Former signature | Durable post-state | Expected outcome after J |
| --- | --- | --- |
| `code_mode_cell_final.7c3eca1e96` (runtime:10, product:9) | root Dispatched; provider Completed `ToolBatch`; evaluation Completed | `ABANDONED{RunningTool}`, finished-but-not-added wording; every record keeps its own evidence |
| `code_mode_cell_nested_then_final.e99d839a60` (runtime:17, product:12) | as above, plus nested child and effect Completed | `ABANDONED{RunningTool}`, same wording; the nested effect is named and never replayed |
| `code_mode_cell_final.fdcce09124` (runtime:11) | root Dispatched; provider, evaluation Finalized (`ToolBatch` exchange committed) | `ABANDONED{ToolRoundCompleted}` |
| `code_mode_cell_nested_then_final.89657b4651` (runtime:18) | as above, plus nested child and effect Finalized | `ABANDONED{ToolRoundCompleted}` |
| `code_mode_cell_final.8b6a4d6455` (runtime:14) | final provider Completed `Terminal` with an answer (the decision commit) | `COMPLETED_ONCE`: finished from the recorded decision |
| `code_mode_cell_nested_then_final.b2ea8aab19` (runtime:21) | final provider Completed `Terminal` with an answer | `COMPLETED_ONCE`: finished from the recorded decision |
| `hosted_tool_call_final.6618879652` (runtime:15) | final provider Completed `Terminal` with a provider-hosted ToolCall and an answer | `COMPLETED_ONCE`: finished from the recorded decision; the hosted call never dispatches locally |
| `capacity_overflow.58600eb911` (runtime:7) | provider Completed `FailTurn` with a `ContextLimit` failure, no terminal | `COMPLETED_ONCE` (capacity denial): finished from the recorded decision, its withheld-prompt choice and remedy included |
The subset keeps all four shapes at every install and reserves the nextest pool
while running `min(16, available_parallelism)` independent cut workers internally.
The inventory test pins both the four-shape membership and that worker bound;
no cut or assertion is dropped.

The real-host matrix retires only cuts subsumed by this install subset. The
subset's `coverage::retired_host_cuts` guard requires the corresponding decoded
states in successful rows, not fixed ordinals (store interleaving remains free):

| Retired real-host RootDelivery cut | Sweep replacement and assertions |
| --- | --- |
| Prepared | `direct_tool_then_final`, submission written before root admission: restored queue, pending submission resubmitted under its own command id on resume, clean follow-up and convergence |
| RuntimeReturned | `direct_tool_then_final`, root command recorded with the projection behind: offline closure, no recovery dispatch, one honest notice derived from the abandonment fact |
| ProductPublished | `direct_tool_then_final`, the projector transaction that applied `CommandAccepted`: consumed item, exact head view and no duplicate dispatch or row |
| ProductCommitted | `direct_tool_then_final`, a later projector transaction of the same turn: same closure and second-relaunch convergence |
| TerminalReady | `direct_tool_then_final`, `TurnTerminal` committed with the projection behind: exactly one original answer derived on reopen, head view at the quiescent head and no redispatch |

Missing replacement rows fail the subset. All rows
add read-only runtime/product/tree rehearsal, exact per-store commit prefixes,
physical dispatch uniqueness, canonical notice/usage uniqueness, and two
relaunches; the retired hand-written cuts did not check all of these.

Keep the CompletedToolRound trigger: the sweep's post-kill durable job facts
do not prove physical-process liveness at the pre-kill boundary. Keep the
Finalization and FinalizationPendingTerminal triggers: their exact
prior and final sealed candidates and mismatched-command rehearsal refusal are
not asserted by the subset. Keep reclaimed delivery after another face advances,
SendNow, Compact, Fork and Rebind cuts, product-projection component matrices,
GC-before-compaction-projection, public-facade matrices and SQLite atomicity
cuts: none is fully subsumed by the four install shapes. The two 31-second lease
waits remain for those retained real-host/GC cases; they protect the production
ownership fence and cannot be replaced with a shorter sleep.

No shape has an allowlisted refusal, and a new shape enrolls none: a refusal,
an UNCLASSIFIED row or an invariant violation is a finding in every shape.
`--strict` and the default run therefore agree on refusals. No duplicate
dispatch, derived terminal row, usage or canonical process notice receipt is
permitted. Observational `job_terminal` snapshots may rehydrate once per
attachment; they do not consume a model-facing notice. Non-refused final head
views must equal the quiescent head with the cursors at the feed tails.
Artifacts are `target/crash-sweep/<run>/sweep.jsonl` and `table.md`.

**Feed and projector laws.** The always-on offline lanes pin:

- *Feed conformance*: memory and SQLite backends append exactly one record per
  accepted mutation and none for a refused, rolled-back or renew-only one; the
  sequence is gap-free from 1 across branches, forks and rebinds; `recover_*`
  returns the record sequence of the original mutation; a cut at every runtime
  commit of the sweep leaves `feed_tail` equal to the number of accepted
  mutations; retention trims a prefix only, by the record and byte bounds, in
  transactions of at most 4,096 rows outside any publishing commit, and reports
  `BehindRetention` below it; a mutation whose record would exceed 1 MiB is
  refused before commit; `committed_at_ms` is stamped on every record.
- *Event/fact parity*: for every pilot, every live event that reports a commit
  names a `feed_seq` whose record carries the matching fact, and every fact
  kind is produced by at least one pilot or sweep row; each atomic transition of
  chapter 01 §3.5 (the decision and consequence commits of a turn's end
  included) and each closure-table and workflow row has a golden fact list.
  `a_cut_after_each_commit_of_a_turn_leaves_a_whole_transition` reopens every
  commit cut, with exactly one terminal and usage equal to the round ledger.
  `dispatch_rejected_fail_turn_cut_reopens_without_redispatch` and
  `dispatch_rejected_fail_turn_refused_consequence_reopens_without_redispatch`
  pin failure publication from a proven-unsent recorded decision.
  `recovery_required_terminal_enrichment_is_add_only_and_once` rejects terminal
  rewrites and repeated enrichment.
  `abandonment_terminal_report_survives_parent_pruning_and_sqlite_inspection`
  pins the recorded report without a retained parent.
  `finalized_terminal_without_root_terminal_is_integrity_not_restore` pins the
  refusal of a Finalized round without its root's terminal consequence.
  `turn_fence_workflow_after_recovery_required_closes_the_turn_not_an_endpoint`
  pins the interrupted turn row and TurnFence settlement for workflow-rooted debt.
  `a_terminal_result_reads_the_recorded_remedy_not_the_conversation` pins the
  persisted remedy as authority even when the conversation suggests another.
  `fold_commit_authority_is_its_own_finalization_not_history` pins fold-local
  publication evidence against unrelated or subsequently rewritten history.
  `a_recovered_context_limit_finish_preserves_the_withheld_prompt_remedy_and_note`,
  `a_recovered_dispatch_rejected_finish_equals_the_live_consequence_commit` and
  `a_recovered_refused_consequence_equals_the_live_proposal` compare the finish
  row's history, artifact, ledger, terminal facts, usage and duplicate-command
  result to the live consequence (including a refused live proposal).
- *Lingering owner*: a fenced host whose lease expired keeps running beside the
  new owner. Its projector transactions fail the cursor CAS or write identical
  views, its intent writes are revision-CAS'd, and every submission it makes is
  refused at the runtime fence; the new owner's views and physical dispatch
  counts are unchanged.
- *Reopen order*: a session whose runtime head moved past the product's cursor
  (another owner committed, the product never saw it) opens at that head with
  no stale refusal; a commit injected between the lease-free feed pass and the
  open is retried once, and a second injected commit is a lease-contention
  refusal, never recovery-required.
- *Row provenance*: a pinned table test enumerates every persisted
  `TranscriptRow` field and its one source (chapter 02 §2.6); a rebuild from
  empty reproduces every field the table sources from facts, reads and
  intents, and no live-only field is persisted.
- *Projector idempotence*: for every pilot feed and a generated corpus of
  well-formed feeds, applying the feed twice (including every page boundary and
  a replayed page) yields byte-identical views to applying it once, for the
  product and the tree projector.
  `converged_memory_views_are_byte_identical_after_replaying_every_feed_page`
  and `converged_sqlite_views_are_byte_identical_after_replaying_every_feed_page`
  replay the retained pages over converged real-host sessions: the cursor
  advances zero, complete views are unchanged, and no writes or successors are
  planned. The SQLite snapshot inventories all live schema tables, classifies
  intents explicitly and rejects any unknown table; every cell of every view
  table is compared byte-for-byte, including the keyed bookkeeping and head.
- *Rebuild equals incremental*: deleting every view row and cursor and running
  the projector from empty yields exactly the views the incremental live run
  wrote, for every pilot and every converged sweep row; a corpus with pruned
  checkpoints pins the head-read and `earlier output not retained` rules
  instead.
  `full_sqlite_view_rebuild_and_plain_reopen_equal_incremental_for_every_sweep_shape` compares
  the complete serialized SQL-cell snapshot before deletion and after a
  lease-free rebuild for all 14 strict-sweep shapes. Its live-schema inventory
  prevents a new view table from silently escaping the comparison.
- *Delete-and-reopen*: deleting the product views (and, separately, the tree
  views) of a sweep row's final home and reopening it yields a decoded summary
  identical to the undeleted reopen, including transcript keys and counters.
  The same law runs over an imported session (its imported transcript intent
  survives exactly and its seeded `Created` record derives nothing) and over a
  fork and a rebind successor (the rebuild follows the branch lineage through
  the `Branched` sequence).
  `full_sqlite_view_rebuild_and_plain_reopen_equal_incremental_for_every_sweep_shape` compares all SQL
  cells after exactly one ordinary JSONL-host reopen in each of two identical
  homes, one with intact views and one with every view table deleted, for all
  14 strict-sweep shapes. Each open must reach the feed tail at Ready, before
  any harness projector pass or subsequent Quit. Cursor equality compares
  the same logical point, including any facts the open itself commits.
  The Working busy-quit pilot first recovers once through an ordinary host,
  reaching the feed tail at Ready, before cloning the comparison homes. This
  gives both arms identical canonical closure facts instead of independently
  minted recovery checkpoint ids; no snapshot fields are normalized.
  The `composite_laws` corpus runs all three product laws over a successful
  manual compaction, fork and clear source/successor records, a recompiled
  rebind's source/successor records, a spawned and finished child, an actual
  export/import archive, and a definitive no-commit compaction whose keyed
  request was submitted and discarded. It reuses the complete SQL snapshot,
  retained-page idempotence and byte-copy plain-open twin helpers, with no
  normalization. Rebind is seeded through the same product intent and public
  runtime workflow, since JSONL has no profile-fork command; its twins open
  through the ordinary host. The fork/rebind cases pin inherited user send
  times and `row_bytes`: deleting successor views must still read the source
  submission intents, stopping at a clear/import runtime boundary. The normal
  pending-command join remains limited to the successor's own submissions.
  `composite_source_deleted_import_obeys_all_three_sqlite_laws` deletes the
  exported source and the imported views, then requires a read-only reload
  rehearsal and all three laws to succeed. Carry-forward ancestry stops before
  loading its source, which may already have been deleted.
- *Every product cut converges*: the subset asserts the convergence rule above
  for every `product:<k>` and `subagents:<k>` cut of its four shapes, with no
  allowlist entry possible.
- *Indexed tree-store cuts*: `indexed_tree_store_commit_cut_matrix` records
  every production `TreeDriver` save in a scripted uncut scenario, freezes
  its exact rows and bounded feed inputs, then cuts the `SQLite` WAL commit
  sequence at every `tree:<k>`. The scenario covers spawn and mail, a
  provisional wait acknowledgement and its canonical commit, Park, Nudge
  and Finalize, a failed child and its snapshot notice, outside cancellation,
  live and deletion retirement, materialization failure, seals and cleanup.
  Its oracle comes from the uncut commit-indexed rows, not the reopened store.
  For class-b cuts it derives the persisted orphan faults from those rows:
  each pending `seal_effect` becomes terminal faulted, and each in-flight
  `Adopting`/`Discarding` workspace yields a typed `workspace_fault` for its
  affected agent and attempt, with the exact request, digest and terminal
  fields of the fault rule. The row comparisons and minimum cut counts remain
  unchanged. Class-a cuts additionally assert that reopen creates no fault
  records. No oracle evidence is read back from the reopened database.
  Class **a** has no persisted in-flight VCS row. Class **b** applies the
  chapter 03 §14.1 fault rule to precisely the saved in-flight set; closed
  occupants of a discarding copy are not faulted. Both classes include the
  restoration-only pause rule and the bounded canonical wait evidence
  (an uncommitted provisional acknowledgement returns to unread).
  One open and a second open must match every column of every tree table,
  excluding only the intentionally advancing revision and host generation.
  The snapshot enumerates the live schema and refuses an unknown tree table.
  Both opens invoke no VCS operation or duplicate effect; the uncut scenario
  pins one failure notice, one result per settled child and one cleanup.
  The matrix prints each cut's class and requires at least 35 cuts, including
  at least 30 class-a and three class-b cuts. This is tree-store boundary
  evidence with authenticated feed fixtures and a recording VCS port, not
  hosted child-session relaunch, real VCS subprocess or live-resume evidence.
- *One code path*: the live host, open, export, `gc` and the rehearsal call the
  same projector entry point; a structure check refuses any other writer of a
  view table.
  `live_and_plain_open_enter_the_same_projector_and_all_view_writes_are_inside_it`
  counts the common driver entry by live/open caller through a test-only
  task-local probe. The memory store rejects view writes outside the common
  entry's blocking-store call for that tracked session, even beside an active
  pass (`a_view_writer_outside_the_common_entry_is_rejected_even_beside_an_active_pass`).
  The ordinary host worker drives both
  the live turn and stored-session reopen; the test does not call the driver
  directly. `only_the_projector_commit_writes_view_tables` independently pins
  the SQLite writer boundary.
  The common-entry probe also requires exactly one `Release` caller pass per
  published host release. `clean_quit_projects_the_release_record_without_a_harness_pass`
  checks the runtime tail and `released=true` immediately after a clean quit.
  `terminal_committed_during_release_is_projected_before_shutdown_returns`
  proves the pre-release views are Working with an open root, then requires the
  terminal committed by normal release to clear those views, render Interrupted
  and reach the tail before any harness pass. The quit-grace regressions pin
  the different emergency exit: it applies the release record but deliberately
  leaves a deaf-cancel Working head for runtime recovery (chapter 01 §2.4).
  `post_release_projection_refusal_is_cleanup_and_preserves_release_failure`
  proves a refused post-pass is a typed cleanup failure, with any child release
  failure retained and canonical ownership still reopenable.
  `post_release_timeout_is_benign_and_plain_open_preserves_all_three_sqlite_laws`
  holds a healthy SQLite writer through quit, requires successful shutdown and
  a logged post-pass timeout with unapplied release debt, then requires the next
  plain open to reach the tail at Ready before any harness projector pass. It
  runs all three complete SQLite laws after that open. The slow-projector
  one-shot and interactive regressions retain their successful-exit assertions.

The product schema's construction binding is write-once; the store tests
`construction_binding_is_write_once` and
`view_head_wins_without_rewriting_construction_binding` prove that subsequent
locator inputs leave the binding unchanged and a projected head never overwrites runtime genesis.
`host_controls_are_intents_not_session_flags` proves pause and wake suppression
come from the intent/view join, not session columns, including a projected resume.

The crash-sweep summary joins `submissions` and `workflow_requests` to their
feed-derived views; it does not read a delivery journal or workflow state
machine. Its classifier checks a closed outcome-to-runtime-verdict table beside
rehearsal parity; rehearsal and inspection share the runtime's pure predicate,
so parity alone is not a closure oracle. Abandonment requires
`RecoversAbandonedTurn`; completion permits `RecoversSavedTerminal`,
`ResolvesCompactionWorkflow`, `RecoversCommit` or `OpensQuiescent`;
not-admitted and requeued prompts require `OpensQuiescent`, and refusals require
the rehearsal's typed `Refused`. If the head belongs to a later root, a distinct
root submission joined to its accepted runtime command, admission sequence and
exact working turn must prove that identity. The later root's own running or
terminal lifecycle selects its table row; the original stays completed.
That lifecycle includes a dispatched root whose newest same-turn provider has
a saved `Terminal` or `FailTurn` decision: it is terminal-recorded, not running,
even before the root command's consequence commit. Intermediate round decisions
and other turns' providers cannot prove that state.
Capacity cuts are selected by SQL commit signature and decoded runtime/view
facts, never transaction ordinals; duplicate signatures are probed at their
exact cut. Converged reads run the lease-free projector, while reopen evidence
requires the reopen alone to converge before the harness checks idempotence.

**Recorded-decision regressions.** Offline `plakous-agent` tests pin chapter 01
§3.5 and §5.5 directly, independent of the sweep:

- Each recorded fact has a codec test and a live-driver test that the fact is
  written at the decision: every `RoundDecision` variant on its provider outcome
  record, each `TurnBudgets` field after the decision that changes it, failed-
  attempt partial usage, `steering-applied` finalization, and the root's direct
  `Dispatched` to `Finalized` terminal transition. A restored continuation or
  effect-decision frame resumes with the budgets its last decision recorded,
  and the content-filter retry stays spent after a fold drops its guidance
  note: `TurnBudgets` is the only authority.
- Each atomic transition is cut-tested with the offline crash harness at every
  commit of the transition's driver path: the checkpoint before the
  transition's commit and the one after it are the only reachable states, and
  no reachable state carries an applied steer without its segment, or a retry
  or `Continue` decision without its note or history. A turn's end has a cut
  between the decision commit and the consequence commit: the reopen finishes
  the turn with the recorded answer (or capacity denial) once, with the steers
  accepted after the decision settled unapplied, and a second reopen is
  `AlreadyApplied`. A store-full refusal of the consequence commit leaves the
  decision commit, and the next open finishes the turn the same way.
  The product's actual-host SIGKILL matrix keys its root finalization cuts on
  the receipt predecessor's recorded `Terminal` decision, not on obsolete
  split terminal commits. Both finalization cases leave the root dispatched
  and the terminal provider completed. Its completed-tool-round cut requires
  a recorded, finalized `ToolBatch` decision before the next provider is
  admitted. Relaunch and a second reopen must converge without redispatch.
  The actual-host matrix also proves exactly one tested root or steer submission
  per cut and no legacy delivery table. Send-now's `ProductCommitted` cut is
  distinct: terminal publication saves the record after steering projection.
  Root-start's corresponding observer adds no write with the matrix's empty
  tree, so its `ProductPublished` cut already covers that boundary. Toy root
  and steer recovery implementations are absent; the matrix runs the real
  handlers and reconcilers and checks a second reopen without redispatch.
- Each closure-table row has a synthetic-ledger test that pins its claim, its
  note, the closed operation set and the exact usage sum, and one test per
  integrity row; reopening the published closure is `AlreadyApplied`. A live
  turn's recorded usage equals the ledger sum recovery would compute for it.
- `recovery_verdict` is the predicate's single use: a structure test fails if
  any production caller decides recovery scope, closure or restoration other
  than through it, and a parity test runs the live open, SQLite inspection and
  rehearsal over the same heads and requires identical verdicts.
- Each session-workflow compaction row (Prepared, uncertain, Completed summary,
  recorded failure) has a test that the open commits its closure and `WorkflowSettled` in one
  commit and never redispatches, and that a Completed summary publishes exactly
  the fold the live workflow would have.
- An alias regression drives pi's `edit` and `write` through one turn and
  checks the committed `exposed_name` on both calls, exact replay of each name
  on every kernel, both history rows' labels and activity kinds, and a
  version 6 export importing with the exposed name equal to `name`. A row
  round-trip test pins `ToolItem.exposed_name` (present when it differs from
  `name`, omitted when equal) through the product row codec and the version 7
  export, and a version 6 import test refuses a version 6 tool row that carries
  it.

The opt-in live-fact pilots cover `non_stop_finish_length`,
`non_stop_finish_refusal`, `steer_accepted_mid_turn`,
`scheduled_compaction_in_turn`, `scheduled_compaction_workflow`,
`code_mode_cell_nested_then_final` and `code_mode_cell_final`. They assert
the committed provider finish, durable accepted/applied steering, finalized
in-turn compaction, a committed product compaction workflow, and real
JavaScript cell/nested-call parentage respectively. The workflow pilot seeds
foldable prior turns and explicitly requests the product compact workflow;
the in-turn pilot crosses the scheduled fence. Cell-final uses a text-only
cell, while nested-then-final joins a real nested process call. These shapes
do not widen the durability subset. Follow-up plans retain each shape's
descriptor and ScriptPlan dials, replacing only its assistant steps.
Requeued originals rerun their shape's lifecycle before the follow-up; each
must publish its answer once and the follow-up once. Durable answer markers
count only turn-owned provider evidence, never a compaction workflow's
provider response. Finding rows retain the pre-closure summary, provider
ownership/marker diagnostics, and the failed fixture directory for inspection.
The opt-in `hosted_tool_call_final` pilot executes a local tool round, then
commits a Stop response containing a completed provider-hosted ToolCall, its
provider-owned result, and final text. It checks the decoded owner/state and proves the hosted call
never dispatches locally. It has no accepted refusal.
The opt-in `capacity_overflow` pilot admits a scripted context-overflow fault
with another prompt queued behind it and disables compaction. The first physical
provider dispatch is file-gated until the harness observes the second prompt in
both the queue snapshot and the durable product queue; only then can the scripted
pre-stream denial proceed. The same gate is installed when an early cut restores
the unadmitted original prompt and its lifecycle is resumed on relaunch. No timed
enqueue window is involved. After resuming
the queued prompt, the pilot quits only after an empty live-drain queue snapshot
following its `assistant_done`. That model-operation marker alone is not a host
turn boundary or a face-idle acknowledgement: an early quit can still send an
interrupt while the JSONL face considers the turn busy. An interrupt is valid
product behavior, but the pilot must wait for face idle and send only quit
deterministically, as every completing shape does. If the host has already finished, its
idle interrupt writes a `wake_suppression` intent; an early quit can therefore add
a `host_intents` commit in only one twin even with identical runtime commits.
The harness decides whether to send a frame before releasing the first gate,
so an early quit deterministically exposes the busy-state race rather than
relying on the ungated twin to happen to be idle. The full
per-store twin-pilot divergence assertion is unchanged; a divergent pair retains both fixtures,
commit traces and JSONL observations for diagnosis. The registry distinguishes expected
`Answer` and `CapacityDenied` terminals. The pilot proves exactly one capacity
publication and a paused queue before explicitly resuming the queued prompt.
Cuts forbid dispatch past an unprojected capacity terminal, preserve the
terminal across follow-up, and converge to the pilot's drain state. Follow-up
and queued work require explicit queue resume after a capacity pause (chapter
02); no automatic retry of the failed original is inference evidence.
Child-turn sweeps are not enrolled: the ordinary scripted durable tree binder
aliases the root runtime locator and simulates child completion, while the real
process-child binder requires multi-branch runtime/dispatch summaries and
per-child relaunch bindings. Neither is a durable child-turn fixture for this
single-root sweep.
Tables include per-shape/per-repeat class counts and each refusal's signature
and cuts. The sweep found two crash-reachable nested Code Cell states
whose recovery candidate failed store `MissingChild` integrity; the checkpoint
codec now publishes only the final reachable candidate DAG, both states close
as `ABANDONED{running_tool}`, and any recurrence is an ordinary finding.

A compact workflow request with no admitted summary at the cut is withdrawn at
relaunch with one `compaction did not start; run /compact again` notice and is
never dispatched without a new operator action. A separate fsynced
`evidence.summary.jsonl` request log pins total physical summary dispatches
across the pre-cut process and both relaunches to at most one, and to zero after
the cut for a request withdrawn at relaunch; a workflow already admitted at the
cut cannot dispatch again. Runtime recovery itself remains offline, and the
second relaunch performs no inference or publication.
The current scripted provider emits only local ToolCall blocks; a
provider-hosted ToolCall final requires an additional output/owner dial.

**Commit trace contract.** The repository-only `plakous-store/commit-trace`
feature is enabled by the offline-crash harness; `plak/test-hooks` directly
forwards the tree store's own `commit-trace` feature and carries three pinned
edges; default and maintainer/release product builds contain no commit tracer.
Plak's harness dev-dependency also enables runtime-only tracing in test builds
without `test-hooks`; a sweep must require `cfg(feature="test-hooks")` to trace
the product and tree as well. The
runtime, product and tree install it on their writable WAL connections, not the
telemetry database or read-only inspection connections. Runtime installation is
beside the existing test commit hook, after schema/domain admission; the two
hooks are mutually exclusive when tracing is active. Product/tree installation
is after WAL configuration and before schema admission.
All commits before installation are untraced. Traced connections must step
writes to completion before calling thread-scoped `commit_trace::settle()`.
A write whose commit completes during reset or finalize (`RETURNING` through
`query` or `query_row`) loses its WAL callback and is untraceable. SQL `RETURNING`
is therefore forbidden in the sources of all three traced stores.
`check_workspace.py` pins this limit by matching the keyword, case-insensitively,
inside Rust string literals, not comments. The conservative scan includes inline
`#[cfg(test)]` modules in those source files.
`VACUUM` is unsupported under trace. Settle runs only outside SQLite, after
transactions have committed or rolled back; it is not a mid-transaction API.

`PLAK_TEST_COMMIT_TRACE` must name a **new** writable JSONL file, one per traced
process (including each relaunch). Without it, installation is a no-op, retains
existing hooks/autocheckpoint, and ignores `PLAK_TEST_COMMIT_KILL`. With it,
`PLAK_TEST_COMMIT_KILL`, if present, must be `<store>:<k>`, where `store` is exactly
`runtime`, `product` or `tree` and `k` is a positive decimal integer. The global-k
form is not accepted. Each durable
WAL commit that writes frames appends and fsyncs exactly one JSON object followed
by a newline. Frameless commits emit no line and advance no counter:

```json
{"ordinal":1,"store":"runtime","store_index":1,"store_ordinal":1,"label":"commit","tables":[],"indexed":true}
```

The exact fields are `ordinal` (nonnegative integer), `store` (`runtime`,
`product`, `tree`), `store_index` (positive integer), `store_ordinal` (nonnegative
integer), `label` (string), `tables`
(sorted unique array of table-name strings), and `indexed` (boolean). No row
values, paths, timestamps, connection IDs, prompts or provider data are emitted.
`store_index` counts all traced framed commits of that store kind across every
connection in the process, starting at 1. `ordinal` counts **indexed** commits
globally across all three stores, starting at 1; cross-store thread interleaving
can change its order between runs, so it is reporting evidence, not a kill
target. `store_ordinal` counts indexed framed commits of that store kind,
starting at 1. Runtime `renew` and `revive` commits have `indexed=false`, retain
the previous global and runtime indexed ordinals (0 before the first), and
are never kill targets; all other commits are indexed. Runtime dispatch closures
scope the label to the store method name; unlabeled commits use `unlabelled`.
SQLite's update hook supplies names only for rowid tables in `main`; WITHOUT
ROWID tables and schema changes can have an empty array. Tables are observational
touches, not an authoritative mutation inventory; rollback clears the pending
set, while savepoint rollback may leave touched names in it.

A process-global binary semaphore spans commit-hook admission through the WAL
callback, trace fsync and optional PASSIVE checkpoint. SQLite executes both
callbacks synchronously on the committing thread; thread-local pending context
bridges rusqlite's non-capturing WAL callback. At the target store's indexed
`store_ordinal=k`, the process
raises `SIGKILL` after the line is fsynced and before any later traced connection
can commit. At operation completion, `settle()` releases any remaining
thread-local claim as frameless, without emitting a line or advancing an ordinal.
Commit and WAL callbacks for a fully stepped framed commit run inside the same
SQLite call, so its claim cannot survive to that boundary. Runtime settles at
the outer actor operation completion, including errors and unwind, with a Drop
guard before replying. Tree store uses the same guard at both database-helper
completion boundaries. The guards are declared before work so transaction rollback
precedes settlement during unwind. Product transaction
operations and the two implicit acknowledgement mutations settle with a guard
declared before their transactions, so rollback/drop precedes settlement on
early returns too. A later commit hook on the same thread releases and reclaims
an outstanding claim; owning-connection acquisition is idempotent for BUSY retry.
Rollback releases only its own semaphore claim. A 5-second semaphore
timeout, invalid controls, trace I/O failure or inconsistent callback ownership
reports `DIVERGENT commit-trace` to stderr and exits 86, never an apparent clean
pilot. The WAL hook replaces autocheckpoint, so non-killing commits perform a
best-effort PASSIVE checkpoint at 1000 or more WAL pages, ignoring its result
just as SQLite's default autocheckpointer does.
The trace is evidence instrumentation, not a persisted product format.

Signal tests do not sleep to reach the middle of a turn. They wait for the first
row of the fsynced evidence log, which the model writes when it announces its
provider dispatch, and which is therefore strictly after the face installed its
signal watcher. A fixed interval races process start under load, and a signal
that lands before the watcher is a different test.

**Code Mode enrolment is harness-only and says so in its own identity.** Local
Code Mode compiles only for a model whose projection target the composition
enrolment recognises, and the scripted model is not any real provider's model.
`plakous-compose` therefore carries one extra enrolled target behind the
`scripted-code-mode-enrolment` feature, which is off by default and reached only
through the harness. The target's every field is a `scripted` or `offline`
identifier — `offline.scripted.code-mode.responses`,
`offline.scripted.code-mode`, `offline-scripted-code-mode-v1`,
`offline-scripted-code-mode-adapter-v1` — so a scripted session can never be
mistaken for an enrolled Codex one, and the feature adds exactly one branch to
the gate while leaving the gate's own conditions byte-identical. No Codex model
name, adapter revision or route appears anywhere in it.

The harness does not mint a `ResolvedProvider`. It builds the composed
configuration directly: the provider configuration owns exactly the two markers
in the `LocalCompositionOnly` presentation, the nested catalog stays in the
registry, and the harness plan comes from `compile_local_composition_plan`, so
the provider-visible exposure is empty. A composition deployment exposes its
catalog through the evaluator, never to the provider, which is why those two
facts are consistent rather than contradictory.

### 2.7 The offline performance smoke

`just perf-smoke` measures six things over the same scripted model and emits one
JSON line per metric:

| metric | what it times | compared |
| --- | --- | --- |
| `cold_start_to_ready_ms` | process launch to the `ready` record on the JSONL face | no |
| `jsonl_tool_round_trip_ms` | one tool row from `running` to `done`, median of five repeats | no |
| `cli_two_tool_turn_ms` | a whole `plak run` process for a two-tool turn | yes |
| `jsonl_bulk_throughput_mb_per_s` | ten prompts, each answered with 100 kB, over the whole batch | yes |
| `tui_transcript_20k_layout_ms` | full layout of a 20,000-block transcript at 120x40 | yes |
| `growth_turn_peak_rss_mb` | maximum resident set over a growth turn through the placeholder rung | yes |

Volume comes from assistant text rather than from tool results, and that is a
product fact rather than a convenience. One tool outcome is bounded at a
mebibyte, the runtime refuses to group a pending exchange whose results run much
above two hundred kilobytes, and the face clips a tool observation before it
reaches a client, so a large tool body never becomes stream bytes. Assistant
text is published verbatim up to a few hundred kilobytes, so that is what the
throughput case uses.

Every line carries a machine id derived from the platform, architecture and CPU,
because a number measured on one machine says nothing about another. Seeds live
in `tests/performance/baselines/offline-perf-smoke.json`, keyed by that id.
`--check` runs `tools/checks/check_perf_smoke.py` against this machine's row. A
machine with no seed row reports its numbers and passes.

All six are measured. `tui_transcript_20k_layout_ms`,
`growth_turn_peak_rss_mb`, `jsonl_bulk_throughput_mb_per_s`, and
`cli_two_tool_turn_ms` are compared with a fifty percent tolerance by
`--check`; the other two are informational. `jsonl_tool_round_trip_ms` uses the
median of five in-run repeats because it varies even on an idle host.
`cold_start_to_ready_ms` lacks enough headroom for a loaded shared host.
Neither informational metric is described as a regression: a measurement alone
cannot distinguish a slowdown from a busy neighbour.

Seeding a new machine waits for one run in which all six metrics measure
successfully on an idle host, so a seed never records a contended number as the
thing to beat.

The recipe takes `plak` from `cargo test --no-run --workspace --all-features`,
the gate's own test build, so it reuses that build, and copies the executable aside before measuring, because
the configured Cargo target's `debug/plak` is shared and a concurrent build
without `test-hooks` would otherwise replace the binary under measurement.

A metric the machine cannot measure is reported as **no measurement**, never as
a regression. The resident-set figure is the case that forced the distinction:
BSD `time -l` reports bytes, GNU `time -f %M` reports kilobytes, and some images
carry neither, so the recipe probes for a usable one. Assuming BSD made a Linux
run abort and the gate call it a slowdown.

`just full` runs the whole recipe informationally and never fails on it. A
measurement on a shared machine is evidence, not a gate: the same rule §2.3's
budget lane follows, and the reason the named render benchmark asserts nothing
itself. The restricted comparison applies the same caution inside the recipe, so
a checker run outside `just full` also refuses to call a swing in either noisy
metric a regression.

**Benchmark workloads.** Three deterministic offline `cargo bench` executables
are outside every gate (§1.2). `perf_core` (`-p plakous`) times
`Agent::prompt` to the authoritative result handle on a fresh agent, asserts
public pointer-identity sharing of pre-turn history into the prepared request
and committed result, and proves the configured root prompt-queue bound by
asserting `Backpressure` on the first prompt past it. `perf_project`
(`-p plakous`) times `CanonicalHistory` append, `ModelRequest` construction
and clones, synthetic `ActiveFamilyArtifact` envelopes and tool-schema
validation over the provider-neutral model API, with exact-scope accounting
on a synthetic wire body. `perf_store` (`-p plakous-store --features sqlite`;
`-- --small`, `-- --long` or `-- --dry-run`) runs the same commit trace on the
memory and SQLite backends, then a separately instrumented SQLite trace, a
process-isolated reopen with Linux RSS, and a bounded-actor saturation
fixture that requires typed `BackendBusy` backpressure. Each emits JSON Lines
(`plakous.perf-core.v4`, `plakous.perf-project.v6`, `plakous.perf-store.v6`)
whose metadata binds toolchain, target, feature closure, declared profile,
commit, dirty state and a tracked-content closure; the caller supplies
`PLAKOUS_PERF_HOST_ID`, `PLAKOUS_PERF_FEATURE_FLAGS` and
`PLAKOUS_PERF_RUSTFLAGS_ID`, because the workload never reads a hostname.
No numeric baseline is ratified: every latency, RSS, size and growth value is
a raw, host-qualified observation, and a primary timing never includes the
instrumented or isolated probes. Allocation counts are unavailable in process
because the workspace forbids an `unsafe` global allocator wrapper; instead
`tests/performance/baselines/perf_allocations.py` runs the
`--isolated-probe` entry points of `perf_core` and `perf_project` under
Linux glibc `memusage` and emits that evidence as separate JSONL.
`tools/perf/perf-check.sh --quick` (or `--full`) runs and schema-validates all
workloads into `target/perf-check.*`.

### 2.8 Transport fault injection

`plakous-http`'s `fault-injection` feature makes chosen exchanges fail the way
the network or a provider would, at the one place every provider's traffic
passes: `Client::execute`. The classifiers above the transport then see the same
`Failure` or response head they see in the field, so a fault exercises today's
kernels rather than a parallel path. A `plak` built with `test-hooks` reads a
plan from `PLAK_TEST_FAULTS` once per process and applies it to every client,
against real providers or local peers alike; tests attach a plan to one client
with `Client::with_faults`.

A plan is `;`-separated rules of `,`-separated `key=value` fields. `host=`
(exact, case-insensitive) and `path=` (a prefix of path and query) select
requests; `nth=` picks the n-th request that rule matched, counting from 1, and
without it every match fires. Each rule counts its own matches, and the first
rule whose ordinal it is wins.

| `fault=` | Effect | What a classifier sees |
| --- | --- | --- |
| `429`, `500`, `502`, `503`, `529` | An in-memory HTTP/1.1 peer answers; nothing is resolved, connected or sent upstream. `retry-after=<s>` adds that header | That status, `content-type: application/json`, `x-plak-injected-fault`, and a body both error dialects read: `{"type":"error","error":{"type":…,"code":…,"message":…}}`, with `overloaded_error` for `529` and `rate_limit_error` for `429` |
| `connect` | The exchange fails before resolution | `Failure(Connect, NotStarted, Io)`, as for a refused connection |
| `write-reset` | The exchange fails before resolution | `Failure(RequestWriteOrResponseHead, RequestIncomplete, Protocol)`, as for a peer that resets the connection while the request body is being written |
| `record-rejected` | The exchange fails before resolution, with no upstream traffic | `Failure(RequestWriteOrResponseHead, RequestIncomplete, Protocol)`, cause `peer_rejected_record`, TLS `AlertReceived(20)`, HTTP/1.1; all kernels label it as corruption in transit and resend immediately on a fresh connection after the first physical failure |
| `reset` | The real response passes until `after-events=<n>` SSE events (a blank line ends one) or `after-bytes=<n>` bytes, cut exactly there | `Failure(ResponseBody, MayHaveTransmitted, Protocol)`, as for a peer that drops mid-body |
| `eof` | The same cut, then a clean end of body | The end of body a closed stream gives |
| `stall` | The same cut, then `seconds=<s>` of silence under the body's own timeouts and cancellation, then the rest | `DeadlineExceeded` if a body deadline expires first |

For example, `PLAK_TEST_FAULTS='path=/v1/messages,nth=1,fault=record-rejected'`
exercises the first-resend rule without uploading content. Like `write-reset`,
this synthetic pre-resolution failure has no connection-use fact; real TLS
pool tests pin preserved fresh/reused facts and eviction of the rejected
connection.

`FaultPlan::parse` refuses an unknown field or fault, `nth=0`, a `stall` without
`seconds`, and fields that do not apply to the fault. A plan from the
environment that does not parse fails every exchange before it starts with
`Failure(Resolution, NotStarted, Internal)`, which no network event produces, so
a mistyped run is never silently fault-free. The transport does not print it:
the product may hold the stderr lock, and a library write would block.

`crates/common/http/tests/faults.rs` proves each fault fires where its rule says
and equals the real event: a refused connect, a peer that closes mid-body.
`multi_wire_loopback.rs` in `plakous-openrouter` runs each status and `connect`
through a real turn and compares it with the loopback gateway answering the
same way, and proves that `reset`, `eof` and `stall` after the first stream
event are replayed and the turn completes with the replay's output alone;
`local_transport.rs` in `plakous-anthropic` does the same for `529` and retries
a `write-reset` before acceptance.
Injected statuses, `connect` and `write-reset` never reach a connection, so
their responses and failures name no `ConnectionUse`; stream faults ride a real
connection, name it, and never return it to a pool.

The turn retry policy (spec 01 §13.1) is pinned at three levels. Unit tests in
`plakous-attempt` (`retry.rs`) cover every decision row, the budget,
`Retry-After` clamping and equal jitter through an injected sample. Loopback
tests pin each family:

- Responses, `attempt_host_tests.rs` in `plakous-responses-dialect`: a reset
  after send is replayed for a cost-only turn and never for a server-effect or
  session-workflow request; a stream cut before any tool call is replayed with
  the retry marker ahead of the replay's events; an in-band `rate_limit_exceeded`
  or `server_is_overloaded` terminal is replayed only for a cost-only request; a
  stream cut after a local tool call began discards the partial and replays; the budget ends on the
  fourth failure; cancellation during the wait sends nothing more; and the
  shared model host restarts a discarded response.
- Messages, `plakous-anthropic`: in `model_tests.rs`, a peer that reads the
  request and drops TLS without `close_notify` is replayed for an API-key turn,
  a truncated stream and an accepted stream that only refuses are replayed, and
  four 429s spend the budget with markers 2/4 to 4/4; in `oauth_model.rs`, the
  same drop is replayed for a Claude OAuth turn and never for a session workflow
  (compaction); in `tests/local_transport.rs`, the drop is replayed for a
  cost-only request and never for a hosted programmatic-tool (server-effect)
  request, a reset after streamed text is replayed with its text discarded, a
  stream cut after a complete local `tool_use` block replays and only the replay's complete response executes the tool, and cancellation during
  the wait sends nothing more.
- Chat, `tests/multi_wire_loopback.rs` in `plakous-openrouter`: injected 429,
  500, 502, 503 and 529 statuses take four requests for a turn; an accepted
  disconnect, an in-band rate limit, an HTTP 5xx and a body deadline each take
  one request as a session workflow; a refused connection is retried, injected
  `reset`, `eof` and
  `stall` stream faults and an in-band gateway overload are replayed, and a
  stream cut after a raw local `tool_calls` delta is replayed with the partial discarded. Chat has no
  server-effect or cancellation-during-wait loopback; both rest on the shared
  policy unit tests.

All kernels pin terminal `x-should-retry: false` and body-classified quota or
billing failures even on 429 with Retry-After; true cannot broaden policy.
SessionWorkflow tests retain single dispatch after possible acceptance or any
HTTP response; unsent retries read typed transmission/phase evidence only.
Loopback TLS 1.3 peers emit authenticated alert 20 both mid-upload and after
the whole body but before the head. Transport fixtures pin `RequestIncomplete`
with cause `peer_rejected_record`; the Messages `SessionWorkflow` path retries
once and succeeds under the existing budget. Alert 20 after a complete head
does not retry that workflow. A partial head already rules out this proof;
transport fixtures pin that first-byte boundary for separately delivered and
co-buffered plaintext plus alert, a non-20 pre-head alert, and unchanged TLS 1.2
and HTTP/2 classification. The proof relies on rustls 0.23 deferring TLS 1.3
KeyUpdate replies until the next outgoing message; TLS 1.2's possible
post-request `no_renegotiation` reply excludes it. These are physical TLS
fixtures, not injected
status faults or classifier strings; local decrypt errors remain distinct.
`plakous-http`'s `tls_integrity` tests pin that the client's own write path
never produces alert 20: a loopback Rustls peer with a small receive window
drains 100 KiB to 3 MiB bodies in odd-sized reads, bursts or stalls,
authenticates every record and checks every body byte against a
position- and request-dependent pattern, on unpooled, reused pooled and
concurrent pooled connections, with a copying observer, with peer TLS 1.3
KeyUpdates mid-body and while idle in the pool, and around uploads cancelled,
dropped or deadlined mid-write and then retried. A positive control flips one
ciphertext bit and requires the peer's `DecryptError` and the writer's
`AlertReceived(BadRecordMac)`. The ignored
`tls_integrity_reproducer_matrix` widens this to every size, pace, key-update
and observer combination plus TLS 1.2 (run with `--run-ignored only`; its
nextest override allows 300-second periods).
Pool fixtures pin the five-second idle bound and content-free checkout age in
failure diagnostics. Product telemetry tests pin replay-loss counts, ids,
`v_replay_loss`, retention and disposable schema version 17.
Responses in-band rate-limit and overload fixtures pin failed lifecycle,
closed observations, fresh terminal facts and one physical request for session
workflows (including compaction) and turns carrying `x-should-retry: false`.

Product tests pin the `plak run` retry line, the JSONL `retry` frame, the TUI
status row and discarded text and reasoning marking, and (in `plakous-hosted`)
that a discarded response's sealed reasoning never reaches the session. `check_features.py` requires that a default `plak`
never enables the feature and that `test-hooks` does.

## 3. Evidence classes

Two classes of evidence exist. Each answers a different question, and neither
substitutes for the other. Offline tests are never a substitute for live
protocol or model behavior, and live observation is never a substitute for an
offline contract. §3.4 is the pre-release bar for those live runs. It does
not move library evidence out of §3.1.

### 3.1 Offline fixture evidence

Child-observation regressions exercise the eight-subscription activity bound,
live counter replacement and terminal clearing, canonical prefix before the
addressed live tail, explicit missing-activity markers, transcript open/close,
and child cancellation/result settlement with an unread saturated host face.
Agent-transcript regressions pin agent-id identity across durable and
progress-only tree revisions. The protocol pins child-live omission from JSONL v1;
child activity must never project as root narrative or usage.

API-route contracts run offline against the production API binding, with
fixture keys and no dispatch: the OpenAI and xAI API configured tool
definitions (names, schemas, descriptions) equal the Codex and Grok OAuth
packs' for the same composition in `direct`, `native-ptc`, `js-ptc` and
`py-ptc` (the presentation minus the costume); the OpenAI API enrollment
table's `CodeModeOnly` rows equal the Codex reference catalog fixture's, the
fixture's client version equals the pinned constant, every `CodeModeOnly`
entry is an enrolled row and every enrolled row is in the fixture or on an
explicit API-only Direct list; an OpenAI API `CodeModeOnly` row constructs,
creates and reopens a durable native-ptc session (both `plakous-default` and
explicit), while `native-ptc` on the Anthropic or xAI API fails construction
and native-ptc without the JavaScript host fails naming the sidecar; and
command parse, template load, the root rebind, `plak run`, `plak agent`, a
seeded TUI session, `plak models`, `plak doctor` and an API child's pin admit
or refuse `native-ptc` by the same table, with `run` and `agent` saving the
same record for the same arguments. One local-TLS wire contract dispatches an
OpenAI API native-ptc turn: the request carries only the `exec` custom tool
(Lark grammar) and the `wait` function, `parallel_tool_calls=false` and
`store=false`, and no Codex OAuth client headers. A session created under a
pre-pack OpenAI API presentation fails closed at reopen (harness
revalidation), as does a Grok session whose recorded projection presents
`wait_agent` or `interrupt_agent`. The rendered-prompt matrix carries the API routes as
single-agent rows through the production API profile and Platform wire.

One-shot parity invokes the production hosted `OneShot` entrypoint; it does
not mirror the CLI with a second turn loop. Gated text fixtures distinguish a
streamed prefix from canonical terminal text, including divergent previews and
completed results with no visible text. Real-path regressions cover canonical
stdout and durable transcript publication, completion winning a refused quit
cancellation, interrupted partial output, retained paused queues, a canonical
SQLite release failure after `AssistantDone`, owned child release refusal after
completion with successful parent release and exact locator publication, combined
parent/child release refusal retaining both failures, and failed product publication
followed by refusal of the stale exact locator on reopen.

Observation-parity goldens retain the host's prompt admission and delivery
receipts, including the queue identity, revision, and prompt text after the
existing identity scrub. These receipts are in-process provenance even though
the JSONL projection does not emit them as standalone wire records. A receipt
change must be reviewed in the goldens; normalization must not hide it.
Usage parity omits the live request's Coding/Agent tool-inventory
informational rows and the timestamp-dependent `Live turn growth (est.)`
category: prepared-request and terminal refreshes supersede one
another, so row presence depends on mailbox drainage. The prepared-request
usage tests pin the exact tool-name split; other informational rows remain
part of the parity comparison.
The turn wait also recognizes the explicit start receipt: a fast completion
may coalesce away the busy=true edge. A real-channel regression pins that
wait behavior without relaxing the recorded observation comparison.
Queue and busy observations are latest-state snapshots: the mailbox can
supersede an unread value even across a prompt receipt or tool line. The parity
oracle compares their exact final values, not intermediate snapshot counts;
all prompt admission/delivery receipts and canonical turn terminals remain
ordered and exact. Workflow waits use their own correlated intent-start
receipt rather than requiring a coalescible busy=true edge.

Native Codex login stall tests first observe the token, JWKS or UserInfo request,
then drive its propagated login deadline with virtual time. Their setup budget
is shorter than the transport's own cap, so an omitted login deadline still
fails. The pooled protocol-terminal close test holds the body unfinished and
requires zero virtual-time delay, excluding scheduler latency from owned-task
joins. Child terminal-retirement fixtures gate provider dispatch until they
have captured the active turn control, rather than assuming a fast failed turn
is still active when start returns.

An actual interactive host test injects failed completion publication and paused-state
writes, restores storage, and proves both a manual prompt and queue drain remain
refused while settings, explicit clear into a fresh conversation, and quit work.

The same hosted entrypoints run with extension-free `Core` and an independent
extension whose marker is neither `Copy` nor `Default`. Real one-shot and
interactive tests cover immutable idle/busy and attached-one-shot facts, usage
publication before independent extension dispatch even in an idle host configured
for one-shot execution, explicit `NoHandler`
refusal without losing the admitted turn, and bounded replacement of extension
observations. The generic interrupt tests retain deferred FIFO work and never
turn a queued quit into cancellation of an admitted paid operation. Plak's
settings dispatch matrix remains binary-owned evidence.

Deterministic, hermetic, credential-free, and reproducible on any supported
host. It covers kernel grammar and framing, canonical history, checkpoint and
recovery semantics, store schema acceptance and rejection, tool contracts,
compile inventories, config parsing, TUI frames, prompt digests, and every
fail-closed refusal path. This is the only class that gates a change.

Fail-closed rows are coverage, not gaps: a test asserting that an unqualified
tuple refuses with an exact message is evidence of the same weight as one
asserting a success.

### 3.2 Live scenarios

A live scenario drives the real `plak` binary against a real provider account
and checks its result independently of a clean protocol terminal. It is opt-in,
may spend provider usage, and never runs in any gate tier. Four commands exist
in `tests/live/` (whose README lists every scenario and its oracle):

- `just smoke <route>` runs twelve short scenarios in a few minutes across
  `plak run`, `plak agent` and the TUI on a pseudo-terminal: a coding task with
  a planted bug, resume, a manual compaction whose earlier fact must survive,
  cancel mid-tool, four transport faults injected with `PLAK_TEST_FAULTS`
  (§2.8), a two-child subagent wave with adoption, and two TUI turns.
  The compaction read's byte-measured observation may hit a small route's
  admitted input ceiling. Only when the driver's catalog window is unknown
  or at most 65,536, a first-turn `ContextLimit` followed by a logged
  `capacity` start and committed compaction is an accepted remedy, not a
  harness failure. The capacity start must follow the first turn's failed
  `turn.lifecycle` terminal. This pass reports `capacity recovery compaction,
  context carried`, distinctly from manual compaction. A ContextLimit on a
  known larger window remains `harness` even if capacity compaction commits.
  In the accepted case smoke uses that compact instead of requesting
  another manual compact; the second turn and fact question must still
  complete, and `ZEBRA-42` must survive without appearing in any repository
  file before the question. A ContextLimit without that committed capacity
  recovery remains `harness`.
- `just soak <route> [--hours N]` keeps one session doing real tasks on a
  pinned open-source checkout, reopening it in a new process per task, with
  faults, SIGSTOP/SIGCONT sleeps, manual compactions and one mid-run rebuild to
  a byte-distinct `plak`, and reports throughput, tokens, peak context,
  compactions, errors and dead ends. It is headless. `just soak <route>
  --long-session --only <names>` does not run that hour loop; it selects the
  named long-session scenarios below. `--all` in place of `--only` selects
  exactly those four. The two flags are mutually exclusive, and a bare
  `long_session.py` launch is refused.
- Opt-in long-session scenarios in `tests/live/long_session.py` are outside
  the default twelve: in-turn compaction, an overflow remedy and continuation,
  SIGKILL mid-tool then reopen, and a child that compacts and still reports.
  The pre-release bar runs all four on every `full` row, via `--all` (§3.4).
  The overflow remedy is whichever the session log shows: an in-turn
  `scheduled` compact, a `context.placeholders` shrink, or a failed
  `ContextLimit` turn followed by a `capacity` compact. `just smoke <route>
  --only <names>` selects them too. Oracles are session-log and file facts,
  not the model's own claims. A missing compact is `harness` only when
  `model.operation_usage` shows prepared input at the catalog window minus
  one read page and no remedy committed; otherwise it is `model`. The child
  scenario does not use that split and does not read a child event log
  (there is none; JSONL v1 does not project child transcripts). After the
  root agent quits, child `cN` is taken from the root's JSONL `tree` frames
  or spawn/wait receipts, or from `plak session show <root>`. `plak session
  log <root>.cN --json` is the child's committed canonical history. A
  committed compaction segment projects as `kind` `compaction` with display
  text `compacted N earlier segments (<pass>)` (or `compacted earlier
  segments` when the checkpoint's provenance does not name it). The summary
  ciphertext is not that text, and an empty assistant item is not the
  segment. `acknowledged.txt` and
  `recalled.txt` in the adopted checkout must match `token.txt`. That
  projection carries no `model.operation_usage`, so a history without the
  segment is `model`. `pending` is a failure of `session show` or `session
  log` itself, not a missing segment. Chapter 07
  has no key that lowers a context window or compaction trigger; the scenarios
  pin `[compaction] enabled` and `[turn] max_model_rounds` on a private home
  and size `bulk.txt` to the route's window.
- `just tui-soak <route> [--hours N]` keeps one TUI session doing checked coding
  tasks, simulates a sleep, quits and reopens, then checks a follow-up in the
  same session. It does not inject transport faults or rebuild the binary.
- `just jobs <route> [--only NAME,...]` runs eleven bounded background-job
  scenarios: promotion at `[jobs] foreground_seconds`, a steer during a
  foreground command and during an await, a completion notice while busy,
  `plak run` waiting for a promoted job, crash reopen as outcome-unknown, a
  child submit refused with `live_work` while its job is live and then settled
  with that job terminal, plus a parent close that stops another live child
  job with `attempt ended`, Ctrl+B, Processes `S`, an idle wake,
  and the idle HUD with a live child. It injects no transport faults. The
  pre-release bar runs all eleven on every `full` row (§3.4). Any other jobs
  run is opt-in. The dry-run estimator prices that full-row cell (§3.4).

All four build `plak` with `test-hooks`; only the smoke and headless soak
inject faults. Each result is
`pass`, `harness` (Plak's failure, including a dead end: a failed turn after
which a plain follow-up also fails), `model` (a completed turn whose work
misses the oracle), `provider` (the real provider refusing without an injected
fault) or `pending` (a failure a named in-flight change is expected to fix).
An injected fault before acceptance must be absorbed and the turn completed.
A stream cut after acceptance but before any tool call on a cost-only turn
request replays once with its partial output discarded (chapter 01, replay of
cost-only requests) and the turn completes with exactly one effect; a break
that could repeat a server effect fails the turn without replay. Either way
the session must take the next prompt. `tests/live/STATUS.md` records the latest
runs.

A published pass names the qualification tuple in §3.4. `tests/live/STATUS.md`
may group rows only when those facts share one recorded revision; evidence
without that revision, or from a dirty tree, remains local diagnostic evidence
rather than publication qualification. A pass does not admit a different tuple.
A later revision is admitted only under the re-confirmation rule in §3.4, and
each cell that rule requires is a new run. A catalog listing, a successful
decode, or a model that is simply reachable authorizes nothing. Transient provider failures (HTTP 429, post-
acceptance rate limits, ambiguous 5xx responses) are recorded as provider
outcomes, never retried silently and never relabeled as product failures.

The same `provider` class covers every surface the drivers score: a JSONL
turn failure, a `plak run` exit, a TUI turn failure, a manual `/compact`
refusal, and a child failure. A typed provider refusal or outage is
`provider`: `RateLimited` or HTTP 429, `Unavailable` (including an OpenAI
`server_is_overloaded` terminal), `QuotaExceeded`, a refusal
rejected before acceptance, and a model refusal terminal (the provider's own
`stop_reason` `refusal`; the driver keeps its typed fields, never its prose). A kernel stream-validation failure is `provider`
only when the raw exchange capture shows the provider's response violated
the documented protocol. The evidence rule is: some `exchange-*.body` in
that turn's capture parses as JSON, or as an SSE `data:` JSON object, and
that object carries `usage` together with a `choices` entry whose
`finish_reason` is missing or null (the chat rule that usage is one
post-finish terminal-metadata chunk). A missing capture, a truncated
capture, or a body that does not show that violation stays `harness`. A
complete, integrity-checked capture whose finished Chat tool calls or Messages
tool-use blocks contain malformed JSON arguments is `model`, not `harness`:
the model emitted invalid tool input and the kernel correctly failed closed.
This evidence is restricted to the failing process's capture segment and its
highest-numbered exchange, ordered by the integer exchange number rather than
the filename. That exchange must be `Complete` or `ProtocolComplete`; a later
unfinished exchange prevents earlier complete exchanges from serving as model
evidence. An earlier scenario, process, or exchange cannot relabel a later
harness failure; multiple process
segments without an identified failing process are insufficient evidence.
The full kernel diagnostic, including the protocol's `stream validation
failed` prefix, must match its own protocol: Chat malformed/ambiguous arguments
require Chat tool-call evidence; invalid Messages tool input requires Messages
tool-use evidence. Cross-protocol captures and bare diagnostic suffixes are
not model evidence. Today only the OpenRouter Chat and Messages provider paths
emit these prefixed diagnostics. Other providers' argument failures remain
`harness`; malformed arguments alone cannot establish this model exception.
Chat argument fragments are joined per choice and tool-call index; duplicate
object keys are also invalid. Messages fragments are joined per tool block
and require its close plus the message terminal. Messages input permits
duplicate keys (the last value wins), matching its kernel; both protocols
reject non-JSON constants and require an object. This exception requires a
complete transport terminal (`Complete` or `ProtocolComplete`) and paired
request/body/metadata with matching recorded byte length; missing, truncated,
unfinished, or valid-argument captures stay `harness`. A model-selected job
stop (`stop_reason` `model`) is `model`, not `harness`.
An injected fault the scenario required the product to absorb stays
`harness` when the turn fails with that same fault; a different provider
refusal during that scenario is still `provider`.

TUI `turn.lifecycle` terminals carry only the turn id and phase. Smoke uses
that turn's last `model_retries` telemetry row to recognize an exhausted
provider retry budget (`next_attempt == max_attempts`). A transient retry
below that budget is not enough to relabel a later product failure.

A live run uses an isolated, disposable `PLAK_HOME` under the ignored
`target/live/`; the ordinary home is never imported. OAuth subscription credentials are the
deliberate exception: they are XDG state at
`${XDG_STATE_HOME:-$HOME/.local/state}/plak/subscriptions/<provider>/credential.json`,
outside `PLAK_HOME`, with no per-run copy, so a refresh during a live run writes
the real credential store. API-key routes instead copy the key installed by
`plak key set` from the ordinary home into the disposable home and remove the
corresponding environment variable from child processes, preventing a stale
variable from shadowing the file without printing credential bytes.

### 3.3 The support rule

Publication and Rust nameability confer accessibility, not support. Neither does
a passing test on an adjacent surface. Every claimed API belongs to an explicit
supported inventory, and publication or internal nameability is kept distinct
from qualification and compatibility promises. The `plakous_agent::internal`
tier is deliberately nameable so official sibling crates can compose against it;
that accessibility is not a support promise, and `#[doc(hidden)]` is not a
boundary.

### 3.4 The pre-release bar

The pre-release bar is the evidence a release candidate must carry before
that revision is qualified. It runs once per release candidate, not per
install. It has three parts, all bound to the same source revision:

1. the integrated gate on every supported platform (`just full` groups across
   Linux x86_64 musl and Apple Silicon) with the offline crash-point sweep
   (§2.3) passing in full, every REFUSED signature reviewed against its
   allowlist;
2. the live matrix below;
3. a maintainer's sign-off after reviewing the candidate's maintainer build in
   the TUI on their own work.

The live matrix is opt-in and paid. It is not a gate tier, and a gate pass
does not satisfy it. Library and facade evidence stays the offline fixtures
(§3.1) and the public compile contracts (§1.1). The bar spends live time on
what daily use does not exercise (routes the operator does not run, crash and
kill timings, overflow, jobs) rather than repeating long soaks that daily use
covers.

A qualification run binds one tuple. The `test-hooks` `plak` named in the
report was built from a clean checkout of one source revision, including the
live drivers. The report names that revision, the host, and the target triple
(`aarch64-apple-darwin` or `x86_64-unknown-linux-musl`). It names the SHA-256
of that `plak` at the start of the run and at the end, and the SHA-256 of the
`plakous-js-host` and `monty` workers from the same build. It names the
account, whether the account is a subscription login or an API key, and the
exact model, effort, protocol and composition. The composition for this bar
is `plakous-default`. It names every scenario in the cell. A dirty tree, a
digest that changes mid-run, or any other tuple is diagnostic only. The
headless soak's intentional rebuild to a byte-distinct `plak` is part of that
scenario: both digests are recorded, and any other digest change is not a
qualification.

Route names are the keys of `ROUTES` in `tests/live/live.py`. Account, model,
effort and protocol are that route's fields. Routes with `planned` false stay
runnable and are not in the matrix: `claude` and `claude-sonnet-medium`.
`deepseek` is Chat, with no protocol. `deepseek-messages` is the same model
with protocol `messages`. `or-ling` and `or-qwen38` are paid model ids at
informational depth.

- `full`: `codex`, `grok`, `claude-sonnet55-low`, `deepseek`,
  `deepseek-messages`.
- `light`: `codex-luna-medium`, `codex-sol`, `codex-astra`, `grok-46`,
  `claude-sonnet55-medium`, `claude-haiku55-low`, `claude-haiku55-medium`,
  `claude-opus-low`, `claude-opus-medium`, `claude-fable`, `or-mimo`,
  `or-glm`, `or-qwen37`, `api-openai`, `api-openai-medium`, `api-anthropic`,
  `api-xai`.
- `informational`: `or-nemotron-free`, `or-laguna-free`, `or-lfm-free`,
  `or-ling`, `or-qwen38`.

`api-openai` is OpenAI `gpt-6-luna` at low. `api-openai-medium` is the same
model at medium. `api-anthropic` is Anthropic `claude-haiku-5-5` at medium.
`api-xai` is xAI API `grok-4.7` at low. All four are `light` and are not
best-effort.

Apple Silicon runs every planned row at the depth of its class. Linux x86_64
musl runs only the twelve-scenario smoke, and only on the `full` rows and the
four API rows `api-openai`, `api-openai-medium`, `api-anthropic` and `api-xai`.

On Apple Silicon, §3.2 names the oracles:

- `full` runs the twelve smoke scenarios, all four long-session scenarios,
  and all eleven jobs scenarios, with `crash_jobs` run three times. One `full`
  row, the one the maintainer uses least, also runs the headless soak at the
  driver's default of one hour. No `full` row runs the two-hour TUI soak; the
  maintainer's TUI review replaces it.
- `light` runs the twelve smoke scenarios.
- `informational` runs the twelve smoke scenarios. The result is recorded. A
  `model` or `provider` outcome does not block the bar. A `harness` outcome
  does.

On Apple Silicon the full cell of a `full` row, with `<route>` that row's
name, is these commands; the soak line belongs only to the least-used row:

```text
python3 tests/live/qualification.py smoke <route>
python3 tests/live/qualification.py long-session <route> --all
python3 tests/live/qualification.py jobs <route>
python3 tests/live/qualification.py jobs <route> --only crash_jobs  # twice more: three crash_jobs runs
python3 tests/live/qualification.py soak <route> --hours 1          # least-used full row only
```

The qualification commands select the same oracles as the raw single-attempt
diagnostics: `just smoke <route>` is the twelve scenarios; `just soak <route>
--long-session --all` is exactly the four long-session scenarios. `--all` and
`--only` are mutually exclusive, a bare long-session launch is refused, and
`--only` does not fill the cell. `just jobs <route>` is all eleven scenarios.
The headless soak is the driver default of one hour; a shorter soak does not
fill the cell and a longer one does. Linux x86_64 musl's cell is
`python3 tests/live/qualification.py smoke <route>` on each `full` row and on
the four API rows.

A refused reopen in `crash_jobs` or `kill_reopen` is a `harness` outcome. The
drivers record the refused head's decoded shape and bucket it as covered,
known-uncovered or unknown; the bucket is diagnostic and does not change the
class.

**Maintainer sign-off.** A maintainer uses the candidate's maintainer build in
the TUI on their own work and signs off when it is satisfactory.
`just daily-driver --since <date>` summarises that use from the product log and
telemetry, read-only, per installed build (active days, session bindings,
warnings and errors by target, retries, reloads, carries, recovered or
abandoned turns). It is a diagnostic, not a cell, and cannot stand in for a
missing cell.

Every independent oracle has to be `pass` on a blocking cell. Classes are
§3.2's. The raw drivers do not retry inference inside a run (§5).
`tests/live/qualification.py <lane> <route> [lane options]` drives a cell's
attempts for `smoke`, `long-session`, `jobs`, `soak` or `tui-soak`. Each
attempt runs the whole selected lane in a fresh artifact directory and home,
with a new session and the same route, build and prompt. Randomized
long-session and jobs fixture inputs use the same seed on both attempts. The
headless soak verifies its retained starting binary after copying and before
restoring. It records `binary_sha256_rebuilt` immediately after its own rebuild
and requires the end digest to equal that digest. Before a rerun it restores
only when the current binary still equals that intentional rebuild, never over
an operator's newer build. Each attempt records the current, intentional and
restored digests and `starting_build_restored`. A pre-attempt mismatch records
`binary changed before attempt; no paid attempt`. Missing or incomplete captures,
dead ends and unexpected build changes are `harness`, not a reason to spend
another attempt. A TUI failed turn that reaches a completed recovery turn is
not an unrecovered harness failure; the recovery's answer oracle is still scored.

Scoring normalizes the first word of a soak outcome, so `model after
stream:reset` remains `model`. HARNESS evidence failures precede PENDING,
PROVIDER and MODEL. A MODEL reason names the task, its evidence status and
captured-exchange count, and its capture segment(s); a MODEL record without
that reference is HARNESS. Forwarded lane options are exact (no abbreviations)
and validated even in dry-run; a lane parser exit is retained as a HARNESS
cell report rather than losing the attempt.

- A `harness` outcome blocks on every row, including informational and
  best-effort. It is not relabeled, waived or rerun.
- A cell whose first attempt is `model` may rerun exactly once as a fresh
  attempt of the same tuple and prompt. The rerun is final: `pass` qualifies;
  anything else counts under its class, with no further rerun. A final
  `model` blocks on `full` and `light`; on `informational` it is recorded and
  does not block. A `provider` on this MODEL rerun is not the second PROVIDER
  outcome and receives neither another attempt nor the best-effort waiver.
- A `provider` outcome is run again once, as its own scenario run of the same
  tuple, on every class. The rerun is classified on its own. The cell passes
  if the rerun is `pass`. A `harness` outcome on the rerun blocks. A `model`
  outcome on the rerun blocks on `full` and `light`. A second `provider`
  outcome blocks, except on a best-effort row, where it is recorded and does
  not block. A `model` on this PROVIDER rerun does not receive a MODEL rerun:
  only a first-attempt `model` is eligible. Rerun allowances do not chain.
- A `pending` outcome does not pass.

The qualification report and printed summary keep both attempts in order:
the first classification with its capture-proven reason and evidence, then
the final rerun classification and evidence. `PASS after MODEL rerun` is
distinct from a clean `PASS`; the first failure is never overwritten.
The dry-run command prints the selected lane and its maximum of two attempts.
`tests/live/estimate.py` reserves one possible extra attempt per cell,
explicitly priced beside the clean-attempt subtotal and before contingency.
The PROVIDER and MODEL rules share that reserve, not successive attempts.

Best-effort rows are the informational rows and the OpenRouter light rows
`or-mimo`, `or-glm` and `or-qwen37`. On those three light rows the only waiver
is the second `provider` outcome. `deepseek`, `deepseek-messages`, every
subscription row, and `api-openai`, `api-openai-medium`, `api-anthropic` and
`api-xai` are not best-effort.

A later revision is re-confirmed by a new twelve-scenario smoke on each
`full` row, on Apple Silicon and on Linux x86_64 musl. That smoke does not
by itself repeat light rows, informational rows, long-session, jobs or either
soak, and it does not replace the offline gate.

In addition, every row whose own rendered-prompt digests (§2.5) or harness
footprints (chapter 02 §6.7) differ from the passing revision reruns at its
class depth. A `full` row reruns the whole cell, and that cell's smoke is its
re-confirmation smoke, not a second one. A `light` or `informational`
row reruns the twelve-scenario smoke, and the Linux smoke when that row has
one. A row whose own digests and footprints are identical carries the earlier pass.

A row's digests are the pack and family render pins for that route: the §2.5
prompt, lifecycle and wire pins for the family stem that route uses
(`codex`, `grok`, `claude`, `openrouter-chat`, `openrouter-messages`,
`openai-api`, `anthropic-api`, `xai-api`) and the packs `plakous-default`
renders there. Its footprints are that route's rows in the chapter 02 §6.7
matrix. Rows that share those pins and footprints move together. A pin or
footprint outside that set does not move the row.

Before a paid matrix or that re-confirmation, run
`just estimate --rate-card PATH [--openrouter-ceiling N]`. It prices the
planned cells from a local rate card and does not fetch a price. The
OpenRouter ceiling is an operator input. This chapter states no amount. The
estimate warns when the OpenRouter cells, including the contingency, exceed
that ceiling, and it still prints every cell. It prices a Mac smoke for every
planned route. For each `full` row it also prices the four long-session
scenarios, the eleven jobs scenarios plus two further `crash_jobs` runs, and a
Linux smoke; it prices one Mac headless soak for the row named by
`--soak-row`. For `api-openai`, `api-openai-medium`, `api-anthropic`
and `api-xai` it also prices a Linux smoke. An API-key row, including
OpenRouter and those four, is priced when the card names its model id. A
subscription row is quota and stays unpriced even when the card names its
model. An API model the card omits is unpriced and left out of the total.
The printed total is not evidence and is not the ceiling.

This bar does not cover:

- the gate tiers in §1
- any route not listed above, including `claude` and `claude-sonnet-medium`
- `wrap_up`, `py_ptc`, and any scenario the class lists do not name
- a composition other than `plakous-default`, or an effort other than the
  route's
- a Linux run of a subscription light row, an OpenRouter light or
  informational row, long-session, jobs, or either soak
- the two-hour TUI soak, which the maintainer's TUI review replaces
- `x86_64-apple-darwin`
- the Harbor bakeoff (`eval/`), which is never a qualification row (§1.2)
- library or facade support
- a catalog listing, a successful decode, a liveness ping, or the dry-run
  estimate
- an earlier pass standing in for a later revision, other than a row that
  the re-confirmation rule above says carries that pass

### 3.5 Private provider wire captures

Raw wire captures (§2.5) are private local observations, not qualification
receipts or committed fixtures. Ordinary maintainer capture lives under
`PLAK_HOME/wire-captures/`; explicit qualification and reference-client
captures live in the ignored `tests/provider-authority/`. The repository
ignores that whole directory: never `git add -f` it, and never quote its
contents in a report, issue or test log. Request and response bodies carry
prompts, reasoning and task content.

For an explicit qualification capture outside the live drivers, create
`tests/provider-authority/` with mode 0700, a private mode-0700 run directory
under it, and a fresh private mode-0700 directory under the run for each
process, and set `PLAK_TEST_EXCHANGE_CAPTURE_DIR` to that process directory's
absolute path for `plak run`, `plak agent` or the TUI. Never reuse a process
directory. The live drivers do this per launch and reopen and report capture
integrity (`tests/live/README.md`). A successful turn with zero `.body` files
is not wire evidence, and instrumented timing is not production performance.
To keep an automatic run past startup retention, copy it privately outside the
managed root (or remove its lock marker only after the process exits).

A structural comparison against a reference client records the client name and
exact version, the model, the account kind (never its identity), the route
and URL path, the request protocol family and the capture conditions. It uses
a disposable task and the client's real interactive route: Claude Code
interactive (its `--debug api --debug-file` output may omit request bodies),
Grok Build, or the official Codex CLI. `codex debug prompt-input` is
zero-inference; an `exec` coding turn spends usage. A local TLS/CONNECT
recorder or `just native-compare-prompts` observes structure with synthetic
credentials and mock responses; only a live request proves service acceptance,
and neither is inferred from the other.

Shareable facts are extracted, never copied. Before extraction remove
authorization and cookie headers, credentials, prompt and tool content,
reasoning and signatures, account and session IDs, e-mail addresses, private
paths and host-specific metadata; keep only event types, field names, block
order, safe enums and anonymized lengths. Review the extracted observation for
leaked values before moving only that sanitized observation into a test
fixture, and prefer synthetic examples. A fixture derived this way names the
request path and client version each structural assertion witnessed and which
provider behavior remains untested.

## 4. Reference product homes

The top-level `configs/` directory holds reference Plak home shapes: a
`config.toml` plus one flat template file per agent under `agents/`. They are
opt-in copies, never loaded automatically. To use one, run
`plak config show | head -1` to learn the home, copy the scenario's
`config.toml` and its `agents/*.toml` beside it, and `plak login` to the
accounts it names — templates name accounts only, and no credential ever appears
in these files. A project may also carry templates under
`<project>/.plak/agents/`, where a project file wins over the user file of the
same name.

The shipped default — an empty `[roster] working_set` and no `[root] agent` —
remains in force when no config file exists, so these scenarios never change
default behavior. `bin/plak/tests/integration/product_config_fixtures.rs` loads
every scenario directory offline, so a schema change breaks the examples rather
than silently rotting them. Each scenario's `config.toml` opens with a comment
that names it and describes the configuration; the same test requires that
name and refuses a scenario pinning `gpt-5.3-codex-spark` or `gpt-5.4-mini`.

## 5. Known flaky classes

Each of these has a named cause and a shipped fix. The fix is always to remove
the environmental assumption, never to weaken the assertion.

| Class | Cause | Fix in force |
| --- | --- | --- |
| Symlinked temporary root | macOS `/tmp` and `/var` are symlinks; SQLite opens with `SQLITE_OPEN_NOFOLLOW` and the credential walk refuses symlinked parents | A physical per-checkout `TMPDIR` outside `HOME` (`/private/tmp/plak-test-*` on macOS) from `scripts/test-tmpdir.sh`, set by `dev-check.sh` and the test recipes, and required for hand-run nextest |
| Fixed-port OAuth loopback | Native auth tests bind fixed ports and share process-local fixtures | The `native-auth-serial` group, covering `native_auth::` and the `local_transport` binary |
| CLI contract interference | Process-level binary contracts collide when run in parallel | The `cli-contract-serial` group |
| Child-start deadline under load | Crash matrices relaunch the current test binary and wait for durable cut markers; CPU and SQLite contention can exceed a bounded start deadline | The durability lane (§2.3): platform and full run these alone with `--test-threads=1`, under a 240-second slow timeout. `just check` does not run them |
| Wall-clock budgets read as contention | Interactive PTY budgets, the 250 ms yield floor, lease and cancellation windows are measurements on an idle machine | The same serial budget lane, assertions unchanged |
| Scripted response windows | A sleeping model can finish before the test's submission, mail claim or crash cut under load | Offline scripts use explicit registry gates for in-process streams and file-sentinel gates for process cuts. Gates preserve cancellation; park predicates register tree notifications before inspecting state |
| Probe peer teardown | A successful TLS 1.3 probe drops its connection before the peer flushes session tickets, so a fixture can drop its listener before the next probe | The byte-count peer disables tickets and gates receipt of the client's Finished until probe teardown; both HTTP ALPNs still require exactly zero plaintext bytes on two fresh connections |
| Hosted projected totals | An interactive terminal's bounded projector pass can defer its view update under load | Counter fixtures explicitly settle the view before asserting counts and rows; a refused-pass regression requires the same counts after catch-up. Presentation tests drive the after-ack pass without the live loop's wall-clock bound, which only defers it |
| Tree activity folding | A terminal snapshot can replace transient per-call and milestone rows with the completed multi-call summary | The screen oracle accepts only the exact successful spawn/finish/adopt summary or the complete split rows, before the answer; incomplete and failed summaries are rejected |
| Follow-up completion mistaken for the previous turn | A JSONL waiter rescans retained records and matches an earlier `assistant_done` | The refused-fold continuation waits for two completed turns before quit |
| Random retry fits a supposedly shorter deadline | Equal jitter makes the first backoff 500–1000 ms, so an 800 ms pre-dispatch fixture can unexpectedly retry | The fixture deadline is below the minimum backoff; a pure `next_delay` test pins both jitter extremes |
| Nested executor during root interruption | The async turn driver reaches descendant cancellation through a synchronous marker that constructs an application executor | The marker and its callers await the existing async cancellation path; a direct async-context regression requires joined child retirement |
| TERM trap deferred behind a foreground child | The Processes journey's shell repeatedly spawns a sleep as long as its TERM allowance | The fixture blocks in a builtin read over a separate FIFO, surviving the stop's stdin close without a foreground sleep; the TERM marker and stop oracles remain unchanged |
| Retries masking a race | A rerun hides a load-dependent interleaving instead of removing it | No nextest profile carries `retries`; a load failure is fixed with a gate, barrier or notification, never a retry or a wider window |
| Live transient provider failures | HTTP 429 rejection, post-acceptance SSE rate limits, and ambiguous 502 responses are provider outcomes | Live scenarios run zero inference retries; a transient failure is recorded as such and the scenario is re-run deliberately |

## 6. Review focus

Automated gates do not replace judgment. A review asks:

1. Does the change violate the Plakous/Plak layer or ownership boundary?
2. Does it add a peer vertical before the protocol kernels can host a plugin?
3. Is the paid and auth fail-closed posture preserved, with tests updated?
4. Do offline tests cover the behavior change?
5. Is a live path needed, and was a live scenario run intentionally?
6. Is growth intentional capability rather than accidental duplication?
7. Does recovery rest on the current format epoch and the recovery-scope
   table, with build identity as provenance only, working obligations failing
   closed, and semantic change flowing through explicit fork or rebind rather
   than resume?
8. Is every claimed API in an explicit supported inventory, with publication and
   nameability kept distinct from qualification and compatibility?

Secrets, authorization headers, private session data, and unsanitized provider
fixtures are never committed. Live Platform and subscription paths are opt-in,
fail closed without authority, and live in dedicated test utilities and product
paths — never in the default workspace green bar.

The product OpenAI credential adapter checks the normalized official origin,
including equivalent default-port spellings, before credential resolution.
Offline adapter tests reject foreign origins, non-default ports and foreign
credential references. Provider-only credential doubles do not replace this
product-boundary coverage. The offline Platform wire regression sends the actual
product tool registry through the production OpenAI adapter into a local TLS
peer. It checks explicit function `strict: false` and exact schema preservation,
including optional process arguments; binding construction alone does not prove
provider request compatibility.

The JSONL driver is exercised with bounded offline input and the real injected
host: a failing owned child shutdown returns a fatal process result after the
parent locator is published and `bye` is emitted. A refused model command
followed by quit still succeeds, distinguishing diagnostics from the worker
terminal result. TUI join helpers propagate that same typed result after
terminal restoration.

Offline refusal presentation tests drive the normal hosted loop and production
one-shot CLI for empty and partial-text refusals, asserting exact stdout, one
specific note, JSONL projection, durable transcript, and a single provider
dispatch with no tool effects. Recovered publication tests reopen SQLite sessions,
retrieve the exact prior command result without redispatch, and preserve the same
text and note contract. Ordinary empty-success canonical rejection remains.

Messages refusal evidence covers fragmented real SSE for empty content, ordinary
partial text, and explicit refusal blocks, with and without refusal stop details.
The canonical complete-response codec and streaming assembler agree on empty
refusal while ordinary empty success stays invalid. Required-durability agent
coverage commits and reopens an empty refusal without invented assistant history
or redispatch. Contradictory stop details, client-tool finishes, and fallback
remain refused; production Messages transport remains streaming-only.

Responses deadline regression coverage drives an unfinished argument stream
through the real TLS/SSE and joined model host, asserting one timeout, no
completed response authorizing tool dispatch, one physical preflight, bounded
content-free diagnostics and joined connection cleanup. Sustained argument
observations and last-observation age are tested separately from raw traffic.
The terminal-before-body-deadline and complete-stream single-terminal tests
remain the authority for successful cutoff and intended dispatch behavior.
Messages refusal/tool mismatch tests pin fixed reason/proposal counts and
retain strict non-execution of refused client proposals.

Keyed spawn replay tests pin immutable input/brief identity across workspace
rewrites, candidate retirement, changed node briefs, and a real SQLite close and
reopen. Different requests and cross-caller reuse remain refusals; a replay never
mints a second child. Tree-schema identity changes require an explicit new schema,
not a fallback decoder.

OpenRouter Chat TLS qualification distinguishes initial HTTP 403/429 rejection
from 403/429 in HTTP 200 SSE, asserting exactly one terminal error, no completion,
and one inference dispatch in each case. Canonical automatic tool choice
lowering keeps tools but omits the redundant explicit choice parameter; hosted
admission tests admit tools-capable Chat endpoints without explicit choice
support and still reject endpoints missing tools. Messages explicit-choice
admission remains covered separately. Three-wire TLS fixtures assert exact
Plak URL/title/category headers for attributed routes and their complete absence
for unattributed routes.

Deterministic OpenRouter Chat and Messages adapter tests complete a physical
operation in the same poll that queues its response-start prefix and error.
They prove the prefix precedes the fact-bearing terminal error, subsequent
polls remain ended, and the operation runs once.

Unified patch regression tests reject unterminated insert, delete, and context
hunk lines before any file changes, including a later malformed file in a
multi-file patch. Explicit no-final-newline markers preserve exact content
whether or not the marker itself has a terminal LF.


Claude native attribution qualification pins first-turn logical prompt IDs,
provider-issued continuation IDs, field order, native byte-range attestation, and
UTF-16 version-suffix vectors against the declared native reference. A durable
agent regression exercises first preparation, tool continuation, human followup,
and exact reopen; negative cases omit missing, ambiguous, imported, or foreign
response provenance. This establishes native-profile fidelity, not causality
for any provider policy refusal. Render fixtures must use stable entry UUIDs;
new attribution fields and their derived hashes remain visible in golden review.
Checksum regressions use independently derived expected hashes, retain literal
serialization bytes, and distinguish excluded routing-field changes from covered
content changes. They exercise adjacent commas, nested fallback arrays, malformed
or nonmatching field forms, and bounded billing-marker mutation. A checksum that
agrees with the implementation's own verifier alone does not establish native
equivalence; source-derived vectors and native-executed observations are distinct
evidence classes. Fully authored requests sent by the native client to an
isolated local Messages endpoint pin exact output bytes and checksum values;
their metadata is synthetic and they contact no provider. A native-executed
model/output-limit variant pins the same checksum, while separate source-derived
vectors cover the field-scanning edge cases.


### 6.1 Identity, finalization and codec regressions

Claude OAuth identity regressions use synthetic TLS peers to prove that existing
valid credentials acquire a provider UUID through the profile GET without a new
login, preserve it across refresh and reopen, and reject missing or conflicting
identity. Generation-CAS tests reject stale hydration writers. Two independent
managers hydrate concurrently on a single-thread executor while both retain
profile-read leases; blocking store acquisition and replacement run off the
executor. Installation identity tests cover concurrent creation, durable reuse across account-slot
retirement, malformed files, and symlink rejection. Wire fixtures assert the
independent account UUID, installation hex ID, and persisted session UUID. These
offline facts do not qualify provider acceptance or explain a live refusal.

Runtime finalization regressions inject both synchronous composition-session
shutdown panic and asynchronous integration-shutdown panic, checking subsequent
integration, lease release and flush. A separate persistent in-memory terminal
installation fault checks queued failure-channel settlement and cleanup; it is
not evidence that repeated store refusal alone causes the same escape.

The durable error codec suite covers every closed store invalid reason and
capacity bound, exact canonical byte roundtrip, and unknown-fact refusal. An
admitted failed turn persists a typed store refusal, then reopens the exact
terminal result with no model preparation or redispatch.

Store admission evidence (Plakous section 5.2) runs one admission conformance
suite against both backends: each class stops exactly at its threshold with a
typed refusal naming bound, usage, limit and class; archive and delete succeed
past the limit; and a store whose limit is lowered below its usage opens,
refuses ordinary writes, and still deletes (`plakous-store`
`*_admission_classes_stop_at_their_thresholds`,
`an_over_limit_store_opens_refuses_ordinary_writes_and_deletes`). A seeded
campaign interleaves ordinary, settlement and recovery writes with deletions
and requires that settlement and recovery are never stranded by ordinary
fill (`memory_admission_campaign_never_strands_settlement_or_recovery`).
Facade tests over SQLite (`plakous` `sessions::tests::capacity::`) drive real
turns at chosen limits: a running turn refused at the ordinary threshold
publishes `Failed`, never `recovery_required`, with no provider preparation;
the next root is refused before admission with `StoragePressure` and writes
nothing; usage reads through the facade; deleting a session restores service.
A paid compaction whose dispatch is the last ordinary write completes in the
settlement reserve and a retry never redispatches; one whose dispatch record is
refused publishes a failure without reaching the provider. An ordinary
refusal at any tool fence (the prepare, a mutating effect claim, or the
atomic dispatch of a read-only group), across a refusal campaign over each
ordinary commit, fails the turn definitely: a typed `StoragePressure`
rejection or a `Failed` result of kind `Session`, quiescent, with every
executed tool call matched by a recorded result. With ordinary work exhausted
while a Code Cell runs, every nested call is refused definitely with the
storage-pressure reason, the cell reaches its own result, and the turn fails
definitely on a quiescent head
(`a_cell_nested_call_under_storage_pressure_is_refused_and_the_turn_settles`).
A compaction whose locator commit is refused and which stops after its
failure-evidence commit recovers to exactly one failure publication, and a
repeat recovery writes nothing
(`refused_locator_failure_evidence_recovers_to_one_failure_publication`).
Hosted spawn under ordinary pressure is a definite `Refused` spawn: a refused
child create leaves no session, and a child whose first turn start is refused
is released, archived and deleted, with no provider dispatch and no node in
recovery (`plakous-hosted` `a_child_*_refused_under_storage_pressure_*`). An archive left by a delete
that failed after archiving is reclaimed on rerun: usage drops and the
deletion tombstone exists. A turn whose
terminal publication meets the settlement threshold ends `recovery_required`,
the open that recovers it writes into the recovery reserve, repeat opens write
nothing, and the store reopens under a limit below its usage. The pre-existing
raised-limit rescue regression still holds for a terminal write larger than
half the reserve. Plak covers `plak session delete` end to end (confirmation,
removal from listing and inspection, refusal of an owned session as open
with the lease held by a live owner process, and an open root refused before
its finished child's history is touched
(`an_open_root_is_refused_deletion_before_its_finished_child_is_touched`), its
dependency and
exact-revision deletion in the product store, agent-tree deletion under the
inspected revision exactly once, the doctor `runtime_store` thresholds, and the
hosted capacity presentation keyed on the typed refusal class (new work
refused versus reserve exhausted needing recovery).

History retention evidence (Plakous sections 4 and 5.1) runs one prune
conformance suite against both backends (`memory_prune_conformance`,
`sqlite_prune_conformance_reopens_exact_and_verified`): the retained set is
exactly the create checkpoint, the runtime-retained checkpoint, the head's
parent and the head, and every other checkpoint loads as typed `Pruned`;
checkpoint usage drops by the receipt; an explicit reclaim drain releases objects and bytes; receipts at or below the horizon recover as
`Pruned` and replaying a pruned commit is `Pruned`, never `Corrupt`, while the
head's commit replays exactly; a prune below its minimum or at a moved head
changes nothing; a repeat reclaims nothing; a fork taken before a second prune
keeps its source and its receipt replays and recovers exactly; a fork after a
prune succeeds; and the SQLite store reopens with identical counters under full
verification, which refuses a surviving checkpoint whose parent is neither
present nor a recorded pruned id (also a case of
`bulk_authority_snapshot_rejects_the_same_relational_corruption_classes`).
`sigkill_prune_cuts_reopen_as_exact_unpruned_or_pruned_history`
kills a child at `prune-after-enqueue`, `prune-before-commit` and
`prune-after-commit` and requires a fully verified reopen that is exactly the
unpruned or the pruned store. A store reopened under a limit below its usage
prunes and then admits an ordinary commit
(`sqlite_prune_over_the_absolute_limit_frees_ordinary_headroom`), and
incremental vacuum returns the freed pages to the file system
(`sqlite_prune_returns_freed_pages_to_the_file_system`). The ignored
`sqlite_prune_sweep_cost_on_a_large_store` measures prune and no-op cost on a
large synthetic store (the name is historical; reclaim is deferred; run with `--run-ignored only`; its nextest override
allows 300-second periods). A failed automatic prune emits `RetentionFinished`
with status `Failed` and leaves the published result, the head and later turns
unchanged (`a_failed_automatic_prune_emits_retention_failed_and_changes_nothing`). Facade tests (`sessions::tests::retention::`, and the
capacity suite) run 300 rounds with automatic retention keeping the checkpoint
count bounded and retained bytes growing no faster in the second half than the
first; over SQLite they observe an automatic prune, an explicit prune that
leaves the export document unchanged and repeats as a no-op, a verified reopen
and a further turn; an attached session is refused for ownership, a stale
locator names the current head, archived sessions are listed until deleted, a
Working head is `RecoveryRequired` at the facade and
`Invalid(PruneHeadNotQuiescent)` at the store, and a store over its absolute
limit is pruned. Over 80 SQLite rounds with automatic prunes, an explicit prune
and a reopen, a duplicate of the previous root command replays its exact turn
and checkpoint, and a duplicate of the command 17 turns back is refused,
unrecorded, as `ControlErrorKind::Reclaimed` without running or moving the head
(`duplicates_replay_within_the_retained_window_and_are_reclaimed_past_it`).
A child recovery whose recorded checkpoint retention reclaimed applies the
child's feed first and opens the head the feed names, and the create checkpoint
stays loadable
(`a_child_recovery_after_retention_opens_the_head_its_feed_names`).
Plak covers `plak session gc` end to end (reclaimed counts, a repeat reclaiming
nothing, a killed owner's Working head skipped as needing recovery after the
projector applied its feed, and a delete
cut short after archiving a root with a finished child finished through the
delete path:
`gc_finishes_a_delete_cut_short_after_its_archive_through_the_delete_path`), the
doctor's file size and last retention failure, the `retention_runs`
telemetry row and `retention` span, and the capacity message naming
`plak session gc` before `plak session delete`.
`gc_after_a_compaction_cut_before_projection_prunes_at_the_compacted_head`
kills the real host right after a compaction's runtime commits, before a
projector transaction applies them, runs `gc --all` (which applies the
projector itself, skips the killed owner's Working head as needing recovery,
and after `plak session recover` prunes at the head the views name, a
quiescent head), and requires the relaunch to show the compaction once, with
no workflow request left pending.
`a_workflow_request_without_a_fact_holds_gc_and_export` writes a workflow
request no feed fact settles and requires `gc --all` to skip the session as
`workflow pending` and `session export` to refuse it (`open the session once
to settle it`); once the request is withdrawn both proceed.
`full_sqlite_view_rebuild_and_plain_reopen_equal_incremental_for_every_sweep_shape` covers all 14 sweep
shapes: every SQL view cell equals the incremental lifecycle, including after
one plain reopen with no harness repair.
`a_lingering_owner_past_its_lease_changes_nothing_of_the_new_owner` stops a
real host past its lease, lets a reopen take the session over, resumes the
stopped host and has it prompt: nothing is dispatched, the new owner's views
are unchanged, and the lingering prompt is never run or resubmitted.
`a_sealed_reasoning_final_answer_rebuilds_to_its_live_thinking_and_assistant_rows`
streams synthetic SSE through the real Anthropic Messages and OpenAI Responses
kernels into the host: one thinking and one assistant row, rebuilt exactly.
`live_tui_rows_equal_the_rows_a_resume_rebuilds_from_deleted_views` compares
the semantic pilot's live transcript tail with a resumed TUI over rebuilt views
for the final-only, direct-tool and provider-hosted shapes.
`pause_lines_and_milestones_are_notice_intents_that_survive_a_rebuild` and
`an_accepted_steer_commits_no_revision_and_rebuilds_from_its_acceptance` pin
the notice-intent and live-only writer classes.

Inner cleanup regressions inject a local-composition lifecycle panic while a
yielded process is owned and a synchronous active-cell termination panic. They
require durable process lifetime retirement and an attempted session shutdown
respectively, despite the earlier dependency panic.

The terminal error codec also roundtrips every built-in tool error category with
exact canonical bytes and preserves its category and diagnostic; unknown tool
categories are refused. This includes `UnsupportedCapability`.

### 6.2 Lease admission regression evidence

`delayed_normal_dispatch_ack_admits_only_before_expiry` and
`delayed_exact_dispatch_recovery_preserves_receipt_but_not_expired_authority`
use a deterministic store clock independent of the driver's observation clock,
a three-second grant and barriers after accepted dispatch publication. Two,
three and four seconds of acknowledgement delay establish strict pre-expiry
admission and refusal at/after expiry. The witness counts prepared model start
capability invocations, not network traffic; exact accepted operation/receipt
recovery remains byte-semantically equal after refusal. These tests make no
clock-rollback, reboot or remote-byte timing claim.

`store_clock_expiry_without_heartbeat_tick_refuses_recovered_start` advances only
the store authority clock at the accepted-ACK barrier, so no heartbeat tick can
substitute for the synchronous expiry predicate. Its mutation control removes
only that predicate and must fail with one actual start; restoration must pass.
`unavailable_store_lease_clock_refuses_start_and_preserves_receipt` covers a
failed clock read. `renewal_ack_after_stamped_expiry_cannot_admit_waiting_executor`
returns the real renewed grant after its original deadline while a tool waits
for final admission; receipt arrival cannot mint a new TTL.

`read_only_and_mutating_tool_permit_waits_recheck_exact_lease` holds a real shared
scheduler permit with a separate ephemeral agent, blocks heartbeat on the durable
lease gate, then observes zero actual executor entries after expiry. Cancellation
and mutable policy revision variants exercise the final gate; policy rejection
remains an ordinary zero-start tool result. The evaluator source-wait test uses
a fake session with a heap-entry counter after its preparation barrier; it proves
the runtime callback contract. Built-in QuickJS/Monty callback placement is
source-audited; existing evaluator integration tests exercise normal execution.

`public_cancellation_during_scheduled_compaction_joins_pending_start_and_stream`
uses public `request_cancel` against an actual pending prepared start and stream;
each joins once without another paid start or waiting for the model timeout.
Delivered-cancellation tests delay the exact compaction ACK and assert zero paid
starts after that control has won. These are offline
runtime witnesses, not provider network, billing or external acceptance evidence.

`public_controls_during_scheduled_compaction_survive_successful_fold_in_order`
accepts an ordinary prompt, targeted follow-up and steering command while the
prepared compaction start is held. After release, it checks exact command records
in the terminal checkpoints, applied steering, follow-up-before-prompt ordering,
and one compaction dispatch. The fixture uses the existing response path; it
does not substitute cancellation for a successful compaction fold.

`public_controls_during_returned_compaction_stream_survive_successful_fold`
exercises the same controls after the prepared start has returned and the actual
response stream is polled. It verifies a finalized compaction publication,
retained command records and ordering, and exactly one provider dispatch.

A root turn has no wall clock (spec 01 §3.3), and three offline tests hold
that line under paused Tokio time. `a_long_turn_completes_without_a_wall_clock`
drives nine model rounds that each think for four virtual minutes and requires
a `Completed` answer after more than half an hour.
`successive_composition_resumes_are_bounded_by_the_round_cap` resumes a
never-finishing composition cell for four virtual minutes a resume and requires
the round cap, not elapsed time, to end it with `RoundLimit` after the wrap-up
round. `restored_turn_above_its_round_cap_does_not_wrap_up_twice` reopens a
paused wrap-up under a raised configured cap and requires the resumed turn to
keep the cap it was admitted under.

## Coverage of the offline gate

Second-save refusal laws exercise idle-fork tool close, interrupt, synchronous
submit and discard, and duplicate child-command projection for both unavailable
storage and stale revisions. Consequence refusal restores views and cursors,
records a tool failure receipt, and dispatches no new seal or removal.
Valid schema-conforming requests and exact store-call counts distinguish
consequence-save refusal from input-validation refusal.
Hosted attempt-end and spent-turn close laws refuse their consequence saves,
then retry. A captured commit cut after closure publication and before seal
settlement reopens without another seal ticket.

Tree fold metadata laws cover immutable first recording, distinct insert orders
for all existing host intent kinds, unbound anchor 0 and cold recording at the
current tree cursor (not the command acceptance sequence). SQLite laws cover
anchor/order pair constraints, integer codecs and round-trip, and NULLing the
node fold position through both view-deletion paths. The generated host-intent
corpus interleaves cancellation, materialization, retirement, turn failure,
recovery and Finalize at live cursors, including unbound then bound turns,
successful and failed seals, producer adoption, and captured interrupts.
Live views equal clear-and-replay and cold reopen; project-twice is inert and
replay reserves no effects. Minimized cases cover an old unbound recovery
or retirement followed by a bound cancellation in the terminal's page, a
failure followed by retirement projected twice, and late Finalize after
recovery. Anchored replay crosses more than 64 pages; below-retention folds
also survive deleted views and a second reopen without another seal.
Projection-lag laws record cancellation, materialization, retirement, host
failure/recovery, close, interrupt, submission and Finalize while a committed
terminal is still unprojected. Their anchor is the tree cursor, not the real-time
session tail; both deletion paths rebuild the exact recorded rows page by page,
with no repeated effects.
Stale-page laws regress the cursor between incremental read and apply: the
refusal preserves all rows, then a full reread converges. A real hosted session
exercises the incremental caller's single retry, carries head wait evidence, and
maps a second refusal to the existing recovery code rather than spinning.
A synthetic session longer than 64 pages reaches its tail and projects all
records in one bounded reread without wall-clock waits.
An old attempt's applied read stays inert after a followup's later retirement;
the current attempt and newer recovery kind survive both reread and deletion.
A deleted followup target refuses before allocating any immutable attempt.
A refused checked feed still settles effects from earlier acknowledged saves,
exactly once; it neither creates a new effect nor drops the owed queue.
Retained late-read laws refold in place with the seed and compare both deletion
rebuilds over the same retained inputs (never resurrecting erased facts).
Full and retained refold pages extending beyond the old cursor count only new
records and preserve exactly their terminal usage for acknowledged publication.
A synthetic repeated turn identity across old and new terminal records proves
the usage filter follows record sequence and emits only the new usage once.
Deleted-fold laws refuse live recorders and close/due restarts without mutation;
a reserved close remains queued until reconstruction. A persisted deletion plus
an ancestor crash-window cascade installs successfully, then converges after the
descendant rebuild, without manual fold calls: the rebuild transaction folds
newly eligible ancestors before computing the restored pause lift.
Late recovery-read laws compare a synchronous in-place closure refold with its
own-position rebuild through both deletion paths. Incomplete pages refuse with
no mutation; a reread from the start converges without publishing deleted views.
A later retirement retains the current attempt and `turn_recovery_required`.
The applied-read fold view survives SQLite reopen and later retirement: rereading
changes neither rows nor revision and never refolds. Deleted views reset the
set and rebuild it from the reads. Two abandoned turns on the same attempt
remain inert on every reread, including SQLite reopen and both deletion paths. Explicit deletion uses the existing recovery
pass, not a process-local late-read callback.
Explicit repeated older reads preserve exact recorded rows and revision, with
no new refold or effect tickets.
Seal-success and seal-failure laws replay anchored completion, including an
empty seal, with fixed failure end stamps and no repeated effects; SQLite
view deletion re-derives a failed seal from its outcome intent. Orphaned seals
after Finalize and submitted cancellation rebuild the completed attempt before
the VCS fault, including a second cold reopen. Fault recording writes no node
views; fault and settlement predicates follow the anchored position and replay
without repeated effects.
SQLite deletion laws rebuild spawn refusal, producer adoption and a failed
workspace cleanup, including their diagnostics, through both view-deletion
paths and a second reopen. Projecting twice is inert and reserves no effects.
VCS faults replay a captured prior diagnostic even after settlement; a second
reopen leaves the rebuilt view rows unchanged.
Operator-intent laws rebuild close (with and without live ownership), effective
and ineffective interrupts, and recovery abandonment through in-memory and SQL
view deletion.
Followup laws rebuild the captured supersession stamp, refuse withdrawal after
another intent, an authoritative future feed tail, Start rejection, or an empty
descendant-close reservation, and leave refused rows unchanged. The open-save /
withdrawal cut reopens with a complete opening; successful withdrawal removes
its own unfacted Start, opening intent and attempt, and deleting views then
folding the remaining inputs produces the same rows.
Topology-only completion and block fixtures record existing finalization and
host-failure intents, rather than raw view writes; their deletion law rebuilds
the same node, attempt, blocked reset boundary and fixed end stamp without effects.
Attempt-view laws reset both in-memory and SQL views to the retry chain root,
then replay three attempts page by page, comparing complete node prefixes with
distinct schemas. Per-attempt schema text and digest survive SQL view deletion
as immutable inputs; no prefix sees a future attempt or schema. Followup
applier-diff and parent-first/child-first laws rebuild the same rows without
effects. Restore/resume preserves folded status rows while tree, list and wait
presentation expose the Paused overlay only for Running nodes.
Every debug-build intent application checks a generic applier diff: non-target
node, attempt, workspace (including generation), mail and operation rows stay
unchanged. Only newly reserved result envelopes and captured descendant-close
inputs under the applying agent's new fence are exempt; target consequences
still require their own applier. Negative laws deliberately alter each foreign
row class and require the assertion to reject it.
Rebuild laws also preserve all external workspace rows; an adopted followup's
workspace stays adopted rather than reactivating under a historical opening.
Retention laws compare a running followup and an older closed, still-unwinding
turn with their unpruned live views. Older openings precede the head seed;
binding never lands on the chain root or disappears under a replayed opening.
Pending-close seal laws rebuild the live `sealing` view before settlement and
the orphaned reservation's recovery fault through SQL deletion, without another
seal reservation.
Same-cursor laws reserve a seal from cancellation, materialization or Finalize,
then apply an effective interrupt or a host decision after an in-flight
submission at that cursor. Both deletion paths rebuild the exact pending view
without another ticket, and the settled outcome also rebuilds exactly.
The pending SQL comparison reads raw persisted rows to isolate the fold from
reopen's intentional orphan-seal fault, covered separately above.
Deferred-close laws replay both the prefix before a failed terminal and the
pending terminal seal. A future fact's reservation cannot lift the earlier
close, and replay invents no result mail.
Feed-failure cascade laws assert both session projection orders leave the same
rows and exactly the original intent/reservation set, including an unsealed
inherited parent workspace.
First-application cascade laws start with an unprojected parent feed failure,
cancel or Finalize at view deletion. Parent-first projection refuses without
advancing; descendants followed by parent retry and a post-order rebuild both
equal no-deletion reopen. Fresh, unfed children have initialized folds and
remain closable by a live parent.
The fixed-seed, bounded multi-child corpus includes a parent and two children,
mixed running/finalized descendants, failure, cancellation and Finalize cascades,
and cross-session projection permutations. It exercises both in-memory and SQL
deletion, including a cascade's first application after deletion. Newly minted
intent identities and insert orders are run-specific recording facts: independent
first applications compare semantic rows and new intents by target, kind,
canonical payload and anchor sequence, not revision-derived ids or fold orders.
Each run then deletes views again through both paths and requires an exact
rebuild against its own already-recorded intents, including ids and orders.
Where no new intent is minted, the original full-row comparisons remain exact.
SQL deletion followed by the real product recovery entry rebuilds a
parent/child tree in post-order with no explicit fold calls; durable session
bindings remain intact and sessionless children fold without opening a session.
Normal reopen selects no new target, and a second recovery pass is inert.
An opt-in ignored tree-fold microbenchmark replays twenty agents with 500
records and 200 intents each, both with all intents above the window and with
fenced intents due throughout it. It reports replay time and 100 whole-tree
clones separately; it has no wall-clock assertion and is not a gate lane.
A companion ignored close benchmark times twenty agents (four five-node
subtrees), each with 500 records and 200 future intents, over five samples.
Test-only per-tree clone counts prove idle due folds and deleted-only close
drains allocate no scratch tree; an unprojected session allocation still joins.
Repeated-close tests assert ten closes of an already Closed subtree add no
intent rows. The regular replay/refusal corpus covers fenced view-only
snapshots and first-application rollback with effect reservations.
An injected fenced applier changes either the input count or its own immutable
row before refusing. Raw `fold_due` and driver consequence boundaries return
`operation_recovery_required` and retain byte-identical complete tree debug
snapshots, including operations, views and process-local queues.
A stale page-intent index law injects a same-agent due intent after indexing
and requires the debug integrity assertion rather than silently omitting it.
The applier-diff injection law covers all foreign row and queue categories,
including commands, cursors, inbound, effects and fault reasons. A malformed
VCS fold input must make `resume_tree` propagate the integrity refusal.
An attach store probe requires the hub entries lock to be available during
every startup save. An empty started singleton saves only its initial input,
and reattaching an occupant saves nothing.
A deletion-parity law combines accepted mail, a withheld admission's returned
mail and an undeliverable envelope. Both paths produce identical deleted rows
and rebuild identical live rows, including the derived return receipt.
Followup withdrawal cuts refuse the consequence save and retain the opening,
mail and receipt unchanged, then retry one CAS that settles all three. SQLite
reopen and view deletion preserve the failure receipt and acknowledged opening;
a later Start's pending mail never includes the withdrawn envelope.
Startup commit-cut tests publish a Finalize intent without its consequence,
then assert terminal views after hub reopen alone. Its first application's
seal dispatches once through acknowledged settlement; the following reopen
has neither another seal nor an orphaned-VCS fault.
Result-mail laws fail an attempt, acknowledge its result, close it and
acknowledge 65 later messages until the original envelope is pruned. Both
view-deletion paths rebuild identical attempt, node, mail and operation rows
without another result push; repeat projection is inert.
Candidate-association laws assert both deletion paths clear the attempt's
`candidate_id` while retaining the external candidate row, then derive the
exact association from `seal_outcome` without effects or repeat writes.
Generation-boundary tests complete a mutating child's attempt on a scratch
clone and through a refused consequence save. Neither publishes the shared
Read/Grep stamp; an acknowledged retry publishes exactly the row generation.
Late Finalize reads retain their bounded acceptance-to-terminal evidence test
without any fold or view mutation.
Waiter-boundary tests refuse both unavailable and stale-revision saves after
Start binding with another waiter registered for the same child. Dropping the
new registration leaves the concurrent waiter registered; its own drop then
removes the last count.
Agent-local fold tests leave another agent's due close untouched and do not
parse its malformed close payload. Driver boundary tests drain a parent
applier's cascade in the same commit, and a refused consequence save restores
the child view and an empty process-local scheduling list without effects.
Hosted failed-terminal tests retain the fork for rescue snapshots while a
turn waiter owns it, including a close before terminal projection. SQL close
rebuild fixtures settle the driver post-pass before deleting views; hub
reload fixtures use the same durable store for startup CAS.
Submitted-output laws delete both output and status and derive
them from the captured intent without reserving effects; repeated projection
is inert. The generated corpus uses real submission intents, including before
historical-attempt host outcomes. Close cascade laws assert that the parent applier records target
intents without changing child rows, and rebuild identically in either session
projection order without adding intents or effects. Their applier-diff checks
leave other agents' rows untouched; a descendant abandonment and parent interrupt
rebuild identically in either session projection order. Abandonment retains its
fixed end stamp and diagnostic across a second reopen, with no effect reservation.
Commit-protocol regressions fail exactly the second save with Unavailable or
StaleRevision: host cancellation, published closure mutation and materialization
fail-closed paths restore their acknowledged pre-fold views and dispatch no
seal until retry. Dirty-input and work-refusal laws pin the fold boundary, and
released process-local ownership stays released after revert. A child-usage
save failure followed by a successful pass emits exactly once without reload.
Indexed WAL cuts separately publish a fork child's Finalize and a submitted
outside cancellation before their consequences. Each reopen applies the saved
decision once, with at most one seal ticket and no repeated effect on reopen.
Caller-terminal laws finalize a cold tree without a terminal cache, reject
foreign turn evidence without mutation, and rebuild the captured terminal after
view deletion. Hosted late finalization reads start at the exact command's
accepted sequence; ordinary reopen passes continue from the tree cursor.
Initial publication refusal dispatches neither seals nor mint hooks.
Refused hook-result publication retains follow-on removal without dispatch;
after a later acknowledged save, removal runs once and the hook never repeats.

Reload-rehearsal regressions use fixture homes only. They cover a live-WAL
SQLite backup, exclusion of secrets/holder locks, unsafe copy inputs, sanitized
runtime environment, staging cleanup on refusal/HUP/INT/TERM, matching maintainer
Cargo selection/build identity reporting, a changed-key carry-forward
rehearsal (installed-build export, scrubbed config refusal, import refusal,
and an untouched live home), and the two refusal classes: a live-session race
(`session_busy_retry_when_idle`) versus any other staging failure, with no
path, SQL, or config on either line.
Current-holder and idle/committed-write WAL connections do not block legacy
pruning; legacy holder/SQLite refusals remain. An active current product writer
refuses pruning without removals, whereas an active runtime SQLite writer does
not block it. Presentation fixtures cover
oversized restore projection, invalid snapshots, closed removed-worktree and
unbound pending nodes, quiescent exact sessions, a projection behind the feed
(reported as `projects:<n>` and passing, for the root and each child), and a
runtime head the recovery predicate cannot close (refused).
Inspector fixtures compare the independent abandoned-turn flag with the runtime
predicate and distinguish saved-terminal closure. The facade fixtures exercise working-head
abandonment and saved-terminal inspection without store mutation and recorded
JS/Monty evaluator identity checks, including changed/missing historical
artifacts. Evidence qualifies pure decoded-head predicates and presentation
limits only: it does not establish authenticated provider construction,
harness reconstruction, staged tool descriptor/presentation equivalence,
actual child recovery or paid execution. A production rehearsal is an explicit
operator action against backups, not part of the fixture suite or permission
to mutate a live home.

`just coverage` runs the same two offline nextest lanes as `just check` under
`cargo-llvm-cov` in a separate target directory and writes an HTML report and a
JSON summary under `target/llvm-cov/`. It is informational: it does not gate,
and its purpose is to turn "paths the offline suite never executes" into a list
rather than a guess. Product paths that only a spawned binary can reach are
covered by the offline end-to-end harness; anything still uncovered there is a
candidate for that harness, not for a mocked unit test.


### SQLite Reclaim and Admission Budgets

The runtime schema-25 fixture checks rowid metadata, separate payloads and
persistent GC candidates. Count-based actor tests pin independent priority
lanes, 32-job aging, zero actor dispatches for `usage()`, publication of usage
before the next background step, and one shutdown checkpoint despite a vacuum
backlog. SIGKILL cuts immediately before and after GC commit verify exact
counter/queue atomicity; the durability lane includes this campaign. A proptest
interleaves real commit, fork, prune, delete and bounded GC operations against
an independent mark/sweep oracle and the root-or-queued ancestry invariant.
Full verification checks payload length as well as canonical object contents.

`crates/store/src/sqlite/admission_budget_tests.rs` is a unit-test module of
the private SQLite actor so its real connection can be traced without a public
test API. The small/large fixture measures a fixed usage/renew/commit delta over
200 sessions, 2,000 checkpoints, 64 KiB payloads, 10,000 feed rows and a
5,000-entry GC queue. `trace_v2` profile counters require equal statement and
VM-step counts, zero full scans and automatic indexes, and no payload reads,
recursive closure walk or aggregate sum in that admission path. Setup and
explicit reclaim are outside the measured phase; this is not a wall-clock gate.

The agent admission double captures only `[renew, commit]` before the first
publication; under pressure a single bounded reclaim precedes that pair and
restored headroom admits the turn.

The opt-in `perf_store --large-store-admission` scenario reports admission and
actor `queue_wait_ns` over a 200-session/2,000-checkpoint store. Timing is
informational and never determines gate success.
