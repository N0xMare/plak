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

Start with `plak doctor`. Every failing row carries its own FIX column, and the
command exits 5 when anything is ERROR. Then `plak status` for credentials.

## `model refused the request`

The model declined the request; this is not a login or refresh failure.
`plak run` exits 5 after saving and releasing the completed turn. An empty
refusal produces no assistant stdout. Plak does not retry, change models, or
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
workspace is removed. `plak state` does not list these unregistered snapshots.
In the Git repository, list them with
`git for-each-ref --format='%(refname)' 'refs/plak/*/candidates/failed-*'`.
After inspecting or retaining the work elsewhere, remove an exact ref with
`git update-ref -d <exact-ref>`. Rescue refs are never adoptable candidates.
Repeated snapshots of the same failed turn refuse to overwrite its existing
ref and report that ref for inspection. For a non-colocated JJ workspace,
run the same commands with `git --git-dir=.jj/repo/store/git`.

## `retrying in 4s (2/4): provider busy (HTTP 529)`

Plak re-sends a turn's model request automatically whenever doing so can only
cost a duplicate inference:

- the provider refused it or it was never sent: a rate limit (HTTP 429),
  service unavailable (503), overloaded (529), or a connection that could not
  be established;
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
the request. Plak retries this automatically, including during `/compact`,
within the same budget below. Other TLS alerts, a locally detected decrypt
failure, alert 20 after response bytes, TLS 1.2, and HTTP/2 do not gain this unsent-only
retry permission.

Plak makes at most four ordinary requests, honoring the provider's `Retry-After` (a
longer wait is shortened to 60 seconds) and otherwise waiting about 1, 2 and 4
seconds (randomized by up to half), with at most 120 seconds of ordinary waiting.
When DNS, connection or TLS fails before anything was sent, it instead rides
out a short outage: up to twelve proven-unsent requests, waiting about 2, 4, 8,
16 then 30 seconds (randomized by up to half), with at most 240 seconds of
network waiting. These do not spend the ordinary retry budget. The retry row
says `provider unreachable (...); waiting for network, try N in Ns`.
Your hard deadline and cancellation still bound the call. Incomplete uploads
use the ordinary budget, not the outage allowance; compaction never replays a
request that could have reached the provider whole. Retries never switch routes.
Each request gets a fresh connection/head budget; a long response does not
consume the next request's budget. After a streamed generation longer than
300 seconds fails, at most one replay is allowed. On Responses routes, after
output starts, 300 seconds without semantic progress is a stall even if
keepalives continue. A newly added non-visible item (hidden reasoning or a
provider-hosted tool) suspends that limit until the next visible output. Before
output starts or while suspended, keepalives can sustain hidden reasoning and
hosted tools without that semantic limit. Each actual retry wait is shown: on the TUI status row as
`retrying in 4s (2/4): <cause>`, on `plak run` as a `plak:` line, in
`plak agent` JSONL as a `retry` frame, and in the log under target `plak.retry`
with session/turn/agent ids, checkout idle age when pooled, transport phase and cause (for a provider refusal, the provider's own
error message with credentials filtered out). If the broken stream had already
shown text or thinking, the TUI dims it and labels it `discarded -- retrying`;
only the replay's answer enters the conversation, and its usage is the only one
counted. Text and thinking from earlier model calls of the same turn are kept.
Press Esc (or cancel the JSONL turn) to stop waiting; nothing is sent after
cancellation.

If the turn still fails with a transient connection or provider error, Plak
can continue the task automatically in a new turn. The notice shows the
continuation budget and wait: connect failures wait for the same route to be
reachable, while other transient errors use a short bounded delay. Completed
actions are not replayed. `orchestration.transient_resumes` defaults to `1`;
set it to `0` to disable this behavior (maximum `2` per user prompt).
A new prompt, steer, Esc, or quit cancels the pending continuation without
blocking your input. Reload drops a pending root continuation; send a prompt
to continue after reloading. `plak run` still obeys its timeout and turn limit.

**Request corrupted in transit (TLS record rejected)** means the peer rejected
a TLS record of the incomplete upload and closed that connection, not that the
provider was unreachable. The first resend is immediate on a fresh connection;
later resends retain normal backoff and the same four-request budget. Repeated
occurrences indicate in-path corruption. Try the same request from another
network (for example a mobile hotspot), then compare with VPN/proxy disabled
where permitted. If that network works, check the original router, VPN/proxy,
and ISP path; do not disable TLS verification or paste request contents into
logs. A local adapter can be the corrupting hop: USB Ethernet adapters with TCP
segmentation offload are a known source. If a large upload with another client
from the same machine (for example `curl --data-binary @file` of a few
megabytes to the same host) also ends in `alert bad record mac`, the path is at
fault, not Plak; switch adapter or network. Retry log records for roots and children include host, route path and
request body byte count, never the body or query parameters.

`plak telemetry status --views` includes `v_replay_loss`: content-free retry
and declined-retry rows. A declined row records `declined_reason` (`deadline`,
`budget`, `server_effect`, `retry_disabled`, or `workflow`); its next attempt
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
request with no provider-side effects Plak re-sends it automatically (see
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
- `plak doctor` reporting `Runtime store` as `full` (or `nearly_full` shortly
  before).

Rarely the reserve itself runs out, for example when many turns were finishing
at once. A turn then fails with the first line
`session store reserve is exhausted (<bound> <used>/<limit> (settlement writes
stop at <threshold>)) - a running turn could not record its outcome and needs
recovery ...` (or `recovery writes`). That turn needs recovery: free space as
below, then run `/recover` in that session.

First reclaim superseded checkpoint history, which keeps every session whole:

```
plak session gc --all
```

It covers every plak session in this home (sub-agent runtime sessions are
pruned by their own turns), prints what it reclaimed and names any session it
skipped: one open in a running plak (close it and run `gc` again), one with an
unfinished turn (run `plak session recover SESSION_ID`, then `gc` again), or
one with a pending workflow (open the session once, then `gc` again). It
also finishes deletes a crash interrupted. Turns reclaim their own history once
enough builds up, so `gc` frees most for long-lived or imported sessions.

If the store is still full, delete sessions you no longer need:

```
plak session list
plak session delete SESSION_ID
```

`gc` and deleting are never refused for lack of space. Deleting is refused while the session is
open in a running plak, while its agent tree has a child that is still running
or resumable or needs recovery (resolve it with `recover_agent`), and while other sessions continue from it or share its history
(forks, rebinds, `/clear` targets); the refusal names what
blocks it. A finished child's history and the agent tree are deleted with the
session. A delete that failed part way finishes when run again (or with
`plak session gc --all`). `/clear`, compaction, a fresh session, and `/recover`
do not reclaim retained state. The database file shrinks as `gc` and deletion
return free pages; `plak doctor` shows the file size and its free bytes.
Opening a session and recovering an interrupted turn keep
working while the store is full, because they write into the reserve.

Do not delete state files or change stored counters by hand, and do not use
export/import as a repair for an unresolved turn.

## `needs recovery · next prompt recovers`

After a crash, a completed answer can be recovered even with queued steering.
One notice tells you if that steering was not applied; it is not automatically
retried. An abandoned turn includes the same steering warning in its single
recovery notice. Steering is applied only when its text and command
finalization were promoted together atomically; an earlier identical text does not prove
application. A crash after recording completion but before that promotion
leaves steering unapplied. A completed model round interrupted before its next
request stays in history with an interruption note instead of being treated as a final
answer.

A turn whose outcome Plak could not observe ends *recovery required*: a stream
that broke where a provider-hosted action was possible, a process it could not
prove had stopped, a checkpoint it could not confirm, a lease it lost. The
session is not lost and you do not need `/clear`. The next thing you do that
needs the session — a prompt, `/compact`, a model change — first reopens it in
place and then runs. `/recover` in the TUI, `{"v":1,"kind":"recover","id":"r1"}`
in `plak agent`, or `plak session recover SESSION_ID` does the same on its own.
`plak run` exits 5 for such a turn, and the next `plak run --session` recovers
it.

If a context-capacity stop cannot be saved to the product transcript, queue drain
stays paused even after storage recovers. The next prompt or `/recover` first
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
still appears once in the current TUI or JSONL stream, but is not saved as
another transcript result. Otherwise it is saved with the recovered result.
This is the same for `/recover`, the next prompt's in-place recovery, and reopening
after a crash; a saved result's notice never gets a second fallback line.

A saved text-only answer also completes when the provider stopped for length
or refusal. A length-truncated local call never runs: recovery fails it as an
output-limit error only if the turn already took the retry for this truncation;
a successful round resets that retry evidence. Otherwise it abandons
the turn without retrying. An existing terminal answer is never replaced by a
second result merely to settle uncertain sibling work. Previously refused
saved-terminal cuts can settle that debt without an interruption note;
already-recoverable interrupted turns keep their existing recovery notice.

The same happens when you reopen a session after a crash, a `kill -9` or a
machine that stopped: the next `plak resume` or `plak run --session` closes the
turn it left. That includes a turn lost inside a Code Mode `exec` (or a
JavaScript or Python cell), even while a command the cell started was still
running: the cell and that command are reported as **a tool may have run**
(or **its pending work never started** when the cell had not begun), and
neither is run again. Recovered child agents end `failed` with kind
`turn_abandoned`;
the parent can re-task them with a follow-up, and a crash does not spend the
tree's attempt budget.

If prompts were queued behind the interrupted turn, they stay queued and
paused, since they were written for a turn whose outcome nobody saw: review
them, then press Tab and `r` in the TUI (or send `resume_queue` in
`plak agent`) to deliver them. A prompt you type now runs at once.

A `/fork` request a crash interrupted before it started is withdrawn on
reopen with `fork did not start; run /fork again`. A fork that already landed
is saved as a separate resumable session, even if its live attachment failed;
reopening the source keeps the source selected. Fork recovery never reruns the
request. A model change that could not complete says it was abandoned and the
session is unchanged; choose the model again to retry.

If recovery itself fails — offline, or a credential that needs `plak login` —
the prompt is refused with the reason and your draft is kept; the next prompt
tries again.

## A laptop slept during a session

A session held by another process reports that it is busy and its lease is
held by another owner. Close that process or wait for its lease to expire;
this is contention, not a session that requires recovery.

A session lease is 30 seconds. When the machine wakes, Plak revives the same
lease if nobody else opened the session meanwhile, and the next prompt runs as
usual. If another `plak` opened it in the meantime, this one can no longer
write to it: its next prompt reports that the session could not be reopened
because it is held elsewhere, and keeps your draft. Continue in the other
`plak`, or quit it and prompt here again.
Quitting immediately after wake also releases an untouched expired lease;
you do not need to send a prompt first. It never releases a replacement lease.

## A session or child requires recovery

Run `plak session show SESSION_ID` to inspect the root checkpoint and its
persisted children. It reports unresolved canonical obligations and distinguishes
them from tree-node retirement. A root can be quiescent while a child still
requires recovery. Inspection does not resume work or clear the condition;
missing or unreadable evidence remains explicit.

A root head stays recovery-required after a reopen only when its saved record
is damaged or contradicts itself (a typed integrity failure); every state a
crash can leave closes on reopen. For such a root session, keep it for diagnosis and start a new one for the remaining work;
a fresh session does not mark an old child recovered.

A recovery-required **child**, including one whose release was refused, is
settled by reopening its session exactly once more: reopening or resuming the
tree does it in the background, `/recover` does it for the whole tree, and a
parent can call `recover_agent` for one child. Nothing is replayed, and a note
says which children were recovered, which were not and why, and which the pass
did not reach. Until it is closed, such a child holds none of its parent's
child slots but counts toward the tree's node budget.

If a child still reads recovery-required:

- `recovery_lease_held`: another process still holds its session. The lease
  clears about 30 seconds after that process is gone; run `/recover` again.
- `recovery_owner_live`: this process is still settling the child; it finishes
  on its own.
- an interrupted seal or adopt, or a refusal no retry can get past (an
  incompatible session, or `recovery_effect_evidence_refused`): its
  `list_agents` row names `close_agent`. Inspect the copy, then close the node.
- a copy removal that failed or was interrupted (`workspace_cleanup`): close
  the node too. Closing it settles the removal, so Plak retries it once and the
  copy ends `discarded`; in a Git checkout, a copy you already deleted by hand
  (directory and worktree registration) counts as removed.

## `session … was created by an incompatible plak build (state …); carry it forward …`

The session was written by a plak build whose stored format this build does not
read. Plak never opens another build's state. To keep working in that
conversation, run the build named by the state key (an installed snapshot
under `~/.local/lib/plak/release/` or `~/.local/lib/plak/maintainer/`, or an
older checkout) with
`plak session export <id>`, then run `plak session import <file>` with this
build: you get a new session carried forward from the old one, with its full
history. `plak --resume <id>` does this by itself when a kept installed
snapshot for that state key exists; resuming a session already carried
forward opens the carried session instead of importing it again. Otherwise
start a new session; the old one is left untouched.

If a carry fails after the export was written (`/reload` could not start the
new build, or the import was refused), the export file is kept under
`~/.plak/carry/` and named in the error. `plak doctor` lists kept files under
`carry_files`; import each with `plak session import <file>`, or delete it.

Reload asks the destination build to check configuration before leaving the
current session. A refusal keeps the session open: fix the named config file
and retry `/reload`. If a successor was already committed, the error names it
and gives `plak --resume <new>`; do not import the original a second time.
Foreign resume checks configuration before running the old snapshot's export.
For a config refusal, fix the named file and retry the same command.
Foreign-resume exports are regenerable and are removed on failure. For a
version or unimportable-export refusal, list this workspace's carried successors
with `plak session list` and resume its newest carried session.

## A service stopped after the turn

Process tools own the process sessions they start for the current root turn.
When that turn ends, Plak terminates and reaps processes in those owned groups;
`nohup` by itself does not keep such a service available afterward. Files
written to the workspace remain. Within a turn, a polled or background command
runs for up to 30 minutes, and a long build's output keeps its newest part: the
result says how many earlier bytes were omitted.

If a later test or client needs a server, check its lifetime and network scope
separately. A failed HTTP check alone does not distinguish an exited process
from an unreachable service. Process tools share the host's network namespace;
see the [security specification](../spec/04-security-and-boundaries.md).

## `protected history cannot be replayed under the selected binding`

The selected model or account cannot replay native or opaque history, or its
exact replay artifact is no longer available. The request was refused before
dispatch; this is not a context-capacity error. Switch back to the model and
account that produced the history, clear context with `/clear`, or fork from a
checkpoint before the incompatible history. Returning to the producing binding
cannot restore an expired or missing replay artifact. `/recover` and `/compact`
do not authorize replay on another binding.

## Context admission failed and the submitted prompt is no longer in the composer

The composer clears on submission, but an ordinary TUI prompt is saved in
session prompt history before runtime delivery. On an empty composer, press
`↑`, select the prompt, then `Enter` or `←` to load it for editing. Loading
does not submit it. An empty queue can mean delivery was accepted before model
preparation failed; it does not mean the prompt was deleted.

A local context-admission denial happens before that model request is sent.
Compaction may be needed before resending; use the context-capacity recovery
controls or `/compact`, which may invoke paid inference. `/recover` settles
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
later reads, but cannot recreate discarded source data. Plak does not silently
replay the task; a continuation prompt is new work and does not turn the earlier
incomplete result into a success.

## `unknown config key: …` / `unknown config section [...]`

`plak config show` and `plak doctor` both fail on an invalid `config.toml`, and
the whole product refuses to start rather than ignoring the key: `plak run`
and `plak agent` exit 1 with `plak: invalid product config: …` before any
prompt, credential read or session.

```
$ plak config show
plak: /path/to/config.toml: unknown config key: tui.show_timestamps (line 2, column 1); run `plak config edit` to fix it
```

A key the product does not know is an unknown key, whatever it once meant.
A few older sections still fail with the replacement named in the message:

- `[agent.<name>]` in `config.toml` — move it to `~/.plak/agents/<name>.toml`.
- `[default]` — its tuple fields moved into template files, and
  `subagents` moved to `[orchestration]`.

Edit the file (`plak config edit`), delete or rename the offending key, and run
`plak config show` again. See [configuration](configuration.md).
For a trusted project overlay, edit the project file named in the error instead.
Doctor also checks trusted overlays and template files; its remedy never asks
you to run a command that needs the invalid configuration to parse first.

## `… is signed out; run …` and other subscription remedies

Subscription failures exit 3 and end with the command that repairs them. A
signed-out, revoked or quarantined credential needs `plak login <account>`; a
credential that needs a refresh names `plak refresh <account>` first. Catalog
or token failures caused by the network say to check it and retry.

## `model re-resolution refused: …`

Codex and Grok subscription models are admitted from a catalog read that is
valid for 6 hours. A root turn or a child agent that runs longer (for example a
root waiting on children) continues on a fresh read of the same model between
model calls, with no action from you. It stops with this message, before
anything is sent, only when that fresh read could not be used: a signed-out or
quarantined credential says `run plak login codex` (or `grok`); a new login,
another account or a changed model row since the session started says an
explicit rebind is required: rebind with **Fork now** in the Agents tab, or
start a new session.
After fixing the login, retry the prompt: it reads the catalog afresh rather
than reusing the read that was refused.

## `the <account> compatibility notice needs an answer on stdin and none came`

The first use of a subscription account asks you to accept a revision-pinned
compatibility notice (see the [CLI reference](cli.md#subscription-compatibility-review-deadlines)).
A `plak run` with a closed stdin cannot answer it. Run `plak login <account>`
once interactively, or pipe the answer: `echo y | plak run …`. A `plak login`
with a closed stdin asks for the same: answer at a terminal, or
`echo y | plak login <account>`.

## `native compatibility profile compiled into this build is inactive`

The compatibility profile compiled into your build passed its review expiry
date (shown in the message and by `plak status`) or was withdrawn. Logging in
again cannot renew it; build a newer plak.

The built-in OpenRouter Chat dialect is pinned the same way: from its expiry
(2026-11-13 UTC for this build) OpenRouter sessions refuse with `OpenRouter Chat
compatibility profile is not currently admissible`. Build a newer plak.

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
`plak login <account>`. Fix what it names and retry. If no path explains the
refusal, the message names the state directory: check that it and its parents
are directories you own and can write.

## `the stored <account> login was written by an older build or another profile …`

A stored subscription credential uses a record format this build no longer
reads. Claude logins made before the grant-bound record format are one case;
Grok logins made before the current login selection format are another.
Plak does not convert old records. Run `plak login <account>`: it retires the
old record locally, prints one line saying so (this build cannot revoke it at
the provider, so the old grant may remain active), and authorizes a fresh one.
`plak logout <account>` also retires it without logging in again. The rest of
the home is untouched. Bare `plak status` puts that repair on the account's
Setup needed row (`Profile changed · run plak login <account>`) and still lists
every other account, exiting 0; `plak status <account>` prints
`credential=profile-changed; run plak login <account>`, and `plak doctor`
reports the account as `profile_changed`. Model runs, usage and refresh print
the repair on stderr and exit 3. In the TUI the header reads
`not ready · credential_unusable: …` with the same words, and a prompt sent
then is refused with that repair and returned to the input instead of being
held.

## `the stored <account> login was written by a newer plak build …`

A newer plak build logged in to this account, and this (older) build reads
its record, receipt or recovery state as newer than it understands. It sends
nothing, and status, usage, refresh and runs leave the files exactly as they
are. Use the newer build. To use this build anyway, run
`plak login <account>` in a terminal and answer `y`: the newer build's login
is retired here and that build must then log in again. Without a terminal, or
with any other answer, nothing changes. `plak logout <account> --local`
retires it without the prompt.

## `plak: … legacy state entries … remain in …`

`plak doctor` lists non-current state paths with their sizes, or a size-unavailable
warning. Remove unused non-current directories and regular flat files explicitly
with `plak state prune-legacy` (or `--yes` without a terminal). It refuses in-use locks, unsafe links and unreadable databases, and
never selects the current key. Startup never deletes legacy state.

Each plak build keeps its state in `~/.plak/state/<format key>/`, where the
key names the database schemas and runtime format it reads, for example
`r25.e27.p32.t7`. When you run a build whose formats differ from the last one,
it starts fresh in its own directory and prints this notice once, naming the
state root it will not open as session authority. Size accounting is best-effort
and cannot fail a command. Nothing was moved, upgraded or deleted: the old
build can still use its own directory. Your `config.toml`, templates, logs,
telemetry and OAuth credentials live outside `state/` and are shared.

Inspect paths with `plak doctor` and use `plak state prune-legacy` only when you
no longer need the sessions in those older or newer builds. A `plak resume` of
one of those sessions says it was created by an incompatible plak build and names `plak session export` / `plak session import`, the way to
carry it into this build.

## `… was created by an incompatible plak build; move it aside to start fresh`

A database in this build's own state directory is at a schema this build does
not read. That only happens to a damaged or hand-placed file. Move the named
file (and its `-wal` and `-shm` companions) out of the directory and run plak
again; it creates a fresh store.

## A reasoning model needs `--effort`

There is no default effort; the catalog row decides. A reasoning model requires
`--effort <none|low|medium|high|xhigh|max|ultra>` and a non-reasoning model refuses
it. `plak agent` says so directly:

```
plak agent requires --effort or an effort in the [root] agent template
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
`plak models codex` to list the models it offers
```

Model ids are exact — no prefixes, no inference, no fuzzy matching. Run
`plak models <account>` for the authoritative list.

## OpenRouter Usage rejects the key

HTTP 401 means the selected key is invalid or revoked; HTTP 403 means access
was forbidden. The Usage reason names the key source, not any key material.
If it names `OPENROUTER_API_KEY`, that variable overrides the stored key file.
Run `unset OPENROUTER_API_KEY`, remove its stale export from your shell startup
configuration, and start a fresh Plak process to use the file. Alternatively,
replace the variable's value through your normal secret setup. `/reload`
inherits the current process environment and cannot clear this override.
If the reason names a `sha256:` fingerprint, run `plak key set openrouter`.
Plak refuses to change key files while an overriding environment variable is set.

A Usage read never refreshes a token. For an expired Codex, Grok or Claude
token it reports that the credential needs a token refresh: the next turn
refreshes it, or run `plak refresh <account>`. "Provider reported a
usage window that has already ended" refers only to provider data; it does not
mean the local access token expired. A Busy read is local writer or refresh
contention (a credential write, or a token refresh in progress in this or
another Plak process); it is retried and is not a storage-safety failure.

## The OpenRouter warning in `plak doctor`

```
Openrouter catalog  WARNING  key present · point the root template at
OpenRouter to probe catalog keys without inference
```

It is a warning, not a failure — the key is fine, Plak just has no OpenRouter
model to probe with. To silence it, either author
`~/.plak/agents/<name>.toml` with `account = "openrouter"` and
`model = "<id>"` and run `plak config set root.agent <name>`, or just use
`plak run --account openrouter --model <id>`. The probe is discovery GETs only;
it never runs inference.

## Evaluator host is unavailable

```
JavaScript host is unavailable: <error>; set PLAK_JS_HOST or place plakous-js-host beside plak
Python host is unavailable: <error>; set PLAK_MONTY_WORKER or place monty beside plak
```

`native-ptc` and `js-ptc` need `plakous-js-host`; `py-ptc` needs a `monty` worker.
A missing sidecar fails closed rather than degrading to `direct`.

`just build` and `just build-release` build and symlink `plakous-js-host` next to
`plak`, so the usual cause is running a `plak` from somewhere else — a copy, or
a stale `~/.cargo/bin/plak`. Rebuild, or point `PLAK_JS_HOST` at the sidecar.
Build the parent with `cargo build --release --locked -p plak`, then both workers
with `cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker`.
Separate invocations keep Monty runtime features out of `plak`.
Rebuild the complete product, or set `PLAK_MONTY_WORKER` to that `monty` binary.

## A Python cell raises `PermissionError` or "is not supported in this environment"

```
PermissionError: Permission denied: '/etc/hosts'
RuntimeError: 'os.getenv' is not supported in this environment
```

The `py-ptc` interpreter has no host access: opening files, reading the
environment, `os.urandom`, the network and `time.sleep` / `asyncio.sleep` are
refused by design. Use the nested tools for workspace effects. Clock reads
(`datetime.now()`, `time.time()`, in UTC) and `random` work inside the cell.

A `MemoryError` past the interpreter's 256 MiB limit fails only that cell; the
interpreter and its globals survive.

## Python tool result delivery was not acknowledged

With `py-ptc`, tools are async Python functions. `await tool({...})` runs a
tool and receives its string result; `await asyncio.gather(...)` runs several
concurrently. Calling a tool without awaiting or scheduling its coroutine does
not run it. Use `print(...)` to expose selected results to the model.

Once a tool starts, cancellation or interpreter loss can leave a file change or
other effect committed without a confirmed delivery of its result to Python.
Plak then ends the turn recovery required rather than silently replaying the
cell; the next prompt records that the cell may have run tools that changed the
workspace, and continues.

The diagnostic `nested local composition result receiver disappeared or
rejected an over-bound result` does not identify one cause and does not by itself
prove a provider error or worker crash. Inspect the generated code and actual
effects before choosing recovery; do not blindly repeat a command that may
already have run. A successful file change alone does not prove the Python cell
completed successfully.

## A jj working copy went stale

Plak checks the working-copy operation before checkout. Harmless changes can
be reconciled in process. A rewritten base, concurrent publication, or partial
workspace operation that cannot be completed safely returns
`workspace_recovery_required` and retains the copy and its modified or
untracked files.

Inspect the retained copy and its tree status before choosing recovery. Plak
will not reset those files, seal a candidate from uncertain state, or report a
failed removal as a discarded workspace. Avoid operating on minted copies
under `.plak/workspaces/` while their agents are active.

If a copy has become a standalone repository, such as after running `jj git
init` inside it, it is no longer a workspace of the launch repository. Preserve
any needed files before replacing it with a new agent workspace.

## Nothing from a child agent showed up in my checkout

That is the design. A child's edits are sealed as a *candidate* and reach your
working tree only when an agent with `may_adopt` adopts them — as uncommitted
changes, never a commit. Check Activity's Agents tab (`/tree`; `Ctrl+T` opens
the last Activity tab) for the child's
status: `active`, `adopting`, `adopted`, `discarding`, `clean_removed`, `discarded`.
`merge_conflict` and `target_changed` are failed adoptions that keep
the child's copy intact. See
[agents and orchestration](agents-and-orchestration.md#candidates-and-adoption).

## The TUI will not start

`plak` needs an interactive terminal and refuses a pipe. If it starts but feels
sluggish, check you are running the release build: `just build` prints a
warning for debug binaries precisely because their cold start is visible.

## Restored sessions with a paused queue

Reopening a session preserves queued prompts but pauses automatic delivery.
In `plak agent`, send `{"v":1,"kind":"resume_queue","id":"resume-1"}` to
resume the queue explicitly. In the TUI, focus the queue with `Tab` and press
`r`. `resume_tree` controls a persisted agent tree separately; it does not
resume the prompt queue.

If scheduled compaction leaves a prompt queued, read the preceding compaction
result and correct its cause before resuming. Retrying compaction is a new
paid operation. Use `clear` in JSONL or `/clear` in the TUI to start a fresh
conversation when retaining the old context is unnecessary. A compaction whose
outcome was never observed is closed without being sent again when the session
is next used; queue resumption does not send it again either.
If Plak stopped during compaction after publishing your answer, reopening keeps
that answer and the history and closes the interrupted compact. If its summary
had already arrived, the compaction is applied from that saved summary.
Otherwise one notice says compaction was not applied: a summary known not to
have been sent says "was not sent", and otherwise it says "may have been
billed". It is never sent again; an explicit later compact is a new operation.
A compact you asked for that had not started when Plak stopped is not started
by reopening: one notice says `compaction did not start; run /compact again`.
An already committed compact keeps its committed outcome on reopen.
If compaction committed before a new turn's first model request and Plak then
stopped, reopening abandons only the unfinished turn. The committed summary
stays in the conversation; neither that compact nor the prompt is retried.

## A turn finished just before Plak stopped

Once a final answer has fully arrived it is saved before anything else
happens. If Plak stopped after that, reopening finishes the turn with that exact
answer once, without another model call, and shows it. If the runtime had also
finished the turn but Plak stopped before updating its transcript, reopening
shows the saved result once. A process job live at the crash still reports an
unknown outcome. A failed shutdown save is reported, but does not prevent
canonical ownership release and an in-process reopen.
After releasing a stored session, Plak applies the release and any final
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
`plak run` can emit it before publishing its product session record; a kill in
that interval can leave an event log without a resumable session. An event log
alone does not establish that a turn was admitted. Start a new session if the
product reports that the id was never published.

JSONL is different: `plak agent` publishes its product session record before
`hello`/`ready`. An idle session that reached that handshake should reopen even
without a first prompt. A failure to reopen it needs investigation; it is not
explained by the telemetry timing boundary above.

## `session store disk is full` / `prompt was not admitted`

Physical disk exhaustion is separate from the logical retained-state limits.
Run `plak session gc --all`, delete sessions you no longer need with
`plak session delete <id>`, or free disk space. The logical reserve does not
reserve disk blocks.

`prompt was not admitted` means the runtime definitely refused the prompt
before starting a turn. Its text remains queued with a `prompt remains queued`
note, not a restored composer copy; definite queued rejections are labeled
`rejected:`. There is no recovery-pending note or recovery fence, but storage-wide
refusals stop the live drain pass. Edit or submit a new prompt explicitly after
fixing the refusal.
Transient queue/backpressure refusals can retry the exact queued item once
capacity returns. A lost acknowledgement is settled by reopening the session,
which learns from the runtime whether the prompt was recorded and otherwise
sends the same queued item again under its own identity, exactly once; do not
resubmit it as a new prompt.
