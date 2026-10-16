# Orchestration

Plak composes many single-agent Plakous runtimes into one durable tree. This
section is the normative behavior of that tree: what a node is, what it may
touch, the ten tools it calls, how work is sealed and adopted, and what
survives a crash.

Plakous itself has no topology. Everything here is product-owned: node
identity and ancestry, mailboxes, workspaces, attempts, candidates, capacity,
and the durable projection. A spawn is a **clean create** — a distinct
public-facade Plakous agent with its own driver, canonical history, checkpoint,
locator, provider binding, and `TurnResult` authority. Spawn never forks or
clones a conversation.

The shape below is user-named templates, spawn tiers, sealed candidates,
adoption, and pushed completion.

**MUST**, **MUST NOT**, **SHOULD**, and **MAY** mark boundaries where omission
would change safety or a claimed contract. Everything else is ordinary
description of shipped behavior.

## 1. Templates and the working set

A **template** is a user-authored named bind: one flat TOML object per file.

| Location | Scope |
| --- | --- |
| `~/.plak/agents/<name>.toml` | user |
| `<project>/.plak/agents/<name>.toml` | project; wins on a name collision |

The file stem is the name. It matches `[a-z][a-z0-9-]{0,31}` and is at most 32
bytes. Seven names are reserved because they are `config.toml` section names:
`root`, `roster`, `orchestration`, `tui`, `compaction`, `telemetry`, `agent`.
Templates MUST NOT be authored through `plak config set`, which refuses
`agent.*`.

| Field | Meaning |
| --- | --- |
| `account`, `model`, `effort` | the bind tuple; never shown on the card unless `show_model` |
| `fast_mode` | Codex-only fast lane (provider-native `priority` processing tier) |
| `composition` | catalog selection for this node (`direct`, `plakous-default`, `native-ptc`, `js-ptc`, `py-ptc`) |
| `protocol` | OpenRouter-only wire selection |
| `description` | **required** for a template named in the working set or in any `spawns` list: the card text the spawner routes on |
| `access` | edit ceiling, default `full`; `read-execute` omits file-edit tools; a higher ask is clipped, not refused |
| `tools` | optional tools enabled by name (`web_search`, `web_fetch`, `x_search`, `fetch_url`, `image_gen`, `mcp:<id>`, `mcp:<id>/<tool>`); default `[]`; subject to the child subset rule (§3.1) |
| `spawns` | array of template names this node may spawn, at most 5, no duplicates; omitted or `[]` is a leaf that compiles no `spawn_agent` |
| `may_adopt` | this node compiles `adopt_candidate`; requires `spawns` naming another template |
| `delegation` | `explicit` (default) or `proactive`: which single policy sentence renders |
| `max_turns?` | per-attempt turn cap |
| `on_mint?` with `[env]` | hook runs once after a copy is minted; `[env]` applies only to this hook, not tool calls |
| `cost_tier?` | a **static label the user writes**, never a measured cost |
| `show_model?` | put the model id on the card, default false |

The main `config.toml` holds only `[root] agent` and
`[roster] working_set = [...]`. The working set holds 0 to
`MAX_WORKING_SET` = 5 names with no duplicates; 1–5 is the intended scale.
`[agent.*]` sections, `[roster] <class> =` keys,
`[orchestration] child_may_spawn`, and a template's `may_spawn` are **named
load errors** that say where the field moved. Every name a `spawns` list
carries MUST be a template file; an unknown name refuses the load, naming the
template and the missing name.

**An empty working set is an ordinary single-agent root.** `subagents = true`
still omits `spawn_agent` until the working set names a template, and such a
root receives no topology paragraph and no delegation sentence.

The root is the **session bind**, not a template in the spawn menu. **The
root's menu is the working set.** Every other node's menu is its own
template's `spawns` list **minus itself**; the root template's own `spawns`
applies only where that template is spawned as a child. A spawn naming a
template that is not on the caller's own menu is refused
`template_not_spawnable` (`template_unknown` when the epoch does not carry it
at all). Cycles in `spawns` lists are allowed; the depth bounds end them. The
host MUST NOT classify, route, or learn: the spawning model is the router and
sees one card per template on its menu.

### 1.1 The template card

The `spawn_agent` description carries exactly one delegation sentence, a blank
line, the header, and one bullet per spawnable template:

```text
Spawn by template name only. Do not name accounts, models, or effort. Templates you may spawn:
- worker: Implements a bounded change in its own fork (tier ≤ mutate · spawns none · adopts no · cost tier 3)
```

The capability line is host-computed and is exactly
`tier ≤ <max_tier> · spawns <menu> · adopts <yes|no>`, where `<menu>` is the
card's own `spawns` list minus itself, comma-separated, or `none`; plus
` · cost tier <n>` when `cost_tier` is set, ` · model <tuple>` only when
`show_model` is true, and ` · tools <names>` when the template's `tools` list is
non-empty: the selected tool names in the template's order, comma-separated,
exactly as written (`tools web_search, x_search, mcp:github`), so a spawner can
route by capability. An empty list adds nothing. There is **no depth-remaining term** and nothing on the
card is telemetry: observed duration is deliberately absent. When no template
is spawnable the block reads
`- (none; spawn_agent fails closed until [roster] working_set names another template)`.

The two delegation sentences are fixed text:

- `explicit` — "Spawn sub-agents only when the user asks for delegation,
  sub-agents, or parallel work. Requests for depth, thoroughness, or research
  are not that permission. When asked, actually spawn; saying you will is not
  delegation. Otherwise do the work yourself."
- `proactive` — "Spawn sub-agents for independent subtasks when that keeps your
  own context clean or lets work run in parallel with yours. Do small or
  sequential work yourself. Choose the template from the cards below."

A spawner renders the sentence chosen by **its own** template's `delegation`,
not the root's. The root's policy is frozen on the tree snapshot as
`root_delegation` and is the fallback.

### 1.2 The tree epoch

Templates and the roster are frozen at tree genesis into an
`OrchestrationSnapshot`. The epoch freezes the **closure**: the working set
plus every template reachable from it through `spawns` lists, transitively.
The root template is not part of the closure unless that walk reaches it, and
its own `spawns` creates no cards. The snapshot carries the working set (the
root's menu) and one card per closure template; the root template has a card
only when the closure reaches it. Each node's menu, card text, and capability
line come from its own card. The tree-epoch digest covers the working set,
every closure card, and the resolved template file **bodies** of the closure,
and additionally the root template's body, not just the main config, so a
mid-epoch template edit is ignored until a successor epoch installs through
the apply gate. Each card freezes its template's `tools` list, and the
snapshot freezes the canonical rendering of every `[tools.<name>]` and
`[mcp_servers.<id>]` table those lists and the bound root template's list
reach (`tool_sections`), inside the digest; a child binds the frozen tables,
never the live file. A table no frozen list reaches is not epoch state, so
editing it changes no digest (chapter 07 §6).
All `[orchestration]` controls come from the operator home, including in a
trusted project. Project overrides are ignored with a warning and project
writes to these controls are refused (chapter 07).

The epoch is the only spawn authority a session has: `spawn_agent`, its card
text, and the binder all read the one roster its tree's digest covers, and
nothing outside the digest widens it. A successor epoch installs on a
quiescent tree (a roster apply, or a model rebind that stays on the tree). A
profile's **Fork now** is different: the new session opens a **tree of its
own**, keyed by its session id, whose genesis freezes the profile's roster.
The source keeps its tree, epoch, and cards, so its checkpoint reopens with
exactly the tools it sealed and the strict reopen check (spec 01 §2.3) holds,
while the new session is briefed as the profile. Neither session can spawn
from the other's cards. Children already running under the source stay in the
source's tree; Fork now is refused while any are live.

All `[orchestration]` bounds are carried in the digest, including the fixed
per-cell agent budget: a ceiling that decides whether a spawn is admitted is
bind authority like the other four.

The snapshot also freezes the product's **account roster** as two membership
sets: which accounts a template card may pin at all, and which speak no native
protocol (so a `native-ptc` card on them is refused). The tree itself names no
account: it only asks whether the card's account is in the frozen set, so every
spawn path — a product binder, or a binder-less JSONL `invoke_tool` — refuses
the same card the same way. The roster is carried in the epoch digest, so a
change to it needs a successor epoch. The API accounts `openai`, `anthropic`
and `xai-api` belong to both sets: they are admitted accounts and refuse
native PTC.

## 2. Tiers and workspaces

`tier` is required on every `spawn_agent` call and is **clipped** to the
template's `tools` ceiling. Asking above the ceiling is not an error: it lands at
the ceiling and the receipt reports the effective tier. Tiers order
`read_only < execute < mutate`, and the clip is a minimum.

Every call also requires `template`, `prompt`, and `output_schema_json`. The
model-visible tool description names all four required fields and shows the
portable result-schema shape. A projected call that omits fields is refused
before execution and names every missing required field; required values are
never silently defaulted.

Checkout is a spawn choice, not a side effect of `tier`. Omit `workspace` or
pass `inherit` to share the caller's live checkout. Pass `workspace=fork` to
mint an isolated copy: a git/jj worktree when the effective tier is `mutate`,
otherwise a detached snapshot. A sealed `inputs` candidate always mints.

| `tier` | File writes | Process / shell tools |
| --- | --- | --- |
| `read_only` | not presented | allowed |
| `execute` | not presented | allowed |
| `mutate` | allowed | allowed |

Default inherit means children share the parent's cwd. The parent assigns
file-disjoint work or runs writers in sequence. `workspace=fork` is for more
than one child writing at once, or work that should stay off this checkout.

The enforcement of "this node may change files" on `execute` is the
**file-write fence**, not isolation: the shell is unconstrained. A
`workspace=fork` snapshot is thrown away unless something seals a candidate
and a parent adopts it.

Workspace process-shim requirements follow the actual workspace identity, not
the tool tier. An inherited ordinary checkout needs no copy shim; an inherited
minted copy retains its required shim, including when the child is read-only.

At construction, `read_only` and `execute` omit workspace-writing coding
presentations (`apply_patch`, `search_replace`, `write`, `Edit`, `Write`) from
Direct and nested catalogs. The compiled set also earns the instructions; unavailable
file tools are not advertised. Process tools remain. Plan capabilities
`update_plan` and `merge_plan` remain allowed by canonical name; Grok's
`todo_write` presentation lowers to `merge_plan`. Both retain their
`ToolEffect::Mutating` classification unchanged. The execution policy still
fences workspace-writing capabilities if invoked outside the visible catalog.
The filtered Direct projection is compiled for the exact provider
configuration the session is constructed with, which includes the Direct
topology extras, so it passes the same candidate admission as the unfiltered
catalog (chapter 01 §8.2).

The ten topology names are **never denied by tier**. `adopt_candidate` is what
removes the read-only-supervisor dead end: file tools cannot write a
`read_only` supervisor's copy, `adopt_candidate` can.

Outer Code Mode markers (`exec`/`wait`, `exec_js`/`wait_js`, `exec_py`/`wait_py`) are
read-only effects, so a tier does not strip the cell on a `read_only` node;
nested file-writing names are omitted while plan and granted topology tools
remain.

**A tier is not a sandbox.** A process on any node that may start a shell
runs with host authority, including inherit `read_only`.

A parent's own tier never bounds its children. Delegation is governed by the
parent template's `spawns` list and the child template's `tools` ceiling. A `read_only` hub cannot patch
the parent tree but can spawn `mutate` children and adopt candidates from them
when its template sets `may_adopt`.

### 2.1 Bind

A child's account, model, effort, processing tier (`fast_mode`),
composition, and protocol come **only** from its own template, resolved against
the frozen snapshot. Nothing is taken from the parent: a template that names the
parent's account and model still binds its own effort (an omitted effort means
the model's catalog default) and its own tier, and a Standard parent may spawn a
Fast child or the reverse. A card that names no account or no model is refused
`schema_mismatch` before any node is spent. Not-Ready, a tuple absent from the
authenticated catalog, a tier the exact catalog row does not advertise, or a
resolve failure is fail-closed (`model_not_admitted` or binder recovery),
never a fallback to the caller. A non-empty `spawns` decides only whether a
node may be a parent; it is independent of the node's tier.

A child bound to SuperGrok or Codex OAuth outlives its admitted catalog window
(6 h) the way a root does (chapter 01 §7.4): its runtime carries the host's
reresolver, so a child started, followed up or woken by mail after the window,
or one whose running attempt crosses it, continues between model rounds on a
fresh exact resolution of its own tuple, charged to its own turn. A refused
re-resolution (a changed login, account, catalog row or model semantics, or an
unusable credential) fails that attempt with the remedy before any inference;
it never falls back to the caller's model or account.

The caller's spawn menu is checked before any binder admission. A template
that is not on that menu is `template_not_spawnable`, or `template_unknown`
when the epoch does not carry it, and the binder does not run: no account
readiness check, catalog read, or mint. A pinned OAuth account that is on the
menu is admitted (refresh, then Ready) before the tree admits a node, so a
not-Ready account refuses `model_not_admitted` without spending a node,
attempt, or copy. A binder refusal after admission — one that proves no
session was created — fails the node as an ordinary `failed` spawn with attempt
failure kind `spawn_refused`, unmints any copy, and records the refusal as the
operation's terminal answer. A create refused for store capacity is such a
proof. Any other refused child-session create is one when the refusal is
neither storage unavailability nor a held session lease and, after the
refusal, the exact accepted-create lookup for the spawn's deterministic create,
issued on the same session-store instance once the create has returned, finds
no session: construction refused before it wrote anything (for example a tool
projection the candidate admission refuses). Only uncertain creation or startup
is `recovery_required`: a create that left a session behind, a lookup that
failed, or a create refused for storage unavailability or a held lease.

An exact replay of a recorded spawn operation resolves its durable receipt
before checking current account readiness. A successful receipt returns the
original child; a terminal refusal returns the original refusal. Neither
creates another child or repeats admission. Caller or request mismatches
conflict, and a prepared operation without a terminal receipt requires recovery.

Delegation never widens a node's grant, workspace, occupancy, approval, or
effect authority, and never widens the child's credential relative to the bound
account. A Grok child does not receive the Codex token.

The caller waits at most 120 seconds for child creation, assignment-command
acknowledgement, and live registration together. The hub reserves the child
before creation and persists active ownership so close cannot seal its workspace
while startup is unresolved. Diagnostics distinguish slot admission, creation, command start,
live registration, and release.

A deadline or abandoned caller requests cancellation; it never aborts the
independent startup owner or treats acknowledgement loss as a rejected command.
The owner persists the created locator before Start and the acknowledged turn
before live registration. A late acknowledgement joins cancellation and release
instead of dispatching a replacement. Cancellation and shutdown also see startup
reservations: a startup's cancelled release is the same cancel-and-release a
retirement runs, so cancel and close join it for the retirement release deadline
(the cancel settle plus `HOST_SESSION_RELEASE_TIMEOUT`, section 16), and
shutdown within its own bound; on expiry they record a joiner fault and report
recovery while retaining unresolved ownership.
A successful release publishes its locator before freeing the reservation;
release failure keeps it fenced. Unresolved startup cannot trigger partial-work
sealing, followup rollback, or automatic redispatch on replay/reopen. A binder
that already durably registered the turn reports that fact so its caller cannot
restore a child that completed before the spawn response returned.

A fresh accepted Start, including a submit-harness restart, retains its
admission-created exact-turn control and result observer, even if the turn
terminalizes before live registration. A Duplicate assignment or submit-harness
restart Start rejoins
only its exact live turn; if no live control remains, startup releases the
attachment and requires recorded-result reconciliation, never a replacement
dispatch. A submit-harness restart returns a typed reconciliation-required
error when its Duplicate turn has retired; the existing live-handle string
boundary carries its exact diagnostic without a new public API. The caller
marks the child `recovery_required`, preserves the prepared result context,
and retires the attachment. `recover_agent`/reopen settles that recorded
command from the child's feed (§14.1); the monitor does not overwrite this state with ordinary completion.
Completed topology-operation receipts still
replay without starting or rejoining a turn.

A newly created child whose Start is definitely refused for storage pressure
may be archived and deleted only after the startup owner atomically claims
cleanup before its first release/archive/delete await. That claim excludes a
later cancellation request. If cancellation wins the claim first, the fresh
genesis is retained for explicit later GC; a pre-await cancellation check alone
is not cleanup authority.

## 3. Grants: which tools a node sees

Topology tools are **grant-compiled**. A name omitted from the catalog is not
visible to the model at all — it is never "shown then denied".

| Tool | Root (non-empty working set) | Leaf child (empty menu) | Hub child (non-empty menu, depth left) |
| --- | --- | --- | --- |
| `spawn_agent` | yes | no | yes |
| `wait_agent` | yes | no | yes |
| `interrupt_agent` | yes | no | yes |
| `close_agent` | yes | no | yes |
| `recover_agent` | yes | no | yes |
| `list_agents` | yes | yes | yes |
| `send_agent_message` | yes | yes | yes |
| `submit_result` | **no** | yes | yes |
| `adopt_candidate` | yes | **no** | only with `may_adopt` |
| `discard_workspace` | yes | no | yes |

A child's grant carries `may_spawn` when its own template's menu (its `spawns`
list minus itself, as frozen in the epoch) is non-empty and depth remains; the
root's grant always carries it. The compile rules are exact: `spawn_agent`,
`wait_agent`, `interrupt_agent`, `close_agent` and `recover_agent` require
`may_spawn` **and** remaining depth, and `spawn_agent` is additionally withheld
while the node's menu is empty; `list_agents` and `send_agent_message` require
`may_spawn` or a non-empty message-peer set; `submit_result` requires the node
not be the root; `adopt_candidate` requires `may_adopt`, which a template may
set only alongside `spawns` naming another template, so a leaf child never
carries it;
`discard_workspace` requires `may_spawn`.

The `wait_agent` row is the grant of the await (§4.4) over children and mail:
a pack presentation of the await that replaces `wait_agent` compiles under the
same rule, and likewise for `interrupt_agent`. On Grok (SuperGrok OAuth and the
xAI API) the host compiles the tree's tools in the Grok vocabulary: the granted
`wait_agent` slot registers as `get_command_or_subagent_output`
(`plak.topology.get_command_or_subagent_output.v1`) and the `interrupt_agent`
slot as `kill_command_or_subagent`, so neither Plak name is presented or
model-callable there and no capability is presented twice. These two claim the
Grok pack's process-only entries of the same names (chapter 01 §9.2). On
OpenRouter, which wears the pi pack, the host compiles them in pi's vocabulary:
the `wait_agent` slot registers as `wait` (`plak.topology.wait.v1`) and the
`interrupt_agent` slot as `stop` (`plak.topology.stop.v1`), claiming pi's
process-only `wait` and `stop`. A Grok or OpenRouter session whose recorded
projection presents `wait_agent` or `interrupt_agent` does not reopen, on every
composition: the live host binds no executor for those names there
(`ExternalBindingUnavailable`). Creating a Grok Direct session is also refused
at harness admission when the projection would present `wait_agent` or
`interrupt_agent`. The session is not silently re-presented
under the claimed names. Leaves and nodes without agent tools keep
the pack's process-only entries. Codex and Claude keep the Plak names. The
JSONL host protocol keeps the Plak names on every vertical. The spawn and adopt descriptions, the topology
paragraph and the completion-channel sentences name the presented await;
`submit_result`'s live-work refusal names the presented remedies.

The **instructions follow the same grant**. A node earns the topology paragraph
only when `spawn_agent` is compiled for it, and the assignment sentence only
when `submit_result` is, so a leaf carries neither. Each child's Start prompt
states the tier and workspace it actually has.

A node's grant also includes its own template's optional `tools`, resolved
against its route when it binds (chapter 01); a tool the route cannot serve
refuses the bind rather than being silently omitted. A child binds its own
frozen card's list, never its parent's. The root binds its `[root] agent`
template's list only while it runs that template's exact account and model;
a root bound by explicit `--account` / `--model` has none. The root's
template (its account, model and list) and the `[tools.*]` / `[mcp_servers.*]`
tables it reaches are frozen into the session record when its runtime is
first created, with the MCP tools the list reached then; reopen, clear and
rebind read only that record, and its clear, fork and rebind successors
inherit it. Editing the template or a table changes new sessions only. A
wildcard `mcp:<id>` tool omitted at creation leaves one Settings notice in the
transcript.

### 3.1 A child gets at most its parent's tools

The rule is checked **statically when the configuration loads** (and again
before any save, so `plak config set roster.working_set` refuses too), on
every edge an epoch can spawn: the root to each `[roster] working_set` member,
and each template to each template its `spawns` list names (a template naming
itself is not an edge). The root's tools are its `[root] agent` template's
list; with no `[root] agent` the root has none.

- `fetch_url`, `image_gen` and MCP tools carry local authority (local network,
  local processes, user secrets): a child's set MUST be a subset of its
  parent's. `mcp:<id>` expands to the server's `tools` allowlist for the
  comparison; a server with no allowlist is "every tool it lists", which
  covers any `mcp:<id>/<tool>` of that server but is covered only by
  `mcp:<id>` itself.
- `web_search`, `web_fetch` and `x_search` (hosted retrieval) are **exempt**:
  they grant no local authority and spend on the child's own route, which the
  capacity and usage budgets already bound. This is what lets a parent whose
  route lacks `x_search` delegate it to a template on a route that has it.

A violation is a load error naming the edge and the extra tools, never a
spawn-time clip. The edit ceiling (`access`) keeps its clip-not-refuse rule.

## 4. The ten tools

Every tool registers under `plak.topology.v1` with registered id
`plak.topology.<name>.v1`, a structured input contract, portable model-content
output, serial concurrency, and nested eligibility. `list_agents` and
`wait_agent` are read-only effects; the other eight are mutating.

Every schema is a closed 2020-12 object with `additionalProperties: false`, and
every request struct denies unknown fields. A decode failure whose serde text
names an unknown field is `unknown_field`; otherwise it is `schema_mismatch`.

Every failure is one JSON object:

```json
{"code": "<snake_case code>", "message": "<≤512 bytes>"}
```

The message is truncated at 512 bytes with no continuation marker.

### 4.1 `spawn_agent`

Arguments — required `template`, `prompt`, `output_schema_json`, `tier`:

| Field | Type | Notes |
| --- | --- | --- |
| `template` | string | enumerated to the caller's spawnable set |
| `prompt` | string | non-empty |
| `output_schema_json` | string | non-empty; object types only, no `$ref` and no combinators; `additionalProperties: true` is refused |
| `tier` | `read_only \| execute \| mutate` | clipped to the template ceiling |
| `workspace` | `inherit \| fork`, optional | omit or `inherit` shares the caller's checkout; `fork` mints an isolated copy |
| `description` | string, optional | short label for the attempt; does not select bind |
| `inputs` | array of strings, optional | schema cap `MAX_SPAWN_INPUTS` = 8; **exactly one** is admitted at dispatch; requires `workspace=fork` |
| `request_key` | string, optional | caller-minted dedup key |

Receipt:

```json
{"agent_id": "…", "attempt_id": "…-a1", "status": "running",
 "tier": "<effective>", "workspace": {"id": "…", "mode": "inherit"}}
```

A `workspace=fork` receipt names `mode` `snapshot` (non-mutate) or `fork`
(mutate) and includes `base` (the mint commit).

The workspace field is `mode`, matching the persisted `WorkspaceMode`. `tier`
is the effective tier **after** the clip. **The receipt is not a result.**

`description`, `prompt`, and `output_schema_json` are each bounded at
`MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES` = 64 KiB (`bound_exceeded`).

Settled children release their direct-child slots once their results have been
delivered and no turn is active; candidates remain adoptable. The running
agent cap is tree-wide, excluding the root.

Typed refusals: `capacity` (carrying the running set), `spawn_budget`,
`depth_exhausted`, `template_not_spawnable`, `template_unknown`,
`input_not_sealed`, `request_key_conflict`, `mint_unavailable`,
`mint_hook_failed`, `model_not_admitted`, `schema_mismatch`.

**`inputs`.** One sealed candidate may be named. The child's copy is minted
over it — the candidate commit for a `mutate` worktree, the same snapshot for
a non-mutate fork — so a pipeline hands one agent's work to the next instead
of restarting from the parent's live state. More than one input is
`schema_mismatch` ("a copy is minted over one base: name a single candidate
input and adopt the rest into it"): a copy has one merge base and
`spawn_agent` is not authorized to merge. An id that is not a sealed candidate
is `input_not_sealed`, not `candidate_unknown` — an attempt id is a legal thing
to name and simply has not sealed. Inherit (the default) with any input is
`schema_mismatch`: a candidate needs a minted copy, so pass `workspace=fork`.

### 4.2 `list_agents`

Arguments, optional booleans: `include_self`, `include_completed`, `include_read`.

Receipt: `{ revision, agents: [...], inbox: [...] }`. Each `inbox[]` row
carries `message_id`, `from`, `purpose`, `priority`, `in_reply_to`, the first
`INBOX_BODY_BYTES` = 2048 bytes of `body`, and — for a host-authored `result`
only — `attempt_id`, `status`, and `candidate`.

By default `inbox[]` contains only unacknowledged mail. `include_read=true`
adds the newest 64 acknowledged records per recipient, newest first, with
unread records still bounded to `MAX_MAILBOX_RECORDS` rows. Listing does not
acknowledge mail. Model-authored mail admits up to 16 KiB, independently of
these previews, and reaches recipients in full through steer, followup,
ordinary session delivery and `wait_agent` pending mail.
Result envelopes remain within 2 KiB; when an output exists they carry its exact
UTF-8 `output_bytes`, `output_truncated` (including first-line and escaped-envelope
cuts), and `full_output_via: "wait_agent"`. Naming the child in `wait_agent`
returns the full submitted output, even after it was delivered.

Agent rows omit `prompt` text and retain `description` and `prompt_bytes`.
A spawner's full row includes `transient_resumes`, the current attempt's
continuation count derived from durable Resume decisions, independently of
submit-harness corrections.
A spawner's row for a parked child carries the same `waiting_on` as its
`wait_agent` row (§4.4).
A result of up to 1024 bytes is `output_json`;
a longer one is `output_preview` plus `output_bytes`, and `wait_agent` with
that `agent_ids` returns it whole.

Per-attempt `compaction` is derived by the tree projector from the child's
`WorkflowSettled` facts: `committed`, `failed`, `no_eligible`, and the latest
committed pass's measured `tokens_before` / `tokens_after`. The typed agent view
and TUI rows/inspector show the same observations. These are disposable views,
not a second telemetry writer or persisted attempt fields. Opening a tree
replays the retained feed with an independent observation cursor; replay does
not spend turns or duplicate counts. `complete=false` marks incomplete retention
coverage; unassignable workflows are not attributed to an arbitrary attempt.
No peak input is shown: the current feed has no per-request input measurement.
No capacity is inferred from billed token sums or persisted as a new field.

### 4.3 `send_agent_message`

Required `agent_id` and `message`.

| Field | Notes |
| --- | --- |
| `agent_id` | equal to the sender is `self_send` |
| `message` | non-empty, at most `MAX_MESSAGE_BYTES` = 16384 bytes (16 KiB); otherwise `bound_exceeded` |
| `purpose` | `coordinate` (default), `finding`, `question`, `reply`, `followup` |
| `priority` | `deferred` or `urgent`; omitted means `urgent` from a parent to its running, active direct child, `deferred` otherwise |
| `in_reply_to` | required with `purpose=reply`, refused otherwise (`reply_invalid`) |
| `output_schema_json` | admitted only with `purpose=followup`, else `schema_mismatch` |

`purpose=result` is host-authored and MUST be refused from a model. There is no
`delegate` purpose: it would also replace a running task.

Receipt: `{ message_id, disposition }`, disposition one of `started`, `queued`,
`steered`. Mail that reaches an interrupted child while its cancelled turn
unwinds starts the resume turn instead, and its receipt reads `started` once
that resume Start is admitted (§9). A `started` mail never joins a turn some
other command admitted as a Start: once that turn is live the send delivers
the mail into it as a follow-up, and the mail's disposition and the receipt
become `queued` once that follow-up is admitted. A `started` mail whose Start
finds the child's slot still starting or retiring (a `followup` sent while the
previous attempt's slot is released, a Start while another one holds the
slot, or a slot another Start took while this one reopened the session) is
retried once the slot is vacant, under the send's one 60 s budget (§9), which
starts when the send claims its mail; a taken slot never faults the child. A
budget spent with no turn bound (an interrupted child, or a running child with
no turn) refuses the send `recipient_attempt_terminal`, settles the mail read,
and retains the followup opening: expiry is not a definitive Start refusal. A budget spent while a
turn another command admitted is still not live leaves the mail unread for
that turn's end to park or wake the child with, and the send answers the
receipt it already recorded: a redelivery no turn admitted does not rewrite
it. Only the
send whose `followup` opened an attempt can withdraw it after a definitive
Start refusal and the guarded tail-read/CAS transaction in §14.3; another
send's refused Start never does. Cancellation retains the opening and unread
mail for reconciliation. A send's Starts to one child are exclusive: a
`started` mail waits while another send's Start to the same child is in
flight. No receipt reads `started` or `queued` for a redelivery no turn
admitted: a redelivery that fails makes the receipt that refusal. A steer stays in the same turn and MUST NOT rotate its `turn_token`;
only followup or a new attempt rotates it. A recipient mailbox holds at most
`MAX_MAILBOX_RECORDS` = 256 unread (`Accepted`) records and the tree's unread
mail is bounded at `MAX_QUEUED_MAIL_BYTES` = 4 MiB;
exhausting either is `mailbox_full`. Acknowledged records beyond the newest
64 per recipient are pruned, except for one current-attempt Result envelope
per non-closed child retained internally as its durable push/delivery witness
(at most 32 witnesses per tree, still only the newest 64 read records exposed);
message ids remain monotonically increasing and
are never reused after pruning. Old admission receipts treat pruned mail as
already consumed. Pruning MUST NOT make a terminal attempt
push its result again. Result envelopes shrink their output excerpt
until the escaped envelope fits `MAX_MESSAGE_BYTES`; output size or escaping
MUST NOT drop a result push. The full result stays on the attempt.

Followup refusal codes distinguish `followup_not_idle` (a turn, seal, or adoption
is still active), `followup_closed` (a closed node, or a failed spawn that
never produced a session), `followup_recovery_required`, and
`followup_workspace_gone` (discarded or removed). Refusal never opens a new
attempt or changes the prior attempt's result.

Mail is **not a second spawn** and is not how a prompt is assigned.

### 4.4 `wait_agent`

| Field | Notes |
| --- | --- |
| `agent_ids` | optional; 1 to `MAX_WAIT_TARGETS` = 32 ids, no duplicates; omitted means every managed child that is not closed and has not reported to this caller yet or has unread, unclaimed mail for it (including mail or a Result envelope an earlier return's budget withheld) |
| `timeout_ms` | optional integer ≥ 1; default 300 000, **clamped** (not refused) to 3 600 000 |

Receipt: `{ agents: [...], mail: [...], timed_out, woke_by }`, where `woke_by`
is `settled`, `mail`, `timeout`, `steer`, or `process`. The receipt has no
presentation subject. The await attaches a host-only presentation subject to
its tool outcome: the comma-joined `agent_id`s, bounded to 240 bytes (complete
ids; a single longer id is clipped with one trailing `…`), omitted when
`agents` names none. That subject is not model-visible. A restored bare wait
reads it from the result observation, not from the 2 KiB preview. A human or unwatched-parent
steer returns no steer text. Watched urgent mail still returns `mail` with its
body through the sole mail consumer. A process wake adds `processes: ["p3"]`:
only terminal transitions of the caller's own jobs after wait admission wake it,
using an allocation-local terminal generation. Previously terminal jobs do not
wake a new wait. Neither non-mail early wake saves a durable receipt. The grant
is unchanged: agents only, roots and hubs, never leaves; process ids are not
wait targets. Process tools refuse ids outside the caller's live session,
including imported or forked history, with typed `not in this session` errors.
`wait_agent` is one presentation of the **await**, the single pack-neutral
capability over the caller's children, their mail and its own processes. An
await names children (the `wait_agent` targets above, or none) and, for
presentations that map them, the caller's own `p<N>` processes; a named
process this session never issued is refused `grant_denied`
(`process pN is not in this session`). An own process that ended and the
process service no longer lists (its notice consumed or its row reclaimed)
does not fail the call: its `jobs[]` row has status `already_reported`. A named process that is terminal,
including one already terminal at admission, wakes it as `process`; each named
process returns a `jobs[]` row `{ id, status, exit_code?, output? }`
(`running`, `exited`, `stopped`, `unknown` or `already_reported`) read from the process service as
a snapshot, which consumes no notice. `wait_agent` names no processes, so its
receipt never carries `jobs`. Every presentation returns the same receipt and
records it under one operation-ledger kind (stored as `wait_agent`), so
retirement and abandonment recovery below apply to every await alike.

**The Grok presentation.** `get_command_or_subagent_output {task_ids,
timeout_ms}` on a node granted agent tools splits `task_ids` into process ids
(`N` or `pN`, nonzero) and agent ids (anything else). A call naming only
process ids is the process tool's own call (chapter 01 §8): per-task `tasks[]`
rows, `not_found` and `already_reported` tombstones, steer return, notices of
terminal rows consumed at the caller's top-level commit, no tree receipt, and
the same bytes as on a leaf. A call naming a child is this await over the named
children (1 to 32, no repeats) and the named own processes (`p<N>`, admitted
as above, so an id this session never issued refuses the whole call `grant_denied`; a reaped own process is an `already_reported` row). An
omitted or zero `timeout_ms` is a snapshot: an await whose ceiling has already
expired, which returns the current rows, acknowledges only mail it returns and
saves no receipt unless it consumed mail or delivered a settlement; a positive
one awaits, clamped to 3 600 000. An agent id that is not the caller's child
takes the await's own refusal. The receipt is returned unchanged inside the
topology envelope `{ "result": "<receipt JSON>" }` and replays from its ledger
entry like any await. Its `jobs[]` rows are snapshots and consume no notice, so
a process that ended is still reported by its automatic completion notice; that
duplicate is accepted.

**The pi presentation.** `wait {ids?, timeout_ms?}` on a node granted agent
tools splits `ids` the same way. A call naming only process ids is the process
tool's own first-terminal await (chapter 01 §8.5) with the same bytes as on a
leaf. A call naming a child is this await over the named children and the named
own processes, as for Grok, except that an omitted `timeout_ms` is 300 000 (0
is a snapshot; clamped to 3 600 000). A bare `wait` (no `ids`) follows one
rule, decided when the call starts:

1. If an own process already ended and is still unreported, or no child is
   pending (every managed child has reported and has no unread mail, or there
   is none), it is the process tool's own bare await over every own process
   still listed, with a leaf's bytes: it returns at once on a finished one and
   consumes its notice at the commit. With no own process listed it returns
   at once with `{ "tasks": [] }`.
2. Otherwise it is this await over every pending child (`agent_ids` omitted)
   and every own running process, which returns on the first child settlement
   or mail, process end, steer or timeout, in the topology envelope.

A call id the ledger already holds as an await is that await again whatever the
live state now says, so a same-call replay (a rejoined turn, or a re-run cell
under its nested call id) returns the retained receipt.

`stop {id}` presents both slots as Grok's kill does: a process id is the
process stop tool's own call with its bytes, and any other id is exactly
`interrupt_agent` on that child, never a close.

**Output budget.** One await return is bounded to `WAIT_OUTPUT_BUDGET_BYTES` =
262 144 serialized bytes, deterministically. Every row (without
`output_json`), process name and job row (without `output`) is always
returned. The budgeted items are then considered in this order, each in target
or arrival order: `output_json` of newly settled rows, mail envelopes,
`output_json` of rows already delivered (re-reads, which only an await naming
`agent_ids` carries), and job output tails. The
first budgeted item is always returned whole, so every await makes progress
(it may exceed the budget alone; the tool outcome limit still holds because
`output_json` is at most `MAX_OUTPUT_JSON_BYTES`). After it, an item is
admitted only if the whole return stays within the budget; an output or tail
that does not fit is skipped, and plain mail stops at the first envelope that
does not fit, so acknowledged plain mail stays a prefix of its arrival order. A
Result envelope whose row carries an output is not plain mail: it travels with
that output, admitted or withheld with it whatever its position, so it may be
acknowledged although earlier plain mail was withheld. A withheld new output
leaves its row undelivered (it is not marked delivered and its Result envelope
stays unread), and withheld mail stays unread, so the next await returns them
at once (a bare await keeps watching a child whose row was delivered while it
has unread mail, and returns that row without its `output_json`; the row's
Result envelope, if still unread, is then plain mail). A withheld re-read
leaves its row delivered: an identical
await would withhold it again, so it is listed apart and the note says to name
that agent id alone, which always returns it (the first budgeted item is
whole). When anything is withheld the receipt adds
`omitted: { outputs: [agent_id], rereads: [agent_id], mail: N, job_output: [id], note }`;
it is absent otherwise.

A mail-settled wait returns its
exact unread envelopes in arrival order in `mail[]`: `message_id`, `from`,
`purpose`,
`in_reply_to`, full `body` up to `MAX_MESSAGE_BYTES`, and the inbox envelope
metadata. A Result envelope whose attempt is in `agents[]` is acknowledged
but rendered only as `{message_id, purpose: "result", attempt_id}` to avoid
duplicating the result body. Only successfully returned mail is acknowledged;
later waits do not
return on that mail again. New mail remains unread even if it arrives while
the earlier batch is being returned. Running rows omit usage rather than
reporting zero counters as live facts. Each row is per attempt and never
rolled up:

| Field | Meaning |
| --- | --- |
| `agent_id`, `status` | the node and its `NodeStatus` |
| `attempt_id`, `attempt` | the current attempt and its `AttemptStatus` |
| `candidate` | the sealed candidate id when one exists — a candidate may exist without a submission |
| `output_json` | only for submitted rows |
| `blocked` | `{ quota, account, resets_at? }`; `resets_at` only when the provider's own retry disposition said when |
| `failure` | `{ kind, message }` from the fault the host recorded |
| `usage` | `elapsed_ms`, `turns`, `tool_calls`, `input_tokens`, `output_tokens` |
| `waiting_on` | while the child is parked (§5.2), or waiting for a transient continuation: `network: true` names live route readiness; otherwise `{ processes: ["p3"], agents: ["c2"] }`, its own running processes and its children that are still live work; both lists empty is the explicit parked marker (an arrival already woke it, or a resume holds it for its reopen wake); absent while a wake is admitted or the attempt is ending; process-local |

A wait returns when a watched child **settles, seals, asks a question, or sends
mail**, including urgent mail. A node that is still `sealing`
has **not** settled, so the wait holds through the seal — which is also when a
candidate can be named. An interrupted node does settle a wait but produces no
result for it. A timeout is not cancellation: `timed_out: true` means call
again. When the waiting agent's own turn is interrupted or settles while it
waits, the wait is abandoned with `wait_caller_settled`; nothing about the
watched children changed.

A **bare** wait (no `agent_ids`) delivers each settled row once. Once a wait
has returned a child's settled row, later bare waits by the same caller leave
that child out until its row changes (a follow-up attempt, a new status), so a
wave of children comes back one settlement at a time and the parent never
busy-loops on a child it already read; a delivered child with unread mail for
the caller stays watched until that mail is read, and such a row comes back
without the `output_json` the caller already holds (it is neither re-sent nor
listed under `omitted.rereads`). The unread check reads every inbound envelope
from that child, past the mailbox projection's cap. When every managed child has
reported and has no unread mail, or is closed, a bare wait refuses
`no_pending_children`. Naming `agent_ids`
returns the named rows, but a delivered settled row does not wake a mixed
wait that still watches undelivered children. If every named row is already
delivered, the call immediately re-reads them. Acknowledging a Result envelope
through any tree API (including wake or admission) also marks its matching
settled attempt delivered to the recipient. A matching acknowledged current-attempt
Result envelope preserves delivery across reopen, including direct-child slot
release. Settled rows without that witness use a process-local delivered-row
record and may re-deliver once after reopen.

Wait receipts are tree intents keyed by the wait's runtime call id in the
tree's operation ledger: each binds the acknowledgement snapshot the wait took
and the rows it newly delivered. A wait acknowledgement stands only once the
caller's change feed (chapter 01 §5.11) shows the call committed. The tree saves
a receipt and its acknowledgements before the await returns, so canonical
history never holds a wait result without its receipt. A timeout with no
consumed mail or new settlement saves no receipt. A result the observation
allowance clips under context pressure is still the committed result: its
acknowledgements stand, and the clipped mail bodies remain readable through
`list_agents` with `include_read` within the retained read-history bound.

**Settlement is a projection of the caller's feed.** The tree projector (§14.3)
applies each caller session's feed records in order and settles that caller's
receipts by these rules, live and on every open alike:

- A `HistoryAppended` fact whose committed call ids include a receipt's call id
  retires that receipt; its acknowledgements stand. What a later compaction
  does to that history does not matter: the fact recorded the commit.
- A `TurnTerminal` fact whose exchange was closed retires every receipt the
  caller's turn still holds, nested waits included: canonical history holds
  every result those awaits returned, a nested one inside its cell's result.
  This covers a completed turn, a cancelled one, a failure the driver closed
  and a context-capacity stop, because the fact states whether the close was
  committed; the host never guesses it from the failure kind.
- A `TurnTerminal` fact whose exchange was not closed (an `Internal` failure or
  an abandonment) restores every receipt of that turn not already retired: its
  mail snapshots return to `Accepted`, the rows it newly delivered are
  forgotten, and the receipt is removed. An abandoned turn's calls never
  commit, so its waits always restore.
- A `RecoveryRequired` terminal leaves the receipts for the record that later
  closes the turn.

A nested wait (from a Code Mode cell) has no model-history entry of its own, so
only its caller's closed-exchange terminal retires it. An operator `invoke_tool`
await is never in canonical history, so its acknowledgements restore at the
caller's next terminal record and the model receives that mail. Settlement is
idempotent and keyed by the caller's cursor: a process loss between a caller's
commit and the tree transaction leaves the tree cursor behind, and the next tick
or open applies the same records with the same result. A root prompt is admitted
only after the tree projector has applied the root's feed to its tail, so
restored mail rides that prompt. Rejoining the same bound turn retains its
receipts for same-call replay; retirement removes only that caller's await
receipts, never receipts for other tools or other callers. The output remains in
a current receipt for same-call replay; its mail snapshots remain for rollback
even if ordinary read history was pruned. A caller therefore holds at most one
receipt per await of its current turn that returned mail or a newly settled row
(a timeout or a non-mail early wake saves none), bounded by that turn's rounds
and the awaits each round and cell makes; its terminal removes them, so
receipts do not accumulate across settled turns. Submit-harness rounds share the
attempt's receipts until the closed-exchange terminal of that attempt's turn.

When a recipient is parked in a wait watching the sender, the wait is the
mail consumer; the live deliverer MUST skip followup and steer for that mail.
The send owner waits for this consumption or for the parked wait to leave,
then rechecks durable mail under the tree lock. A cancelled wait releases its
claim and wakes the send owner; the final timeout poll releases the claim under
the same lock so subsequently persisted mail is not stranded.
A delivery already claimed before the wait began is excluded from its mail
projection so the same envelope cannot be consumed by both paths.

Cancelling the caller's tool context ends a pending wait with `Cancelled`;
it does not interrupt watched children or acknowledge mail that the wait has
not returned. Cancellation already present before polling takes priority over
ready mail. A ready result and its synchronous mail acknowledgement that have
already completed remain delivered. Other topology operations retain their
existing effect ownership through cancellation.

The wait is poll-driven internally on a 1 s safety tick, with a notify fired from
the tree's own bump so a seal, question, or mail wakes it without waiting out
the tick. It is not an event loop.

**Cost, quota balances, and subtree rollups MUST NOT appear in any prompt.**
They belong to the TUI and the telemetry sink.

### 4.5 `interrupt_agent` and `close_agent`

Both take `agent_id` (required) and `expected_attempt_id` (optional). A mismatch
is `attempt_mismatch`.

`interrupt_agent` returns `{ agent_id, status }`. `close_agent` returns
`{ agents: [{ agent_id, status }, …] }` for the node and its descendants. A
`recovery_required` node whose session outcome is unresolved is not closed:
its row reports `recovery_required` with `remedy: "recover_agent"`, keeps its
attempt failure, and authorizes no retry. Two kinds of `recovery_required` node
are closed like any other: one whose only remaining blocker is an interrupted
seal, adopt, or copy removal, once the operator has inspected the copy, and one
whose recovery met a refusal no retry can get past (section 4.5a), whose
attempt the close settles `failed` with that refusal's code as its kind.

Close stops the promoted jobs of every closed node before its seal.
Failed child turns, including recovery-required outcomes, stop their own
promoted jobs with attempt-end attribution before the owner is parked or
released. A cleanup failure remains recovery-required rather than reporting
successful cleanup. Parked jobs still appear with their child owner in the
Processes view and count toward the live process HUD until they stop.
Interrupt leaves promoted jobs running and preserves their host-owned services
for the next same-branch attachment.

On Grok, `kill_command_or_subagent {task_id}` presents both slots: a process id
is the process stop tool's own call with its bytes, and an agent id is exactly
`interrupt_agent` on that child (its receipt kind and request
`{ agent_id }`, the current attempt, live cancellation, reversible), returning
`{ agent_id, status }` in the topology envelope. It never closes; `close_agent`
stays. An agent id that is not the caller's child takes the interrupt's own
refusal code. Pi's `stop {id}` presents both slots the same way (§4.4). Neither
presentation carries `expected_attempt_id`: the interrupt it runs always
targets the child's current attempt, so it never refuses `attempt_mismatch`.

### 4.5a `recover_agent`

`{ agent_id, expected_attempt_id? }` settles a `recovery_required` child through
the one supported path, the same exact session reopen a relaunch performs, and
returns `{ agent_id, attempt_id, recovered, status, attempt, failure_kind }`.
Authority is `close_agent`'s: the caller must manage the node (its managed or
close set), on a live or terminal-cleanup tree; a stale `expected_attempt_id`
is `attempt_mismatch`. It writes no operation receipt and needs none: a node
that is not waiting on recovery returns its current state with
`recovered: false`, so a repeat is idempotent. When the session part settles
but an interrupted seal, adopt, or copy removal still holds the node, the
receipt also carries `workspace_recovery`, naming that blocker; `close_agent`
then ends the node.

`recover_agent` is the one trigger that waits on a lease: it waits up to 35
seconds for a lapsed owner's lease and then refuses. It polls: each try takes
the tree's reattach lock and never waits on the lease, and the lock is released
between tries, every two seconds, so a held lease never stalls the tree's other
recoveries, recycles, or reattaches. What it settles,
and how, is section 14.2. Nothing a child ran is replayed — no model call, no
tool. A refusal before the session settlement is published never settles the
attempt and records no fault on the node; the reopen it attempted may have
advanced the child session's locator. Post-settlement seal or publication
failures are reported separately (section 14.2). A refusal names
why: `recovery_lease_held` (another owner still holds the child session's
lease; retry after it lapses), `recovery_owner_live` (this process still owns the
child's turn, startup, or retirement, including a fenced turn's waiter that is
releasing and reopening the session; it settles on its own and recovery never
waits for it). A slow release keeps its owner after the joiner's deadline;
when release returns, on success or failure, that slot is cleared and a background
pass tries recovery in this process. It is not a permanent refusal.
Other refusals include `recovery_storage_unavailable`,
`recovery_effect_evidence_refused` (the head needs executor-owned effect
evidence the host does not hold), `recovery_session_unavailable` (the session
could not be reopened or recreated, its recorded local recovery scope is
unavailable to this build, or the host has no session port), and
`recovery_vcs_ambiguous` (only a seal, adopt, or copy removal was in flight;
section 14.2).
Code Cell abandonment and local finalization debt alone are not missing
executor evidence and must not be labelled `recovery_effect_evidence_refused`.
An already-terminal root with only exact same-turn finalization debt closes
quiescently on reopen (chapter 01 §5.5), without another result or
abandonment report. The fenced turn waiter's successful recycle then settles
the attempt as an ordinary failed attempt (`turn_abandoned`), not
`recovery_required`; an explicit `recover_agent` settles such a quiescent
reopen as `RecoveredWithoutTurn` (`recovered_without_turn`).
An abandonment report already proves a successful recycle; only a reopen
without that report needs a quiescent-head probe. A probe failure is logged as
such and keeps the attempt recovery-required; it is not a reopen failure.

Two refusals are terminal, because no retry can get past them: a head that
needs effect evidence (`recovery_effect_evidence_refused`, any phase) and a
session an incompatible build created (`recovery_session_unavailable` from an
incompatible session). Reconstruction refusals retain a content-free category
(harness plan, tool presentation, stable request prefix, recovery scope or
runtime revalidation, with runtime reconstruction as the fallback); arbitrary
executor text never enters that diagnostic.
The host remembers a terminal refusal for the node's
current attempt, in process: `list_agents` then names `close_agent` as the
row's remedy with the refusal as its `recovery` reason, the automatic pass
stops retrying the node, and `close_agent` settles the attempt `failed` with
the refusal's code as its kind and ends the node, freeing its slot. After a
restart the next recovery observes the refusal again.

### 4.6 `submit_result`

Required `turn_token` (integer ≥ 1) and `output_json` (string, at most
`MAX_OUTPUT_JSON_BYTES` = 262 144 bytes). A root caller is
`submit_root_forbidden`; a stale token is `submit_stale_token`; a differing
resubmission on the same token is `submit_duplicate_conflict` while an
identical resubmit is idempotent.

Submission refuses with `live_work` while the caller has running child attempts
or jobs. The refusal names their ids; child remedies are filtered through the
caller's compiled topology tools, and processes can be finished or stopped with
the caller's process tools. Leaves are never told to use `wait_agent`,
`close_agent`, or `interrupt_agent`. Idle JSONL `invoke_tool` consults the same
JobHost despite having no active turn.

Receipt: `{ agent_id, attempt, candidate? }` — the candidate is absent until
the copy is sealed.

### 4.7 `adopt_candidate`

`{ candidate_id, expected_target?, evidence?: [attempt_id] }`. Receipt:
`{ candidate_id, target_before, target_after, files_changed }`. Section 7 owns
the semantics.

Adoption succeeds when the caller has running jobs, with a warning line naming
the processes still using the checkout.

### 4.8 `discard_workspace`

`{ workspace_id }` → `{ workspace_id, status, discarded_candidates }`, where
`discarded_candidates` lists the exact unadopted candidate ids relinquished by
this explicit discard, including the final durable success receipt. It drops a child copy without
adopting anything from it, explicitly giving up its unadopted candidates too.
It MUST refuse (`workspace_referenced`) while live or unresolved occupants or
another live copy's base still need the workspace. A discarded candidate can
never be adopted; the durable workspace status fences it before VCS dispatch.
The tree durably marks the row `discarding` and prepares the operation before
removal. Only successful VCS acknowledgement changes it to `discarded` and
publishes the success receipt. Cleanup failure keeps the row reserved, records
`workspace_recovery_required`, and preserves the retained files and pins for
recovery; exact replay cannot repeat pending removal. A reopened `discarding`
row makes its occupants that are not closed recovery-required; a removal never
touches its owner's checkout, so the owner is not faulted for it. A failed candidate-unpin is a seal failure, never a
successful empty candidate; unresolved seal or session work cannot be discarded.

**Closed copies are pruned by the host.** After a close, a seal, or an adopt,
every forked copy whose occupants are all closed and settled, that backs no
unadopted candidate and no live copy's base (the checks above), is removed the
same way: marked `discarding`, published, removed, then `discarded`. An adopted
candidate's content already lives in its adopter, so its copy is disposable
once its occupants close; an open finished node retains its copy and base for
followup attempts. Removal also drops that candidate's pin (§6.1). A
closed node with no candidate also releases its copy. A candidate still
adoptable always retains its copy and pin. A prune that fails puts the row back to its prior status and records a
fault on the root; the closed nodes do not become recovery-required, and the
host does not retry that copy until the tree is opened again.
A stale `discarding` row (a removal that failed, or that a lost process left in
flight) whose occupants `close_agent` has closed, whether the parent or the
operator closed them, is pruned the same way: the close settles that removal,
so the host retries it once per process and marks the row `discarded` on
success. The explicit discard that reserved such a row already gave up its
unadopted candidates, so they do not hold the copy. In a Git checkout, a copy
whose directory and worktree registration are both already gone counts as
removed; only its `plak/<agent_id>` branch is then dropped.

### 4.9 Error codes

The complete `TopologyErrorCode` set, in wire spelling:

`unknown_field`, `schema_mismatch`, `bound_exceeded`, `grant_denied`,
`self_send`, `recipient_closed`, `recipient_attempt_terminal`,
`recipient_recovery_required`, `reply_invalid`, `mailbox_full`,
`spawn_budget`, `capacity`, `request_key_conflict`, `depth_exhausted`,
`template_not_spawnable`, `template_unknown`, `mint_unavailable`,
`mint_hook_failed`, `input_not_sealed`, `workspace_identity_mismatch`,
`workspace_recovery_required`, `workspace_not_owned`, `workspace_referenced`,
`model_not_admitted`, `submit_root_forbidden`, `submit_stale_token`,
`submit_duplicate_conflict`, `live_work`, `tree_restored_paused`,
`operation_recovery_required`, `candidate_unknown`,
`not_sealed`, `not_authorized`, `evidence_mismatch`, `target_changed`,
`merge_conflict`, `adopt_not_applicable`, `discard_not_applicable`,
`followup_not_idle`, `followup_closed`, `followup_recovery_required`,
`followup_workspace_gone`, `quota_blocked`, `attempt_mismatch`,
`no_pending_children`, `wait_caller_settled`, `recovery_lease_held`, `recovery_owner_live`,
`recovery_storage_unavailable`, `recovery_effect_evidence_refused`,
`recovery_vcs_ambiguous`, `recovery_session_unavailable`.

`lease_saturated`, `class_not_pinned`, and `merge_not_owner` are not codes and
MUST NOT deserialize.

## 5. Attempts

**The attempt, not the node, is the unit of work.** `status`, `turn_token`,
`turn_id`, `output_json`, `submit_harness_nudge`, the `blocked` reason, the
`failure`, the per-attempt usage row, `retry_of`, `request_key`, and
`candidate_id` all live on `agent_attempts`, keyed `(tree_id, attempt_id)`.
`attempt_id` is `<agent_id>-a<seq>`.

`AttemptStatus` is `running`, `submitted`, `completed`,
`completed_without_submission`, `failed`, `cancelled`, `recovery_required`,
`closed`, `blocked`, `adopted`. Only `running` and `submitted` are
non-terminal. A `failed` attempt whose failure kind is `turn_abandoned` is one
whose turn a reopen abandoned after its owner was lost or fenced (Plakous
§13.1): its node is `failed` and idle, so a `followup` can re-task it, and the
attempt is refunded — it does not count against the tree's attempt budget.

`NodeStatus` is `pending`, `running`, `paused`, `sealing`, `completed`,
`interrupted`, `failed`, `closed`, `recovery_required`. Because a node can be
re-tasked, **terminal means "can never run again"** — `closed` or
`recovery_required` — while `completed` and `failed` are idle, not terminal.
The older "no turn running" meaning survives as *finished* (`completed`,
`failed`, `closed`, `recovery_required`) and drives listing filters, epoch
quiescence, and detach accounting. A wait settles on the finished set plus
`interrupted`; `sealing` settles nothing.

### 5.1 `request_key` and spawn identity

`spawn_agent` accepts a caller-minted `request_key`. A repeat carrying the same
key **and the same brief** returns the first child's receipt and spawns
nothing. A repeat carrying the same key on a **different** brief is refused
`request_key_conflict` rather than silently aliased.

The brief includes the exact admitted input candidate identities. A key already
spent by **another** caller is refused without naming that caller's child, which
the caller may not be able to see.

The existing `call_id` digest replay is retained and is not a substitute: it
dedups retries of one call, not two independent spawns.

`interrupt_agent` and `close_agent` accept `expected_attempt_id`
(`attempt_mismatch`). A node created as a retry carries `retry_of`.

Keyed spawn identity is stored on the original attempt: caller, template,
description, prompt, admitted output-schema digest, and input candidate IDs.
Replay compares that immutable identity, never the mutable node brief,
workspace head, or a candidate's continued presence. Git and JJ use the same
comparison. Requested tier remains outside this identity; the existing receipt
reports the effective tier. A missing admitted identity is recovery-required,
never permission to reconstruct it from current state or spawn again.

The tree store is schema **6**, retaining this admitted identity and the nested `spawns` roster. Another schema lives
in another build's state directory (chapter 02 §2.5) and is never read; no
migration or guessed identity is performed.

### 5.2 `submit_result` and `completed_without_submission`

Attempt success requires the child to call `submit_result` with the current
`turn_token` and schema-valid `output_json`. **Last assistant text is never a
result and is never harvested into `output_json`.**

Every attempt end (Completed, including work started after submission,
completed_without_submission, Failed, Blocked, or Cancelled) closes the live
descendant subtree without completion mail and stops the attempt's jobs with
`attempt ended`, before sealing. Recovery-required and interrupted states do
not end the attempt and preserve children and promoted jobs.

**Parking.** A child turn that completes without `submit_result` while the
attempt still owns live work, or when tracked work already arrived for it,
parks instead of being nudged; arrivals wake the park at once. Live work is a
running promoted process of the child's own (including one a Code Mode cell
started), or a child of its own that will still report: an open attempt on a
`running`, `pending` or `paused` node, or a node `sealing` its result.
Interrupted and recovery-required children are not live work. A parked child's
attempt stays `running`, its bound turn is released, its live session stays
attached (its processes need it), no corrective Start is admitted and
`submit_harness_nudge` does not move. The ended turn committed its exchange,
so releasing it retires the child's await receipts (§4.4): however often a
child is woken, it holds at most one turn's receipts. A follow-up turn that
queued mail admitted behind the ended turn is not a park: the host observes it
as the attempt's next turn, binding it even when it passes `max_turns` (the
runtime admitted it before the host could refuse it); its unsubmitted end then
ends the attempt `blocked`. A follow-up or steer that reaches the child after
the turn it targeted ended (mail sent between the turn's end and this
decision) is not a delivery failure while a wake can still carry it: the send
stands, the mail stays unread and unclaimed, and it wakes the child like any
other arrival. A delivery that finds the child's live slot still starting or
retiring reached no turn and no turn ended: the send looks again once the
slot changes, and follows the mail into the turn that slot admits. Mail whose delivery is still in flight when the turn ends parks
the child, and the wake waits for that delivery to return. If the attempt had
already submitted, ended, or is ending after a wake that was not admitted,
nothing would wake it: the send is refused `recipient_attempt_terminal`, the
refusal names the attempt's state, the mail is settled read, and the send's
operation receipt becomes that refusal, so a replay is refused too. Mail that
reaches an interrupted child while its cancelled turn is still unwinding is
redelivered as the resume Start once the turn is released (§9). An interrupt
that lands after the turn ended but before this decision ends the turn as an
interrupted one (§9): the bound turn is released and nothing parks. A Code Mode cell
still running at turn end is ended by turn cleanup as before; a cell is never
a wake source.

The tree host wakes a parked child on the work it tracks for it: a terminal
notice of its own process, unread mail addressed to it (its children's
results, questions and other mail, and its parent's mail of either priority),
or the loss of its last live work without a report (for example a child closed
by another manager). Arrivals coalesce for 1 s, then one host-instruction turn
(`HostWake` origin) runs on the same attempt and `turn_token` (§8). A wake turn
binds like any other turn, so the template's `max_turns` counts it. An
unsubmitted turn end with `max_turns` spent neither parks nor admits a
corrective round: the attempt ends there (`blocked`). There is no park timer: a park is
bounded by the processes' hard cap, an interrupt or close from the parent or
the operator, and `max_turns`. An interrupt (including a root interrupt) moves
a parked child to `interrupted` exactly like a running one, ending the park
and keeping its processes; a close ends the attempt and stops them. An
interrupt or close that lands while a wake is being admitted wins if the
wake's Start was not yet admitted: nothing starts and the wake's batch stays
unread; after admission it cancels the bound wake turn like any running turn.
A wake the runtime definitely rejected releases its batch unread; a wake
that cannot be admitted ends the attempt as an exhausted corrective round
would. While it ends, mail to the child is refused
`recipient_attempt_terminal`, an interrupt no longer applies, and a mail Start
never joins the live session, whose turn the tree no longer binds; mail that
was waiting for the wake is settled read and named in the node's fault. A
wake that was admitted, and possibly bound, but whose batch cannot be settled
is routed like a corrective restart that needs reconciliation: its turn is
cancelled and the attempt becomes `recovery_required` under the wake turn,
which settles the parent's wait; its batch is left for the reopen to settle
from the child's feed. Parking
never settles a parent's wait: the node stays `running`. Its `wait_agent` and
`list_agents` rows carry `waiting_on` (§4.4). Unread mail or an unconsumed
notice that the previous wake already carried (past the prompt bound) does
not by itself wake the child again.

If a child turn completes without that call, with no live work and nothing
newly arrived, the host
admits at most one reminder Start and one force Start on the same attempt with
the `turn_token` unchanged; the harness round count persists on the attempt
(`submit_harness_nudge`, 0–2). Zero extra rounds run if `submit_result`
already ran. Each corrective Start command identity includes the admitted
attempt and round, so exact retries retain their identity and a followup
attempt on the same node gets its own corrective Starts. After those two rounds
still miss, the attempt ends
`completed_without_submission`. That is a termination path like any other, so
a minted (`workspace=fork`) node still seals its candidate there. Inherit has
nothing to seal. Bind and the
spawn Start already succeeded — this is model-following, not a plumbing
failure. A **failed** harness Start is `recovery_required`, not a miss.

Request-level `tool_choice=required` is not a Start overlay: the force round is
prompt-required.

Submission records an anchored `submitted` intent after schema and token
validation. Its exact output and attempt identity derive the submitted view;
a duplicate with identical bytes reuses that result and adds no second intent.
Both view-deletion paths clear output and rebuild it from the fold. Submission
alone seals nothing and sends no result mail.

### 5.3 `purpose=followup`

A `followup` re-tasks an **idle** child on the same node with a fresh attempt,
a rotated turn token, and an optionally new `output_schema_json`. The
transition is `completed | failed | interrupted → running`. The previous
attempt keeps its own `output_json`, usage, and candidate — which is why the
attempt is the unit.

A failed fork's ordinary sealed candidate remains adoptable; the list/wait row
names its candidate beside the failed attempt status, which identifies it as
partial rather than a successful result. Adoption is explicit. Followup
continues in the same child copy without adopting it first.

An interrupted attempt is *superseded*: it ends `cancelled` with no result, so
exactly one attempt of a node is ever live.

`followup_not_idle` identifies a running turn, sealing, or adoption that must
settle before another attempt. Use an urgent steer for an active turn.
`followup_closed` identifies a closed node, or a node whose spawn never
produced a session (attempt failure kind `spawn_refused`): there is no session
to re-task, so the caller spawns a new agent; `followup_recovery_required`
identifies unresolved session ownership; `followup_workspace_gone` identifies
a discarded or removed working copy. These refusals do not open a new attempt.
A Start rejected for storage pressure before admission is likewise a definite
refusal: the attempted follow-up is rolled back, the existing child session and
its prior history remain intact, and the caller receives `capacity`. Lost or
cancelled startup ownership is not proof of refusal and therefore retains the
session for recovery instead of deleting it.

Children are ordinary sessions and keep the ordinary compaction program. A
child has no host idle fence, so the driver's turn-admission fence compacts
each admitted child turn whose anchored occupancy meets the trigger (spec 01
§10.3, fence (a)).
Whenever a child ends with terminal typed `ContextLimit`, its attempt fails with
kind `context_exhausted`, whether or not the compaction ladder was exhausted.
Its failure message tells the parent to respawn with a handoff, not a follow-up
into the same exhausted history. This uses the existing TEXT failure kind/message
fields, without changing the tree schema; it is not a recovery-required turn.
Once resolved partial work seals, `context_exhausted` and `turn_failed` list/wait
failure messages and the TUI fault line name `partial candidate cand-<attempt>`
and the fresh `spawn(inputs=["cand-<attempt>"])` handoff recipe. The result
notice carries the same recipe as `continuation`. This is derived from the sealed
candidate, never promised before sealing or manufactured from a rescue ref.
Template cards advise a fresh spawn with a candidate input for large new work
rather than an unbounded follow-up chain.

A terminal typed content-policy error (`Policy`) fails the child attempt with
kind `refused`, distinct from `turn_failed`, retaining the provider message.
The failure result instructs the parent to re-run a refused or
context-exhausted child as a **fresh child with a handoff**, never a follow-up
into the same session. Neither error authorizes automatic retry or rebind.

Every failed child turn with an active forked workspace also takes a rescue
snapshot after stopping its background jobs, outside the tree mutex. Git pins
it under `refs/plak/<tree namespace>/candidates/failed-<attempt id>-<turn id>` without
moving the child's branch, HEAD, real index, or working files; the normal secret
exclusions apply. Failed-turn refs are create-only: a repeat refuses to overwrite
an existing pin and reports its exact ref. The failure message names the ref and
commit, or explicitly reports that the snapshot could not be made while retaining the original
failure. This rescue ref is inspection/cherry-pick evidence, not an adoptable
candidate or proof of session settlement. Resolved partial work still seals
as an ordinary candidate for `adopt_candidate`; a recovery-required turn may
retain a rescue ref but MUST NOT create a new adoptable candidate or clear its
unresolved session evidence. Inherited checkouts are never snapshotted by this
failure path. No additional tree or wire fields are required.
Rescue refs remain pinned until the operator explicitly deletes them, including
after workspace removal or tree maintenance. They are never registered as
candidates and are never adoptable. `plak state` inventories recorded artifacts,
not these unregistered rescue refs. In the workspace's Git repository, list them
with `git for-each-ref --format='%(refname)' 'refs/plak/*/candidates/failed-*'`;
delete one with `git update-ref -d <exact-ref>` only after inspecting or retaining
the work elsewhere. Deletion releases that snapshot's garbage-collection pin.
For a non-colocated JJ workspace, run the same commands with
`git --git-dir=.jj/repo/store/git`.

### 5.4 Followup after adoption

An **adopted** copy is admitted. The workspace row returns to `active` for the
new attempt, so that attempt's terminal seal runs — a seal only ever runs on an
`active` copy.

On a successful landing the copy's base has already advanced to the **adopted
state**: the adopted candidate's own snapshot on both backends. The next candidate is
therefore the **delta** over what was adopted, and a second adopt lands only
that delta instead of re-applying the first.

## 6. Sealing and candidates

A **candidate** is a pinned, content-identified snapshot of a child copy that a
parent may adopt. Only a minted copy (`workspace=fork`) seals. Inherit shares
the caller's checkout and has nothing to seal. `read_only` may still fork a
snapshot; process tools are allowed, Plak file writes are not.

The single VCS primitive is `snapshot(path) -> { commit, content, change_id }`.
It computes the base at mint and the candidate at seal, so a copy and its
candidate are always compared on the same identity.

### 6.1 git

Snapshot runs against a **temporary index** seeded from
`git rev-parse --git-path index` — not a literal `.git/index`, which is wrong
inside a worktree — then `git add -A`, the secret unstage below, `git
write-tree`, and `git commit-tree`:

```sh
TMP=$(mktemp -d)/index; cp "$(git rev-parse --git-path index)" "$TMP"  # 0700 dir, 0600 file
GIT_INDEX_FILE=$TMP git add -A
GIT_INDEX_FILE=$TMP <secret unstage>
T=$(GIT_INDEX_FILE=$TMP git write-tree)
C=$(git commit-tree "$T" -p HEAD -m "plak snapshot <label>")
```

`content` is the written tree id `T`; `commit` is the wrapper commit `C`. The
`commit-tree` call pins `user.name=plak`, `user.email=plak@local`,
`commit.gpgsign=false`, and both author and committer dates to `@0 +0000`, so
the wrapper is a pure function of tree, parent, and label. The `-p` is omitted
when HEAD does not resolve (an unborn branch). The working copy, the real
index, and HEAD are never touched, and `change_id` is none.

**Secret unstage.** Before `write-tree`, the staged path list
(`git diff --cached --name-only`, falling back to `git ls-files --cached` on an
unborn HEAD) is filtered and each secret path is removed from the temp index
with `git reset -q -- <path>`, or `git rm --cached --force -q -- <path>` when
that fails. A path is secret by basename: `.env` and any `.env.*` other than
`.env.example`; exactly `.netrc`, `.npmrc`, `credentials.json`, `id_rsa`,
`id_ed25519`; and the extensions `pem`, `key`, `p12`, `pfx`. A candidate commit
therefore cannot pin a secret the tool policy already excludes.

**Mint.** The copy is `<launch>/.plak/worktrees/<copy_id>`, where `copy_id` is
`sha256(tree_id)-<agent_id>`. The tree-scoped identity also names the
`plak/<copy_id>` branch. Consequently two trees sharing a checkout may both
use the logical child id `c1`, and delayed cleanup from a crashed or partially
pruned tree cannot address the other tree's path or branch. The base is the
supplied candidate, or `snapshot(<parent cwd>)` — never the launch HEAD. The
argv is `git worktree add` plus `-B plak/<copy_id>` for a `fork` (a stale
branch the same tree left in this checkout is reset, never a refusal; git
still refuses a branch checked out elsewhere) or
`--detach` for a `snapshot` copy, then the destination and the base commit, run
in the launch checkout. `.plak/worktrees/`, `.plak/workspaces/`, and
`.plak/bin/` are appended to the repository's `info/exclude` if missing.

**Orphan reclaim.** The tree never mints a copy id it has recorded, so a
destination that already exists for the id being minted was left by a crash
between `git worktree add` and the tree persisting the copy, and no agent ever
ran in it. The mint reclaims it first: `git worktree remove --force <dest>`,
removal of the directory, `git worktree prune`, and `git branch -D
plak/<copy_id>` when that branch exists. Child-id allocation therefore does not
skip a `.plak/worktrees/<copy_id>` the tree has no row for.

**Seal.** `snapshot(<child path>)`, then
`git update-ref refs/plak/<namespace>/candidates/<candidate_id> <commit>` run inside the
child, so discarding the worktree and the `plak/<agent_id>` branch cannot
orphan the candidate. The child's own `HEAD` is deliberately **not** advanced;
adoption never needs a checkout.
Model-visible candidate ids stay short: `cand-<attempt_id>` (the node id
when no attempt exists). Physical pins are `refs/plak/<namespace>/candidates/<id>`,
where the namespace is the first 16 hex digits of sha256(tree_id). Before
pinning, a private immutable ownership file under the Git common directory
records the full digest; a short-prefix collision refuses the seal and retains
existing artifacts. Reservation and ref publication hold one repository
namespace-directory lock. Pruning drops an owned namespace's immutable evidence
only after its last pin is gone, under that same lock; concurrent seals cannot
publish across the empty-ref check. Tickets resolve the short id in their own tree namespace,
so one tree's cleanup cannot unpin another tree's adoptable work. Worktree paths
and fork branches already use the full tree digest and do not collide.

**Discard.** `git worktree remove --force <dest>`, then the directory, then
`git branch -D plak/<copy_id>`. Only a copy whose candidates are all adopted,
or whose settled owner explicitly gives up its candidates, is removed (§4.8),
and once it is gone the tree deletes each of those candidates'
`refs/plak/<namespace>/candidates/<candidate_id>` pins (best effort: a pin that survives
only keeps an adopted or explicitly relinquished commit reachable). An adopted candidate whose copy is
removed is then refused as a spawn `inputs` entry with `input_not_sealed`; fork
the node that adopted it instead.

Because the base is computed at mint, a dirty parent checkout is captured
exactly, rather than copied in as loose files and merged afterwards.

**Every `git` is bounded, neutral, and never prompts.** Each call drops the
inherited `GIT_*` steering variables, runs with `-c core.fsmonitor=false -c
core.hooksPath=/dev/null` so no repository hook or fsmonitor command runs, and
writes its scratch files privately (chapter 04 §2.6). It runs with
`GIT_TERMINAL_PROMPT=0`, an empty `GIT_ASKPASS` and, when the environment has
no `EMAIL`, the fallback `EMAIL=plak@local` (a configured `user.email` still
wins): an unconfigured identity never makes a reflog write resolve the host
name, which under musl without a network waits out the resolver timeout. It
runs in its own process group, under a ceiling: 30 s for a read (status, rev-parse, counts, which also set
`GIT_OPTIONAL_LOCKS=0`) and 120 s for a step that mints, seals, adopts, or
removes a copy. At the ceiling the whole group is killed and the step fails
with the typed error it already had (a failed seal is a fault, §6.5), never a
hang that holds the seal or the adopt.

### 6.2 JJ

The optional `jj-lib` backend runs in process, with no JJ CLI fallback. It
supports Git-backed JJ repositories and loads a fixed library configuration
(`plak`, `plak@local`, signing disabled), without user configuration or aliases.
An installed JJ executable is needed only for a human/model that chooses to
run JJ commands, not for host workspace operations.

In colocated repositories an external Git HEAD move is imported in process:
the library records the new parent and resets only JJ working-copy metadata,
then snapshots actual files. It does not reset the Git index, export private
candidate refs, or overwrite staged edits. If both JJ history and the working
copy changed incompatibly, recovery is required. Native adoption keeps the
Git HEAD parent and staged index unchanged. Missing CLI tools produce a
refusing model-facing fence; native operations do not require installing them.


Snapshot acquires the library working-copy mutation lock and compares the
recorded operation/tree with the current repository view. A rewritten stale
working copy returns `workspace_recovery_required` **before checkout**. Dirty
tracked and untracked files remain available for inspection; the host does not
run `workspace update-stale`, restore an operation, or silently commit a
recovery interpretation. Benign operation advancement with an identical tree
is allowed. Snapshot publication or state-save uncertainty also requires
explicit recovery, because an error does not prove no mutation happened.

`commit` is the immutable native commit ID; `change_id` is presentation and
lineage information, never a replacement for a sealed commit's identity.
`content` is `jj-tree-v1:` plus SHA-256 of domain-separated, length-framed native
merged tree IDs and conflict labels, including their term ordering. Sealing
pins the exact commit with a Git-backend `refs/plak/<namespace>/candidates/<candidate_id>` ref (which does not follow
JJ rewrites or change JJ immutability rules).
New secret paths use the same filename policy as Git snapshots, including the
`.env.example` exception. Changes to already tracked secret paths are refused
without modifying their working files. `.plak` machinery is excluded from
native snapshots and the repository's Git `info/exclude` keeps it out of
external JJ snapshots as well.

**Mint.** The library creates `<launch>/.plak/workspaces/<copy_id>` with name
`plak-<copy_id>`, using the same tree-scoped identity as Git, and a new empty
working-copy commit over the exact candidate or snapshotted parent. The
recorded base is that child's initial commit.
In-process mints serialize per launch repository; library locking coordinates
working-copy mutations with external JJ callers. An incomplete mint retains
its directory and reports recovery with its path instead of silently deleting
possibly published state.

**Discard.** The host removes the workspace registration through the library
before deleting its directory. If registration removal fails, the directory
is retained. Cleanup returns an explicit acknowledgement or typed failure;
`discarded` must not be inferred from scheduling a cleanup. Candidate unpin
also returns a result, so an unpin failure cannot be reported as an empty
successful seal. Candidate refs remain independently pinned.

A minted copy whose `.jj/repo` is a directory rather than a workspace pointer
is a standalone repository; snapshot, seal, and adopt refuse it.

### 6.3 When a seal runs

Every minted (`workspace=fork`) node seals at **turn completion**, not at
`submit_result` — the child can still write until its turn ends — on every
termination path: `completed`, `completed_without_submission`, `close_agent` on
a running child, quota-blocked, and fail-closed session materialization.

Three fences narrow that:

- A transient failed turn with a saved Resume decision seals no candidate
  while continuation is pending; exhaustion or a refused continuation settles
  the original failure and seals the ordinary partial candidate.

- An **interrupted** turn seals nothing and pushes no result. An interrupt
  leaves the copy as it is rather than pinning a candidate from a half-finished
  turn.
- `close_agent` seals only when **no turn is bound**. While a turn is still
  unwinding, the cancelled branch of turn finalization is the fence, and that
  branch seals only a node `close_agent` already marked `closed`. A bare
  interrupt therefore still seals nothing.

A candidate sealed **before** its attempt ended is **replaced** by the terminal
seal, never kept beside it: the id is `cand-<attempt_id>`, so the terminal seal
overwrites it with the final commit and no stale pin survives.

Minting reserves the spawn under the tree mutex, performs VCS work after
releasing it, and commits or aborts the reservation under the mutex. A reserved
child is not visible as an admitted node while its mint is in flight.

**Every seal runs outside the tree mutex.** All four paths mark the row
`sealing` under the lock, hand back a seal ticket, and finish through one
deferred driver after the lock drops: run the VCS subprocess, re-lock, record
the candidate.

### 6.4 Divergence

A copy that never diverged from its base seals nothing. The divergence check
runs in the same unlocked phase as the snapshot and, when nothing diverged,
**unpins** the candidate with
`git update-ref -d refs/plak/<namespace>/candidates/<candidate_id>` before returning none,
so a `candidate: null` seal never leaves a ref behind.

Git and JJ share a content predicate. Git compares written tree IDs; JJ
compares the current native merged tree with the immutable base commit's tree.
Commit or change-ID churn alone is not divergence. An edit committed by the
child remains visible because the comparison uses whole trees, not a current
working-copy diff.

### 6.5 A failed seal is a fault

Snapshotting runs a subprocess and can fail: a stale jj copy, a full disk, a
refused `update-ref`, a `git` killed at its ceiling. The seal path propagates that error rather than swallowing
it. The node is left `recovery_required` with a fault reason naming the seal
failure, and **no** `result` claiming "nothing diverged" is pushed.

**`candidate: null` means only what it says: the copy genuinely did not
diverge.**

A reopen observing `sealing` is `recovery_required` and MUST NOT auto-retry.
A seal that fails while the attempt's session outcome is also unresolved keeps
that session failure's kind on the attempt; settling the session (section
14.2) then begins the seal once more, and a second failure records
`seal_failed`. A seal that failed on its own (`seal_failed`) is not retried
by recovery and reports `mint_unavailable` with the seal diagnostic. An
interrupted seal reports `recovery_vcs_ambiguous` (section 14.2). Neither is
settled by the automatic recovery pass or by `recover_agent`; the operator
inspects the copy and closes the node.

### 6.6 Dormant checkout reclamation

The attached host automatically reclaims dormant fork checkouts after
`workspace_idle_seconds`, or oldest-idle-first when the tree's fork checkouts
exceed `workspace_budget_mib`. The disk budget is soft: active, inherited,
shared-with-live, unresolved and unprovably covered copies are never deleted to
meet it. Probes do not follow filesystem links. Inspection and VCS effects run
outside the tree mutex. Result delivery alone does not permit deleting a copy
whose runtime owner, turn waiter or seal still exists.

The tree acknowledges a reclamation intent, stops every occupant's jobs, seals
with the normal candidate rules, and acknowledges the exact snapshot and
candidate before removing the checkout and its local build cache. Candidate
pins are retained. A second coverage check immediately before removal refuses
changed content. Unknown ignored files, secret paths, independent staged
versions and any uncertain inspection retain the entire checkout. The local
`target/` cache and exact minted process fences are disposable. JJ coverage
inspection is currently unavailable, so JJ copies are conservatively retained.

These owned intents are recorded in `agent_operations`. Reopen resumes their
seal/remove/restore stages rather than treating them as an uncertain explicit
discard. Removing an already absent checkout is idempotent. A follow-up first
rechecks admission and rehydrates the exact sealed content at the same
tree-owned path, without rerunning `on_mint`, then rechecks capacity before
opening its new attempt. The workspace keeps its adoption base. A restored copy remains reserved from
reclamation and retirement until follow-up advances its attempt; a crash or
failed admission in that interval retains the restored checkout for retry. A partially
rehydrated directory is reused only after proving exact coverage; otherwise it
and its pins remain for recovery. Candidates from a reclaimed git copy remain
adoptable and usable as spawn inputs through the launch repository.

### 6.7 Conversation retention

The attached host retires sealed, delivered dormant child conversations after
`child_retention_days` (default 7), or oldest first above
`child_retention_count` (default 128). Close requests retirement immediately,
subject to the same settled-owner and coverage proofs. Live, undelivered,
recovery-required, unfacted-command, waited-on and unsealed children are not
collected. Leaf children are collected before their parents.

Retirement first publishes an exact runtime-session deletion intent and closes
follow-up admission. Only the facade may archive the exact released session
locator and delete its archived revision; ownership or stale-locator failures
retain the intent and records. Recovery finishes an interrupted archive/delete
idempotently. Runtime deletion precedes collection of attempts, child tool
receipts, command and acknowledged mail records. A durable allocation watermark
prevents reuse of collected child identities and does not refund spent attempts.
Immutable receipts belonging to still-retained callers remain replay authority.

Retention never deletes work. Unadopted, nonempty candidate refs never expire
automatically: only explicit discard or user deletion relinquishes them.
After conversation retirement, keep each such candidate plus a minimal listable
closed child record naming all retained candidates, and minimal ancestor rows
needed to retain their tree membership. It says the conversation
was retired, accepts no follow-up, and remains adoptable and usable as spawn
inputs. Adopted and empty refs may be collected; their exact unpin intents stay
until cleanup succeeds. Unknown ignored/secret or uncertain checkout contents
prevent sealing, reclamation and retirement, regardless of either bound.
Bounds are soft where safe collection is unavailable or protected work remains.

## 7. Adoption

`adopt_candidate { candidate_id, expected_target?, evidence? }` lands a sealed
candidate into the **caller's own copy**, which is canonical for its subtree;
the root lands into the launch checkout. Integration is three-way, with the
candidate's recorded base as the merge base, against the *current* target
snapshot.

**Adoption is acceptance.** There is no `Accepted`/`Rejected` state anywhere.

Three rules govern every landing:

- **No checkout, no child mutation, no commit.** The child's work arrives in the
  caller's working tree as ordinary changes; new files arrive untracked.
- **The root checkout MUST NEVER receive a Plak-authored commit.** Children
  commit freely on `plak/<agent_id>` branches inside `.plak/`. Commit, push, and
  pull request happen only when the user instructs the root, which then uses the
  `git`, `gh`, or `jj` CLIs itself. **Plak ships no commit or PR tool.**
- **A conflict never destroys the only copy.** Conflicts stay in the retained
  child copy and are a failed result, never a success status.

Admission runs under the tree lock, the VCS landing runs unlocked under an
`adopting` workspace status, and the outcome is recorded on re-lock. A reopen
observing `adopting` is `recovery_required` and MUST NOT auto-retry.

Receipt: `{ candidate_id, target_before, target_after, files_changed }`, plus
`warning` when running processes still use the checkout. The warning is JSON
inside the stored receipt and replays verbatim, not a fresh observation.
The candidate durably records `merged_at`, the commit component of
`target_after`, alongside its adopter and evidence. `list_agents` exposes this
field and `plak session show` says `merged at <commit>`. On Git this is the
internal target snapshot, never a new commit on the root branch. A failed or
conflicted adoption records no merged-at commit. Cleanup does not erase this
provenance even when its worktree, branch and pin are removed.

### 7.1 Base identity and expectations

A snapshot's identity token is `<commit>:<content>`. `expected_target`, when
supplied, is compared against a fresh snapshot of the target and matches on the
full token, the commit alone, or the content alone. A mismatch is
`target_changed{current}` with nothing applied.

### 7.2 The git recipe

1. Snapshot the target. A failure here is `mint_unavailable`.
2. Check `expected_target`.
3. `git merge-tree --write-tree --merge-base=<base_commit> <target_commit> <candidate_commit>`.
4. `git diff --no-ext-diff --no-textconv --name-only <target_tree> <merged_tree>`
   gives `files_changed`. A zero count lands nothing and reports
   `target_after == target_before`.
5. `git diff --no-ext-diff --no-textconv --binary -M <target_tree> <merged_tree>`
   produces the patch; a repository-configured diff driver or textconv filter
   never runs and never alters the patch.
6. `git apply <patch>` in the caller's own path, the patch a `0600` file in a
   private directory — atomic by default,
   worktree-only, binary-safe, no commit.
7. Snapshot the target again for `target_after`. The child copy is untouched.

**Failure classification is typed, never collapsed.** `merge-tree` exiting
**1** is `merge_conflict{paths, child_path}`, the paths parsed from its
conflict report. Any **other** non-zero exit — a bad base object, a corrupt ref
— is `mint_unavailable` carrying a 240-byte stderr excerpt, so a broken
repository is never reported as a content conflict.

On the `git apply` leg, only stderr matching *"does not match index"*, *"patch
does not apply"*, or *"already exists"* yields `target_changed`: those mean the
worktree moved under the adopt. Anything else — modes, line endings,
permissions — is a typed VCS failure rather than a silent "target moved". That
closes the snapshot-to-apply race without taking a second snapshot.

### 7.3 The native JJ recipe

Adoption merges the immutable recorded base tree, sealed candidate tree, and
current target tree through `jj-lib`. It does not rebase or squash the child's
history, and it never includes edits made after sealing. Conflicts return
`merge_conflict` before any target checkout or transaction publication.

The target is snapshotted, the expected target checked, and then the working
copy is locked and re-snapshotted before publication. Working-file changes
refuse adoption; concurrent repository advancement requires reconciliation. Publication compares
the sole operation head under the standard JJ store lock, shared with external
JJ publishers; a stale prepared transaction remains unpublished.
A successful adoption rewrites the target working-copy commit and reparents
descendants without rebasing their trees. It publishes, checks out under the
retained working-copy lock, and saves the working-copy operation. Child files
and trees remain unchanged; their commit parentage may advance. The next
candidate uses the adopted candidate as its baseline, as on Git.

Publication, checkout, or state-save uncertainty returns
`workspace_recovery_required`, not a claim that nothing happened. There is no
operation-wide rollback that could erase an external concurrent operation.
In-process work retains execution ownership until it returns: expiring a
caller's deadline does not terminate or detach a repository mutation.

### 7.4 Authority and refusals

`may_adopt` is a grant field, true for the root. A node with `may_adopt` may
adopt any *sealed* candidate **in its own subtree**. That is what removes the
read-only-supervisor dead end without a special case.

Typed refusals: `not_sealed`, `not_authorized`, `candidate_unknown`,
`evidence_mismatch`, `workspace_referenced`, `target_changed{current}`,
`merge_conflict{paths, child_path}`, `mint_unavailable`, and
`adopt_not_applicable`.

`adopt_not_applicable` covers four cases:

- an already-adopted candidate;
- a node adopting its own candidate;
- a copy whose previous candidate was adopted by a **different** node —
  refused by name rather than silently dropping the earlier delta;
- a candidate over which an **unfinished child's copy was minted**, because the
  stale recovery that follows adoption would reset that child's uncommitted
  files.

### 7.5 Review evidence

`[orchestration] require_review_evidence` ships **off**. When it is on, an
adopt MUST pass `evidence` naming an attempt **in the caller's subtree whose
`output_json` contains the candidate id**; anything else is
`evidence_mismatch`.

`evidence` is recorded on the candidate and persisted either way — validated
and dropped would lose what stood behind a landing. Because each review node's
`output_json` links the candidate commit it reviewed, `list_agents` can show
"candidate X, reviewed by Y: reject" with no new state.

## 8. Wait and pushed completion

Completion is **pushed**, not only polled. A host-authored `result` message is
enqueued into the tree inbox from the seal, so it can name the candidate; an
inherit node seals nothing and pushes from turn completion instead. A forked
copy pushes from the seal so the mail can name the candidate. One
message per attempt, and only for a **terminal** attempt.

Its body is a contract, not prose — a JSON envelope of `purpose`, `from`,
`attempt_id`, `status`, `candidate` once sealed, and the first line of the
child's `output_json` bounded to 1 KiB. The payload itself stays on the wait
row and in the child's own transcript.

`purpose=result` MUST be refused from a model.

The envelope reaches a parent through `list_agents` `inbox[]` and through the
wait rows. Each inbox row carries up to 2048 bytes of `body` and, for a
result, its `attempt_id`, `status`, and `candidate`, so a parent with no
session delivery can adopt without parsing prose.

If the parent's inbox or the tree's queued-mail byte budget is **full**, the push is dropped and a **fault reason**
naming the attempt is recorded on the child; the wait row still carries the
result.

An attached interactive root host observes mail at its existing 200 ms tree
observation cadence. When `[turn] auto_continue` is true
(default), pending process notices or unread `result` or `question` envelopes addressed to the idle root
start one host-originated turn after a 2 s quiet window. Near-simultaneous
arrivals coalesce. No root turn, admission, or delivery may be in flight,
and no live queued user prompt may be about to drain. Findings, coordinate
mail, and replies never wake the root; they ride the next admission.

A submitted or queued user prompt always takes precedence. User interrupt or
cancel disables waking until the next user prompt. Recovery fences,
`recovery_blocked`, paused restored queues, and closing sessions forbid wakes.
A stale pause marker on an empty queue is not restored work; a new wake uses
ordinary live queue admission, including after a clean idle close and reopen.
There is no consecutive-wake limit: each wake requires a fresh external
completion or unread result/question. The consecutive count is informational;
wake suppression is an intent-backed product view (chapter 02 §2.6) that
survives reopen and rebuild, and the count is process-local.

Before binding a wake delivery, the host budgets the complete pending mail
batch and driver-inserted notice segment and uses ordinary scheduled compaction when needed, while no root submission lacks a feed fact. A failed or interrupted preflight retains unread mail
and pauses auto-continue until the next user prompt. An error writes an error
notice intent and a `wake_suppression` intent; a failed write keeps the
suppression in memory for the process.
A transient semantic-store refusal of a wake read (`StorageBusy`, or
`StorageUnavailable`, from the canonical context query or a feed read of the
tree or product projector) does neither: the observed batch stays, nothing is
suppressed or written, and the wake is retried after 1, 2, 4, 8 and 16 s. The
sixth consecutive refusal writes one warning notice naming the class (busy or
unavailable) and the wake is then retried every 60 s until the store answers;
a retried wake still prepares exactly one delivery. Every wake refusal is
logged at WARN under target `plak.wake` with its cause class (`busy`,
`unavailable`, or `refused` for a permanent refusal) and no session content.
A product store that refuses the wake's delivery submission as busy (another
connection holds its write lock past the busy timeout; nothing was written)
is retried the same way: the root batch recorded for that submission is
withdrawn with its mail unread, and the retry records a fresh one.
Integrity, ownership, compatibility and other product-store refusals keep the
pause until the next user prompt.
The host parks the wake and stays attached. Non-live trees are not reloaded
on idle ticks. It never repeatedly dispatches paid compaction for the same idle
wake. Prompt and wake preflight share one scheduled-compaction trigger and
budget the full canonical command, including selected mail.

Process notices can wake an interactive root without a tree runtime. Wake
receipts carry only triggering process ids (in command identity), never notice
text. The command identity joins every process trigger id, not just the first.
`wait_agent` resolves the caller's current process service at each poll and
subscribes to terminal-change notifications before polling it, so new services
and terminal edges wake promptly rather than relying on the fallback tick.
The driver inserts and consumes notices at ordinary admission. The fixed
prompt is: `Background processes finished or sub-agent results arrived while
you were idle; continue the current task, or reply briefly if nothing remains.`


Before admission a content-free system note names the waking process ids and child ids and
wire purposes, e.g. `auto-continue: c1 result, c2 question`. The
short fixed prompt is host-authored, never a transcript row, and stored as
`User` with `Instruction` provenance in canonical history, never as user
input. It asks the root to continue the current task or reply briefly if
nothing remains. The turn otherwise behaves normally, including Esc cancel.
`plak agent` JSONL keeps explicit-prompt behavior; no new JSONL line is
introduced. `plak run` continues until idle independently of `turn.auto_continue`. Other interactive hosts sharing this loop use the
same policy. An attached interactive root with waking enabled (it has
`spawn_agent` and no `submit_result`) and every child of the tree host (which
parks and wakes it, §5.2) may end a turn while children run. JSONL roots and
interactive roots with waking disabled must use `wait_agent` to learn results
before ending a turn.

The root's `turn.max_model_rounds` budget is per turn, not per orchestration
task or child lifetime. An orchestrating root with waking enabled should end
its turn while children run; child results wake it into a new turn with a fresh
budget. There is no automatic continuation of a budget-exhausted turn and no
budget reset from waiting in the same turn. Explicit-prompt JSONL roots and
interactive roots without waking retain the waiting rule above.

The next root admission (user or host-originated) includes unread envelopes
of every purpose in mailbox order,
within the existing prompt-byte bound. Before the product writes the root
command's submission (chapter 02 §2.6), the tree records the exact message batch
as an intent keyed by that command id; a resubmission reuses the stored batch
and bytes, never a reselected one. The tree projector acknowledges the batch
when it applies the root's `CommandAccepted` for that command id, and returns it
to unread when it applies a `CommandRejected` for it. Admission precedes the
model's first request, so a prompt the host knows is over the window (sent after
a scheduled pass that could not make it fit) carries no batch: its turn withholds
it, and its mail stays unread for the next admission. A prompt that fits only
without its batch is sent without it, and the batch stays unread (chapter 02
§4.2). A wake whose batch does not fit is not admitted; it pauses as a failed
preflight. When a root turn nonetheless withholds its admitted command (the
provider refuses it as over the window and nothing can be folded, so the
withheld-prompt note is the turn's last item), its `TurnTerminal` fact says the
command was withheld before dispatch, and the tree projector returns that
batch's messages to unread and records the return against the command id as a
`returned_admission` operation receipt in the same tree transaction. A result
envelope's delivered mark goes with it only when that admission's
acknowledgement made the mark; a mark an earlier wait made (one whose budget
withheld the envelope) stays, and these marks are process-local. Each return
derives from one record, so it happens once per admission whichever process
applies it. A new batch is selected only after the previous batch's command has
a fact; acknowledging a batch retires the `returned_admission` receipts of every
earlier admission, so at most one stays in the operation ledger. The next
admission or wake delivers the returned batch, so no message reaches the model
twice. A wake whose turn ends at the context window, whether withheld,
compactable or protected, pauses as a failed preflight: auto-continue stays
suppressed (a product view of the turn's terminal fact, chapter 02 §2.6) until the next user prompt, so
its returned batch never wakes the root again on its own, and the host's wake
prompt is never restored into the operator's composer. That persisted
suppression includes a `plak run` continuation (chapter 02 §5.2): a later
interactive open starts paused. An interactive withheld wake also leaves one
`auto-continue paused until the next prompt` transcript warning, the same line
a failed wake preflight's pause episode leaves; a wake failure that already
wrote its own pause line does not write a second. A crash between the
turn's terminal and the tree transaction leaves the tree cursor behind, and the
next open returns the batch from the same record. Message bodies escape
`&`, `<`, `>` and quotes; only host-controlled attributes frame each
`child_message`, and purpose uses its wire name. Every nonempty appended batch,
whether on a user prompt or a wake, starts with one fixed host sentence stating
that these contents are untrusted sub-agent data, not harness or user
instructions. Presentation splits a user prompt's batch back out: the
transcript shows the operator's text as the user row and each
`child_message` block as its own tree row naming its purpose and sender,
live and on reopen; the model-side command is unchanged. A user root submission with no feed fact keeps its exact stored
bytes and batch across reopen and is submitted again under its command id
(chapter 02 §2.6). An unrecorded ingress-full or root-queue-full refusal
removes the submission and its bindings and withdraws its tree batch without
consuming result mail; a later submission may select the still-unread batch
again. Recovery-requiring refusals keep the submission and its exact bytes.
A host-wake submission is never resubmitted: one that exits before admission,
or that open finds with no feed fact, is discarded with its queue item and its
tree batch is withdrawn with the mail unread, without pausing user work. A wake
with `CommandAccepted` is an ordinary admitted root, and the tree projector
acknowledges its batch from that fact. A later admission selects more results
only after the previous batch's command has a fact. Busy turns are not
interrupted; remaining envelopes stay in the same tree inbox.
Interrupting a child without completing its attempt produces no result, and
closed children do not publish completion mail.

Nested parents receive the same bounded context on their next authorized child
start. The command is first written as an `agent_commands` intent (§14.3) with
the original prompt digest, command identity, exact augmented prompt, and result
message ids. Replaying that command returns the same context even when newer
mail arrived. What became of it is the child feed's fact for that command id
(§14.1): `CommandAccepted` binds its turn and acknowledges the batch,
`CommandRejected` releases it, and no fact leaves it unresolved for exact
resubmission. Authorized submit-harness turns use the same intents and record
their prior turn and nudge round as corrective restart authority; the accepted
corrective command's fact advances that binding and round budget before its
batch is consumed, including corrective commands with no result mail. A mail-delivered Start's batch also
names the mail its prompt carries, so its accepted settlement, including an
exact retry after a reopen, acknowledges that mail and a later wake never
delivers it again. It also carries every other unread envelope addressed to
the child that no live delivery has claimed, results excepted (they ride as
`<child_result>` blocks), framed as a wake batch is: mail an interrupted child
received while nothing could deliver it rides its resume. An unresolved
batch refuses a different command of the same attempt until the original
command has a fact or is withdrawn; a wake or corrective command written for an
earlier attempt (its identity names the attempt) never blocks the current one.
`agent_commands` is capped at 4096 records per tree; exhaustion refuses admission and preserves the
inbox rather than evicting an unresolved command or silently losing results.

**Parked-child wake.** A parked child's wake (§5.2) is an `agent_commands`
intent like any child command, under the command identity
`plak:park-wake:{agent}:{host_generation}:{attempt}:{bound turns}`, admitted
with `Instruction` provenance on the live session. Its fixed instruction is:
`Work you were waiting on changed while you were idle. Continue the
assignment; call submit_result when it is done, or end the turn if only
waiting remains.` The driver inserts the
child's process notices at admission, as for the root. The batch is every
unread envelope addressed to the child that no live delivery has claimed, in
mailbox order, within the prompt bound: its parent's mail reads as the
assignment's `Message:` delivery, and every other envelope as an escaped
`<child_message>` block after the root's fixed untrusted-data sentence.
Its `CommandAccepted` acknowledges the batch, and reopen settles it from the
child's feed like any child command (§14.1). Mail sent to a parked child is never delivered live and never
claims a Start: its receipt disposition is `queued` (`steered` when urgent),
and it stays unread until the wake consumes it.

Persisted mailbox reconstruction preserves the numeric message sequence across
`m9`/`m10` boundaries; malformed stored message sequence identities are refused.

## 9. Interrupt, close, and resume

An **interrupt cancels this turn, nothing more.** The runtime reports a joined
cancel as a turn result whose outcome is `cancelled` as well as an error of
kind `cancelled`, and the host treats both as the cancel signal.

**A root interrupt also interrupts the running descendants.** When the root's
cancel settles (`Esc`, `Ctrl-C`, JSONL `cancel`, a `plak run` SIGINT), every
descendant with a running turn becomes `interrupted` exactly as `interrupt_agent`
leaves it, and its live turn is cancelled so it stops spending. Nothing is
closed and no attempt ends: mail resumes a child as below, and the next root
prompt can keep delegating. The transcript says how many children were stopped.
A child whose cancel does not release is a retirement failure, as for a close.

Hosted cancellation awaits descendant retirement on its existing async
executor; only a synchronous application edge drives that future with its
application executor. The interrupt marker never constructs a nested executor.

An interrupted child that was **not** closed keeps the node `interrupted` and
its attempt `running`, with no failure kind, no seal, and no pushed result. The
bound turn is released, which is what makes the node reusable, and its session
locator advances to the head the cancelled turn released so the next reopen
names it.

A parked child (§5.2) is interrupted the same way, by `interrupt_agent` or a
root interrupt: it becomes `interrupted`, its park ends, and its promoted
processes keep running.

Each target records an anchored interrupt intent before its consequence. The
intent captures its attempt and whether live ownership or a non-ending park
made the interrupt effective. Replay derives the same interrupted view even
without that process-local ownership; an ineffective request stays ineffective.
The applier changes no other agent's rows and never seals or sends result mail.

Close captures each target's attempt, recovery hold or terminal recovery
refusal, outstanding seal ownership, whether a live owner defers sealing,
and a fixed close stamp. Stop-all and host descendant closure use the same
per-target fold path, including calls without a tool operation id. Replay
consults none of that process-local state and never dispatches a second seal.
A close whose seal is still pending at its fold position derives `sealing`
from that anchored reservation, including when the close itself reserved it.
Its orphan fault therefore rebuilds identically after view deletion.

**The resume is a new turn on the same attempt**, started by any mail — an
urgent steer or a `coordinate`. There is no resume verb. A `followup` instead
supersedes the interrupted attempt (section 5.3). Mail sent while the
cancelled turn is still unwinding finds that turn ended; it is neither a
delivery failure nor left unread, since an interrupted child has no park and
no wake. The send waits until the unwind releases the turn (and its live
slot), then delivers the same message again as the resume Start; once that
Start is admitted, the mail's disposition and the send's receipt become
`started`. A cancelled turn the attempt still binds when no live slot holds
it and no turn waiter in this process will release it is not waited for: the
delivery fails closed at once, the send answers `operation_recovery_required`
and the child becomes `recovery_required` (§14.2). A send already waiting for
the unwind fails closed the same way at its next look once the turn's waiter
has ended without releasing the turn. A turn's waiter is registered under
the tree lock that binds the turn (or, for a turn bound before, before its
live slot becomes visible), so a turn still being started never reads as one
no waiter will release; a Start that marked the child active but has not
bound its turn yet (an interrupt can land in between) is waited for under the
budget like any Start in flight. A resume Start that is not admitted refuses the send, and its
receipt becomes that refusal, so a replay is refused without delivering
again. One 60 s budget bounds the whole send, however often the Start finds
the slot still being started or released; it binds the send that holds the
resume reservation too, which checks it before every renewal. An unwind that
does not release the turn in time, a resume Start not admitted in time, or an
attempt that ends meanwhile, refuses the send `recipient_attempt_terminal`
with the mail settled read and the reservation released. The restart is
exclusive: of several sends waiting on the same unwind, the first to see the
release reserves the resume Start, and the others wait for it, then deliver
their mail into the resumed turn as a follow-up (if that Start is not
admitted, the next one takes the restart). Their mail was claimed while the
resume Start was composed, so that Start never carried it; only a follow-up
that finds the resumed turn already ended leaves it unread, for that turn's
waiter to park or wake the child with it. A sender cancelled while its send
waits gets a `cancelled` tool result: ordinary mail stays unread under its
original receipt. For an opened `followup`, cancellation or SlotBusy expiry
before runtime submission takes the guarded withdrawal in §14.3; its mail is
settled read and its receipt becomes a `recipient_attempt_terminal` refusal,
so a replay is refused too. A refused withdrawal retains the started attempt
or holds an unbound opening for recovery before settling the mail.
Cancellation during submission with an unknown outcome instead retains the
opening and unread mail under a recovery hold (§14.3). The budget never
expires a submission in flight: the send awaits the delivery's own bounded
reopen and admission, so a Start admitted after the budget is delivered late,
and its attempt keeps running and can still submit.
The resume Start carries any other mail
still unread and unclaimed (§8).

A turn cancelled by the provider or the server, with no host interrupt, is not
an interrupt: it falls through to a `{ kind: "cancelled" }` attempt failure
rather than stranding the node `running`.

`close_agent` ends the node **and its descendants** permanently. Closed rows
stay inspectable in `list_agents`. Closing a producer preserves the candidates
it sealed. Closing the last open child leaves an idle tree, not a stopped one:
the epoch stays `live` and the root may spawn another wave. Only the operator's
`stop_all` stops delegation for the current root turn (`tree_terminal`); spawns
then refuse `grant_denied`. The next explicit root prompt restores `live` only
after pending spawns and active descendants have retired. A child that requires
recovery runs nothing, so it does not hold the stop; the prompt leaves its
recovery obligation where it was. This retains the frozen roster, closed rows, and live node counts; it does
not start a new epoch or reopen children. A rebind or roster apply remains an
explicit successor-epoch operation.

`interrupt_agent` and `close_agent` both accept `expected_attempt_id` and
refuse `attempt_mismatch` when it is not the child's current attempt.

A `recovery_required` child with an unresolved session outcome is not closed
by `close_agent` and cannot be re-tasked; `recover_agent` (section 4.5a) or the
automatic pass settles it as section 14.2 describes: `failed` and idle, after
which `followup` and `close_agent` work as for any failed attempt; `completed`
for a retirement-only quiescent reopen with submitted output; or `closed`, with
no result mail, when the parent had already closed it.

## 10. Capacity, quota, and usage

### 10.1 Refusal precedence

The **direct-child budget** is checked first. A settled child's slot is
released once its row is delivered and it has no active turn, without closing
the node or discarding its candidate. Undelivered finished children still
hold slots; `spawn_budget` lists them and their statuses. Closing also frees
the slot while preserving the inspectable row and adoptable candidate. A
`recovery_required` child runs nothing and holds no direct-child slot. Every
explicitly abandoned recovery obligation leaves its node closed and also holds
no direct-child slot; no runtime or VCS work is started by that transition. Every
settled, delivered child is **dormant** and also releases its tree-wide slot.
Dormancy is derived from settlement and delivery, not a second persisted node
status. Followup reactivates both slots and refuses `spawn_budget` if doing so
would exceed either bound. The attempt budget counts every
attempt except those whose failure kind is `turn_abandoned`.

Running-children saturation is the typed `capacity` error carrying the
**running children**, so the caller has ids to interrupt or wait on. **There
is no queue.** The bound counts executing children only — the root's own turn
is free — and because `MAX_DIRECT_CHILDREN` and `MAX_RUNNING_CHILDREN` are
both 8, `capacity` is reachable only across several hubs, never from one
parent alone.

| Bound | Value | Configurable |
| --- | --- | --- |
| `MAX_TREE_DEPTH` | 4 | `[orchestration] max_depth`, 1–4 |
| `MAX_DIRECT_CHILDREN` | 8 | `[orchestration] max_children`, 1–8 |
| `MAX_TREE_NODES` | 32 live nodes, root included | `[orchestration] max_nodes`, 1–32 |
| `MAX_RUNNING_CHILDREN` | 8 | — |
| `MAX_PENDING_SPAWNS` | 16 | — |
| `MAX_TREE_ATTEMPTS` | 64 per root prompt | `[orchestration] max_attempts`, 1–64 |
| per-cell agents | 8 | — (fixed) |

Every configured bound is a closed `1..=ceiling` range: a value outside it is a
load or `config set` error naming the key, never a silent clamp, and a raise
through the tree API is ignored. The epoch finalize clamps remain defense in
depth. `max_attempts` is carried in the epoch digest.

Both tree budgets are active counts, not lifetime totals. The node budget counts
the root, active or undelivered children, and `recovery_required` nodes; dormant
children keep their identity and candidates without consuming capacity.
The attempt budget counts attempts opened since the root's
current prompt began, and every root prompt starts a fresh count.

### 10.2 Blocked and failed attempts

The hosted transient-continuation classifier uses only canonical model-error
kind, phase and retryability. Transport, timeout, unavailable, incomplete-response
and short retryable rate limits qualify; unknown/permanent dispositions and
possible provider-hosted external effects never qualify. Duplicate-inference or
billing risk may qualify because continuation is a new turn, not request replay.
Connect failures and transport failures during authentication wait for exact-route
reachability; other qualifying failures honor retry advice clamped to 1-60 seconds
(30 seconds without advice). A model without a probe uses that fixed fallback.
Reachability is shared by route, uses credential-free probes with exponential
5-60 second backoff, and has no wall-clock wait cap. Interrupt and close cancel
one waiter; the final subscriber cancels the probe task.

Failed child diagnostics include the canonical model error's operation phase,
HTTP status and provider code when present, followed by its sanitized message
and physical-attempt summary. These details are terminal reads, not additional
tree-store authority; replay derives the same diagnostic from the same record.

A template's `max_turns` ends the attempt `blocked` with whatever partial
output exists rather than granting another round.

A provider **quota exhaustion** marks the **attempt**
`blocked { quota, account, resets_at? }` and the **node** `failed` — a blocked
attempt has no node status of its own. A `rate_limited` disposition blocks only
when it is non-retryable or its `retry_after` is at least the 5-minute
threshold; a short retryable 429 is an ordinary attempt failure
`{ kind: "rate_limited", message }` and does not park the tree.

Before a qualifying failed terminal is projected, the host persists a Resume
decision. The failed turn remains canonical; its attempt stays running and its
jobs are not stopped while the continuation waits. The home-scoped
`orchestration.transient_resumes` budget is 0-2, default 1, per attempt, and
`max_turns` must still admit a turn. The continuation is a new host-instruction
turn on the same assignment and token, never a replay; its stable identity is
`plak:resume-transient:{agent}:{attempt}:{round}`. Refusal or exhaustion preserves
the original failure and seals partial work normally. No other account's quota
is spent without explicit user instruction. Results are durable, so the tree
holds while the user is away.

### 10.3 Usage

Model-visible rows carry per-attempt usage only: `elapsed_ms`, `turns`,
`tool_calls`, `input_tokens`, `output_tokens`. Stamps are milliseconds and
`elapsed_ms = ended_at − started_at`.

**Cost, quota balances, and subtree rollups MUST NOT appear in any prompt.**
They belong to the TUI and to the telemetry sink, which is never recovery
authority. The context-occupancy meter is untouched by any of this.

## 11. Code Mode cells and the per-cell budget

Code Mode cells — native, JavaScript, and Python alike — are the pipeline
layer. Topology tools appear as **nested** names inside the cell when granted;
the outer markers stay `exec`/`wait`, `exec_js`/`wait_js`, or `exec_py`/`wait_py`.

A cell carries its own agent budget, enforced host-side at one dispatch choke
point shared by all three interpreters. The key is
`(caller_id, evaluation_operation_id, interpreter_generation, cell_id)`. The
per-nested-request fields — request sequence, child call id, depth — are
deliberately excluded because they vary within one cell, and `caller_id` is
included so a root cell and a child cell at the same operation id do not share
a counter.

The per-cell agent budget is fixed at 8 and is not a config key. Like
every other bound in this section it is frozen on the tree's epoch snapshot at
install, so a tree persisted before the field existed reopens at the ceiling
until its next epoch rather than at whatever the live config says — there are
no migrations, and a running tree's ceiling never moves under it. What is
charged is only work that creates a new agent: `spawn_agent`, and
`send_agent_message` with `purpose=followup`. Ordinary mail, `wait_agent`,
`list_agents`, `close_agent`, `recover_agent`, `interrupt_agent`,
`submit_result`, `adopt_candidate`, and `discard_workspace` are free.

Admission charges the budget **before** `request_key` dedup and **before** the
capacity check, so a cell cannot spend the tree's running slots probing past
its own bound. Exhaustion is an ordinary tool failure:

```json
{"code":"cell_budget_exhausted","message":"…","cell_id":"…","budget":8,"used":8}
```

so an exhausted cell can still wait on, message, and close what it started. A
poisoned budget lock reports exhausted rather than silently unbounding the
cell. Per-tree budgets and per-tree cell keys evict LRU past 64 trees and 512
cells.

**No Plakous L0 file counts topology names.** The budget is host-side, and no
journal becomes recovery authority. A replay journal would be authoritative
only for nested *return values*, while `request_key` is what dedups topology
*effects*; neither substitutes for the other.

## 12. The copy shim

Path tools are contained by the workspace policy, but process tools start a
shell **inside** the copy, and that shell could otherwise run
`git -C <parent>` or `cd <parent> && git`. Plak therefore writes a basename
shim to `<copy>/.plak/bin/git`, and to `<copy>/.plak/bin/jj` for a jj copy,
and the hosted workspace constructor supplies that workspace-relative directory
as an explicit required process prefix to the core environment. Core validates
and prepends it without discovering product paths. The required directory's
canonical path and opened filesystem identity are revalidated before every
process spawn; disappearance or replacement fails closed. Occupancy policy and
known copy placement require this prefix even when its shim files disappeared
before environment construction; construction then refuses the missing shim.
A login shell
re-exports the prefix after profile scripts run, preserving the profile's own
base PATH. This does not freeze profile execution or turn the basename shim
into hostile-content containment. The real executable is discovered by
walking `PATH` while skipping any `.plak/bin` entry, with fixed fallbacks. The
shim is published by writing a temporary sibling and renaming it, so a
concurrent fork holding the write descriptor cannot make the child's exec fail.

The git shim refuses, with exit 128:

- `core.worktree` set through `GIT_CONFIG_PARAMETERS`, `-c`, or `--config-env`;
- `git init` and `git init-db`;
- `git clone` whose destination resolves to the copy root;
- `git worktree add`;
- an effective working tree — cwd after `-C`, overridden by `--work-tree` or
  `GIT_WORK_TREE` — outside the copy;
- a `--git-dir` or `GIT_DIR` that is not this copy's git directory.

The jj shim refuses, with exit 1: `jj init`, `jj git init`, `jj git clone`,
`jj workspace add|forget|rename`, a `-R`/`--repository` outside the copy, and a
cwd outside the copy.

Repo creation inside a copy is refused because a child re-initialising its copy
turns it into a standalone repo the launch checkout cannot see, and its seal
then faults.

**What the shim cannot enforce.** It is basename-only. An absolute
`/usr/bin/git`, a `PATH=/usr/bin git`, a `git worktree add` invoked outside the
copy, and a hostile overwrite of the shim itself are all residual. Isolation
remains host-level; the shim is a guard rail, not a boundary.

## 13. `on_mint`

A template may declare an `on_mint` hook and an `[env]` block, both empty by
default. The hook runs **once per fresh, non-replayed spawn**, after the copy
is minted and **after the tree lock drops**, with cwd the new copy.

It is handed to `sh -c <command>`. Command text comes from a template the user
wrote in their own repository, so shell injection through it is by design.

The hook runs with a **cleared environment** plus `PATH` and `HOME` read from
the host and the template's own `[env]`. The tree then supplies three values,
overriding any template entries of the same name:

- `PLAK_LAUNCH_CHECKOUT`: the tree's original launch checkout path.
- `PLAK_PARENT_PATH`: the spawning parent's workspace path, which may be a copy;
  captured when the workspace is planned, not looked up after minting.
- `PLAK_COPY_ID`: the new copy's tree-scoped VCS identity, not its logical agent id.

No other ambient host variables cross: the API-key store reads `*_API_KEY`
from the environment when set, and a template-authored shell command has no
business seeing the user's keys. A hook that needs more declares it in its own
`[env]`. Neither `[env]` nor these hook context values are passed to the copy's
tool calls.

The hook is spawned in its **own process group** with a 300 s ceiling, and the
timeout signals the group, so a shell's own children are not orphaned holding a
build lock. Output is captured to a scratch file so a chatty hook cannot
block on a full pipe; the last 2048 bytes are quoted on failure.

A non-zero exit or a timeout is `mint_hook_failed`: copy removal is queued,
the node is left `failed`, and the operation record replays the refusal.
Removal runs only after its `discarding` reservation is durably published.
An unpublished reservation or a failed removal retains the copy and marks the
node `recovery_required`; neither case claims cleanup succeeded.

Plak learns no ecosystem build caches and provides no package-manager
knowledge. Sharing a build cache or priming a copy is the user's hook to write.

## 14. Persistence, reopen, and recovery

Product state lives in `product.sqlite3` at **schema 32**: intents and
feed-derived views (chapter 02 §2.6). Its `submissions` bind queued commands to
their exact bytes; the result-mail batch a root command carries is a tree
intent. The tree lives separately in `subagents.sqlite3` at **schema 6**, owned
by `plakous-recursive-subagents-store`. Its ten domain tables are below; §14.3
classifies every column family as an intent, a view of the feed, or an
external-effect record.

Tree-store transaction begin and commit failures report the static phase and
SQLite category, such as `disk_full` or `database_busy`, without backend text.
A failed commit remains ambiguous until exact durable state is reconciled; an
operator must not repair it by guessing a new tree locator.


| Table | Key | Holds |
| --- | --- | --- |
| `agent_trees` | `tree_id` | root agent, revision, host generation, admission state, `writer_mode` (`single_lease` only), selected tuple, launch checkout, the frozen orchestration snapshot |
| `agent_nodes` | `(tree_id, agent_id)` | ancestry, depth ≤ 4, brief, template, bind tuple, `NodeStatus`, workspace id, tier, current `attempt_id`, session locator, path, branch, active-turn flag, nullable INTEGER `fold_order` view position |
| `agent_grants` | `(tree_id, agent_id)` | `may_spawn`, `may_adopt`, depth and node and child ceilings, and the visible / peer / managed / interrupt / close id sets |
| `agent_operations` | `(tree_id, operation_id)` | the canonical request, its digest, the terminal code and response for replay, and paired nullable INTEGER `fold_anchor` / `fold_order` for host fold intents |
| `agent_messages` | `(tree_id, message_id)` | body, `purpose` (including `followup` and the host-authored `result`), priority, reply link, disposition, durable state |
| `agent_attempts` | `(tree_id, attempt_id)` | `AttemptStatus`, turn token and plain turn id, `output_json`, the 0–2 harness nudge count, `candidate_id`, `retry_of`, `request_key`, start and end stamps, the blocked reason, the failure `{kind, message}`, and the five usage counters |
| `agent_candidates` | `(tree_id, candidate_id)` | workspace, producing agent and attempt, base commit / content / change id, candidate commit / content / change id, seal stamp, adopter, merged-at target commit, evidence |
| `agent_workspaces` | `(tree_id, workspace_id)` | owner, path, `mode` (`inherit`, `snapshot`, `fork`), `status` (`active`, `adopting`, `discarding`, `clean_removed`, `adopted`, `discarded`), base identity, current `candidate_id`, generation, and the in-flight adopter (`adopter_agent_id`, set only while `adopting`) |
| `agent_commands` | `(tree_id, command_id)` | every runtime command the tree prepares — a root admission's result-mail batch, a child Start, wake, mail Start, followup or corrective round — with its recipient, attempt, kind, exact canonical text and message ids, all immutable; the tree's withdrawal of a command no fact settled; and, as a view, the fact the recipient's feed recorded for it (`accepted`, `rejected`, or `returned` by a withheld terminal) with the plain turn id it bound and nullable INTEGER `accepted_seq` |
| `agent_feed_cursors` | `(tree_id, runtime_session_id)` | the last feed sequence the tree projector applied for that session, root or child |

One in-flight status per row: a node seals, a workspace adopts, and neither
status appears on the other.

**There is no migration.** A schema change moves the product's state directory
(chapter 02 §2.5); an older store stays in its own directory and is never read. The store fails closed on a **DDL
fingerprint**, not the version integer alone, so a store written by an earlier
build of the same version number is refused too. The refusal message still
reads as an ordinary version mismatch rather than naming the fingerprint drift.

### 14.1 Reopen

A pending child Resume is re-derived from its immutable decision and the exact
canonical failed terminal, including when death occurred before that terminal
was projected into the tree. No saved decision means conservative ordinary
failure settlement. The new owner re-enters the same gate and retries the same
`plak:resume-transient:{agent}:{attempt}:{round}` identity. A prepared exact
Resume keeps its original prompt/mail batch rather than being withdrawn like a
generation-dependent corrective command. Accepted commands are never replayed.
The reopened gate holds a cancellable hub startup reservation, not the global
reattach lock; interruption, close and hub shutdown release the session.
While waiting for reachability, live list/wait `waiting_on` includes
`network: true`, and the host tree view includes `network`. This label is not
persisted recovery authority.

A root or child, including the API lane, reads its own exact stored evaluator
artifact and process bounds before compiling its runtime attachment. Current
installation siblings are construction inputs for new sessions, not reopen
authority. Child reopen uses the child's locator, never its parent's artifact;
new children explicitly clear any parent reopen pin. A reopen follows chapter
02 §4.2's order: the tree projector applies the child's feed to its tail
without a lease, the host reads the construction facts of the head that feed
names and opens it, and a stale refusal (another owner committed in between)
repeats the feed pass once; a second stale answer is `recovery_lease_held`,
never `recovery_required`. No artifact lookup advances or repairs a
checkpoint.

A restored tree opens `restored_paused` and does not dispatch until the user
resumes it; a tool call meanwhile is `tree_restored_paused`. Root rebind
follows the settled-tree predicate.
`paused` is a presentation overlay on a folded `running` node, accessed through
`NodeRecord::presentation_status`; restore and resume change tree admission,
not child statuses. Interrupted, sealing and terminal statuses are never hidden.

An unresolved seal reservation, or a workspace row observed `adopting`, is
`recovery_required` and MUST NOT auto-retry: the subprocess may have completed.
Recovered ambiguity is an explicit obligation, never a blind replay. A reopen
records anchored `vcs_fault` intents for a pending seal, the occupants of an
`adopting` or `discarding` row that are not closed, and the adopter of an
`adopting` row unless it is closed: the
landing writes the adopter's checkout however the producer ended. When an
adopt begins, the row records its caller (`adopter_agent_id`), and the record
is cleared when the row leaves `adopting`; an ancestor's adopt of a
grandchild's candidate therefore faults the ancestor, never the hub between
them that owns the copy. A row without the record keeps the owner rule. A
closed node was settled by `close_agent`, the remedy, and is never faulted
again. Recording faults writes no node view: each affected agent folds its
own fault at its persisted cursor, after earlier Finalize or cancellation
intents. A seal without a cursor uses its reservation anchor; a workspace
without a cursor uses the agent's greatest recorded fold anchor, or 0.
A removal never touches its owner's checkout, so a
`discarding` row whose occupants are all closed faults no one.
The root is never `recovery_required`: nothing can recover it, so an adopt
landing into the root is recorded as the root's fault reason instead, and a
root persisted `recovery_required` reopens `running`.

A child that was mid-turn when its parent's process was lost is reopened on
reattach, and that open abandons the turn the lost process left (Plakous
§13.1). The attempt then ends `failed { kind: "turn_abandoned" }` with the
abandonment's operator line as its message, the node is `failed` and idle, its
partial work is sealed like any failed attempt's, and one `result` mail goes to
the parent. A child whose own turn a live owner fenced is recovered the same
way at once, without waiting for the parent: its session is released and
reopened as soon as the turn settles. Only a reopen that cannot abandon — the
session cannot be opened, or its head is some other unresolved shape — leaves
the node `recovery_required`; that status remains only for such heads and
for seal, adopt and retirement ambiguity.

A child the previous process left parked (§5.2) is recognised from durable
rows alone: a `running` node whose current attempt is
open and unsubmitted, with no bound turn, no active turn, a session, and at
least one turn already spent or a child command intent (a retained-head
seed may no longer observe earlier turns). An unstarted spawn or fresh
followup with neither has no parked work; an interrupted node is `interrupted`.
Reattach, which runs after a
`restored_paused` tree resumes, reopens each one and admits one wake (§8):
processes stopped at release come back as terminal notices in that turn, and
its end parks, nudges or settles as usual. A reopen that abandons a turn or
cannot open the session settles as for an in-flight child, its await receipts
settled from the abandoned head's committed history. No wake runs while the
tree is `restored_paused`. Resuming the tree holds each such child for its
wake until reattach has run it. While held, its rows carry the empty
`waiting_on` parked marker and mail sent to the child waits for that wake (it
never starts a second turn beside it). An interrupt or close ends the hold:
the child becomes `interrupted` (or closed) and no wake or rejoin runs for it.
The reopen wake moves the hold to an admission state before any Start or
bind, and its reservation, bind and rejoin each re-check it under the live
lock, so an interrupt that lands meanwhile refuses the Start (or cancels the
rejoined turn) instead of being written back to `running`; the session is
released and any batch prepared for a Start that was refused before the
runtime admitted it is settled unread. A turn the runtime had already
admitted carried its batch into history: a rejoined turn that is cancelled,
and a reopen wake whose bind is refused after admission, settle that batch
accepted, so the resume does not deliver it again. The parked
set, the hold and `waiting_on` are process-local.

Before that wake dispatches, the tree projector applies the child's feed to its
tail (§14.3), which settles the child's await receipts (§4.4), and a spent
`max_turns` ends the attempt `blocked` as the live path does, its subtree closed
and processes stopped. Every child command is an `agent_commands` intent written
before submission, and what became of it is the child feed's
`CommandAccepted` or `CommandRejected` fact with that command id:

- A command with `CommandAccepted` binds the attempt it names to the fact's turn
  id; the binding is a view, so a turn admitted before a process loss is bound
  however far the lost process got. Its batch is acknowledged. If that turn has
  no `TurnTerminal` yet, it is still running when the child reopens, and it is
  rejoined, never left `recovery_required`. A mail Start that reopens such a
  child joins that turn and acknowledges its own mail only when the accepted
  command is its own Start; a turn another command admitted (a lost wake,
  another mail's Start) is joined all the same, and the send delivers its mail
  into it as a follow-up (§4.3).
- A command with `CommandRejected` is settled rejected: its mail returns to
  unread and nothing is retried.
- The live submission site settles the same outcome from its acknowledgement,
  so the live host never waits on a feed pass: an admitted command takes the
  accepted fact (the later pass over its record changes nothing), and a
  command the runtime definitely refused before admitting it, which no fact
  will name, is withdrawn with its mail unread. An accepted command binds no
  turn onto a node an interrupt or close settled; its batch, already in
  history, is still acknowledged.
- A command with neither fact was never recorded. A single unrecorded command
  of the current attempt — a wake, a mail Start, a followup — wins over a new
  wake: it is submitted again exactly, under its own command id and text, so
  nothing starts twice. Several unrecorded commands have no single exact retry
  and are withdrawn (their mail unread) for one fresh wake. A command whose id
  names an earlier attempt is never submitted onto the current one; it is
  withdrawn. An unrecorded corrective round is withdrawn and its round is
  re-derived from the attempt's state.

A tree cursor that reached a child's tail through a `BehindRetention` rebuild
proves nothing below `feed_retained_from`: an `agent_commands` intent with no
fact is then submitted again under its identity. For a `Duplicate`, the host
projects the child feed to its tail and settles from an acceptance present in
the tree views. If no acceptance is present and `feed_retained_from` is positive,
the command predates the retained feed and settles as consumed. An absent
acceptance with `feed_retained_from == 0` is an integrity refusal. A `Reclaimed`
refusal also settles as consumed, its batch acknowledged and nothing derived
from it. This rule does not depend on an acknowledgement's `feed_seq`, which
may be absent on a reopened receipt. Both the
Start and the parked-wake submission sites apply this; a reopen wake whose
feed pass went behind retention withdraws none of the several unresolved
commands, and submits their ordered retry queue on one reopened attachment,
continuing past consumed acknowledgements before starting a fresh wake. The pass
writes keyed `agent_operations` intents of kind `behind_retention_command` for the
unresolved command identities, atomically with its cursor. These markers survive
cursor catch-up and a process loss between retries: each remaining marked
command still wins over a fresh wake and is never withdrawn merely because the
next pass is no longer behind retention. A consumed command settles only its
own batch; it does not erase the other commands' markers. The snapshot rows
are intents, not views: `clear_feed_views` and feed replay preserve them.
A command's settlement removes its own marker, never another command's.
A rebuild that again falls behind retention takes a new snapshot of unresolved
identities and resubmits consumed commands under the same identities; their
duplicate acknowledgements reproduce the same consumed settlement and mail
acknowledgement without running them again.
A fresh wake skips any identity already settled consumed; this chooses a new
command id, not a spent turn. The hold and interrupt checks remain in force
between consumed acknowledgements and the next submission.

A turn binds to at most one attempt. An accepted command whose attempt already
binds another turn fails closed: the binding is untouched, the recovered turn is
cancelled and released, and the operation is `operation_recovery_required`,
leaving the child `recovery_required` (§14.2). The send itself never binds a
turn or marks one active; active-turn is the view "bound turn with no
`TurnTerminal`", so a turn that ended or was replaced before the delivery
returned is never marked active again. A running turn that no tree command
admitted is left to recovery.

### 14.2 Settling `recovery_required`

`ProductAgentTree::apply_host_recovery`, called only by the tree
projector's application of a child's feed records (§14.3) and by the explicit
settlements below, is the only place that settles a **recovered** unresolved
session failure (`turn_recovery_required`, `session_retirement`, or a startup
or materialization failure) to a terminal outcome. The ordered session-binding
intent can instead clear its exact materialization failure and continue running
(§14.3); close, seal completion, adoption and replayed binding never clear it.
Terminal recovery ends the current attempt `failed` with kind
`turn_abandoned` (the reopen abandoned the lost turn, message its operator line),
`recovered_without_turn` (the head was quiescent and no result exists), or
`spawn_refused` (the startup never produced a session; see the replay below).
One exception preserves result authority: a quiescent reopen of retirement-only
uncertainty with a durably submitted result settles `completed`, retaining that
result and clearing the retirement failure. An actually abandoned turn, or a
recorded authoritative terminal failure, still settles failed even with submitted
output. Close intent takes precedence over either outcome: any unresolved session
failure makes the node `recovery_required`, including a fault after close. Only
the attempt retains close intent as `closed`, through both successful and failed
completion of an already dispatched seal. This keeps the child visible to
`list_agents`, recovery and the session-delete refusal (chapter 02 section 2.5.2).
Recovery settles both node and attempt `closed`, without pushing result mail
either while recovery is pending or after settlement. Already recorded
output stays inspectable; recovery does not reopen the node or replace its
original terminal timestamp. No result is manufactured. The first
settlement of an attempt wins: a waiter that already recorded the abandonment,
or a second recovery of the same attempt, finds nothing left to settle and
returns the node as it stands with `recovered: false`.

Host decisions use per-kind guards, not a blanket terminal-attempt guard:

| Kind | Attempt guard and consequence |
| --- | --- |
| Cancelled or Materialization | Refuses a submitted decision on a `recovery_required` node before recording it; leaves an attempt stopped by another consequence unchanged at join time |
| Retirement | Annotates the unresolved owner-release obligation, sets the node `recovery_required`, preserves the attempt's already stopped status and combines its diagnostic |
| Retirement after a settled seal | The anchored `seal_outcome` and unresolved session obligation derive a `recovery_required` node at their fold position; the attempt's `closed` status remains `closed` |
| Host turn failure | Leaves another settled outcome unchanged; an unresolved session outcome still annotates recovery after close, preserving the close intent and combining its diagnostic |
| Host recovery | Settles the unresolved session obligation, but leaves an otherwise settled or adopted attempt unchanged |
| Session bound after failure | Clears only the named materialization decision's exact failure, status and end stamp; never clears an owner-release or VCS-fault obligation |
| Finalize | Uses its stored terminal evidence and turn-ceiling decision; leaves another stopped outcome unchanged and supersedes Park or Nudge for its exact ended turn |
| Park or Nudge | Derives no terminal attempt outcome; cannot override Finalize |

A retirement annotation joins in the same order as other host intents and is
idempotent. Clearing it removes only its derived diagnostic and node obligation;
it never clears an attempt's `closed` or completed outcome.

Explicit `plak session delete --abandon-children` is a different transition.
After confirmation and runtime archival fences, the product closes every
recovery-required child obligation in its own tree, descendants included, by
`ProductAgentTree::abandon_recovery_for_delete`. The existing attempt failure
record carries kind `recovery_abandoned` and a bounded message naming the
operator action and that recovery will not run; its existing agent and attempt
identities name who was abandoned and which attempt. The attempt becomes failed
and the node closed. No result is invented, no session is reopened, and no seal,
adopt or workspace operation is replayed. An interrupted deletion preserves the
fact across restart; recovery and late failure reports cannot replace it. This
does not use `apply_host_recovery` and never claims successful recovery.
Unfinished workspace records remain facts until deletion removes the tree.
An anchored `recovery_abandoned` intent captures the attempt, bounded diagnostic
and abandonment stamp. Its agent-local fold derives the failure and closed node,
including after view deletion, without consulting the clock or live recovery state.

When nothing else holds the node it becomes `failed` and idle and its partial
work seals like any failed attempt's. A seal that failed for the attempt
while its session failure kind was retained (section 6.5) is begun once more,
and a second failure records `seal_failed`. A parent receives at most one
`result` mail per attempt: settlement pushes the settled result only when the
attempt's failure had not already pushed one, and the outcome also reaches the
caller through the `recover_agent` receipt or the pass note below. The host
builds that receipt from the authoritative tree after all queued effects have
been recorded and published, not from the intermediate `sealing` state. Both
resolution and the final receipt are fenced to the recovered attempt: if a
replacement attempt became current, recovery returns `attempt_mismatch`
rather than describing the replacement. A successful seal leaves no
`workspace_recovery` blocker. A seal refusal is reported as `mint_unavailable`
with the seal diagnostic and records `seal_failed`; after restart its persisted
failure message supplies that diagnostic, with the inspect/close remedy. It is
not an interrupted
VCS outcome and is not reported as `recovery_vcs_ambiguous`. A
settlement that abandoned a turn leaves the same fact on the node that any
abandoning child reopen leaves, so the caller's face says once what the cut
may have done (for a running tool: that it may have changed the workspace).
The warning distinguishes the unobserved conversation result, which is not
kept or replayed, from workspace changes, which remain in the child's
workspace or candidate for review; recovery does not erase those changes.

The session part is settled first. A node whose seal, adopt, or copy removal
was also interrupted has its session settled the same way, but its node status,
fault, seal state and copy are left untouched: it stays `recovery_required`,
the receipt's `workspace_recovery` names the remaining blocker, and
`close_agent` ends the node once the operator has inspected the copy.

The host settles a child by reopening its session exactly (§14.1). A head that
restores a resumable working turn is cancelled and released, never rejoined,
and settles as `turn_abandoned`. A head that requires effect evidence is
refused `recovery_effect_evidence_refused`: the product holds no
executor-owned effect evidence to finalize it with. A startup that never
published a session locator is settled by replaying the spawn's own
deterministic create (`plak:spawn:<tree>:<agent>`): it returns the session that
create already made, or makes the one the spawn was admitted to make, which is
bound and then reopened. A create whose session already moved past genesis
cannot be named this way and is refused `recovery_session_unavailable`.
Replay inspects the exact accepted create before compiling a child binding.
Its deterministic workflow identities and immutable stored genesis reconstruct
the original create request, which the store's exact receipt recovery authenticates.
An accepted create uses that child's recorded evaluator, job budgets and output
bound, not the current sidecar or the parent's reopen pin. No accepted state is
rewritten. Only an absent deterministic session selects a genuinely new child
binding, which clears inherited reopen pins and uses current construction inputs.
If no session was ever created, its fresh create remains ordinary work, not
reserve-consuming recovery work, and may be refused for capacity; that refusal,
like storage pressure while reopening or publishing already-owned recovery
state, is `recovery_storage_unavailable`. When that fresh create, or the child
binding it needs, is refused for any other reason and the exact accepted-create
lookup still finds no session, the spawn never produced a session (the
binding's refusals are not told apart, so a transient account-readiness or
catalog failure while binding settles too, as it refuses the spawn outright at
spawn time): nothing ran and nothing is left to reopen, so recovery settles the
attempt `failed` with kind `spawn_refused` (the kind a spawn-time refusal
records) through `apply_host_recovery`; any copy the spawn minted is
unminted as at a spawn-time refusal, not sealed, since nothing ran in it. The
node is idle: `wait_agent` answers and `close_agent` ends it. It has no session
to re-task, so `followup` refuses `followup_closed` at admission without
consuming the brief or opening an attempt, and the node stays `failed`. A
storage refusal (the store full, under pressure or unavailable) or a held-lease
refusal of that create stays a retryable refusal,
`recovery_storage_unavailable` or `recovery_lease_held`, and settles nothing.

A recovery reopen settles nothing on its own: what it closed is in the child's
feed, and the attempt's settlement is the tree projector's application of that
record (§14.3). A child's `TurnTerminal` abandonment fact derives
`turn_abandoned` with its operator line, in this process or any later one; so
does the closure record of a turn whose terminal was `RecoveryRequired` (it
finalizes the turn's operations and appends its history with no second
terminal), when its `feed_terminal` read reports the abandonment. If
the tree transaction that would record it is refused, the tree cursor stays
behind and the refusal is `recovery_storage_unavailable`; the next attempt
applies the same record and settles the same way. There is no in-process memory
of an observed abandonment. `recovered_without_turn` is derived only when the
child's feed, applied to its tail, holds no terminal for the attempt's bound
turn and its head is quiescent; its message says no abandonment evidence is
available, never that nothing was abandoned. A locator advance is a view and is
never published separately. Refusals record no fault reason and no failure kind
change.

If publication fails after queued recovery effects ran, every completed
effect is still recorded in memory and the caller receives
`recovery_storage_unavailable`, never a successful recovery receipt or a VCS
ambiguity diagnosis. The initial session settlement may already be durable;
the effect completion is not claimed durable until its publish succeeds. A
subsequent exact reopen uses only the store-acknowledged state, so an
unpublished seal completion remains unresolved there. If the same effect drain
also records a seal failure, the storage refusal includes bounded publication
and seal diagnostics, rather than hiding the seal failure.
An explicit retry in the same process publishes the recorded state before
returning even a no-op receipt; it does not rerun the seal or claim to have
settled the session a second time.
For an already-settled, failed-idle child this retry is `recover_agent`, not
`/recover`: the background pass only selects unresolved child session outcomes.
A dirty whole-tree revision alone does not identify which child's recovery
outcome has not yet been reported; it is not automatic child recovery authority.

Settlement has three triggers:

- `recover_agent` (section 4.5a), for one child, with its bounded lease wait;
- the tree's background pass. One is started per root open, per `ResumeTree`
  (from the idle loop or during a running turn), per returned child startup or
  retirement release with recorded fault or remaining recovery need (including
  release/publication failure), per next-prompt or `/recover`
  recovery of an interrupted root turn, and per explicit `/recover` with no
  interrupted root turn; it never runs on a `restored_paused` tree, and an
  explicit `/recover` on one says the tree is paused and that resuming it
  recovers its children. The host loop never waits on it. The hub keeps the
  pass's task, at most one at a time. A start while one runs is not dropped:
  the running pass runs once more before it reports, so a child that became
  recoverable meanwhile is tried, and its one report lists every child either
  run recovered and what the last run refused or did not reach. Release-triggered
  passes publish the recovered tree view and log their report. An explicit
  `/recover` says so, and one that starts a pass with children to try says so. The pass takes the
  tree's reattach lock, tries each child with an unresolved session failure
  once, never waits on a lease (a held lease is `recovery_lease_held` at
  once), and has 60 seconds from when it was started, waiting for the lock
  included; children it did not reach are reported as not attempted. Quit and
  `/reload` wait briefly for a running pass at hub shutdown and then abort it,
  before any lease is released (below).

A fenced child's turn waiter releases and reopens the child session itself
(section 14.1). Its reopen may wait out a lease, so it never holds the
reattach lock while it does: the node is marked as owned by the waiter under
that lock before its live entry goes, the lock is released for the reopen, and
it is retaken to record the outcome and clear the mark. The mark is held by a
guard, so a recycle that panics or is cancelled clears it too. Recovery treats the
mark as a live owner (`recovery_owner_live`) and reattach leaves the node to
the waiter.

Each host-loop-requested pass that settled, refused, or skipped anything reports
once to the user as a system note naming the children it recovered, each refused
child with its `recovery_*` code, and the children not attempted. A refusal note names
`recover_agent` as the retry and explains that `/recover` only selects unresolved
child sessions, not an already-settled child's pending publication. A
not-attempted note may also name `/recover` as a retry. A child whose session
settled but
whose seal or adopt is still ambiguous is listed as refused
(`recovery_vcs_ambiguous`), never as recovered. A VCS-ambiguous child stays
`recovery_required`; `list_agents` and `/tree` show it.

`list_agents` lists `recovery_required` rows even without
`include_completed`, and a full row carries `remedy: "recover_agent"` when a
session recovery would settle it, or `remedy: "close_agent"` with a
`recovery` reason when only a workspace ambiguity remains or a terminal
refusal holds it. A failed copy removal (`workspace_cleanup`) is a workspace
ambiguity, not a session recovery. Interrupted seals,
adopts and copy removals are never settled automatically or by
`recover_agent`: their outcome is a VCS question, and the tree records no
pre-landing target snapshot or expected seal commit to compare the evidence
against, so a node held only by one is refused `recovery_vcs_ambiguous`. The
one follow-up is a copy removal whose occupants the operator has closed, which
the next prune retries (§4.8).

The product's read-only session diagnostic loads the selected tree's exact
persisted rows under one SQLite snapshot, after schema and fingerprint checks,
without applying the reopen law or acquiring a tree writer. Its bounded,
cycle-checked product-session lineage lookup reports storage and read failures
instead of treating them as tree absence. Child node/attempt state and its
separately inspected Plakous checkpoint are reported as distinct authorities;
checkpoint quiescence alone never establishes child retirement.

`agent_operations` replays a completed operation's terminal response for the
same request digest, including a refusal — a failed `on_mint` replays as the
same `mint_hook_failed`.

### 14.3 Intents, views, and the tree projector

A `Resume { round }` decision is durable before the failed terminal is projected.
For the current bound turn only, it suppresses failed settlement and releases the
turn binding, leaving the attempt running. Before that decision a crash settles
failed conservatively; afterwards replay derives running without reserving a seal
or result mail. Continuation command ids contain agent, attempt and round, never
host generation; `Resume` commands do not spend submit-harness nudges.

The tree follows the product's pattern (chapter 02 §2.6). Plakous is the
authority for what every agent's session did; the tree store holds what the
tree asked for, what it derived from its sessions' change feeds (chapter 01
§5.11), and the records of the external effects only it performs.

| State | Class | Identity or source |
| --- | --- | --- |
| Tree identity, selected tuple, launch checkout, frozen snapshot, admission state (`live`, `interrupted`, `restored_paused`, `tree_terminal`, `closed`), host generation | intent | the root session; `ResumeTree`, interrupt and close requests |
| Node ancestry, brief, template, tuple, account, effort, tier, workspace id, path, branch; grants | intent | the spawn request (`request_key`, spawn identity) |
| Child-to-runtime-session association | external-effect record | `agent_operations` of kind `child_session`, keyed by child id, records the exact locator allocated by the create effect; another session or branch is refused |
| Node session locator | view | the child session's `Created` and newest record on its branch |
| Node status | view | the current attempt's view, interrupt and close intents, and the seal and workspace records below; `pending` until the child session's `Created` record |
| Node current attempt and schema | view | the `retry_of` chain root seeds the fold; `followup_opened` advances identity at its anchor. Node schema text and digest derive from that current attempt's immutable admitted schema, never a future opening |
| Active-turn flag, attempt turn binding | view | `CommandAccepted` for an `agent_commands` intent of that attempt, with no `TurnTerminal` for that turn |
| Attempt identity: turn token, `retry_of`, `request_key`, start stamp, brief, schema text and digest | intent | written once from the spawn request or `followup_opened` payload at attempt creation |
| Followup opening | intent, joined into views | anchored `agent_operations` of kind `followup_opened`, with previous and opened attempt identities, fixed supersession stamp and admitted schema; derives previous cancellation and the newly running node |
| Transient-continuation count | view | immutable Resume decisions for the attempt, separate from submit-harness corrections |
| Submit-harness nudge count | view | accepted corrective commands, including the round encoded in their immutable identity |
| Child turn-end decision | intent | `agent_operations` of kind `child_finalization`, keyed by agent, attempt, exact ended turn and decision, with the owner-release outcome; Finalize also carries immutable exact terminal sequence, outcome, commit stamp and whether its turn ceiling was spent |
| Attempt outcome: end status, end stamp, failure kind and message when they come from the runtime (`turn_abandoned`, a runtime failure kind), and the five usage counters | view | the child's `TurnTerminal` facts for the attempt's bound turns (status, kind, usage), the record's `committed_at_ms` (end stamp), and the failure message or abandonment line read from the root terminal record through `feed_terminal` |
| Account-admission block | intent, joined into views | anchored `host_turn_failure` with captured Blocked disposition, provider reset boundary and observation stamp |
| Recovery abandonment | intent, joined into views | anchored `agent_operations` of kind `recovery_abandoned`, per attempt, with the operator diagnostic and fixed abandonment stamp; derives failed attempt and closed node, without effects |
| Attempt outcome from recorded host or workspace decisions | view | the anchored host and workspace outcome intents below; their captured stamps and evidence, not current external-effect state |
| Attempt candidate association (`candidate_id`) | view | the candidate id captured by its anchored `seal_outcome`; the external candidate and workspace records remain inputs |
| Fenced-recycle and open-time host failure decisions | intent, joined into views | `agent_operations` rows of kind `host_turn_failure`, keyed by agent, attempt and turn, with the disposition, observation, fixed observation stamp and wait evidence; only the projector writes their attempt and node consequences |
| Host recovery without a runtime fact | intent, joined into views | `agent_operations` of kind `host_recovery`, keyed by agent and attempt; first exact-open/release or never-created decision, fixed observation stamp and closed/submitted-retirement disposition |
| Outside-host cancellation, uncertain retirement and materialization failure | intent, joined into views | `agent_operations` of kind `host_decision`, keyed by agent, attempt, observed turn and decision; immutable diagnostic, observation stamp, submitted/closed disposition and anchored fold position; the projector alone derives the attempt and node consequences |
| Tool, operator and cascading close | intent, joined into views | anchored `agent_operations` of kind `close`, per target attempt; captures recovery hold, terminal refusal, outstanding seal ownership, seal deferral and close stamp; only the target applier derives its rows |
| Descendant-close reservation | external-effect record | `agent_operations` of kind `descendant_close_effect`, keyed by the terminating parent attempt; captures target ids even when empty, so a feed-derived terminal reserves the cascade once as host intents do |
| Result-mail reservation | external-effect record | `agent_operations` of kind `result_effect`, keyed by the producing agent and attempt; fences the first eligible push, including a capacity-refused push, independently of envelope retention |
| Tool and root descendant interruption | intent, joined into views | anchored `agent_operations` of kind `interrupt`, per target attempt; captures whether live ownership or a non-ending park made the request effective, and derives only that agent's interrupted status |
| Session bound after materialization failure | intent, joined into views | `agent_operations` of kind `session_bound_after_failure`, keyed by the exact materialization intent, ordered with the other host intents |
| Submitted output and submitted attempt status | intent, joined into views | anchored `agent_operations` of kind `submitted`, per attempt, captures the validated exact `output_json`; its agent-local fold derives output and status, including historical attempts |
| Topology operation receipts (`agent_operations`), wait receipts | intent | the runtime call id; terminal responses replay for the same request digest |
| Mail body, sender, purpose, priority, reply link, disposition | intent | the sending call |
| Mail durable state beyond `accepted` (`acknowledged`, `delivered`), returned admissions | view | wait receipt settlement (§4.4) and `agent_commands` batches against `CommandAccepted`, `CommandRejected` and withheld `TurnTerminal` facts (§8) |
| Child commands and root result-mail batches (`agent_commands`) | intent | the runtime command id |
| Below-retention command consumption, undeliverable mail | intent, joined into mail views | `agent_operations` kinds `consumed_command` (exact resubmission's command id), `failed_wake_mail` (agent, attempt and ended turn; immutable mail ids and notice), and `undeliverable_mail` (a terminal or cancelled send's envelope id) |
| Session retirement settled | view | the child's `LeaseReleased` record after its last terminal |
| Seal state, candidates, workspaces and their `sealing`, `adopting` and `discarding` statuses, `on_mint` | external-effect record | the tree's own VCS and copy operations, recovered by §14.1 and §14.2, never from a feed |
| Seal reservation | external-effect record | `agent_operations` of kind `seal_effect`, keyed by agent and attempt; captures `reserved_at` and `resume_status` before dispatch, including a seal with no divergent candidate |
| Seal outcome | intent, joined into views | anchored `agent_operations` of kind `seal_outcome`, with attempt, completion stamp, failure and candidate id; the fold derives settled status and `seal_failed` at that position and fences result mail once |
| Orphaned VCS operation and its settlement | intent, joined into views | anchored `agent_operations` of kinds `vcs_fault` and `vcs_fault_settled`, per affected agent and attempt; reopen records seal, adopt and removal ambiguity, and acknowledged workspace completion records settlement |
| Spawn refusal, adoption acceptance and failed workspace removal | intent, joined into views | anchored `agent_operations` of kinds `spawn_refused`, `attempt_adopted` for the producer's attempt, and `workspace_cleanup` per occupant; captured failure, diagnostic and spawn end stamp derive agent-local consequences |
| Feed cursors (`agent_feed_cursors`), node fold order | view metadata | cursor per runtime session and last applied intent order at the agent cursor; NULL node order restarts the fold |

**An unstarted followup is withdrawn, not reversed by another event.** Only a
followup's first acknowledged fold may reactivate an adopted workspace, from
its captured prior status and under the opening's effect fence. Replaying an
opening never rewrites the retained workspace record, including a later adoption.
Only a definitive non-start permits withdrawal: a definitive Start refusal,
or send cancellation/SlotBusy expiry before submission reached the runtime.
Cancellation during submission with an unknown outcome (the send budget does
not expire a submission in flight) retains the opened attempt under a `host_decision` materialization recovery
hold; it does not withdraw the opening or consume its unread mail. A refused
withdrawal takes the started/failure path: a bound or stopped attempt is retained,
and an unbound opening is held for recovery before its opening mail is settled.
There is no running opened attempt whose opening mail is settled with neither
a bound turn nor a pending Start. These consequences survive view deletion.
A successful withdrawal, its opening envelope's `undeliverable_mail` decision
and the send's terminal failure receipt publish in one `fold_and_commit` CAS.
A refused CAS retains all three parts of the complete opening. A definitive
Start refusal settles the opening too; no withdrawn envelope can reach a later Start.
The later automatic recovery pass exact-opens and owns the child session. If
its authoritative head is quiescent with no admitted Start or abandonment,
the existing `host_recovery` / `recovered_without_turn` decision settles the
opened attempt as failed and sends its parent a failure result. It does not
leave a clean opening requiring an explicit `recover_agent` call. Recovery
captures the opened attempt's unread opening envelope as an existing
`undeliverable_mail` intent in the same acknowledged save as `host_recovery`;
the mail projection settles it read. It cannot be counted as unread, carried
in a later attempt's Start, or used to wake that attempt. Replaying recovery
consumes only that original envelope, never a later followup's mail.
The deliverer reserves startup
and exact-opens the child, then reads its authoritative feed tail before the
tree revision CAS. Startup and session ownership remain held through that CAS,
so no admission can race the tail read. The tail and tree cursor must both equal
the opening anchor. Any feed record (including Start rejection), bound/accepted
command, other command or intent, or reserved effect (including an empty
`descendant_close_effect`) refuses with a typed error and no partial tree write.
The opening send's own unfacted Followup Start command is part of the opening:
one CAS removes it, the `followup_opened` intent and the opened attempt together.
Captured pre-opening fields are restored only under these guards; the remaining
inputs fold to the same live views. Open-save/withdrawal crash cuts retain either
the complete opening or the complete withdrawal, never dangling attempt references.

**Live ownership is not a view.** A hub startup reservation, live turn and
its waiter retain process-local mutation ownership until admission, turn-end
decision or release settles. That reservation changes no persisted row, feed
cursor, revision or turn budget. Interrupt and close consult this ownership
before the projected active-turn flag, which is the fallback after a restart.
Headless run waiting also observes live ownership, so an idle turn view never
ends a run while a child owner still holds live work.
In particular, projecting `TurnTerminal` clears the view while the waiter
still fences a racing close from sealing a copy it could mutate. Parking or
finalization releases that reservation; `CommandAccepted` alone derives the
attempt binding and spends its turn. Losing live-slot registration never
undoes an admitted binding or refunds a turn: its cancellation and release
feed settle the view. Durable uncertainty is a keyed `agent_operations`
intent, never a persisted ownership flag.

Cancellation, retirement and materialization sites save keyed host decisions,
then invoke the same pure intent join used by feed passes. The host never
projects a decision's consequences or reserves effects before the
intent save succeeds. A refused first save leaves attempt and node views
unchanged; the host reports the publish fault instead. After joining, the host
publishes the consequences before settling their reserved effects with the
tree mutex released, then saves the external-effect results. A refused
consequence save runs no newly reserved effect and restores the acknowledged
pre-fold snapshot, including the feed cursor and pending usage. A retry folds
the saved intent again and reserves its effects exactly once.
Host recovery and host-decided turn failures use the same first-save fence.
Tool close, interrupt, submit and discard consequences, and duplicate child
command projection, use this acknowledged snapshot boundary too. A refused
tool consequence records its failure receipt on the restored tree. The effect
drain refuses seal and adopt tickets while the tree revision is unpublished.
Adoption publishes its workspace and ticket reservation through the same
boundary before landing. Raw mutation-and-drain callers publish after their
callback (which may itself publish intents); they cannot dispatch an
unacknowledged ticket or wrap nested intent saves in a revert snapshot.
Hosted attempt-end and spent-turn descendant closes also publish their intent,
view and seal reservations through this boundary before retiring live children,
stopping their processes and draining effects. A refused close save leaves no
new descendant consequence or effect to dispatch.
A successful session bind after a materialization failure records and saves a
`session_bound_after_failure` intent before joining it. Binding the locator
alone changes no outcome. The ordered join clears only the named failure's
exact status, diagnostic and end stamp, never a closed, adopted, submitted or
VCS-faulted outcome or an unresolved owner-release obligation. Materialization
keys include the latest bind/restore generation for that attempt, so another
failure after restoration is a new decision, even with the same observed turn.
A pending `seal_effect` on reopen follows the in-flight VCS fault rule:
the non-closed node becomes `recovery_required`, even if its `sealing` view
was lost, and the uncertain effect is never automatically retried.
Each agent's tree view is an anchored fold of its session's feed records and
its own host intents: `host_turn_failure`, `host_decision`,
`session_bound_after_failure`, `host_recovery`, `child_finalization`,
`seal_outcome`, `vcs_fault`, `vcs_fault_settled`, `spawn_refused`,
`attempt_adopted` and `workspace_cleanup`.
A VCS obligation is read as of that agent's fold position, never from a
workspace's current status or a mutable external-effect outcome. A later
settlement cannot erase an earlier fault during replay.
Fault recording captures any prior process-local diagnostic; applying the fault
uses that immutable text rather than the diagnostic present during replay.
A seal reservation is read as of the agent fold position: it derives
`sealing` only after its reservation and before its outcome applies. A settled
seal never changes an earlier Finalize or cancellation join. A failed seal
derives `seal_failed` and its end stamp from the captured completion stamp,
not the wall clock during replay.
A failed or empty seal consumes its external reservation too: replay of the
runtime terminal cannot replace that reservation or dispatch another seal.
Result mail likewise reserves once per attempt. Its durable `result_effect`
survives closing the node, pruning its acknowledged envelope, and deleting
fold views; feed replay cannot push a second result. A mailbox capacity refusal
consumes this reservation too: the wait row remains the result surface.
Fresh root and child nodes start with fold order 0 even before their first
feed record; NULL arises only from view deletion. A first descendant-close
reservation refuses with retryable `operation_recovery_required` if any
descendant fold is NULL, including descendants below a stopped child. The
enclosing acknowledged fold boundary restores the parent without advancing it
or reserving effects. Multi-agent rebuilds and reopen session recovery project
descendants before ancestors; a parent-first refusal returns to recovery and
retries after its descendants. Behind-retention seeding resets only the named
agent and obeys the same cascade rule. An existing cascade fence is replayed
without recapturing targets, regardless of projection order.
Reopen also selects every deleted child fold for session recovery, even when
its reset attempt view has no unresolved-outcome flag. `Session` means either
an unresolved outcome or deleted fold views. Deleted-view session identity
comes only from the durable `child_session` construction binding; view deletion
does not remove that binding. A sessionless target rebuilds its intents alone
without opening or replaying a create. A rebuild projects the feed without
inventing a new host-recovery decision. Ordinary reopen selects no extra target.
Deleted-fold rebuilding runs even under the restored-paused dispatch fence;
while paused, this pass selects only deleted folds, not ordinary unresolved
outcomes. It dispatches neither model calls nor tools. The normal resume fence
continues to gate ordinary recovery and live work.
Recovery reports `recovery_storage_unavailable` for a refused preparation or
consequence publication, including a retry of an unpublished effect result;
an in-memory completion cannot stand in for an acknowledged save.

Recording an intent captures `fold_anchor`, the agent session's current tree
cursor (0 before session allocation), and `fold_order`, the tree revision at
insert. Each insert advances the revision. The immutable pair is both NULL
for ordinary receipts or both present for fold intents, with nonnegative anchor
and positive order. Payloads carry the host's observation and its fixed stamp,
not join scheduling or process-local anchor evidence.

Intents apply in `(fold_anchor, fold_order, operation_id)` order, immediately
at the current cursor during live recording and at that anchor during replay.
`agent_nodes.fold_order` is the order of the last intent applied at its current
cursor; NULL means the views were deleted or the fold has not started. A NULL
position starts at cursor 0, applies the anchor-0 intents, and then advances
through feed records. After each record, the position resets to order 0 and
applies that record's anchored intents before the next record. An intent is
applied exactly when the position is non-NULL and its anchor is below the
cursor, or its anchor equals the cursor and its order is no greater than the
position. While applying a record's facts, its `(seq, 0)` position is already
installed privately: an effect those facts reserve belongs to that record,
never its predecessor. Only the complete record is published, and its anchored
intents then fold. A pending seal at that position derives `sealing`, including
after a deferred close. The pending-seal view is lifted after each intent, before
the next same-cursor intent reads node status; batching intents cannot change
their consequences. Projecting again at the same position changes no consequence.
There is no unbound-intent deferral, per-record history re-join, binding guard,
terminal-cache guard, message-suffix deduplication or recovery supersession
probe. `agent_commands.accepted_seq` remains a nullable integer view recording
actual command acceptance, not the fold anchor.

Pages skip records already at or below the tree cursor. The first unapplied
record, like every subsequent record, must be exactly `cursor + 1`; a retention
seed explicitly sets that cursor before checking. A stale incremental page after
cursor regression is refused without mutation. `TreeFeed::apply_checked` preserves
internal typed contiguity and refold refusals across the hosted boundary. Every
incremental projection caller rereads from the session start (or its retention
seed) outside the tree lock and retries once, carrying authenticated wait
evidence. The reread spans the retained session through its initial observed tail,
not a fixed page window; its record bound is the runtime session retention limit.
A second refusal maps to the existing `operation_recovery_required`;
no internal retry condition adds a model-visible wire code.

Deleted (`NULL` fold order) agents refuse live fold-intent recording and ordinary
`fold_due` retryably. Close drains leave them queued, without starting their folds.
Only feed reconstruction and the explicit sessionless deleted-fold rebuild start
those folds. Public due folds skip the scratch clone when there are no due
intents, no pending seal lift and no unprojected session allocation to join.
A close drain with only deleted targets also skips the clone and preserves its
work list. Startup defers an agent with any deleted descendant until the
post-order rebuild has made its captured cascade targets available. Each restored
rebuild consequence then folds newly eligible startup ancestors descendant-first
in the same acknowledged transaction, before lifting the pause overlay.

A late recovery terminal/closure read may settle directly only at that record's
own position: no later record or intent has applied to that agent. Otherwise
projection requires the session from sequence one, or from its retention seed,
through at least the current cursor.
A complete page refolds that agent synchronously on the transaction's scratch
copy, with reads at their own positions; no cleared live view is ever published.
An incomplete page returns retryable `RefoldRequired` without changing any state;
the caller rereads from the session start outside the tree mutex and retries.
The attempt's `applied_terminal_read_seqs` set is a fold view: it records every
own-position abandonment read, survives later host obligations, and is emptied
by both view-deletion paths. Each exact matching reread is inert, including after
retirement or reopen. A refold counts only records beyond the pre-refold cursor and publishes only
their new terminal usage, selected by record sequence rather than turn identity;
already acknowledged usage and pending usage are
preserved, and acknowledged effect fences prevent repeated reservations. Ordinary,
live-child and release observation read after the tree's session cursor. Live-map
locks are released before feed I/O: observation owns a cloned session capability,
not a borrowed live handle. Release evidence is reached when the tree already
records the released locator, the cursor is at or beyond the feed tail, or the
new records name the released head. Only `RefoldRequired` rereads from the start.
A bounded reread that stops short of its initial tail refuses projection rather
than publishing a partial refold, including a feed temporarily above its trim cap.
Root wait evidence may also join the exact branch head freshly authenticated by
the facade history read in that same settlement call. Unlike children, the root
has no node locator view, and an already-applied head need not occur in a cursor
read's new records. A subsequent feed read that observes a different head on
that branch refuses settlement for retry; an earlier or cached head is never a
seed. The observation is bounded by that feed read's initial tail.
Terminal evidence is read only for failed or abandoned terminals and recovery
closure candidates. A closure candidate is a Quiescent advance that finalizes
the turn's operations and appends its history without a terminal, and is the
last record in the read run naming that turn. Ordinary Working tool rounds
never request terminal evidence. The facade batch shares each distinct
checkpoint load and each pruned-fallback branch-head load within the read run.
For each branch this is at most one head load per read, plus one for a
`BehindRetention` seed. Seed running-turn observations remain separate exact
head reads on that rare path.

Each applier joins the immutable attempt identity, including a historical
attempt after followup. Only the current attempt changes its node or reserves
external effects. Cancelled and Materialization cannot overwrite an attempt
already stopped by another consequence; Retirement annotates its recovery
obligation without changing an already stopped attempt's status (§14.2).
Submitted output completes cancellation or materialization, but uncertain
retirement never treats output as proof that the owner released its session.
Recording cancellation or materialization with submitted output refuses a
`recovery_required` recipient before inserting the intent. Session binding
clears the named materialization failure only, not an unresolved owner release.
Finalize carries its exact immutable terminal outcome, sequence and commit
stamp. The late Finalize read parses only bounded records after its accepted
command using command-to-attempt identity; it never scratch-folds the tree or
reserves effects. Workspace generation changes remain row-local inside folds;
shared Read/Grep generation counters derive from the rows only on an
acknowledged save, never from a scratch or refused fold. Finalize also carries
whether its turn ceiling was spent. Park and Nudge derive no
terminal outcome. Completion never fabricates a runtime turn.

The first successful application reserves its effects under the intent's
durable `effects_reserved` fence. Deleting views does not delete that fence:
replay re-derives outcomes and stamps without recording another descendant
close, reserving another seal or sending result mail again. A close cascade
records one captured intent at each target's cursor under its parent's effects
fence. The parent applier changes no descendant view; after it completes,
target appliers fold the newly reserved closes. `fold_due` is agent-local:
it never scans or applies another agent's intents. Recording a cascade returns
its targets and queues them on a process-local `pending_close_folds` work list,
not fold state or evidence. Feed projection drains that list after its page;
the driver drains it inside `fold_and_commit` before publishing, then refreshes
the epoch and prunes closed copies alongside the pause lift. Pruning waits
for both process-local turn ownership and every turn waiter to end, even if
the terminal fact already cleared the active-turn view; failed-worktree rescue
snapshots retain the copy until then. A refused save
restores the work list with the views. The target's own fold and startup's
unapplied-intent pass remain authoritative even without the list. Projection
order across agent sessions cannot change those consequences. A feed-derived terminal reserves
its cascade under `descendant_close_effect`; host intents reserve that same
attempt-scoped fence under their own fence. Replay never recaptures the
descendants' live ownership or recovery state. A semantic consequence refusal
drops that intent's partial writes and effects atomically and returns an
`IntentJoinRefusal`; its fold position and later facts still advance.
Malformed immutable data is an integrity error. A feed pass indexes its own
fold intents once by anchor, order and id; each record joins only its due
range. Appliers never mint another same-agent due intent; debug checks compare
the page index's due range to a fresh index before and after folding it.
Already effects-reserved appliers never rewrite operation inputs or
reserve effects, so their rollback snapshots copy views and process-local
state without cloning immutable operation rows. First applications retain a
complete rollback snapshot. Repeated close requests skip a target already
Closed at its current fold position instead of recording another close intent.
Fenced replay resolves the session before transferring operation inputs and
restores that collection on every refusal. A release-mode count and replayed-row
check rejects writes with `operation_recovery_required` before joining views.
Public `fold_due` and nonempty close drains use one scratch per call; callers
already holding a driver snapshot or feed scratch use the in-place variants.
The atomic caller boundary restores exact inputs on an integrity refusal.
Resume propagates a fold integrity refusal. Void restore, workspace-fault
settlement and seal-completion wrappers log a tracing error with the agent
and refusal instead of silently swallowing it. The hosted sessionless rebuild
keeps the public scratch boundary across the crate boundary.
The debug applier-diff invariant includes foreign commands, session cursors,
inbound queues, effect tickets and fault reasons. A newly reserved result
envelope and its inbox append are external effects, not foreign view writes.
In-memory and SQL view deletion reset the same columns and derived rows,
including command-acknowledged and failed-wake/undeliverable mail and the
command-keyed `returned_admission` receipts. Current schema resets to the
chain root's retained schema until opening intents advance it again.
The driver publishes the intent, then folds due consequences and lifts
`restored_paused` if the tree is
quiescent inside `fold_and_commit`. That boundary requires an acknowledged
revision and snapshots immediately before its work, after the caller's
process-local ownership changes. It restores that snapshot on a work or save
refusal. Start binding, waiter registration and reopen unpark happen under
the same tree lock before that snapshot, not in its reverting closure. A
refused save retains each registration until its RAII owner drops, so dropping
one refused registration cannot remove another live waiter's count. Ownership
released before the boundary is never resurrected. The
pause lift is not an applier. Feed projection and published closure mutations
use the same boundary. Intent recording and publication precede it, never
nest inside it. Pending terminal usage is emitted only by an acknowledged
save; a failed feed save restores the cursor so the next pass re-derives and
emits usage once without reloading the tree. Before installing a reopened
runtime, startup folds every unapplied intent whose agent view has started,
outside the process-wide hub entries lock, then rechecks the occupant before
installation. An empty target/work list with no restoration overlay to lift
skips the consequence clone and save. Startup folds those targets in
descendant-first order inside one acknowledged `fold_and_commit`, then
recomputes quiescence and lifts the restoration overlay. Deleted views remain
with the post-order authoritative-feed recovery rebuild, not this startup
pass. Work/save errors refuse installation as `TreeRuntimeError`. Once
installed, the same settlement path dispatches the acknowledged effect list;
no startup reservation is discarded or mistaken for an orphan on next reopen.
A settlement failure retains the installed runtime and its recorded recovery
state for inspection and recovery. Settlement publishes completed
external effects before dispatching any follow-on reservations; a refused
result save retains those reservations for a later acknowledged publication.

The feed carries the runtime's actual `retained_from`. A behind-retention pass
marks commands and folds intents anchored strictly below `retained_from - 1`.
The exact head then seeds the pruned window at `(retained_from - 1, 0)`, resetting
cursor and order. Intents at that boundary fold after the head, followed by
retained records and their anchored intents. The head is the newest knowledge
of the pruned window: it binds the attempt selected by the older openings, and
an interrupted or closed but still-unwinding turn remains active and bound.
Requested status and outcome remain intent-derived, not overwritten by the head.
Head binding is seeded only when the fold has not started or the
retention boundary advances past its cursor, not on another read at the same
fold position. It then replays the retained window normally, never jumping to the
tail. Attempt identity and immutable terminal evidence let seed folds apply
without reconstructing old bindings. There are no consumed-intent companions
or seeded-binding boundary columns. Counters restart at retention.

Both view-deletion paths reset each child's current attempt to its `retry_of`
chain root. Schema text and digest re-derive from that immutable attempt when
the fold starts, then from each opening at its anchor. They clear the node fold position, child status, active
turn and locator, attempt binding, output, candidate id, outcome, failure, blocked reason and
turn/tool/token counters (elapsed time only grows),
command fact, turn and acceptance sequence, and feed cursors. No stopped
outcome is preserved as an input: followup opening, close, interrupt, recovery abandonment,
spawn refusal, producer adoption, per-occupant cleanup failures, seal outcomes
and VCS faults rebuild from their anchored events. Reopen never retries an
uncertain VCS effect automatically.

The live child restart port takes the tree driver explicitly, so its duplicate
settlement uses the same feed-to-view pass as an initial Start or reopen wake.
Submission acknowledgements never write command facts or acknowledge batches.
Acceptance and rejection come only from the feed projector; a definite
unrecorded refusal withdraws the intent with its mail unread. A Duplicate
first projects to the tail: a retained acceptance stands, an absent acceptance
with a positive retained boundary is consumed, and absence without that boundary
is an integrity refusal. Reclaimed also proves consumption. Consumption is a
keyed host intent whose projector join acknowledges only its stored batch,
without inventing a runtime turn, binding, usage or corrective round. It survives
view deletion. A failed wake records its exact undeliverable non-result mail
and diagnostic once; only the projector acknowledges that snapshot. Results
remain unread and a retry never adds mail to the saved decision.
Mail Start, followup and steer intents all include the primary envelope their
command text carries. Their deliverer projects admission before returning
success; the send tool never acknowledges from that live return. A send
refused because its recipient ended, or cancelled after its unstarted followup
is rolled back, records an undeliverable-envelope intent instead. Its projector
join and view rebuild derive that acknowledgement without a runtime fact.

Root and child quiescent reopen and explicit recovery pass retained wait-call evidence as
bounded reads at the exact committed head, alongside its feed. The projector
authenticates the read's session and checkpoint against that feed or the child's
retention-seeded locator (the root uses its feed's exact retention heads), then retires committed receipts or rolls back
uncommitted ones. A retained unclosed terminal also consults this evidence when
its earlier history fact has been pruned, so it cannot roll back a committed
acknowledgement. Replaying a page or joining reads below the cursor changes no
settled receipt and never refolds usage. A refused open or release publishes
neither these reads nor their consequences.
Root terminal handling uses this same feed/read pass; there is no separate
live root-wait retirement writer.
An exact child open and release that finds no runtime abandonment, or a
construction replay that proves the session was never created, saves a keyed
host-recovery decision. Only its projector join settles attempt status,
failure, node status and end stamp. The first decision stands; writing it
alone changes no view. View deletion preserves that intent and its seal/unmint
effect fence, so rebuilding cannot perform a second VCS effect. A committed
abandonment still settles directly from its feed fact, never from a fabricated
host recovery fact. A stale or refused open/release saves no recovery decision.

**Child finalization is a terminal join.** A committed `TurnTerminal` ends
its binding but leaves an ordinary child attempt running with its turn-end
decision pending. The waiter or existing reopen path records a keyed
`child_finalization` intent: `Park`, `Nudge` or `Finalize`, and whether live
ownership was released. For one turn, Park and Nudge are mutually exclusive;
the writer refuses the second. There is at most one Finalize, and its payload
is immutable. Finalize deterministically supersedes Park or Nudge regardless
of application order. Without Finalize, the single Park or Nudge keeps the
attempt open; without any decision it remains decision pending. Completion,
its seal and result publication are fenced only by a saved Finalize intent
joined with that exact committed terminal, never by the active-turn view.
The projector returns exact `ChildTerminal` evidence to the caller for each
ended turn. Finalize captures that caller-supplied evidence, including its
attempt, turn, sequence, outcome and commit stamp; the tree has no terminal
cache. Ordinary hosted feed passes start at the persisted tree cursor. A late
or reopened Finalize reads bounded evidence after its command's `accepted_seq`,
not from sequence zero, and does not refold usage. Missing retained evidence
never fabricates completion. Clearing views preserves the intents
and external-effect fences (the Finalize receipt reserves completion effects
once), clears completion outcomes, bindings, turn counts and
corrective budgets, and replay derives the same views and no second effect.
The child's end stamp is the joined terminal's `committed_at_ms`. Once replay
settles all work, a restoration-only pause is lifted as on a quiescent reopen.
Abandonment notices are derived from committed terminals and their bounded
reads, not from a live reopened agent. Stale heads remain construction inputs
until an exact open succeeds (and, for explicit recovery, release succeeds).

**The tree projector.** One pure function applies one record of one session's
feed to the tree's views, reading only the record, the tree's intents and
external-effect records, and the bounded reads at the record's checkpoint
(`feed_items`, `feed_call`, `feed_terminal`, chapter 01 §5.11). Each
pass joins the immutable child-session allocation, then derives its locator
from `Created`, `Advanced`, `Branched`, `LeaseReleased` and `Pruned` headers
on that exact session and branch. Records of other branches never move it.
The create and reopen sites record the allocation or apply this same feed
pass; they do not bind or advance the locator view themselves. A neutral
binder that only allocates a child session records the same keyed
`child_session` external-effect record and runs the allocation seed through
this projector; its returned locator is not a live view write. A live child's
release carries an ordered feed page and its bounded terminal/head reads back
to the hub, which applies the same pure projector before publishing the tree.
A bare construction locator returned by release never advances a node view.
If a bounded release pass cannot reach its released head, it reports projection
debt and retains recovery fencing rather than publishing that construction
locator as the view.
A view rebuild clears the allocated child's locator and derives it again from
this join.
An explicit recovery keeps a stale-open head only as a construction input
until exact open and release succeed. Its copy-and-publish feed transaction
then derives the locator with the attempt settlement; a refused open cannot
settle a still-held session obligation by publishing only the stale-head pass.
Each tree transaction applies a page of one session's records in order, writes the
derived rows, and CAS-advances that session's cursor together with the tree
revision; a concurrent applier fails the CAS and changes nothing. It runs in one
code path: live after each feed notification of the root or a child, before a
root prompt or a child wake is admitted (both require the tree cursors of the
sessions they depend on at their tails), on every open and recovery pass, and in
memory in the read-only rehearsal. Deleting the view columns and cursors of a
tree is safe: the next open re-derives them from the root's and every child's
feed. Attempt bindings and active-turn flags are derived in feed order from
accepted commands and their terminals, independent of live ownership.
Clearing views clears those bindings and their turn counts, too; replay
counts each distinct admitted turn once and rebuilds the same binding as
an incremental pass. Behind retention, an exact lease-free head read seeds the child's
running-turn binding and active flag as well as its locator, only for its own
session and branch; a quiescent or workflow head seeds no ordinary turn. This
observation does not spend a turn. Each newly applied child `TurnTerminal` also
produces a disposable usage observation keyed by turn id. The process-local
buffer is never persisted. Every tree-driver save, including release and
recovery saves, emits through the required topology observer only after the
store acknowledges the winning revision CAS. A refused fold save restores
the pre-fold snapshot without emitting; the next successful pass re-derives
usage from its acknowledged cursor, without requiring a reload.
Reprojection emits nothing; a rebuild emits identical keyed telemetry
rows, leaving counts and totals unchanged (chapter 02 §7.1).
A child's `TurnTerminal` folds its usage (local tool attempts, model
input and output tokens) onto the attempt its command served, and its
`committed_at_ms` raises the attempt's elapsed time; the child's turn waiter
runs this pass when the turn ends, so the live rows never wait for a reopen. A child's failed `TurnTerminal` ends the attempt it binds: the
projector reads the terminal through `feed_terminal` and derives the failure
kind and status from the model error's kind and retry disposition (a block's
`resets_at` is the record's `committed_at_ms` plus the provider's delay, never
the time of the pass), the end stamp from `committed_at_ms`, and the message
from the read error. A terminal whose read is no longer retained ends the
attempt on the fact's kinds alone. What the host observed when the turn failed
(the failed-worktree snapshot line) is not feed data: the live path records
it first as a failed-turn notice, an `agent_operations` row of kind
`failed_turn_notice` keyed by agent, attempt and turn, and the projector joins
it into the failure message, so a rebuild derives the same row. While that
turn's waiter is live, every feed pass stops before its failed terminal until
the waiter has recorded the notice (including an empty notice when no snapshot
line is available); neither the cursor nor terminal consequences advance early.
The live path
keeps only the external effects: the job stop before the notice, and the seal
and result push the projected failure begins. Unlike the product's background projector task (chapter 02 §2.6), the
tree projector runs inline in the host loop and the topology operations that
need it: tree saves are counted against the store's fault budgets per save,
and the tree's commit trace must be deterministic for a given sequence of
feed records, which a coalescing background task would not give. Its
`feed_terminal` reads, like every content read, are served from the live
runtime's cache of recently committed checkpoints when the record's
checkpoint (and, for a terminal, its parent) is one (chapter 01 §5.11), so
an inline pass after a live commit does not reload checkpoints from the
store. A child session behind its feed's retention seeds its attempt views from
the child's current head (the node's session locator advances to the head of
its branch that the behind-retention page names) and the tree's intents, then
resumes at that page's tail. It records no outcome it cannot read; records
committed after the head snapshot apply on top of its seeded running turn.

**Intents reach the runtime only under their own identity.** A child command is
written to `agent_commands` before it is submitted, and resubmitted only under
the rules of §14.1. A spawn whose child session has no `Created` record is
submitted again through its deterministic create operation
(`plak:spawn:<tree>:<agent>`, §14.2). One product-tree join remains: at open the
product names the command ids of its pending root submissions, and the tree
withdraws any root batch whose command id is neither named nor recorded, so a
crash between the tree batch and the product submission leaves no mail bound to
a command that will never run.

**External effects keep their own recovery.** Seals, adoptions, discards, copy
mint and unmint, `on_mint` and processes are not runtime commits and have no
feed facts. Their records and the `recovery_required` rules of §14.1 and §14.2
stand unchanged; a node held by one stays `recovery_required` until the
operator settles it. Everything a feed record settles is settled by the
projector instead: a child's abandoned turn, a lost retirement, an admitted but
unbound turn, a wait's acknowledgement, a batch's acknowledgement or return.

## 15. The JSONL host protocol

`plak agent` is a long-lived stdio client of the host: newline-delimited JSON
envelopes projecting a closed subset of user actions and host events. Same
session, tree, queue, reopen, and effect laws as the TUI. Every line is
`{ "v": 1, "kind": "<closed>", "id": "<correlation>", … }`. Unknown additive
fields are ignored within v1; an unknown `kind` is a typed diagnostic and the
connection stays up.

Topology-relevant commands:

| `kind` | Effect |
| --- | --- |
| `tree` | request a topology snapshot |
| `resume_tree` | resume a `restored_paused` tree |
| `interrupt_node` | this-turn cancel of one child; requires `agent_id` |
| `close_node` | terminal close of one node and its subtree; requires `agent_id` |
| `stop_all` | host postorder close of every non-root, non-terminal node |
| `invoke_tool` | dispatch one grant-compiled topology tool |

`invoke_tool` takes `name`, an `arguments` **object**, and an optional
`agent_id` that defaults to the attached root. The `name` must be one of the
ten topology names, checked at the codec before any dispatch; anything else is
a diagnostic. It runs the **same executor as the model**: grant compile is not
bypassed, and tier, working set, tree epoch, bind, and submit-token laws are
unchanged. Coding tools and composition exec are not this kind, and there is no
TUI slash for it.

`invoke_tool` is **idle-only**. While a root turn is busy the host emits
"available when idle (finish or interrupt the turn)" and starts no second turn
and no fake model turn. It is also blocked while the session is
`recovery_required`.

Receipts appear on stdout as a `tool` event pair — `state` `running` and then
`done` or `failed` carrying `call_id`, `name`, and the tool's text — followed by a
`tree` snapshot event when the tree revision advanced.

Tree rows in JSONL carry the bound model `account`, `tier`, `template`, `attempt`, `workspace_mode`,
`workspace_status`, `candidate`, and `blocked`, omitting rather than defaulting
what the host does not hold. A node that will not run again carries `status`
`closed` or `recovery_required`.

Spawn, mail, wait, submit, and adopt remain **root-agent tools**; the client
prompts and the model calls them. `invoke_tool` is an operator escape hatch
over the same executor, not a second control plane.

## 16. Hosted tree access and child retirement

The host observes each running child with one independent bounded activity
subscription, with at most eight such subscriptions per host face. The complete
tree view carries optional `live_progress`: exact turn identity, current activity
verb (the same classification the root HUD uses), dispatched tool-call count,
last-observed activity time, and provider-reported live token counters. A progress
material change can publish a complete snapshot at the same durable tree revision,
with progress-only publications limited to about one per second. Timestamps alone
do not trigger a projection. Existing observers retain their slots; stable agent-id
order selects newcomers, and failed attachments back off before retrying. Counters
are seeded from the atomic snapshot, not reset on attachment; it
does not manufacture a tree mutation. Terminal observation or removal clears
the live progress. These counters and timestamps are observations, not billing,
terminal authority, or reconstructed history.

The open agent transcript (below) is the only child content subscription. Its
live turn is attached through the live child's attached agent capability for an
atomic snapshot and its gapless tail. The admission-created `TurnEvents` is never
retained for later viewing. An atomic snapshot does not replay prior uncommitted
streaming blocks: when prior activity is not covered, or the tail loses activity
or lags out, an addressed `earlier activity not shown` marker makes the gap
explicit. Closing drops this subscription; a lagged tail reattaches, at most once
a second, with an explicit gap. Adjacent live text deltas coalesce
only for the same `(agent_id, turn_id)`; child traffic occupies at most 128 of the
512 observation slots, with a single addressed gap when its share is exceeded.
Dropping the host drops all of its child observers. Unread presentation debt
never backpressures the child or its terminal result.

Child text, reasoning (under the root's visibility gate), tool start/completion,
usage and terminal observations use the root block constructors inside
`HostEvent::ChildLive { agent_id, turn_id, op }`. They never enter the root's
`apply_event` or mutate its transcript, usage, queue, or turn control.

**Agent transcript.** `OpenAgentTranscript { agent_id }` opens one agent's
transcript for the surface's main view; at most one is open per host face, and
opening another replaces it. Its identity is the agent id alone: tree revisions
never reset, re-send or refuse it. The host resolves the agent as a non-root node
of the attached session's tree, in any status. A node without a live turn is read
from its own bound session through `AgentSessions::transcript_source`, at the
current head of that session and branch in one load. A node with a live turn is
attached first, through `SessionTranscriptSource::attach`: one atomic snapshot
yields both the committed history at the attachment's cursor and the gapless tail
after it, so the rows are exactly that history and the ops exactly that tail; no
item committed before the cursor is in neither, none is sent twice, and what the
tail cannot replay is marked with the same gap notice. Items map to the
row types the root's live path writes: a non-harness user prompt is `User`,
assistant text is `Assistant`, reasoning is `Thinking` under the root's
visibility gate, each call joined with its result by call id is one `Tool` row
whose subject, detail, state and wait facts come from the functions the root's
live path calls (a restored bare pi `wait` that ran the await takes its row
subject from the result observation's `subject`, the receipt's bounded agent
ids, not from the 2 KiB preview; a settled process wait's subject from the same shared function,
reading the host's job registry and the commands of earlier process starts in
the same history), a compaction is the `compact` tool row, and delivered mail is
one `tree` notice. Local nested calls retain their canonical cell identity and
their explicit evaluation owner from the same checkpoint's retained operation
ledger (chapter 01), with an exact child receipt required before the evaluation
lookup. Marker cell identities come from linked nested lineage in the retained
window, so a marker may have no cell identity when those nested parts are absent.
Normal evaluator children are projected from retained definitive `Effect`
requests and outcomes on that same snapshot, after the explicit marker in
durable acceptance order; missing or oversized effects are absent rather than
manufactured. Canonical and effect-backed parts share the same redaction and
item/byte bounds, and duplicate call ids are emitted only once.
Provider-programmatic rows without an explicit owner drop their execution-id
cell identity when rebuilt (the live events retain it), to avoid reading as yields.
Successful live child tool completion events use the root's `done` token;
rebuilt settled tool rows retain their `succeeded` state.
Nested rows and wait continuations are parented only by
that durable link to an opening cell row in the bounded window, never by order,
name, or a reused cell id. A parent outside the window is cleared along with
its cell id. If the owning receipt was pruned, nested calls remain ordinary
unparented rows with no cell identity (so they cannot be mistaken for a yield);
a cell always keeps the state of its own result, and known
nested failures contribute to its displayed failure tally. These rows have
no durations because history carries no timestamps; a call without a committed result is
`interrupted`. A tool exchange commits whole, so a live turn's call still in
flight at the attach has no row: its start precedes the cursor, the gap notice
says so, and the read at the turn's end shows it settled. One read projects at most the newest 512 items, newest first,
each string over at most the history redaction window (chapter 01), as
synchronous work on the blocking pool behind one permit per host face. Rows are
bounded like stored rows and the newest that fit the presentation bounds are
kept; one `Truncated` row leads them only when something older was dropped, and
rows that exactly fit are all kept. The answer is one
`AgentTranscript { agent_id, rows, live_turn_id, note }`; a live turn is named in
`live_turn_id` and its ops stream as `ChildLive`. When that turn ends the host
sends a fresh `AgentTranscript` from a new atomic read without a live turn; when
a new turn of the agent starts it sends a fresh one naming it and streams that
turn. Reopening the open agent on the same live turn is a no-op. A failed later
read re-sends the last good rows beside its `note` rather than empty rows; only
a send that passes the generation check below replaces the last good rows. An
unknown agent, the root, a node without a bound session, a host without a hub or
session store, and an unreadable first read answer at once with empty rows and a
`note`, and open nothing. Every send of a view (`AgentTranscript` and
`ChildLive`) is checked, under one lock, against the face's view generation it
was started under; close, replace, a new turn and a session switch bump that
generation under the same lock, so nothing of an earlier serve arrives after
them. `/clear`, `/resume`, fork and a profile rebind close the view before the
switch emits its `ClearView` or new session snapshot. This view does not count
against the eight progress observers. A history row reads both names from the
committed tool call (chapter 01 §8.2): it is labelled by the exposed name the
model called, so pi's `edit` and `write` over one patch tool stay two distinct
rows, and its activity kind and subject derive from the canonical name, exactly
as a live row's do.

Production consumers access tree state through `TreeDriver` and hub closures.
A mutation retains the store-acknowledged revision from its explicit publish;
settling an empty effect batch does not itself publish a mutation. Every queued
effect is drained after the tree mutex is released, including when the initial
publish fails. Chained cleanup is recorded before the original failure returns.
Raw tree handles are available only to test fixtures.

A retiring live child reserves its agent slot by exact turn identity. A tracked
worker owns release and locator publication independently of the caller's future.
Concurrent cancel and close join that same completion; pending retirement is
never reported as not-live. Release and publication run without the live-table
mutex. A returned release (including a reported cleanup failure), a publication
failure, or a contained panic clears that exact reservation: the runtime owner
has returned and retaining its Agent would falsely deny in-process recovery.
Durable recovery-required evidence still fences uncertain session/workspace
state; clearing process-local ownership does not establish successful retirement.
A deadline expires the joiner's answer, never the release future or its slot.
That release deadline (for the owner's evidence and the joiner's answer alike)
is the cancel settle plus `HOST_SESSION_RELEASE_TIMEOUT` (60 s): a released
runtime's shutdown joins any store call already in flight, such as idle history
retention after the child's last turn, then commits its shutdown receipt, each
under the runtime's `DEFAULT_STORE_OPERATION_TIMEOUT` (30 s); `plakous-hosted`
asserts at compile time that the release timeout covers both. A slow but healthy release
is therefore waited for, not fenced, and close of a completed child keeps its
terminal.
After publication and slot removal the host requests the tree's background
recovery pass only if a fault was recorded or session recovery remains required.
Clean, fast retirement does not start a pass; a tree-read failure requests a pass
rather than skipping one. Joiner deadline expiry is a typed refusal, separate
from owner-confirmed failures through cancel, batch cancel and the tool/UI
callers. Those callers return a bounded fail-closed answer without persisting
another fault for expiry: the release owner records its deadline evidence.
Caller publications of owner-confirmed failures from the close, interrupt,
stop-jobs and navigator paths are scoped to the attempt captured before
awaiting and skip an already settled or replaced attempt. The child-start
caller and attempt-end work still publish unscoped after awaiting; in a narrow
race they can leave a redundant `recovery_required` on an attempt a pass had
already settled; nothing re-requests a pass for it, so it settles again on the
next pass (root open, next prompt or `/recover`). Caller-layer faults,
including job-stop failure, request recovery after publication. A late joiner
cannot re-fence, re-push or re-seal an already settled attempt.
The release owner's confirmed fault is never discarded for an attempt mismatch:
while its slot excludes startup, a newer attempt can only be an unstarted
followup. The owner fences that attempt so refused startup cannot silently roll
it back and erase release evidence; the delivery path instead fails closed.
Submitted output alone does not establish successful session
retirement; after a quiescent exact reopen it can establish completion under
section 14.2. The owned worker catches
release/publication panics and records a fixed, payload-free retirement failure
before shared completion, even if its requesting caller has been cancelled.
The startup worker also has a drop guard: if it ends without publishing a
release or transferring its exact reservation to a live child, the guard
publishes a fixed, payload-free startup failure, removes that reservation and
requests recovery. Inside a running runtime, neither a panic nor task cancellation
leaves a nonexistent startup owner fencing in-process recovery. Outside a runtime
context, drop logs and skips spawning cleanup rather than panicking; explicit
host teardown owns abandonment. A clean startup release starts a
pass only when recovery remains required; a stale startup completion cannot
fault or publish over a transferred or replacement owner.

A child's delivered terminal result is classified by its authoritative outcome,
not by successful delivery of the result handle. A committed failed turn records
failure (or the existing quota/rate-limit disposition) and can seal resolved
partial work. A recovery-required turn, or failure to publish any terminal
authority, records recovery-required evidence without starting a submit-harness
turn or a new candidate seal (the non-authoritative rescue snapshot in section
5.3 is permitted). Previously submitted output and candidates remain evidence;
they do not alone authorize completion or reuse. The retirement-only quiescent
reopen exception in section 14.2 requires both submitted output and settled
session authority. Close, candidate adoption, replayed
binding, and seal completion cannot clear unresolved session failure. A later
retirement failure retains the original terminal cause as well as its own.
Terminal content reads during retirement use the runtime terminal record's
authenticated publication checkpoint for a retained completed turn. Advancing a
child's history or pruning an intermediate checkpoint must not turn its healthy
terminal into an identity-corruption failure. A retirement join deadline bounds
the caller, not the independently owned release: the child slot remains owned
until the worker returns, and its late successful release is still published.
An already closed node remains closed after an ordinary committed failure. An
unresolved session failure instead makes the node `recovery_required`, with
close intent retained only by the attempt's `closed` status.
Settlement returns the node and attempt to closed, preserves any recorded output for
inspection, and never sends the closing parent a result mail (section 14.2).

Interrupt and close change the node's requested status while retaining actual
active-turn ownership until terminal observation or successful release. A closed
node cannot start another turn, but can still own its unwinding turn. Repeated
close and stop-all therefore cannot seal a copy that is still changing. A close
arriving during an existing seal retains its `sealing` ownership; the attempt
records the close intent. The sole seal completion resolves the node to `closed`
only when no session failure remains unresolved; otherwise it leaves the node
`recovery_required` and the attempt `closed`, even when sealing itself fails.

Attempt-end and close first cancel and retire the live turns of closed children,
then stop jobs for exactly the owners ending or closing in that operation, then
settle their effects. A job promoted between closure and cancellation ends
`stopped` by `attempt ended`, rather than remaining parked. Job stops run in
parallel on blocking workers; every owner is attempted and failures become
per-node retirement faults. Settlement never scans historical ended attempts
or abandons owed effects because stopping a job failed. Attempt-end cleanup
retires every closed child, settles effects, and retires its own live owner
before reporting a cleanup failure.
Seal ownership is a private per-dispatch identity, independent of node status and
retained after the effect queue is drained. Faults cannot authorize a second seal;
only its exact completion consumes ownership, including when an attempt legitimately
reseals an earlier candidate. Retirement refusal keeps the attempt's unresolved
session evidence after close/stop and after either successful or failed seal
completion; a prior close intent is retained, and a seal failure
retains the retirement failure alongside its own evidence. Ownership is transient:
durable reconstruction restores no runnable seal or completion claim. Both
model tools and navigator close/stop-all report retirement failures and preserve
unresolved recovery evidence. Late turn waiters cannot retire a replacement or apply
terminal mutations to a newer attempt or turn.

Process-exit abandonment is separate from joined retirement: the hub closes
admission, aborts and joins its recovery pass before collecting root/child
runtime capabilities, cancels startups and aborts retirements. Live and retiring
child leases are included in the root's batched release, without waiting for
provider settlement or tree publication. A one-second product exit bound is
mandatory; ordinary retirement and interrupt retain the joined contracts below.

Retirement joiners have a 65-second ceiling (the five-second cancellation
settle bound plus `HOST_SESSION_RELEASE_TIMEOUT`, 60 s). The worker continues awaiting
release after this ceiling, records slow-retirement recovery evidence, and logs
the returned cleanup result and total duration. Driver telemetry separately
times tool drain, job-terminal publication, heartbeat join, lease release and
backend flush as cleanup kinds in `durability_attempts`, with joined/failed
outcomes; these observations are not recovery authority.

Shutdown closes admission (no recovery pass starts after that), then gives a
running recovery pass its
own two-second allowance; at expiry it aborts the pass and joins it for at most
250 ms, so no reopen of a child session outlives the leases released next. It
then starts active releases, and joins them together with existing retirements within
one two-second allowance. At expiry it returns a bounded failure and retains
fault evidence, while release workers retain ownership until they return and
may publish their released locator afterward. No new recovery pass starts on
the closed hub. Explicit process-exit abandonment, not ordinary joined shutdown,
aborts these workers.
Admission checks active and retiring slots under the same mutex as the closed
flag. Batch cancellation starts and joins siblings concurrently; its report retains
input order, while independent retirement workers may publish in completion order.
Shutdown returns typed child/tree failures as well as retaining tree fault evidence.
The owning attachment always attempts canonical parent release afterward and keeps
a successful parent locator available for final publication even when a child failed.

Tool interrupt/close receipts remain prepared until cancellation, release, and
locator publication have joined successfully. Replaying a prepared cancellation
returns `operation_recovery_required` without another dispatch; failed retirement
records a failed receipt. Completed receipts replay without touching any later
turn. If the requesting future is cancelled, its independently owned retirement
still settles, while its unfinished operation receipt remains fail-closed under
the existing uncertain-operation contract. No domain-intent receipt is presented
as proof that cancellation completed.

## 17. Known limits

These are shipped gaps, stated so nothing above reads as a promise it does not
keep.

1. **No `+N` marker on truncated conflict paths.** A `merge_conflict` list is
   bounded only by the 512-byte message cap and says nothing when it is cut.
2. **No depth-remaining term on the template card**, and no observed duration.
3. **A snapshot copy minted over a candidate can answer `git status` with "no
   usable git dir".** Cosmetic; the seal and adopt paths are unaffected.
4. **Schema fail-closed message.** A DDL fingerprint drift is refused, but the
   message reads as an ordinary version mismatch.
5. **Storage cost is real.** Each `workspace=fork` `mutate` copy is a true
   worktree; a non-mutate fork is a detached snapshot. Inherit mints nothing.
   Workspace crates fingerprint on absolute path and rebuild per
   worktree, and cargo's exclusive target lock serializes a reviewer behind a
   worker build, so a cold build per copy is the expected cost — roughly
   1–3 GB per worker-and-reviewer pair on this repository.
