# Troubleshooting

Network failures name DNS lookup, provider connection, TLS handshake, request
write or interrupted response stream. A TLS handshake failure can indicate a
captive portal or intercepting proxy. Retry rows use the same phase language;
the diagnostic details retain the transport classification.

Subscription requests proven never sent do not spend your turn's request or
input-token allowance. A resend takes a fresh authorization on the same route;
incomplete or possibly sent requests still consume their allowance.

Terminal model failures include a short attempt summary: requests started,
last failure phase, whether nothing was sent, and why retry stopped. This
summary describes the failed call; it does not authorize replaying completed
actions or provider-hosted effects.

Start with `plakd doctor`. Every failing row carries its own FIX column, and the
command exits 5 when anything is ERROR. Then `plakd status` for credentials.

## Startup Model Lookup

Before attaching, plakd retries transient catalog/discovery GET failures up to
three times, without changing the URL, account or model. CLI and host status show
`retrying model lookup (2/3): <cause>`. The waits are roughly half a second and
two seconds; provider `Retry-After` advice is honored only within the lookup's
caller-supplied overall deadline, and only when enough time remains for a useful
next attempt. Each request is capped at 30 seconds or the remaining lookup
budget, whichever is shorter; retries and pages never extend that budget. Codex/Grok catalog
renewals inside an attached session remain single-attempt; the retry window
applies to an operator-waited startup, attach or explicit model switch, not to
holding an in-session credential lease. Background native catalog refresh and
native child binding use a single attempt. OpenRouter and API-key child
binding still retry because they hold no subscription locks or leases.
Native catalog operations allow up to 120 seconds for credential setup, the
lookup window and cleanup; an outer timeout is labeled `operation deadline`
and retains the charged-dispatch count. An explicit model switch allows that
inner operation to finish rather than cutting its retry off at 30 seconds.

`model lookup failed after N attempts: <cause>` means the transient failure
outlasted that budget. Check connectivity or provider availability and try
again. Authentication failures, missing models and invalid catalogs do not
retry; use `plakd status`, `plakd models <account>` and the reported remedy.
Cached catalogs can avoid a network lookup. Claude subscription models use a
pinned local catalog, so they do not make a startup catalog GET.

## `this build's contract is out of date`

`<reason>; this build's <route> contract is out of date; install a newer build`
means the provider's contract no longer matches this build's compiled pins.
It can appear on an unavailable picker row, during login, during a turn, or as
the failed result of a web search, page fetch or image request.
Install a newer build; logging in again or refreshing the same credential
does not repair contract drift. Other subscription failures keep their own
reported remedy; not every failure calls for login or refresh.

## `model refused the request`

The model declined the request; this is not a login or refresh failure.
`plakd run` exits 5 after saving and releasing the completed turn. An empty
refusal produces no assistant stdout. plakd does not retry, change models, or
clear your conversation automatically. Rephrase the request or explicitly start
a new conversation if the service continues to refuse it. To try another model,
select it explicitly for a new conversation; `--session` retains the recorded
account, model, effort, and composition.

JSONL clients receive `model_refusal` with the turn id when known. Treat streamed
output as provisional until the terminal: discard refused provisional output,
and do not treat a usage record as evidence that the requested work succeeded.
The agent connection stays open for explicit user action.

A child with failure kind `refused` retains the provider's content-policy
message. Re-run it as a fresh child with a handoff of completed work and the
remaining task, not a follow-up into the same session; use the same remedy for
`context_exhausted`. Failed turns in forked workspaces report a rescue ref and
commit in their failure message (or a snapshot error). Inspect that ref with
`git show <ref>` or cherry-pick it explicitly; it does not move the child's
branch. A normal sealed candidate remains the `adopt_candidate` path. A rescue
snapshot alone does not settle a recovery-required session.
Each failed turn has its own ref, retained until you delete it, even after its
workspace is removed. `plakd state` does not list these unregistered snapshots.
In the Git repository, list them with
`git for-each-ref --format='%(refname)' 'refs/plak/*/candidates/failed-*'`.
After inspecting or retaining the work elsewhere, remove an exact ref with
`git update-ref -d <exact-ref>`. Rescue refs are never adoptable candidates.
Repeated snapshots of the same failed turn refuse to overwrite its existing
ref and report that ref for inspection. For a non-colocated JJ workspace,
run the same commands with `git --git-dir=.jj/repo/store/git`.

## `retrying 4s` and `plakd: retrying in 4s (2/4): ...`

plakd re-sends a turn's model request automatically whenever doing so can only
cost a duplicate inference:

- the provider refused it or it was never sent: a rate limit (HTTP 429),
  service unavailable (503, including the Codex subscription's HTTP 401
  `authentication failed (status=503)` overload, which is not a login
  failure), overloaded (529), or a connection that could not be established;
- it was sent but no response arrived, for example
  `RequestWriteOrResponseHead (MayHaveTransmitted, Protocol)`, or the provider
  answered HTTP 500, 502, 504 or 408;
- the response stream broke (a reset, an early end, a stall, or an in-stream
  overload or rate-limit error), including after a local tool call began streaming. No tool from that incomplete response ran.

The last two apply only when the request can do nothing on the provider's side
beyond inference: no provider-hosted tool (web or X search, code interpreter),
no server-side conversation or background mode. A request with any of those, a
hosted side effect, and authentication, quota, billing or usage-limit errors are
never re-sent. `x-should-retry: false` forbids retry; `true` does not grant it.
Compaction is a separate operation and retries only when typed transport proof
says the server could not have received the complete request, never after any
HTTP response or possible acceptance.

One such proof is `peer_rejected_record`: on HTTP/1.1 with TLS 1.3, the server sent TLS
`bad_record_mac` (alert 20) before any response byte, rejecting a record of
the request. plakd retries this automatically, including during a compact operation,
within the same budget below. Other TLS alerts, a locally detected decrypt
failure, alert 20 after response bytes, TLS 1.2, and HTTP/2 do not gain this unsent-only
retry permission.

OpenRouter Chat also retries one in-band upstream timeout (`408` or `504`)
within the existing budget; a second ends the turn as a timeout. Partial output
is discarded, and a request with a possible hosted effect is never replayed.

plakd makes at most four ordinary requests, honoring the provider's `Retry-After`
(a longer wait is shortened to 60 seconds while it still fits the 120 seconds of
ordinary waiting left; advice beyond that ends the attempt at once, declined
for `advice_beyond_budget`) and otherwise waiting about 1, 2 and 4 seconds
(randomized by up to half), with at most 120 seconds of ordinary waiting.
Advice of a day or more is shown as `retry after 24 h or more`, never as an
exact number of seconds. A Claude subscription whose session or weekly limit
is used up names that limit and its reset time instead, and is not retried.
When DNS, connection or TLS fails before anything was sent, it instead rides
out a short outage: up to twelve proven-unsent requests, waiting about 2, 4, 8,
16 then 30 seconds (randomized by up to half), with at most 240 seconds of
network waiting. These do not spend the ordinary retry budget. The retry row
says `provider unreachable (...); waiting for network, try N in Ns`.
Provably incomplete uploads have a separate allowance: up to sixteen incomplete
physical requests and 900 seconds of waiting, with equal-jittered backoff bases
of 10, 20, 40, then 60 seconds. They do not spend either the unreachable or
ordinary allowance. An HTTP response or a request that may have been fully
written cannot enter this allowance; a possible provider-hosted effect forbids
replay. Your hard deadline and cancellation still bound every wait. This also
covers proven-incomplete compaction uploads, never a second paid dispatch.
Retries never switch routes. See the
[retry contract](../spec/01-plakous-library.md#131-errors-and-retry).
Each request gets a fresh connection/head budget; a long response does not
consume the next request's budget. After a streamed generation longer than
300 seconds fails, at most one replay is allowed. On Responses routes, after
output starts, 300 seconds without semantic progress is a stall even if
keepalives continue. A newly added non-visible item (hidden reasoning or a
provider-hosted tool) suspends that limit until the next visible output. Before
output starts or while suspended, keepalives can sustain hidden reasoning and
hosted tools without that semantic limit. Each actual retry wait is shown
on `plakd run` as a `plak:` line, in
`plakd agent` JSONL as a `retry` frame, and in the log under target `plak.retry`
with session/turn/agent ids, checkout idle age when pooled, transport phase and cause (for a provider refusal, the provider's own
error message with credentials filtered out). If the broken stream had already
shown text or thinking, clients must discard that provisional response,
including text and thinking that preceded a tool call of that same response;
only the replay's answer enters the conversation, and its usage is the only one
counted. Text and thinking from earlier model calls of the same turn are kept.
Send an interrupt operation (or cancel the JSONL turn) to stop waiting; nothing is sent after
cancellation.

If the turn still fails with a transient connection or provider error, plakd
can continue the task automatically in a new turn. The notice shows the
continuation budget and wait: connect failures wait for the same route to be
reachable, while other transient errors use a short bounded delay. Completed
actions are not replayed. `orchestration.transient_resumes` defaults to `1`;
set it to `0` to disable this behavior (maximum `2` per user prompt).
A new prompt, steer, interrupt or quit cancels the pending continuation without
blocking your input. Reload drops a pending root continuation; send a prompt
to continue after reloading. `plakd run` still obeys its timeout and turn limit.

**Request corrupted in transit (TLS record rejected)** means the peer rejected
a TLS record of the incomplete upload and closed that connection, not that the
provider was unreachable. If this is the call's first physical failure, the
resend is immediate on a fresh connection; otherwise resends use the
incomplete-upload backoff above. Repeated
occurrences indicate in-path corruption. Try the same request from another
network (for example a mobile hotspot), then compare with VPN/proxy disabled
where permitted. If that network works, check the original router, VPN/proxy,
and ISP path; do not disable TLS verification or paste request contents into
logs. A local adapter can be the corrupting hop: USB Ethernet adapters with TCP
segmentation offload are a known source. If a large upload with another client
from the same machine (for example `curl --data-binary @file` of a few
megabytes to the same host) also ends in `alert bad record mac`, the path is at
fault, not plakd; switch adapter or network. Retry log records for roots and children include host, route path and
request body byte count, never the body or query parameters.

`plakd telemetry status --views` includes `v_replay_loss`: content-free retry
and declined-retry rows. A declined row records `declined_reason` (`deadline`,
`budget`, `advice_beyond_budget`, `server_effect`, `retry_disabled`, or
`workflow`); its next attempt
number is proposed, not dispatched. Zero delay and `output_discarded=false`
are declined `ModelRetry` event facts, not SQL columns. The rows include
correlation ids, attempts, cause/phase, text/thinking byte counts, completed
items, elapsed time and replay class. On a declined row the byte/item counts
describe the failed generation and were not discarded. No discarded content
is saved there. Analytics use a bounded best-effort writer, not recovery storage.

When the budget is spent the turn fails with the last error and the session
takes your next prompt.

## A response was cut off mid-stream

A stream that breaks and is not re-sent (see above) fails the turn with a
transport or protocol error. The partial answer is discarded. The session
stays ready: send the next prompt to continue. Only a provider-hosted tool
that may have started (not a local tool) leaves the turn needing recovery.

## Transport failure after a request may have been sent

`RequestWriteOrResponseHead (MayHaveTransmitted, Protocol)` means the
transport failed after the whole request was sent, while waiting for the
response head. The request may have reached the service. For an ordinary turn
request with no provider-side effects plakd re-sends it automatically (see
above); this error reaches you only when the retry budget is spent or the
request could have had an effect beyond inference.

Inspect the saved transcript and workspace before submitting more work: tools
from earlier exchanges in the turn may already have changed files. Once the
session is ready, send a new prompt describing the remaining work. If it reports
recovery required or a paused queue, follow that recovery instruction first.
A failed turn is not a completed answer; a usage record does not prove success.

## Tool errors and recovery

Tool argument rejections made before any external effect are ordinary tool
errors: the model can correct the arguments and continue. Definite MCP errors
(JSON-RPC errors or `isError` results) likewise reach the model, or are catchable
inside a Code Cell. An MCP connection failure after sending a request may still
require recovery because the server might have acted.
Output mapping or size-limit failures after execution also require recovery:
failure to represent the result does not mean the tool had no effect.
A mutating tool whose returned output violates its declared contract likewise
requires recovery; a read-only contract mismatch remains an ordinary tool error.

## `session store is full`

This is a retained-state limit across the runtime database, not a context-token
limit or a raw-capture quota. SQLite defaults allow 4 GiB of accounted object
bytes, 4,194,304 objects, and 524,288 checkpoints. The last eighth of each is a
reserve: new work (a new prompt, a model call, a tool call, a compaction) stops
once the store reaches seven eighths, so that turns already running can still
finish, publish their result, and recover. You see it as one of:

- a new prompt refused before anything runs. The first line is
  `session store is full (<bound> <used>/<limit> (ordinary writes stop at
  <threshold>)) - new work is refused ...`, followed by the raw
  `prompt was not admitted: new work was refused under storage pressure: ...`
  detail. The prompt stays queued without a recovery fence;
- a running turn that ends failed, with the same first line
  `session store is full (...)`. It is a plain failure, not
  *recovery required*: nothing was dispatched after the refusal. A Code Cell
  sees each refused nested call as an ordinary error it can catch;
- `plakd doctor` reporting `Runtime store` as `full` (or `nearly_full` from
  70% of the point where new work stops).

The store is `runtime.sqlite3` in this build's state directory
(`~/.plak/state/<key>/`); the notices and `plakd doctor` name the exact file.
`plakd doctor` reports retained usage and pressure; host notices identify
when new work is blocked and name the reclaim commands.

Sub-agents stop first. Their new work stops one reserve earlier (3 GiB of
object bytes by default, against 3.5 GiB for the root), so a tree that fills
the store does not take the root with it: `spawn_agent` is refused with
`storage_low`, a message to a sub-agent is refused, and a sub-agent's running
turn ends failed, while your root session can still finish its turn, compact
and clean up. The remedies below are the same.

Rarely the reserve itself runs out, for example when many turns were finishing
at once. A turn then fails with the first line
`session store reserve is exhausted (<bound> <used>/<limit> (settlement writes
stop at <threshold>)) - a running turn could not record its outcome and needs
recovery ...` (or `recovery writes`). That turn needs recovery: free space as
below, then request recovery in that session.

First reclaim superseded checkpoint history, which keeps every session whole:

```
plakd session gc --all
```

It covers every plakd session in this home (sub-agent runtime sessions are
pruned by their own turns), prints what it reclaimed and names any session it
skipped: one open in a running plakd (close it and run `gc` again), one with an
unfinished turn (run `plakd session recover SESSION_ID`, then `gc` again), or
one with a pending workflow (open the session once, then `gc` again). It
also finishes deletes a crash interrupted. Turns reclaim their own history once
enough builds up, so `gc` frees most for long-lived or imported sessions.
It then prints the bytes that remain against where new work stops and the
sessions retaining the most, which are the ones worth deleting. Run it with
the same build as the plakd you are using: each build reclaims only its own
state directory's store, and `gc` says so when a running plakd on this home
uses another one.

If the store is still full, delete sessions you no longer need:

```
plakd session list
plakd session delete SESSION_ID
```

`gc` and deleting are never refused for lack of space. Deleting is refused while the session is
open in a running plakd, while its agent tree has a child that is still running
or resumable or needs recovery (plakd recovers it; an explicit recovery retries one), and while other sessions continue from it or share its history
(forks, rebinds, new-session targets); the refusal names what
blocks it. A finished child's history and the agent tree are deleted with the
session. A delete that failed part way finishes when run again (or with
`plakd session gc --all`). Starting a new session, compaction and recovery
do not reclaim retained state. `plakd state prune-legacy` removes only other
builds' state directories (listed by `plakd doctor` as `Legacy state`); it frees
disk space but never this build's store. The database file shrinks as `gc` and deletion
return free pages; `plakd doctor` shows the file size and its free bytes.
Opening a session and recovering an interrupted turn keep
working while the store is full, because they write into the reserve.

Do not delete state files or change stored counters by hand, and do not use
export/import as a repair for an unresolved turn.

## `needs recovery · reopening`

After a crash, a completed answer can be recovered even with queued steering.
One notice tells you if that steering was not applied; it is not automatically
retried. An abandoned turn includes the same steering warning in its single
recovery notice. Steering is applied only when its text and command
finalization were promoted together atomically; an earlier identical text does not prove
application. A crash after recording completion but before that promotion
leaves steering unapplied. A completed model round interrupted before its next
request stays in history with an interruption note instead of being treated as a final
answer.

A turn whose outcome plakd could not observe ends *recovery required*: a stream
that broke where a provider-hosted action was possible, a process it could not
prove had stopped, a checkpoint it could not confirm, a lease it lost. The
session is not lost and you do not need a new-session operation. The host reopens it in place
at once, with no action from you. If that reopen fails (for example while you
are offline), the next thing you do that needs the session — a prompt,
compaction, a model change -- first reopens it in place and then runs. `{"v":1,"kind":"recover","id":"r1"}`
in `plakd agent`, or `plakd session recover SESSION_ID` does the same on its own.
`plakd run` exits 5 for such a turn, and the next `plakd run --session` recovers
it.

If a context-capacity stop cannot be saved to the product transcript, queue drain
stays paused even after storage recovers. The next prompt or a recovery operation first
reopens the session and publishes the saved result; the failed turn is not retried.
After a crash, its capacity error appears once and queued prompts remain paused
until you explicitly resume them or resolve the context limit.

Recovering never re-sends a model request or re-runs a tool. It records once,
in the transcript and in one `plak:` line, which of these is true:

- **a tool may have run and changed this workspace** — check the workspace
  before relying on what came before that note. The conversation result was
  not kept or replayed; workspace changes remain for review (for a child, in
  its workspace or sealed candidate);
- **its model request may have been processed and billed** — nothing from it
  was kept in the conversation;
- **compaction was not applied** (it may have been billed) — any earlier completed
  rounds of the interrupted turn remain in history; the conversation
  is unchanged;
- **its pending work never started** — nothing after that boundary ran. This includes an admitted prompt interrupted before its first model request, or an unstarted tool from its last round. Earlier rounds stay in the history; the prompt is not retried.

If the interrupted turn has no publishable product result, the recovery note
still appears once in the current host or JSONL stream, but is not saved as
another transcript result. Otherwise it is saved with the recovered result.
This is the same for a recovery operation, the next prompt's in-place recovery, and reopening
after a crash; a saved result's notice never gets a second fallback line.

A saved text-only answer also completes when the provider stopped for length
or refusal. A visible Length answer is kept with a truncation warning; ask to
continue. `plakd run` prints it but exits 5 (output-limit), rather than claiming a
complete answer. An empty or reasoning-only Length retries once at the same
bound and then fails with OutputLimit if still empty; wrap-up and provider-hosted
work never get this retry. Discarded reasoning is not saved by an interrupted
retry. A length-truncated local call never runs: recovery fails it as an
output-limit error only if the turn already took the retry for this truncation;
a successful round resets that retry evidence. Otherwise it abandons
the turn without retrying. An existing terminal answer is never replaced by a
second result merely to settle uncertain sibling work. Previously refused
saved-terminal cuts can settle that debt without an interruption note;
already-recoverable interrupted turns keep their existing recovery notice.

The same happens when you reopen a session after a crash, a `kill -9` or a
machine that stopped: the next host attach or `plakd run --session` closes the
turn it left. That includes a turn lost inside a Code Mode `exec` (or a
JavaScript or Python cell), even while a command the cell started was still
running: the cell and that command are reported as **a tool may have run**
(or **its pending work never started** when the cell had not begun), and
neither is run again. Recovered child agents are marked `interrupted` (owner
lost) with their attempt kept. This applies to children lost mid-turn as well
as parked children. Resume the held child through JSONL or wire operations, or have
its direct parent send `purpose=followup` with `acknowledge_recovery` equal to
the child's current `attempt_id`. That settles uncertainty before admitting
fresh work; it never replays the old tools. A crash does not spend the tree's
attempt budget. See [child recovery](agents-and-orchestration.md#what-a-spawned-child-sees).

If prompts were queued behind the interrupted turn, they stay queued and
paused, since they were written for a turn whose outcome nobody saw: review
them, then explicitly resume held work and the queue. JSONL `resume_queue` resumes only queue
delivery; `resume` is the explicit Resume for loss-held work. A prompt you type
now runs at once.

A fork request a crash interrupted before it started is withdrawn on
reopen with a fork-not-started notice and an explicit fork remedy. A fork that already landed
is saved as a separate resumable session, even if its live attachment failed;
reopening the source keeps the source selected. Fork recovery never reruns the
request. A model change that could not complete says it was abandoned and the
session is unchanged; choose the model again to retry.

If recovery itself fails -- offline, or a credential that needs `plakd login` --
the prompt is refused with the reason and your draft is kept; the next prompt
tries again.

## A laptop slept during a session

A session held by another process reports that it is busy and its lease is
held by another owner. Close that process or wait for its lease to expire;
this is contention, not a session that requires recovery.

A session lease is 30 seconds. When the machine wakes, plakd revives the same
lease if nobody else opened the session meanwhile, and the next prompt runs as
usual. If another `plakd` opened it in the meantime, this one can no longer
write to it: its next prompt reports that the session could not be reopened
because it is held elsewhere, and keeps your draft. Continue in the other
`plakd`, or quit it and prompt here again.
Quitting immediately after wake also releases an untouched expired lease;
you do not need to send a prompt first. It never releases a replacement lease.

## A session or child requires recovery

Run `plakd session show SESSION_ID` to inspect the root checkpoint and its
persisted children. It reports unresolved canonical obligations and distinguishes
them from tree-node retirement. A root can be quiescent while a child still
requires recovery. Inspection does not resume work or clear the condition;
missing or unreadable evidence remains explicit.

A root head stays recovery-required after an exact reopen only when its saved record
is damaged or contradicts itself (a typed integrity failure); every state a
crash can leave closes on reopen. For such a root session, keep it for diagnosis and start a new one for the remaining work;
a fresh session does not mark an old child recovered.

A recovery-required **child**, including one whose release was refused, is
settled by reopening its session exactly once more: reopening or resuming the
tree does it in the background, a root recovery does it for the whole tree,
and a JSONL `invoke_tool` of `recover_agent`
retries one child. No agent has a recovery tool. Nothing is replayed, and a note
says which children were recovered, which were not and why, and which the pass
did not reach. Until it is closed, such a child holds none of its parent's
child slots but counts toward the tree's node budget.

If a child still reads recovery-required:

- `recovery_lease_held`: another process still holds its session. The lease
  clears about 30 seconds after that process is gone; request recovery again.
- `recovery_owner_live`: this process is still settling the child; it finishes
  on its own.
- an interrupted seal or adopt, or a refusal no retry can get past (an
  incompatible session, or `recovery_effect_evidence_refused`): its
  `list_agents` row names `close_agent`. Inspect the copy, then close the node.
- a copy removal that failed or was interrupted (`workspace_cleanup`): close
  the node too. Closing it settles the removal, so plakd retries it once and the
  copy ends `discarded`; in a Git checkout, a copy you already deleted by hand
  (directory and worktree registration) counts as removed.

## `session ... was created by an incompatible plakd build (state ...); for offline migration ...`

The session belongs to another state key. `plakd run --session` does not
export it, import it, or redirect to a successor. Follow the
[offline migration procedure](host-service.md#offline-migration-at-a-format-cut):
stop the host, export with the source build, and import with this build.
Use the new session ID printed by import. A duplicate import names its
existing successor. Fix any configuration refusal and retry the import
with the existing file.

This build imports export versions 12 and 13. Release 0.0.2 wrote version
8; finish those sessions on 0.0.2 or start new ones. The old state remains
untouched.

## A service stopped after the turn

Process jobs can survive an individual turn. By default a foreground command
is promoted after 30 seconds and has a ten-hour hard runtime cap, configurable
under [jobs](configuration.md#process-job-bounds). Jobs remain owned: explicit
stop, session release or the owning attempt ending cleans them up. `nohup`
does not escape that ownership. Files written to the workspace remain.

`plakd run` waits for promoted work but not shell `&` leftovers, and stops owned
jobs at exit unless `--leave-jobs-running` releases the leftovers as unowned.
Redirect service output to a file if it must outlive capture closing. See
[run lifetimes](cli.md#plakd-run).

If a later test or client needs a server, check its lifetime and network scope
separately. A failed HTTP check alone does not distinguish an exited process
from an unreachable service. Process tools share the host's network namespace;
see the [security specification](../spec/04-security-and-boundaries.md).

## `protected history cannot be replayed under the selected binding`

The selected model or account cannot replay native or opaque history, or its
exact replay artifact is no longer available. The request was refused before
dispatch; this is not a context-capacity error. Switch back to the model and
account that produced the history, start a new session, or fork
from a checkpoint before the incompatible history. Returning to the producing binding
cannot restore an expired or missing replay artifact. Recovery and compaction
do not authorize replay on another binding.

## Context admission failed after prompt submission

A submitted prompt can be retained in prompt history even if model preparation
failed. An empty queue does not prove the prompt was deleted. Clients should
reread the host's history and delivery facts before offering a retry.

A local context-admission denial happens before that model request is sent.
Compaction may be needed before resending; use the context-capacity recovery
controls or a compact operation, which may invoke paid inference. Recovery settles
interrupted durable work; it does not make a full conversation smaller.

The displayed occupancy and admission bound are different quantities. In
particular, a first or imported Claude OAuth request can have wire bytes near
its operating envelope while provider-reported occupancy is much lower. Once a
completed response anchors it, unchanged input is covered by that count and only
new or changed items use the conservative byte bound. This is not evidence that
the session database is corrupt.

## `Tool output was withheld for context capacity`

A tool can finish successfully while its model-visible output is clipped or
replaced to fit the next model call. A clipped result keeps its first and last
lines, with a `[N bytes omitted from the middle of this output …]` line between
them, so a build's final error survives; a paged file read keeps its beginning
and says where to continue. A successful read therefore does not prove the
model received the file contents. When nothing can be kept, the notice says:

> Tool output was withheld for context capacity. The harness compacts automatically when enabled; retry with a smaller read or a narrower query afterwards.

With Auto compaction enabled, capacity clipping schedules a compact immediately
after the tool group commits, even when the retained output fits and occupancy is
below the normal trigger. Tools are not rerun. If no compact commits, the turn
stops with a context-limit error rather than answering from that shrunken history.
For a child, this is reported as `context_exhausted`: respawn with a handoff
of the completed work and remaining task instead of sending a follow-up into
the same full session. A result no larger than the withheld notice is kept
verbatim, but exhausted capacity still requests automatic compaction.
With Auto compaction disabled, the bounded result remains and the model can retry
more narrowly. A tool's own per-result cap does not itself request compaction.

Inspect the resulting artifact before accepting the task as complete. To
continue, name the missing work and ask the model to reread the missing sources
in smaller batches, save verified progress as it goes, and check the complete
artifact against the original requirements. Compaction can create room for
later reads, but cannot recreate discarded source data. plakd does not silently
replay the task; a continuation prompt is new work and does not turn the earlier
incomplete result into a success.

## `unknown config key: …` / `unknown config section [...]`

`plakd config show` and `plakd doctor` both fail on an invalid `config.toml`, and
the whole product refuses to start rather than ignoring the key: `plakd run`
and `plakd agent` exit 1 with `plakd: invalid product config: ...` before any
prompt, credential read or session.

```
$ plakd config show
plakd: /path/to/config.toml: unknown config key: misspelled_key (line 2, column 1); run `plakd config edit` to fix it
```

A key the product does not know is an unknown key, whatever it once meant.
A few older sections still fail with the replacement named in the message:

- `[tui]` and its subtables — delete the section and all its settings before
  upgrading to 0.0.4. For a section beginning on the first line, the exact
  config diagnostic is `the [tui] section was removed with the public terminal client in 0.0.4; delete it (line 1, column 2)`.
  Leave `[host] reload_requests` unset until the old build is retired;
  the previous 0.0.3/U27 build rejects it. There is no acceptance or automatic
  conversion of old terminal configuration.
- `[agent.<name>]` in `config.toml` — move it to `~/.plak/agents/<name>.toml`.
- `[default]` — its tuple fields moved into template files, and
  `subagents` moved to `[orchestration]`.

Edit the file (`plakd config edit`), delete or rename the offending key, and run
`plakd config show` again. See [configuration](configuration.md).
For a trusted project overlay, edit the project file named in the error instead.
Doctor also checks trusted overlays and template files; its remedy never asks
you to run a command that needs the invalid configuration to parse first.

## `… is signed out; run …` and other subscription remedies

Authentication and subscription-consent failures exit 3 and name their remedy.
An unsupported model, effort or composition is a usage error (exit 2). A
signed-out, revoked or quarantined credential needs `plakd login <account>`; a
credential that needs a refresh names `plakd refresh <account>` first. Catalog
or token failures caused by the network say to check it and retry. Contract
drift calls for a newer build, not another login; a turn-time contract refusal
is an agent failure (exit 5 in `plakd run`) with that same newer-build remedy.

## `plakd mcp login` finds no authorization metadata

Discovery never follows redirects: a well-known metadata URL that redirects
counts as absent, and any other redirect fails the login.

## MCP Metadata Issuer Mismatch

`authorization server metadata found … but its issuer … differs from expected
issuer …` means discovery found a document, but its `issuer` is not identical
to the issuer listed in protected-resource metadata (including a trailing
slash). Ask the server operator to align those values; plakd will not send
registration or token requests using that document. This is not missing
metadata or a resource-path mismatch. Diagnostic URLs omit user information,
query and fragment.

## `model re-resolution refused: …`

Codex and Grok subscription models are admitted from a catalog read that is
valid for 6 hours. A root turn or a child agent that runs longer (for example a
root waiting on children) continues on a fresh read of the same model between
model calls, with no action from you. It stops with this message, before
anything is sent, only when that fresh read could not be used. Follow its
specific reason: contract drift needs a newer build, while a signed-out or
quarantined credential names `plakd login codex`, `grok`, or `claude`. A same-account
Claude login preserves the session binding; retry the prompt or attach.
Another account or a changed binding refuses exact reopen: restore the original
login/configuration or explicitly carry/rebind history through host operations.
Codex/Grok re-login changes the login-selection fence even for the same account;
restoring that old selection is not possible. Explicit carry is required rather
than an implicit account substitution.
`plakd doctor` and `plakd status` show the sanitized last refresh failure with its
time and credential generation; no raw response body or token is retained.
After following the reported remedy, retry the prompt: it reads the catalog afresh rather
than reusing the read that was refused.

## `the <account> compatibility notice needs an answer on stdin and none came`

The first use of a subscription account asks you to accept a revision-pinned
compatibility notice (see the [CLI reference](cli.md#subscription-compatibility-review-deadlines)).
A `plakd run` with a closed stdin cannot answer it. Run `plakd login <account>`
once interactively, or pipe the answer: `echo y | plakd run ...`. A `plakd login`
with a closed stdin asks for the same: answer at a terminal, or
`echo y | plakd login <account>`.

## `native compatibility profile compiled into this build is inactive`

The subscription compatibility profile compiled into your build passed its
expiry (shown in the message and by `plakd status`), 90 days after its review,
or was withdrawn. Logging in again cannot renew it; build a newer plakd. A
re-reviewed build that changes no pinned contract keeps your acceptance and
your login. A build whose notice changed asks you to accept it again, without
a new login; a build whose OAuth contract changed carries a new profile and
needs `plakd login <account>`. This build's subscription profiles (Codex, Grok,
Claude) are valid until 2027-02-13T00:00:00Z.

The documented API routes (OpenAI and xAI Platform, Anthropic API, OpenRouter
Chat) never refuse on the calendar. Their reviews are due 90 days after the
review date (2027-02-13T00:00:00Z for every API route in this build); after
that the routes keep working and a newer build carries re-reviewed facts.

`plakd doctor` and both forms of `plakd status` write one line 14 days before
each compiled deadline and after it, naming the surface and UTC date, including
accounts you have not configured: a `Warning:` that the build will refuse or
refuses a subscription route, or a `Note:` that API facts are or were due for
review. Install a newly reviewed build; neither a warning nor a new login
extends a subscription profile. Publication dry runs and uploads refuse a build
with a third of any review window or less remaining (30 days of 90); this
check is not part of the development qualification gates; their package step
only reports remaining days.

## `native subscription storage is unsafe: <path> …`

The credential store under your XDG state directory refused a path that breaks
its rules. The message names the path, the rule and the repair, with the path
quoted for the shell. Inside `plak/subscriptions` directories must be mode
`0700` and files `0600` with exactly one hard link, everything owned by you.
The state directory itself must be yours, give you `rwx`, not be writable by
group or others and carry no setuid, setgid or sticky bit. No path on the way
may be a symbolic link. When the problem is the state directory's location
rather than its mode (a link such as macOS `/tmp`, a directory another user
owns, a parent you cannot read or write), the message tells you to set
`XDG_STATE_HOME` to a directory you own whose path has no links instead of
changing that path. A credential file you move aside needs a fresh
`plakd login <account>`. Fix what it names and retry. If no path explains the
refusal, the message names the state directory: check that it and its parents
are directories you own and can write.

## `the stored <account> login was written by an older build or another profile …`

A stored subscription credential uses a record format this build no longer
reads. Claude logins made before the grant-bound record format are one case;
Grok logins made before the current login selection format are another.
plakd does not convert old records. Run `plakd login <account>`: it retires the
old record locally, prints one line saying so (this build cannot revoke it at
the provider, so the old grant may remain active), and authorizes a fresh one.
`plakd logout <account>` also retires it without logging in again. The rest of
the home is untouched. Bare `plakd status` puts that repair on the account's
Setup needed row (`Profile changed - run plakd login <account>`) and still lists
every other account, exiting 0; `plakd status <account>` prints
`credential=profile-changed; run plakd login <account>`, and `plakd doctor`
reports the account as `profile_changed`. Model runs, usage and refresh print
the repair on stderr and exit 3. Host admission refuses with the same repair.

## `the stored <account> login was written by a newer plakd build ...`

A newer plakd build logged in to this account, and this (older) build reads
its record, receipt or recovery state as newer than it understands. It sends
nothing, and status, usage, refresh and runs leave the files exactly as they
are. Use the newer build. To use this build anyway, run
`plakd login <account>` in a terminal and answer `y`: the newer build's login
is retired here and that build must then log in again. Without a terminal, or
with any other answer, nothing changes. `plakd logout <account> --local`
retires it without the prompt.

## `plakd: ... legacy state entries ... remain in ...`

`plakd doctor` lists non-current state paths with their sizes, or a size-unavailable
warning. Remove unused non-current directories and regular flat files explicitly
with `plakd state prune-legacy` (or `--yes` without a terminal). It refuses in-use locks, unsafe links and unreadable databases, and
never selects the current key. Startup never deletes legacy state.
The same explicit command lists and removes older-schema telemetry databases
and their SQLite sidecars in `telemetry/`, including `telemetry.sqlite3`.
Current and newer telemetry schemas remain untouched; nothing is pruned
automatically.

Each plakd build keeps its state in `~/.plak/state/<format key>/`, where the
key names the database schemas and runtime format it reads, for example
`r28.e39.p39.t11`. When you run a build whose formats differ from the last one,
it starts fresh in its own directory and prints this notice once, naming the
state root it will not open as session authority. Size accounting is best-effort
and cannot fail a command. Nothing was moved, upgraded or deleted: the old
build can still use its own directory. Your `config.toml`, templates, logs,
telemetry and OAuth credentials live outside `state/` and are shared.

Inspect paths with `plakd doctor` and use `plakd state prune-legacy` only when you
no longer need the sessions in those older or newer builds. To move eligible
sessions to another key, use
[offline migration](host-service.md#offline-migration-at-a-format-cut).

## `... was created by an incompatible plakd build; move it aside to start fresh`

A database in this build's own state directory is at a schema this build does
not read. That only happens to a damaged or hand-placed file. Move the named
file (and its `-wal` and `-shm` companions) out of the directory and run plakd
again; it creates a fresh store.

## A reasoning model needs `--effort`

There is no default effort; the catalog row decides. A reasoning model requires
`--effort <none|low|medium|high|xhigh|max|ultra>` and a non-reasoning model refuses
it. `plakd agent` says so directly:

```
plakd agent requires --effort or an effort in the [root] agent template
```

If a profile's Fork now changes the live root, the rebind
reports the same rule in its own words: `rebind target does not admit a reasoning effort` when you gave
one to a model that takes none, `reasoning rebind target requires an explicit
effort because medium is not admitted` when the model has no safe default, and
`rebind target effort is absent from the exact Ready catalog` when the level is
not on that model's list.

Put `effort = "low"` in a template to stop passing the flag every time.

## The model is not in the catalog

```
model `gpt-5-turbo` is absent from the authenticated `codex` catalog; run
`plakd models codex` to list the models it offers
```

Model ids are exact — no prefixes, no inference, no fuzzy matching. Run
`plakd models <account>` for the authoritative list.

## OpenRouter Usage rejects the key

HTTP 401 means the selected key is invalid or revoked; HTTP 403 means access
was forbidden. The Usage reason names the key source, not any key material.
If it names `OPENROUTER_API_KEY`, that variable overrides the stored key file.
Run `unset OPENROUTER_API_KEY`, remove its stale export from your shell startup
configuration, and start a fresh plakd process to use the file. Alternatively,
replace the variable's value through your normal secret setup and restart the
resolving process; another shell cannot clear a running host's environment.
If the reason names a `sha256:` fingerprint, run `plakd key set openrouter`.
plakd refuses to change key files while an overriding environment variable is set.

A Usage read does not refresh an already expired token. For an expired Codex,
Grok or Claude token it reports that the credential needs a token refresh: the
next turn refreshes it, or run `plakd refresh <account>`. A still-fresh token rejected by
the provider can trigger one forced refresh and retry. "Provider reported a
usage window that has already ended" refers only to provider data; it does not
mean the local access token expired. A Busy read is local writer or refresh
contention (a credential write, or a token refresh in progress in this or
another plakd process); it is retried and is not a storage-safety failure.

## The OpenRouter warning in `plakd doctor`

```
Openrouter catalog  WARNING  key present · point the root template at
OpenRouter to probe catalog keys without inference
```

It is a warning, not a failure -- the key is fine, plakd just has no OpenRouter
model to probe with. To silence it, either author
`~/.plak/agents/<name>.toml` with `account = "openrouter"` and
`model = "<id>"` and run `plakd config set root.agent <name>`, or just use
`plakd run --account openrouter --model <id>`. The probe is discovery GETs only;
it never runs inference.

## Evaluator host is unavailable

```
JavaScript host is unavailable: <error>; set PLAK_JS_HOST or place plakous-js-host beside plakd
Python host is unavailable: <error>; set PLAK_PY_HOST or place plakous-py-host beside plakd
```

`native-ptc` and `js-ptc` need `plakous-js-host`; `py-ptc` and `py-sdk` need the `plakous-py-host` worker.
A missing sidecar fails closed rather than degrading to `direct`.

`just build debug` and `just build release` build `plakous-js-host` and `plakous-py-host`
into the same Cargo output directory as `plakd`. `just install release` and
`just install maintainer` install a snapshot with those workers beside `plakd`.
The usual cause is running a `plakd` from somewhere else -- a copy, or a stale
`~/.cargo/bin/plakd`. Rebuild and install, or point `PLAK_JS_HOST` at the sidecar.
Build the parent with `cargo build --release --locked -p plakd`, then both workers
with `cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker`.
The Python interpreter is linked only into `plakous-py-host`.
Rebuild the complete product, or set `PLAK_PY_HOST` to that `plakous-py-host` binary.

## A Python cell raises `PermissionError` or `RuntimeError` for host access

```
PermissionError: Permission denied: '/etc/hosts'
RuntimeError: 'os.getenv' is not supported in this environment
RuntimeError: filesystem access is not available in cells; use the nested tools
```

The Python cell has no host filesystem, environment or network access.
`time.sleep` and `asyncio.sleep` raise instead of waiting. OS entropy is also
unavailable: `os.urandom`, unseeded `random` and `uuid4` are refused. Clock
reads (`datetime.now()`, `time.time()`, in UTC) and explicit seeding such as
`random.Random(42)` work. Use the nested tools for workspace effects.

The interpreter charges at most 256 MiB of heap. Going over that limit raises
a catchable `MemoryError` and does not reset the interpreter. A separate
656 MiB worker allocator backstop leaves room for transient native allocations;
exceeding it is worker loss. It is not a process-tree or RSS limit.

## Python tool result delivery was not acknowledged

With `py-ptc`, use `await tool(key=value)` or pass one argument dictionary.
Structured results are Python values, text results are strings, and a rejected
tool raises `ToolError`. `await asyncio.gather(...)` runs independent calls
concurrently. Calling a tool without awaiting or scheduling its coroutine does
not run it. Use `print(...)` to expose selected results to the model.

Once a tool starts, cancellation or interpreter loss can leave a file change or
other effect committed without a confirmed delivery of its result to Python.
plakd then ends the turn recovery required rather than silently replaying the
cell; the next prompt records that the cell may have run tools that changed the
workspace, and continues.

The diagnostic `nested local composition result receiver disappeared or
rejected an over-bound result` does not identify one cause and does not by itself
prove a provider error or worker crash. Inspect the generated code and actual
effects before choosing recovery; do not blindly repeat a command that may
already have run. A successful file change alone does not prove the Python cell
completed successfully.

## A jj working copy went stale

plakd checks the working-copy operation before checkout. Harmless changes can
be reconciled in process. A rewritten base, concurrent publication, or partial
workspace operation that cannot be completed safely returns
`workspace_recovery_required` and retains the copy and its modified or
untracked files.

Inspect the retained copy and its tree status before choosing recovery. plakd
will not reset those files, seal a candidate from uncertain state, or report a
failed removal as a discarded workspace. Avoid operating on minted copies
under `.plak/workspaces/` while their agents are active.

If a copy has become a standalone repository, such as after running `jj git
init` inside it, it is no longer a workspace of the launch repository. Preserve
any needed files before replacing it with a new agent workspace.

## Nothing from a child agent showed up in my checkout

With the default `workspace = "inherit"`, child edits already affect the shared
checkout. A child using `workspace = "fork"` instead seals its edits as a
*candidate*, which reaches your working tree only when an agent with `may_adopt`
adopts it -- as uncommitted changes, never a commit. Request the tree view
through your client or inspect `plakd session show <id>` for the child's status:
`active`, `adopting`, `adopted`, `discarding`, `clean_removed`, `discarded`.
`merge_conflict` and `target_changed` are failed adoptions that keep
the child's copy intact. See
[agents and orchestration](agents-and-orchestration.md#candidates-and-adoption).

## `tree_hosted_elsewhere (<build>): agent tree … is hosted by …`

Another running `plakd` holds this session's agent tree: only one process
works a tree at a time. `plakd run` and `plakd agent` exit 7 (`plakd agent`'s
error line carries `code: "tree_hosted_elsewhere"`). Quit the other `plakd`, then open the session
again. If that `plakd` crashed on this machine, the next open takes the tree
over by itself. If it ran on another machine sharing this home, or is gone
in a way that cannot be proven, release it explicitly with
`plakd tree release --force <tree-id>` (see the CLI reference), then open the
session again.

## Restored sessions with a paused queue

Reopening a session preserves queued prompts but pauses automatic delivery.
In `plakd agent`, send `{"v":1,"kind":"resume_queue","id":"resume-1"}` to
resume queue delivery explicitly. JSONL clients use `resume` for held work
and the restored queue;
`resume_queue` alone does not resume the tree.
The default loss policy holds work even after recovery settles its old turn.
See [host-loss policy](../spec/07-config-reference.md#3-configtoml).

If scheduled compaction leaves a prompt queued, read the preceding compaction
result and correct its cause before resuming. Retrying compaction is a new
paid operation. Use `clear` in JSONL or the host new-session operation to start a new
session when retaining the old context is unnecessary. A compaction whose
outcome was never observed is closed without being sent again when the session
is next used; queue resumption does not send it again either.
If plakd stopped during compaction after publishing your answer, reopening keeps
that answer and the history and closes the interrupted compact. If its summary
had already arrived, the compaction is applied from that saved summary.
Otherwise one notice says compaction was not applied: a summary known not to
have been sent says "was not sent", and otherwise it says "may have been
billed". It is never sent again; an explicit later compact is a new operation.
A compact you asked for that had not started when plakd stopped is not started
by reopening: one notice says a compaction-not-started notice with an explicit compact remedy.
An already committed compact keeps its committed outcome on reopen.
If compaction committed before a new turn's first model request and plakd then
stopped, reopening abandons only the unfinished turn. The committed summary
stays in the conversation; neither that compact nor the prompt is retried.

## A turn finished just before plakd stopped

Once a final answer has fully arrived it is saved before anything else
happens. If plakd stopped after that, reopening finishes the turn with that exact
answer once, without another model call, and shows it. If the runtime had also
finished the turn but plakd stopped before updating its transcript, reopening
shows the saved result once. A process job live at the crash still reports an
unknown outcome. A failed shutdown save is reported, but does not prevent
canonical ownership release and an in-process reopen.
After releasing a stored session, plakd applies the release and any final
abandonment or job results to its views. A slow pass that reaches its time
limit leaves harmless projection debt for the next open and does not make
quit or a one-shot command fail. A store or integrity refusal is reported as
a session cleanup failure; the next open retries the projection.

If the host stopped after the tools finished but before the next model request,
reopening reports an abandoned turn instead of a finished answer. Completed
tool results stay in history, live jobs report an unknown outcome, and a new
prompt works without retrying the tools.

If a tool finished before its result was added to the conversation, reopening
instead says that its result was not added. Its recorded completion is retained,
but recovery does not invent a conversation result or retry the tool. Verify the
workspace before relying on that unfinished turn.

## A session id was printed, but resume says it was not found

The telemetry event `session.ready` means the runtime attached. A fresh
`plakd run` can emit it before publishing its product session record; a kill in
that interval can leave an event log without a resumable session. An event log
alone does not establish that a turn was admitted. Start a new session if the
product reports that the id was never published.

JSONL is different: `plakd agent` publishes its product session record before
`hello`/`ready`. An idle session that reached that handshake should reopen even
without a first prompt. A failure to reopen it needs investigation; it is not
explained by the telemetry timing boundary above.

## `session store disk is full` / `prompt was not admitted`

Physical disk exhaustion is separate from the logical retained-state limits.
Run `plakd session gc --all`, delete sessions you no longer need with
`plakd session delete <id>`, or free disk space. The logical reserve does not
reserve disk blocks.

## Low storage

`plakd doctor` and host notices warn when the product volume has less than
5 GiB free. Below 512 MiB, new root turns and spawns are refused
(`storage_low`); work already running is not cancelled. Free space, then
retry. `plakd doctor` names the volume, and `plakd session gc --all` reclaims
history this build no longer needs. plakd does not delete files to make space.

`prompt was not admitted` means the runtime definitely refused the prompt
before starting a turn. Its text remains queued with a `prompt remains queued`
note, not a duplicated input draft; definite queued rejections are labeled
`rejected:`. There is no recovery-pending note or recovery fence, but storage-wide
refusals stop the live drain pass. Edit or submit a new prompt explicitly after
fixing the refusal.
Transient queue/backpressure refusals can retry the exact queued item once
capacity returns. A lost acknowledgement is settled by reopening the session,
which learns from the runtime whether the prompt was recorded and otherwise
sends the same queued item again under its own identity, exactly once; do not
resubmit it as a new prompt.

## MCP Resource Mismatch

OAuth discovery requires the published `resource` to share the endpoint's
scheme, host and port. An origin resource covers `/mcp` only when served at
`origin/.well-known/oauth-protected-resource`. More generally, path-segment
coverage (`/mcp` covers `/mcp/x`, not `/mcp2`) is allowed only at the published
resource's own RFC 9728 well-known URL. plakd normalizes that location by
stripping one trailing slash from the resource path. Elsewhere, including
custom challenge and endpoint-path documents, resource and endpoint must be
equal ignoring one trailing slash. Check both the published resource and the
location serving it; do not substitute a sibling path or another origin.
Diagnostics omit user information, query and fragment.

For live MCP testing, use a dedicated `PLAK_HOME` (mode 0700). New `[mcp]`,
`auth`, `oauth_*` and `resources` configuration keys do not load in earlier
builds; keep the installed build's home and any running session unchanged.

OpenRouter sessions bound to the older wire-admission identity refuse exact
reopen with Revalidation after the input-admission derivation changes. Start a
new session, or export/import as a new session; do not expect an exact reopen
to change its admitted contract silently.
