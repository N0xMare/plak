# Agents and orchestration

A Plak session starts with one agent. If you give it a working set of templates
and permission to spawn, it can hand pieces of the job to children. By default
they share your checkout; pass `workspace=fork` to mint an isolated copy and
later fold sealed edits back with `adopt_candidate`.

Everything here is driven by configuration files. There is no roster editor in
the TUI.

## Templates

An agent template is one flat TOML file, named by its filename:
`~/.plak/agents/<name>.toml`, or `<project>/.plak/agents/<name>.toml` for
templates that travel with a repo. A project file wins over a user file of the
same name.

```toml
# ~/.plak/agents/review-claude.toml
account = "claude"
model = "claude-sonnet-5"
effort = "low"
composition = "direct"
description = "Claude reviewer that reads and reports"
tools = "read-execute"
```

A template names an account and a model, never a credential. The fields you will
reach for first:

| Field | Meaning |
| --- | --- |
| `account`, `model` | The binding. Required. |
| `effort`, `composition`, `protocol`, `fast_mode` | Model options and tool composition. `composition = "native-ptc"` is the vertical's own programmatic tool calling, enrolled today only for Codex Code Mode; a card on another vertical is refused, so use `js-ptc` or `py-ptc` there (see [Providers and accounts](providers-and-accounts.md)). `fast_mode` is a template setting; its provider-native processing tier is resolved at bind. |
| `description` | The card the spawning model reads. Required for anything in the working set or in a `spawns` list. |
| `tools` | `full` or `read-execute`. Default `full`. `read-execute` omits dedicated file-edit tools but still permits shell commands. |
| `spawns` | The templates this agent may spawn when it runs as a child, e.g. `["worker", "reviewer"]`. Default `[]`: a leaf with no `spawn_agent`. |
| `may_adopt` | May it adopt a child's work? Default `false`, and requires `spawns` naming another template. |
| `delegation` | `explicit` (default) or `proactive`. |
| `show_model`, `cost_tier`, `max_turns`, `on_mint`, `[env]` | Optional extras. |

`config.toml` holds no templates: an `[agent.<name>]` block there is refused at
load with a message to move it into `agents/`.

## The working set

`config.toml` picks the root template and the roster:

```toml
[root]
agent = "hub"

[roster]
working_set = ["hub", "implement-grok", "review-claude", "explore-luna"]

[orchestration]
transient_resumes = 1 # 0 disables automatic continuation after transient failure; maximum 2
subagents = true

```

`[root] agent` is the template a new session binds. With no `[root]`, a new
session has no root template until you save one from the Agents tab.
`plak run` and `plak agent` take the tuple from their own flags when those
flags are set.

`[roster] working_set` is the list of templates **the root** may spawn — at
most five, no duplicates. Every child spawns from **its own template's
`spawns` list**, minus its own name, so a template cannot spawn itself and a
template with no `spawns` is a leaf. Nesting is declared up front: a `hub`
child with `spawns = ["implement-grok", "review-claude"]` can spawn exactly
those two, whether or not they are in the working set. A name in a `spawns`
list must be a template file with a `description`; a typo is a load error.
Cycles are allowed, and `[orchestration] max_depth` ends them. The root
template's own `spawns` only matters where that template is spawned as a
child.

`[orchestration]` holds the tree budget: `subagents`, `max_depth`,
`max_children`, `max_nodes`, `max_attempts`, `require_review_evidence`,
`turn.auto_continue`. Interactive roots may end a turn while children run;
the host wakes them with results or questions when auto-continue is enabled.
Use `wait_agent` when a result is needed before continuing the current turn.
Children never need to sit in a wait either: a child whose turn ends without
`submit_result` while its own background processes or children are still
running is *parked*, not nudged. It keeps its attempt, shows `waiting_on`
(for example `{processes: ["p3"], agents: ["c2"]}`) in its parent's
`wait_agent` and `list_agents` rows, and is woken with a new turn when one of
those processes exits, when its children report or write, or when its parent
sends mail. Mail or a result that arrives just as a child's turn ends also
wakes it instead of costing a submit reminder. Every wake turn counts against
the template's `max_turns`; interrupt or close a parked child as any other.
`plak run` keeps waiting for a parked child and names it on its waiting line
(`c2 (parked on p1)`), `plak agent` tree rows carry `waiting_on` (`[]` when it
waits on nothing named yet), and the Agents tab shows `waiting on p1` or
`parked`. Mail to a child whose attempt already submitted, sent as its last
turn ends, is refused `recipient_attempt_terminal` rather than left unread. Mail
to a child you just interrupted, while its turn is still stopping, waits for the
stop and then starts the child's resume turn; several such messages start it
once, and the others reach the resumed turn as follow-ups. A `followup` sent
while the child's previous turn is still being released waits for the release
and then starts the new attempt. Cancellation or slot-wait expiry before
runtime submission withdraws the new attempt when its session has recorded
nothing since opening; the child keeps the attempt it had. If withdrawal is
refused, the attempt is retained and an unbound opening needs recovery.
Cancelling the send while its submission is in flight also leaves the new
attempt held for recovery, not running without a Start. The send's wait budget
never cuts a submission in flight: a Start that is admitted late is delivered
late, and the child keeps running.
After a
restart, a parked child wakes once when the tree resumes; interrupting it before
that wake runs keeps it interrupted. JSONL roots and roots with
auto-continue disabled must use `wait_agent` to learn child results before
ending a turn. An idle wake
failure pauses auto-continue, emits an error note and leaves the host attached;
child mail remains unread until an admitted turn consumes it. A busy or
unavailable session store, or a busy product store, does not pause it: the wake is retried, then
re-armed every 60 s (see [tui](tui.md)). Each
bound is a closed range up to its built-in ceiling; anything outside it is a
load error. Full key list and defaults in
[configuration](configuration.md). `max_nodes` counts live nodes, so closing a
finished child frees its slot, and `max_attempts` counts attempts within one
root prompt; neither is a lifetime total.

A parent that calls `wait_agent` with no `agent_ids` gets each child's result
once: every call returns when the next child settles, and after the last one it
answers `no_pending_children`. Naming `agent_ids` re-reads a child's row at any
time when every named row has already been delivered. A mixed named wait parks
for children that have not reported instead of returning repeatedly for an
already-delivered child. Results consumed by a wake or user turn also count as
delivered. One wait return is bounded to 256 KiB: past that, the remaining
results and mail stay pending, the return lists them under `omitted`, and the
next wait returns them at once. `list_agents` rows carry 1 KiB previews of a long prompt or result.
Result notices include the full output's `output_bytes` and whether the excerpt
is `output_truncated`; name the child in `wait_agent` for its full output.
Model mail accepts up to 16 KiB, while `list_agents` inbox previews remain 2 KiB.
Pending mail returned by `wait_agent`, and mail delivered into an agent's turn,
retain the full body. Omitted message priority steers a running direct child
(`urgent`); other recipients default to `deferred`. Set `deferred` explicitly
to queue mail instead of steering.

`list_agents` and the Agents tab also show per-attempt compaction counts
(committed, failed, no-eligible) and the latest committed pass's measured input
before/after. These are derived from the retained child feed, not billed token
totals; `complete=false` / `retained only` means retention prevents a complete
reconstruction. Peak input is not shown. A failed fork's `context_exhausted` or
`turn_failed` message names its sealed partial candidate and gives a fresh
`spawn(inputs=["cand-<attempt>"])` handoff recipe. Use a fresh child with a
brief of completed and remaining work, not a follow-up into exhausted history.
Delivered settled children release their direct-child slots without losing
adoptable candidates; the running-agent limit applies across the tree.

## Tiers

A spawn asks for a **tier** (what the child may do) and optionally a
**workspace** (where it sits). The requested tier is clipped down to the
template's `tools` ceiling. Omit `workspace` or pass `inherit` to share your live
checkout. Pass `workspace=fork` to mint an isolated copy (a git worktree or jj
workspace when the child may write; otherwise a disposable snapshot).

| Tier | File writes | Process / shell |
| --- | --- | --- |
| `read_only` | No | Yes |
| `execute` | No | Yes |
| `mutate` | Yes | Yes |

`read_only` and `execute` children are not shown file-writing tools, in either
Direct calls or nested cells. They can still use their pack's plan tool
(Claude's pack has none) and granted agent tools. Shell commands remain unconstrained: these tiers are not a sandbox.

A session recorded under a different occupancy policy or native exec
description cannot be exactly reopened by this build, so `/reload` does not
carry it. It moves by export/carry into new bindings.

Default: children share your checkout. Give them file-disjoint work, or run
writers in sequence. Fork when more than one child must write at the same time,
or when the work should stay off this checkout.

Isolated copies live at `<project>/.plak/worktrees/<agent_id>` (git, branch
`plak/<agent_id>`) or `<project>/.plak/workspaces/<agent_id>` (jj). Closing a
child removes its copy once nothing needs it: at once if it produced nothing,
or after its candidate is adopted. A copy whose candidate was never adopted
stays until you adopt or `discard_workspace` it.

## What a spawned child sees

- The task text and output schema its parent passed to `spawn_agent`.
- Its **own** template's binding. A child does not inherit its parent's account,
  model, effort or Fast mode — it gets what its template says, even when the
  template names the same account and model as the parent. A Fast parent may
  spawn Standard children and the reverse.
- Its **own** template's authority. A fresh child's spawn menu is its own
  template's `spawns` list, and it may adopt only when its template sets
  `may_adopt`, so nothing is inherited down the tree.
- A checkout shaped by `workspace` (inherit vs fork) and a capability
  ceiling shaped by its tier, as above.

Tools a model can see, grant-gated: `spawn_agent`, `list_agents`,
`send_agent_message`, `wait_agent`, `interrupt_agent`, `close_agent`,
`recover_agent`, `submit_result`, `adopt_candidate`, `discard_workspace`.
`spawn_agent`, `wait_agent`, `interrupt_agent`, `close_agent` and
`recover_agent` appear only with a non-empty spawn menu
and remaining depth; `adopt_candidate` only with `may_adopt`; `submit_result`
on every non-root node. On Grok (SuperGrok OAuth and the xAI API) the await and
the interrupt are Grok's own `get_command_or_subagent_output` and
`kill_command_or_subagent`, which take child ids as well as process ids;
`wait_agent` and `interrupt_agent` are not shown there. Killing a child
interrupts its current turn; it does not close it. On OpenRouter (the pi pack)
the await and the interrupt are pi's `wait` and `stop`, which also take child
ids; a bare `wait` watches every child that has not reported and your own
running processes, and `stop` on a child interrupts it without closing it.

When a child finishes, its parent receives a bounded result notification. It
wakes an idle interactive root only through auto-continue, and a parked parent
as described above; otherwise the next admitted parent turn receives pending
results in its context. Results that do not fit remain available in the tree
inbox. Retries reuse the original batch.

Child startup has a 120-second caller deadline. If creation or acknowledgement
remains uncertain, the tree reports recovery required and retains ownership;
a timeout does not mean the child was cancelled or that spawning it again is
safe. A repeated identical `request_key` identifies the original admitted spawn,
including its candidate inputs, even after its workspace changes.

A child that was mid-turn when your `plak` crashed, or whose own turn ended
recovery required, is recovered rather than stranded: its session is reopened,
the unfinished turn is recorded once as abandoned (never re-sent or re-run), and
the child ends `failed` with kind `turn_abandoned`. The parent can re-task it
with a follow-up or close it, and a crash does not spend the tree's attempt
budget. A child that still reads `recovery_required` holds none of its parent's
child slots, but it does count toward the tree's node budget until it is
closed.

Such a child is settled by reopening its session once more, without replaying
anything it ran:

- automatically, in the background, when the tree is reopened or resumed;
- when you run `/recover` (on a paused tree, `/recover` says to resume it
  first: `r` on `/tree`, or `--resume-tree`; running it while a pass is
  already going makes that pass run once more);
- by its parent calling `recover_agent { agent_id }`, which `list_agents` and
  `close_agent` name as the remedy on that row.

Recovery runs in the background and never holds up the session. Each pass
that did something posts one note naming the children it recovered, the ones
it could not (with the reason), and any it did not reach in time. `recover_agent`
retries a refusal; `/recover` only selects children with unresolved session
outcomes. A child whose interrupted turn may have run a
tool gets a warning that its workspace may have changed. The tool's
unobserved conversation result is not kept or replayed; workspace changes
remain in the child's workspace or sealed candidate for review, not erased.
A recovered child ends `failed` with kind `turn_abandoned`, or
`recovered_without_turn` when the session was quiet and nothing was left
running, or `spawn_refused` when its session was never created (the create
was refused before it wrote anything, so there was nothing to reopen). A durably submitted result is preserved as `completed` when recovery
finds a quiet session and only retirement was uncertain; a recorded failed
terminal or an abandoned turn still fails. The parent can then re-task it with a follow-up or
close it, except a `spawn_refused` child: it never had a session, so a
follow-up is refused (`followup_closed`) and the parent closes it and spawns a
new agent. A delivered terminal with only local finalization bookkeeping also
recovers this way, without rerunning tools or producing a second result. A
fenced child's automatic waiter recycle settles a successful quiet reopen as
an ordinary failure, not another recovery request.
A refusal to reopen or initially publish settles nothing and says why:
`recovery_lease_held` means
another process still holds the child's session (the automatic pass does not
wait; `recover_agent` waits up to 35 seconds), and the others are
`recovery_session_unavailable`, `recovery_storage_unavailable`,
`recovery_effect_evidence_refused` and `recovery_owner_live` (the child is
still being settled by this process). A close or interrupt can time out while
release continues in the background. Its ownership slot clears when release
returns, even on failure, and the tree automatically tries recovery; no relaunch
is needed just because release was slow. When no retry can help (a session an
incompatible build created, or `recovery_effect_evidence_refused`),
`list_agents` names `close_agent` instead, and closing the child records that
refusal as its failure and frees its slot.

A child whose seal or adopt was interrupted has its session settled the same
way, but whether the VCS step landed is not something Plak can tell, so the
child stays `recovery_required`: `recover_agent` reports the blocker in
`workspace_recovery` (or refuses `recovery_vcs_ambiguous` when the session was
already settled), `list_agents` names `close_agent` as the remedy, and closing
it after you have inspected the copy ends it and frees its slot.

A failed child turn reports its canonical error class and, when available,
operation phase, HTTP status and provider code before the sanitized failure
message and request-attempt summary. These details help distinguish an unreachable
provider from a provider refusal; they do not authorize retrying completed work.

Recovery reports the state after sealing finishes. A successfully sealed
child is reported as failed and idle, not as a VCS ambiguity. A seal that
actually fails reports `mint_unavailable` with the seal diagnostic and leaves
`seal_failed` on the attempt; inspect its retained copy before closing it.
If the tree store refuses to publish the seal completion, recovery reports
`recovery_storage_unavailable` instead of claiming it succeeded. For a child
already failed and idle in this process, use `recover_agent` to retry the
publication: `/recover` does not select that child. This does not rerun its
seal. If a seal failed in the same drain, the storage refusal includes both
diagnostics. A recorded `seal_failed` diagnostic survives restart and still
names the retained copy's inspect/close remedy; it is not a guess about whether
a seal landed. Only a
genuinely unresolved VCS step is reported as `recovery_vcs_ambiguous`.

## Candidates and adoption

This is the part worth understanding before you turn spawning on.

A child spawned with `workspace=fork` **seals** its result as a *candidate*: a
pinned snapshot of that minted copy's edits, identified by a `candidate_id`
that shows up on the child's row in `list_agents` and in the Agents list.
Inherit children write (if `mutate`) or read on the shared checkout and seal
nothing.

A candidate is not in your checkout. Nothing the child wrote reaches you until
an agent with `may_adopt` calls `adopt_candidate`, which three-way merges the
candidate against your live tree and applies it **as working-tree changes with
no commit**. Your HEAD and your index are untouched; you review a normal
uncommitted diff, exactly as if you had made the edits.

Two things can go wrong, and both are ordinary failed results rather than
corruption:

- **`merge_conflict`** — the merge did not apply. The child's copy is kept, so
  nothing is lost.
- **`target_changed`** — your tree moved since the adopting agent last looked.
  Re-read and adopt again.

On jj, adoption updates your working-copy tree while preserving the child's
files. On git the child's copy is left untouched and only your working tree is
patched.

Workspace rows carry a status you will see in the Agents inspector: `active`, `adopting`,
`adopted`, `discarding`, `clean_removed`, `discarded`. (`blocked` is an *attempt* status, not
a workspace one.)

Finished children are revivable until their conversation is retired. By default,
sealed dormant conversations are retained for 7 days and up to 128 conversations
(oldest first). Retired children accept no follow-up. Retention never discards
unadopted work: `list_agents` names every retained candidate on its retired row,
and those candidates still work with adoption and spawn inputs.

## Talking to a running child

After its result is delivered, a finished child is dormant: it no longer uses
the active-child or tree cap and remains revivable with follow-up mail.
Plak automatically reclaims covered dormant git checkouts after ten minutes
or when fork checkouts exceed the default 4 GiB soft budget. It seals work and
keeps candidate refs first, and restores the same checkout on follow-up.
Unknown ignored files, secrets, independent staged versions, shared/live
copies and uncertain coverage (including JJ) retain the checkout instead.
Home-scoped `orchestration.workspace_idle_seconds` and
`orchestration.workspace_budget_mib` configure these triggers.

- **Follow-up** — `send_agent_message` with `purpose = "followup"` re-tasks a
  finished child on a new attempt, optionally with a new output schema. This is
  how a parent iterates on a child's result instead of spawning a fresh one.
- **Interrupt** — `interrupt_agent` cancels that child's current turn only. Any
  open `wait_agent` returns `interrupted`, the node stays listable, and it can
  still take follow-up mail. There is no separate resume tool.
- **Your own Esc** — interrupting the root's turn also interrupts every
  child that is running, so nothing keeps spending behind your back. They stay
  listable and resume on mail; nothing is closed.
- **Close** — `close_agent` ends the child and its descendants permanently.
  Closed rows and any already submitted output stay inspectable in `list_agents`.
  If cleanup needs recovery, the node stays `recovery_required` even if its
  pending seal succeeds or fails. Recovery preserves the close and does not
  send you a new result from that child.

From your side of the screen those are `i`, `c` and `s` (stop every non-root
agent) in the Agents list, plus `/stop-all` in the composer, all behind the
same confirmation pop-up: focus starts on the action, so `Enter` sends it and
`←` then `Enter` declines. See [the TUI guide](tui.md#agents).

## git and jj

Session VCS is exclusive and jj wins: if the project has a `.jj` directory —
including a colocated, git-backed jj repo — Plak is in jj mode; otherwise `.git`
is git mode; with neither, minting a copy fails with `mint_unavailable`.

Plak uses JJ in process through `jj-lib`; workspace operations do not require
the `jj` executable. The optional Git backend uses the Git CLI. Default builds
include both; [custom builds](getting-started.md#build) can select either.
A repository whose backend was omitted is refused, with no fallback.

- Harmless operation changes can be reconciled automatically. If another
  workspace rewrites the child's base and safe recovery cannot be established,
  Plak returns `workspace_recovery_required` and preserves the child's modified
  and untracked files. It does not reset the copy to make it usable.
- Do not run `jj git init` inside a Plak-minted copy: that creates a separate
  repository instead of a workspace of the launch repository.
- Native operations are tested alongside external JJ CLIs in the 0.39-0.45 range. Human JJ
  commands remain usable; immutable candidate pins do not create JJ bookmarks
  or tags that change normal rewrite rules.

## `plak agent`: driving Plak as JSONL

Each input frame may contain at most 1 MiB, counting its newline. An oversized
frame disconnects the host after bounded cleanup; it is not drained for
recovery. This includes extra fields and tool arguments. Decoded prompt text
has a separate 64 KiB limit, and command ids have a 256-byte UTF-8 limit.
Malformed JSON and ordinary command errors remain recoverable diagnostics.


`plak agent` attaches the same host loop the TUI drives, over newline-delimited
JSON (`v=1`) on stdin and stdout. Every object carries `v` and `kind`; a
command may also carry an `id` that the host echoes on its replies. Stdout is
machine-only; diagnostics go to stderr. Flags match `plak run`.

```
$ plak agent --account grok --model grok-4.6 --effort low
< {"v":1,"kind":"prompt","id":"1","text":"list the crates in this workspace"}
> {"v":1,"kind":"hello",...}
> {"v":1,"kind":"ready",...}
> {"v":1,"kind":"busy",...}
> {"v":1,"kind":"tool","state":"running",...}
> {"v":1,"kind":"tool","state":"done","duration_ms":812,...}
> {"v":1,"kind":"assistant","text":"..."}
> {"v":1,"kind":"assistant_done",...}
> {"v":1,"kind":"turn",...}
< {"v":1,"kind":"tree","id":"2"}
> {"v":1,"kind":"tree",...}
< {"v":1,"kind":"quit","id":"3"}
> {"v":1,"kind":"bye",...}
```

Commands you can send: `prompt`, `interrupt`, `background`, `compact`,
`recover`, `resume_queue`, `resume_tree`, `tree`, `interrupt_node`,
`close_node`, `stop_all`, `clear`, `invoke_tool`, `fork`, `quit`.

Events you will commonly receive: `hello`, `ready`, `status`, `busy`,
`assistant`, `assistant_done`, `tool`, `error`, `turn`, `tree`, `queue`, `bye`.
The complete event table is in [the product spec](../spec/02-plak-product.md)
§4.3.

Every tool the host announces is terminalised: a `running` line, then exactly
one `done` / `failed` / `cancelled` / `rejected` line for the same `call_id`,
carrying `duration_ms` when the host could stamp the call's wall time.

`invoke_tool` is an idle-only host injection of the same grant-compiled
executor the model uses, with an optional `agent_id` to target a node. During a
turn, models still call the topology tools themselves. `fork` maps to
the TUI's `/fork`.

To attach to an existing session, `plak agent --session <uuid>`; its recorded
binding wins over any flags. A session that was left as a paused tree needs
`--resume-tree` to attach, the same as `plak run`.

## Trying it

`configs/` holds complete, offline-tested example homes: `reference/` documents
every key inline, `providers/` pins the same four roles to one account each,
and `scenarios/` has single-agent, mixed-account tree, direct and PTC variants. Copy a scenario's
`config.toml` and its `agents/*.toml` into the directory `plak config show`
reports on its first line, log in to the accounts it names, and start a session.
Nothing there is loaded automatically.

### Your first tree

Use the annotated reference home, in a throwaway `PLAK_HOME` so nothing you
already have is disturbed. Give it an absolute, non-symlinked path: on macOS
`/tmp` is a symlink, and the SQLite stores refuse a symlinked parent.

```sh
export PLAK_HOME="$HOME/plak-tree-demo"
mkdir -p "$PLAK_HOME" && cp -r configs/reference/. "$PLAK_HOME/"
plak config show | head -1      # confirms which home you are on
```

That home is already a tree: `[root] agent = "root-agent"` names
`agents/root-agent.toml`, and `[roster] working_set = ["worker", "reviewer",
"scout"]` names the three templates the root may hand work to; the three
are leaves. Edit `account` and `model` in each of the four
files to accounts you are actually logged in to (`plak status`), then:

```sh
cd ~/your-project     # a git or jj checkout; the tree needs one
plak
```

Ask the root to delegate — the wording matters only in that it must ask for
work to be *handed off*:

> Have the worker, with workspace=fork, add a `NOTES.md` with three
> bullet points about this repository, then have the reviewer read it, then
> adopt the result.

Press `Ctrl+T` while it runs. The Agents list shows the root, then a
`worker` child appearing below it; its inspector names its tier and
workspace. When the forked child seals, its row gains a `candidate_id` — that
is the candidate row. Nothing has reached your checkout yet.

When the root calls `adopt_candidate`, the child's edits land in your working
tree as **uncommitted changes**: `git status` shows `NOTES.md` as a new file,
`git log` has not moved, and any edit you had in flight is still there. Review
it exactly as you would review your own diff.

If nothing appears, the usual causes are an empty working set, a working-set
template with no `description`, or `[orchestration] subagents` turned off.

Transient child failures can continue automatically on the same assignment
(default one continuation; `orchestration.transient_resumes = 0` disables it).
A network wait is shown as `network`; interrupt or close cancels it. Pending
child continuation survives reopen/reload. When continuation is exhausted, a
failed fork still exposes its ordinary partial candidate for explicit adoption,
or you can follow up on the same child copy.
