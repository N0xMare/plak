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
| `tools` | optional tools enabled by name (`web_search`, `web_fetch`, `x_search`, `fetch_url`, `image_gen`, `mcp:<id>`, `mcp:<id>/<tool>`); default `[]`; a copy of the template gets exactly this list (§3.1) |
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

The built-in profiles (spec 07 §8) ship a depth-1 tree: the root orchestrates
its working set and every role under it is a leaf. A deeper tree is user
configuration: a template whose `spawns` names other templates, plus an
`[orchestration] max_depth` that admits the extra level.

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
Profile creation and genesis have distinct freeze boundaries. Protocol-5
creation records write-once root_profile root-template/working-set names and
the selected root account/model/effort. It does not freeze card/tool bodies.
The process-wide roster source receives that session's optional root_profile
alongside its root binding; saved names resolve through selection_config for
roster, root tools and genesis, never through a mutable global profile choice.
Home default/profile-file edits cannot replace saved names. Template-body
edits before genesis still apply; a missing saved name refuses open. None
retains the home-default genesis path and fills root_profile once. Embedded
NewSession stays payload-free and selects the home default.
Fresh API and native `/new` roots acquire their reserved target topology and
profile; neither borrows the source's attached hub for the new conversation.
An explicit frozen root name and binding do not rewrite a reachable child
card of that same template name; child cards retain their own configured
account/model until their normal genesis freeze.

The roster a genesis freezes is read from the session's own workspace
context: the home's templates and `[roster]`, and a trusted project's
`.plak/config.toml` and `.plak/agents/` over them (a project template wins
on a name collision), so a trusted project's template in the working set is
on the root's menu; an untrusted project contributes nothing. A remote
workspace never causes host-side project discovery using its path spelling:
remote choices come from home configuration and the saved session selection.
The shared roster service resolves each session's context independently.
All `[orchestration]` controls come from the operator home, including in a
trusted project. Project overrides are ignored with a warning and project
writes to these controls are refused (chapter 07).

The epoch is the only spawn authority a session has: `spawn_agent`, its card
text, and the binder all read the one roster its tree's digest covers, and
nothing outside the digest widens it. Changed epoch-scoped orchestration or
roster configuration is applied by `/reload` on an idle root with no live
children or background processes: it carries the full history into a new
session whose tree freezes the current settings at genesis and recompiles the
root's tool descriptions. The roster compared and frozen is the session's own
profile — the root template and working set its tree froze — re-resolved
against the template files as they are now, never the home's default
`[root] agent` / `[roster] working_set`; a source whose templates no longer
load falls back to the home default with a notice (chapter 02 §5.5). The transcript says `orchestration settings changed;
carried into a new session to apply them` and links `carried forward from <id>`.
Only roster and bounds are refrozen: the source account, model, effort,
composition and target selection stay selected, revalidated under the current
login. The carry preserves committed session-owned named JSON state and keeps
replay with validation when its binding is unchanged.
Persisted non-terminal children refuse carry even with no root attachment.
The installed build performs the comparison after the normal reload handoff;
a comparison reads effective bounds, occupancy/reclamation/evidence controls,
working set, tool tables and every template field from the source snapshot and
compares them with the current frozen values, including genesis bound clamping.
Only `digest`, `templates_digest` and `bind_hints` are excluded; tool tables are
compared as TOML values. A changed effective knob or template field, including
description, cost tier, model visibility and template names, applies even when
it came from a changed built-in/default rather than a file edit. Bind hints and
digest/render-recipe changes alone keep exact resume through successive builds;
each hop compares against the tree the preceding hop actually kept.
A cross-state import already creates the desired genesis and is not carried
again. An unreadable config or tree skips apply with a notice and continues
normal reload; settings then apply to new sessions.
Once canonical carry creation commits, any completion failure retains and
selects its successor identity with a repair notice; it is not a refusal that
allows a later reload to carry the source again.
The source keeps its tree and frozen bounds; no config edit silently changes
an existing tree. Unchanged settings keep exact reopen. A successor epoch may
also install on a quiescent tree during a model rebind that stays on it; that
epoch keeps the tree's roster (working set, cards, bounds, tool tables,
account roster and root template) and moves only the root's account, model
and effort, so it never re-reads the home roster. A
profile's **Fork now** is different: the new session opens a **tree of its
own**, keyed by its session id, whose genesis freezes the profile's roster.
The source keeps its tree, epoch, and cards, so its checkpoint reopens with
exactly the tools it sealed and the strict reopen check (spec 01 §2.3) holds,
while the new session is briefed as the profile. Neither session can spawn
from the other's cards. Children already running under the source stay in the
source's tree; Fork now is refused while any are live. Fork now changes only the new
session; the home's default profile is written only by its explicit Set as
default action.

A plain `/fork` (and `/fork-then-compact`) is a new node in a tree of its own
too, with a new agent line. Before the runtime fork, the host writes the fork's
genesis tree, keyed by its session id and with its root agent named the same,
through the tree store: epoch 0 is the source tree's current epoch, byte for
byte, so the fork is briefed with exactly the roster and cards the source runs
now, and `forked_from` records the source node (tree and root agent), its
line, the branch the fork copied and the source epoch number. Nothing else
travels: no nodes, mail, workspaces, candidates, operations or successor
epochs, so a source candidate is unknown to the fork and refused there. The
fork's tools compile over a detached copy of that tree (no lease), and its
attach owns the tree from then on. The runtime fork copies the job counter
(`next_id`) and no job rows (chapter 01 §5.6). The source's tree, children and
cards are untouched; a fork is refused while that tree has a live descendant
or a spawn mid-mint. A source without a tree seeds nothing, and the fork
bootstraps its own tree on first attach like any new session. When the
runtime fork definitely did not commit (its runtime build failed, it was
interrupted before dispatch, or the runtime refused it outright), the host
deletes the seeded tree again, compare-and-swapped on the revision the seed
published and under a custody claim like every tree write (the seed's save
and this delete each take one for the write, §14). A process that dies
before the runtime fork commits leaves the seed behind; the source's next
open withdraws the unsettled fork request (chapter 02 §2.6) and then deletes
the tree under the fork's id the same way, provided no product record exists
under that id (a fork that published owns its tree) and the tree is a fork
genesis whose root is its only node, or only the custody record when the seed
died before its rows. Tree resolution follows a session's lineage only while its
product node is its source's (a rebind that stayed on its source's tree); a
record whose node is the root of a tree named after it (a new session,
`/new`, a fork, an import or carry, a rebind onto a tree of its own) resolves
that tree and never its predecessor's.

All `[orchestration]` bounds are carried in the digest, including the fixed
per-cell agent budget: a ceiling that decides whether a spawn is admitted is
bind authority like the other four.

A tree holds its genesis snapshot as epoch 0 in its header, successor epochs
as `agent_tree_epochs` rows numbered from 1, and `current_epoch`, the epoch the
root follows and root spawns bind (§14). Each node records the `epoch_no` it was
spawned under and the spawning parent's composition (`parent_composition`), the
one profile-dependent spawn-parent input: everything else a child's bind reads
from its parent (tree, workspace root, product home, state roots, compaction,
round bound, topology admission) belongs to the session, not its profile. A
templated child's bind does not read `parent_composition`: its own card in its
own epoch fixes its composition, and a card that sets none is `FollowCatalog`,
which is itself a pin, not an inheritance of the parent's composition. The
recorded value reaches a bind only for a node with no template binding, which
no flow creates today. A store load refuses a current or node epoch the tree
does not hold and an epoch row whose digest is not its snapshot's.

No reader sees "the" snapshot; each names whose epoch it reads:

- **Spawn authority** is the caller's epoch: the menu, the cards, the
  capability lines, the bind a spawn freezes, review-evidence policy at adopt
  and the caller's per-cell agent budget. The root's epoch is always the
  current one; any other node's is its `epoch_no`, and a child is spawned under
  its caller's epoch.
- **A node's bind, reopen and lifecycle** use the node's own epoch: its card's
  tools, tier, composition and frozen tool tables, its turn ceiling, its mint
  hook and its transient-resume bound.
- **Housekeeping** uses the current epoch: retention, checkout reclamation and
  its idle interval. So do the root's own surfaces (the profile, the root
  template, the settings cards and the carry comparison).
- **The reopen gate** checks the digest of every retained epoch.

`append_epoch` adds the next epoch and makes it current. The root moves to it
(its bind, spawn ceilings and per-prompt attempt bound), while every other node
keeps its epoch, cards, grant ceilings and cell budget, so live descendants do
not block it. It refuses a root spawn mid-reservation (a child's spawn binds the
child's own epoch and does not block it) and a snapshot whose digest does not
match, keeps the admission state (it is not a resume and does not lift a stop),
and first folds successor epochs no non-root node references; epoch 0 stays as
the header. At most eight epochs are referenced at once (the new current one
and every non-root node's), and an append past that is refused
`bound_exceeded`. Finished and closed agents count: a node keeps the epoch it
was spawned under for as long as its row exists, because a followup can revive
it on that epoch's card, so its epoch is released only when retention removes
the row (§6.7). The refusal says so and points at a new session; it never asks
the operator to close agents. A profile's **Switch now** (chapter 02 §5.5)
appends the profile's epoch to the session's own tree, with live descendants:
before the runtime publication of the switch's successor the tree records the
epoch's exact bytes as an `epoch_pending` row, the successor's root compiles
its spawn menu, cards and briefing against those bytes appended to a detached
copy of the tree, and once the successor is published the live tree appends
exactly those bytes and removes the row (§14.3). A switch whose profile differs
from the current epoch only in the root's account, model and effort appends
nothing: the tree's root binding (the root node's account, tuple and effort)
is a view of the line head and moves alone, also with live descendants. A
roster apply on a quiescent tree appends when a non-root node still references
the current epoch, so a finished child keeps its card, and replaces the current
epoch in place only when nothing but the root references it. The root's
per-prompt attempt bound is epoch state: an install, an append and every reopen
derive it from the current epoch. Each spawn records its caller's composition
as the child's `parent_composition`, and a child's bind and reopen read that
frozen value rather than the composition its parent runs now, so a switch of
the parent never changes it. A template card may carry a `runner` name
that places a child on that runner rather than its parent's. The field is
frozen on the card inside the snapshot JSON and its digest only when set, so a
card without one keeps its encoding and digest. No template file sets it yet,
and every node is placed on the local runner.

The snapshot also freezes the product's **account roster** as two membership
sets: which accounts a template card may pin at all, and which speak no native
protocol (so a `native-ptc` card on them is refused). The tree itself names no
account: it only asks whether the card's account is in the frozen set, so every
spawn path — a product binder, or a binder-less JSONL `invoke_tool` — refuses
the same card the same way. The roster is carried in the epoch digest, so a
change to it needs a successor epoch. The API accounts `anthropic` and
`xai-api` belong to both sets: they are admitted accounts and refuse native
PTC. OpenAI API admits native PTC only on enrolled CodeModeOnly models, so
its per-model admission stays at the product boundary.

The product adds the exact named references reached by the frozen template
closure to those existing string sets. Classification of
a named reference uses its vertical, but membership and credential selection
keep the complete reference. An un-enrolled sibling name remains inadmissible;
the tree never parses provider names or falls back to a default account. With
no named child references these sets and their epoch digest bytes are unchanged.
A root-only account change moves the line binding, not this card-membership
roster or the epoch; the root receives its own exact runtime admission.

The host supplies one child controller that declares which child services it
provides (binding spawned children, mail delivery, live-turn cancellation and
recovery); an undeclared service behaves exactly as if the host had none.

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

A child runs on its parent’s runner. Root genesis freezes explicit runner
placement before publication; reopen verifies that saved identity against node
services and installs its VCS port. Local remains recorded as none. Workspace
stamp, mint-base and occupancy lookups are qualified by `(runner_id, path)`;
equal paths on different machines share no stamp or base. The tree admits a spawn placed elsewhere only by the rules of the
runner split: an `inherit` child shares its parent's checkout, so another
runner is `runner_mismatch`; a `fork` or `snapshot` copy on another runner is
carried there as a git bundle of its parent's live state (§14) and the child
is placed on that runner. Such a copy is `cross_runner_copy_unavailable` when
the tree's VCS port reaches no such runner (`VcsOps::transfer_port`), the
parent is on the local runner, or the spawn names candidate inputs. Both
refusals reserve nothing. The library's `spawn_reserve_on` names the runner;
no spawn tool argument or card field does yet, and the product's port
reaches only the selected runner, so the product never places a child
elsewhere.

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

Native child materialization does not independently probe the catalog before
provider resolution. Source-backed enrollment chooses only the tool-presentation
candidate; resolution's fresh exact authenticated row must affirm the model,
native tool mode and any named processing tier before a session is created.
If the authenticated row's tool mode has drifted from release enrollment, child
binding fails closed with the model and both modes and asks the caller to choose
another model. Unlike root `/model` rebind, it does not derive a new presentation
from that row.
That resolution performs one catalog GET per operation. On a transient lookup
failure it may repeat the whole operation once under chapter 01 §6.2's policy.
Resolution and its retry nest inside the caller's existing 120-second startup
wait for creation, acknowledgement and registration, starting before startup
reservation; they do not receive a fresh 120 seconds. The resolver returns at
least ten seconds before that cut, and retries only if a delay of at most 15
seconds, a full 30-second lookup and the ten-second startup margin fit. It
releases the compatibility flock, lifecycle lock and Ready lease before waiting.
An overlong `Retry-After` returns the lookup cause without sleeping, rather than
letting the startup cut turn it into recovery-required. Authentication,
policy, absent-model and invalid-evidence failures never enter that retry.
Reopen/re-task and recovery resolution have no caller startup deadline: they
use only chapter 01's 120-second operation bound, with the same ten-second
reservation.
Materialization failures preserve the redaction-safe cause, and exhausted
lookups preserve the total physical attempt count, without exposing response
prose or credentials.

A child bound to SuperGrok or Codex OAuth outlives its admitted catalog window
(6 h) the way a root does (chapter 01 §7.4): its runtime carries the host's
reresolver, so a child started, followed up or woken by mail after the window,
or one whose running attempt crosses it, continues between model rounds on a
fresh exact resolution of its own tuple, charged to its own turn. A refused
re-resolution (a changed login, account, catalog row or model semantics, or an
unusable credential) fails that attempt with the remedy before any inference;
it never falls back to the caller's model or account.

Refusal messages below the product boundary are neutral facts. Child-account
admission preserves the exact template, original account and credential-gate
state as typed advice; incompatible-session recovery preserves the exact source
session UUID. Topology tool error payloads keep their code/message and may carry
this closed structured advice and an optional semantic remedy inside the existing
tool result. Child startup, reopen and mail delivery retain both when composing
runtime failures; rollback or recovery must not flatten them into prose. Plak's TUI, run
and JSONL tool-detail renderers restore the corresponding login, exact tree
resume and export/import instructions; model-facing results stay neutral.
Neither the error code alone when it is broader than the refusal, nor rendered
message substrings, are a sufficient classifier. Tree pause/rebind refusals
remain typed across the host boundary. Session-deletion abandonment retains its
durable decision/failure kind; Plak adds the historical
`session delete --abandon-children` provenance from that kind, not from prose.

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
retirement runs. Explicit bounded cancellation joiners use the retirement
release deadline (the cancel settle plus `HOST_SESSION_RELEASE_TIMEOUT`,
section 16), recording a scoped joiner fault on expiry while retaining ownership.
Tool and navigator close/interrupt instead return after requesting startup
cancellation and join it in the background; shutdown uses its own bound.
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
and retires the attachment. A recovery reopen settles that recorded
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
| `list_agents` | yes | yes | yes |
| `send_agent_message` | yes | yes | yes |
| `submit_result` | **no** | yes | yes |
| `adopt_candidate` | yes | **no** | only with `may_adopt` |
| `discard_workspace` | yes | no | yes |

A child's grant carries `may_spawn` when its own template's menu (its `spawns`
list minus itself, as frozen in the epoch) is non-empty and depth remains; the
root's grant always carries it. The compile rules are exact: `spawn_agent`,
`wait_agent`, `interrupt_agent` and `close_agent` require
`may_spawn` **and** remaining depth, and `spawn_agent` is additionally withheld
while the node's menu is empty; `list_agents` and `send_agent_message` require
`may_spawn` or a non-empty message-peer set; `submit_result` requires the node
not be the root; `adopt_candidate` requires `may_adopt`, which a template may
set only alongside `spawns` naming another template, so a leaf child never
carries it;
`discard_workspace` requires `may_spawn`.

Recovery is never a model tool: the host recovers children itself (§14), and
the operator's per-child retry is the Agents list's `u` or a JSONL
`invoke_tool` of `recover_agent` (§4.5a), which no catalog presents.

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
frozen card's list, never its parent's. The root's optional tools are a
property of the session, decided once when its runtime is first created: a
session created on its `[root] agent` template's exact account and model
freezes that template (its account, model and list), the `[tools.*]` /
`[mcp_servers.*]` tables it reaches and the MCP tools the list reached then
into the session record; a session created on any other tuple (a root bound by
explicit `--account` / `--model`) freezes none and has no optional tools for
its life. Reopen, clear and rebind read only that record, and its clear, fork
and rebind successors inherit it and bind the same selection, sections and MCP
records whatever tuple they move to, so canonical tool authority never changes
across a rebind (chapter 01 durable rebind). A rebind whose target route
cannot serve one of the frozen tools refuses before any durable work with one
line naming the tool and the remedy (`/new`, or a new session on that
model). Editing the template or a table changes new sessions only. A wildcard
`mcp:<id>` tool omitted at creation leaves one Settings notice in the
transcript.

### 3.1 The spawn graph is the capability grant

A child gets exactly its own template's `tools`, whatever its parent was bound
with. The grant is the spawn graph itself: a node can start a template only
when that template is on its menu (the root's working set, or its own card's
`spawns` list), so whoever writes the roster and the `spawns` lists decides
which tools each node can reach, directly or through a child. There is no
parent-subset rule: neither the configuration load nor `spawn_agent` compares
a child's list with its parent's, and a root with no optional tools may spawn
a template that lists `fetch_url`, `image_gen` or an MCP tool. Every list
still passes the reference checks at load (each `mcp:<id>` names an
`[mcp_servers.<id>]` table, each `mcp:<id>/<tool>` a tool that table allows),
and every bind still refuses a tool its route cannot serve. The edit ceiling
(`access`) keeps its clip-not-refuse rule.

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

Before template admission, binding, minting or tree writes, the hosted binder
asks the product store's capacity probe. A product-home volume below the
512 MiB admission floor refuses with `storage_low` and a path-free storage reason
(chapter 02 §2.2). No child or attempt is created. This affects only new
spawns, not running children, mail, retirement or recovery. A shared session
store past the children's mark refuses the same way (§10.1).
The model-facing refusal says the operator's storage is below the admission
floor and instructs the agent not to delete files or retry; only operator
doctor output names the volume. `capacity` retains its running-set meaning.

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

Typed refusals: `capacity` (carrying the running set), `storage_low`, `spawn_budget`,
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

A textual Git conflict candidate is also a valid input. Its resolver starts
with the pinned conflicted files, but its recorded merge base and copy-base
pin are the original adopter's target snapshot, not the marker-bearing input.
The resolver's sealed work records `resolves` as the original source candidate.
Only that conflict's target may adopt its resolution.

### 4.2 `list_agents`

JJ ahead counts non-empty commits in `base..@`. Behind counts non-empty
commits in `base-..parent@`, excluding commits whose tree equals the recorded
F tree: copied mint state is not new parent work. Fresh forks, even over a
dirty parent, report `(0, 0)`; a subsequent parent edit contributes one behind.

Git ahead counts commits in `<base>..HEAD` on the child whose tree differs
from the base tree, plus one when
the child work tree differs from the `<base>` tree: a tracked non-secret
change whose work-tree bytes are not the base blob, or an untracked
non-secret path `<base>` does not contain. Uncommitted work that already
matches `<base>` — an adopted child's files, still dirty against its own
HEAD — adds nothing, as does a commit of exactly that adopted tree. Work-tree
comparison considers only paths changed against HEAD, so a committed new path
cannot be counted again merely because unrelated mint-time dirt remains.
Git behind counts
parent commits since `<base>` whose tree differs from the base tree, plus
one when the parent work tree has a non-secret change that is neither in
HEAD nor in `<base>`. The same fresh-fork contract holds: a dirty parent
is `(0, 0)`, one later parent edit is one behind, and a parent commit of
the mint-time dirt adds nothing. No recorded base omits `ahead` and
`behind`. Base blobs are read in one `ls-tree -r -z` query and working files
are hashed in a batch with `hash-object --stdin-paths` (exceptional quoted or
newline names use individual reads). Hashing never writes objects and uses
`--no-filters`: raw work-tree bytes must equal the base blob to be considered
mint-time dirt; clean/smudge transformations are not run by the hash probe.
The comparison disables rename detection (`--no-renames`).
Reads use the 30 s query ceiling and `GIT_OPTIONAL_LOCKS=0`,
and write no objects. Each occupancy-probe run memoizes Git parent observations
by (parent path, base); the cache is never reused by a later render.

A row that names `candidate` also names non-empty `candidate_commit` and
`candidate_base` from that record. Compare those immutable snapshots using
the checkout's VCS (`git diff <candidate_base> <candidate_commit>` on Git).
Candidate-bearing rows expose `candidate_kind` (`work` or `conflict`), and applicable
`source_candidate`, `conflict_target`, `conflict_paths`, `resolves` and
`adopted_via`. `candidates` lists retained unadopted candidate objects with
the same metadata and immutable commit/base ids, including conflict candidates
which do not replace the attempt's ordinary candidate. A displayed conflict-path
list keeps at most 16 whole `{path, kind}` entries and marks truncation;
the durable record retains every path.

Arguments, optional booleans: `include_self`, `include_completed`, `include_read`.

Receipt: `{ revision, agents: [...], inbox: [...] }`. Each `inbox[]` row
carries `message_id`, `from`, `purpose`, `priority`, `in_reply_to`, the first
`INBOX_BODY_BYTES` = 2048 bytes of unread `body`, its full `body_bytes`, and —
for a host-authored `result` only — `attempt_id`, `status`, and `candidate`.

By default `inbox[]` contains only unacknowledged mail. `include_read=true`
adds the newest 64 acknowledged header records per recipient, newest first:
`message_id`, `from`, `purpose`, `priority`, `in_reply_to`, `body_bytes` and a
UTF-8-safe 160-byte `body` preview, with unread records still bounded to `MAX_MAILBOX_RECORDS` rows. Listing does not
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
| `refresh` | followup only; default catches up live-fork copies when clean, `false` keeps their base; explicit `true` on a snapshot or candidate-minted copy is `schema_mismatch` |

`purpose=result` is host-authored and MUST be refused from a model. There is no
`delegate` purpose: it would also replace a running task.

Receipt: `{ message_id, disposition, refresh? }`. `refresh` reports
`status: refreshed | skipped | conflict`, the new `base` on success, or bounded
`reason` / conflict `paths` and `paths_total` (at most 16 entries, with
`paths_truncated` when cut). Unchanged and design-ineligible copies omit it.
The same result contributes one bounded host-authored line to the child's Start
mail; the line counts toward the queued-mail budget, not the caller's 16 KiB
request-body limit. `disposition` is one of `started`, `queued`,
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
is `settled`, `mail`, `timeout`, `steer`, `notice`, or `process`. The receipt has no
presentation subject. The await attaches a host-only presentation subject to
its tool outcome: the comma-joined `agent_id`s, bounded to 240 bytes (complete
ids; a single longer id is clipped with one trailing `…`), omitted when
`agents` names none. That subject is not model-visible. A restored bare wait
reads it from the result observation, not from the 2 KiB preview. A human or unwatched-parent
steer returns no steer text. `ToolPreempt::Notice` likewise returns early with
`woke_by=notice`, the current agent/job rows and no notice text; host information
keeps its existing consumer. Like a steer-only wake, a notice-only wake saves
no durable receipt. Newly terminal process ids are always retained in
`processes`; a process wake takes precedence over a simultaneous Notice
(not over consumed mail or a steer). Watched urgent mail still returns `mail` with its
body through the sole mail consumer. A process wake adds `processes: ["p3"]`:
only terminal transitions of the caller's own jobs after wait admission wake it,
using an allocation-local terminal generation. Previously terminal jobs do not
wake a new wait. No non-mail early wake saves a durable receipt. The grant
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
(`running`, `exited`, `stopped`, `released`, `unknown` or `already_reported`) read from the process service as
a snapshot, which consumes no notice. `wait_agent` names no processes, so its
receipt never carries `jobs`. `released` records explicit ownership relinquishment,
not an inferred exit or permission to signal the former process. Every presentation returns the same receipt and
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

Any wait return may carry its unread envelopes in arrival order in `mail[]`:
`message_id`, `from`, `purpose`, `priority`, `in_reply_to`, `body_bytes`, full
`body` up to `MAX_MESSAGE_BYTES`, and the inbox envelope metadata. A Result envelope whose attempt is in `agents[]` is acknowledged
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
| `waiting_on` | while the child is parked (§5.2), or waiting for a transient continuation: `network: true` names live route readiness; otherwise `{ processes: ["p3"], agents: ["c2"] }`, its own running processes and its children that are still live work; both lists empty is the explicit parked marker (an arrival already woke it); absent while a wake is admitted or the attempt is ending; process-local |
| `cause` | on an interrupted child, why it stopped: `owner_lost`, `handoff`, `operator`, `parent`, `cascade` or `stopped` (the cause its current attempt's latest interrupt recorded, §14) |
| `continuation` | on a child a lost or handed-off host interrupted, where its continuation stands: `held` (waiting for the operator's Resume), `pending` (consent recorded, the turn not yet admitted) or `running`; absent once it ends (§14.1) |

A wait returns when a watched child **settles, asks a question, replies, or
sends urgent mail**. Deferred finding/coordinate mail does not wake it; it
remains unread and rides any later return (settlement, waking mail, timeout,
steer or process), under the same budget order. A node that is still `sealing`
has **not** settled, so the wait holds through the seal — which is also when a
candidate can be named. An interrupted node does settle a wait but produces no
result for it. A timeout is not cancellation: `timed_out: true` means call
again. When the waiting agent's own turn is interrupted or settles while it
waits, the wait is abandoned with `wait_caller_settled`; nothing about the
watched children changed.

A **bare** wait (no `agent_ids`) delivers each settled row once. `Submitted`,
`Completed` and `Adopted` of the same `attempt_id` match one delivered output;
other transitions are new. Named waits still re-read output. A bare wait whose
only pending items are non-waking mail returns that mail immediately rather
than parking, with `woke_by=settled` (only waking mail returns `woke_by=mail`).
Once a wait
has returned a child's settled row, later bare waits by the same caller leave
that child out until its row changes (a follow-up attempt, a new status outside
the output-bearing class), so a
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
receipts by these rules, live and on every open alike. Live means while the
caller's turn runs too: a child's feed is applied as its records arrive, and
the root host applies the root's feed inline once per model round of a running
root turn (a round starts after the previous round's tool results committed),
besides its passes between turns. The root's tree projector runs at exactly
four points, structurally, in the root's actor: per model round at the
`ModelStarted` prefix; after the command acknowledgement; at prompt admission
(and host-wake admission), to the tail; and when a turn settles or the actor
enters idle without one (open, in-place recovery, an idle-only workflow,
release), to the tail. Nothing else runs it: neither a child's notification,
an intent, nor an idle observation tick. That tick still reclaims dormant
checkouts and collects dormant children, which is tree housekeeping, not a
pass over the root's feed. Each running pass stops at the committed
prefix captured by that round's `ModelStarted` event, even when the runtime
has advanced before the host observes it. Observation latency cannot coalesce
later rounds into an earlier cursor publication. A prefix behind retention is
deferred to the between-turn refold rather than seeded from a newer head.
The post-acknowledgement pass likewise stops at the command acknowledgement's
publication sequence; an already-running first round cannot extend it, and
this bounded pass never refolds.
A root wait in model history therefore
retires by the next round; a nested wait (from a Code Mode cell) and an
operator `invoke_tool` await hold their receipts until the caller's terminal.
That running-turn pass applies only the records past the cursor; a page that
would need a refold is left for the settled-turn pass or the next prompt's
admission pass, both of which own unbounded refolding.
A round without a committed prefix skips the running pass and logs projection
debt once per turn; durable product rounds assert that their prefix exists.
The rules:

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
successful initial response acknowledges the durable intent and cancellation
dispatch when observed, not completed release. The dispatch signal fires only
after the exact live owner's cancellation request has been issued, never merely
because its release future was first polled. The tool waits at most the five-second
cancel-settle allowance for that signal. Expiry or a dropped signal sender still
acknowledges the saved intent, without claiming dispatch or completed cancellation;
the owner continues release and publishes any fault. Once dispatch is signalled, the initial answer is always
the intent acknowledgement, even if release fails before that answer is polled.
Such a failure appears only through `recovery_required` and the owner's retirement
notice, not as a scheduling-dependent initial tool failure. A close with no live
canceller explicitly acknowledges its intent as well. Release and the operation
receipt settle in the background; replay while they are pending refuses rather
than dispatching
again. The navigator's close, interrupt and stop-all have the same prompt-return
contract. A
`recovery_required` node whose session outcome is unresolved is not closed:
its row reports `recovery_required`, keeps its attempt failure, and authorizes
no retry: the host recovers it (§14.2). Two kinds of `recovery_required` node
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

### 4.5a The operator's retry of one child's recovery

Recovery is the host's job, and no model is granted a recovery tool. The
operator keeps one retry per child. JSONL `invoke_tool` of `recover_agent`,
`{ agent_id, expected_attempt_id? }`, an operator-only name no catalog
presents, settles a `recovery_required` child through the one supported path,
the same exact session reopen a relaunch performs, and returns
`{ agent_id, attempt_id, recovered, status, attempt, failure_kind }`. It runs
on the operator's port whichever node the call names as its caller: any child
of a live or terminal-cleanup tree, never the root (`grant_denied`); a stale
`expected_attempt_id` is `attempt_mismatch`. It writes no operation receipt
and needs none: a node that is not waiting on recovery returns its current
state with `recovered: false`, so a repeat is idempotent. When the session
part settles but an interrupted seal, adopt, or copy removal still holds the
node, the receipt also carries `workspace_recovery`, naming that blocker;
`close_agent` then ends the node.

The Agents list's `u`, after its confirmation pop-up, is the same port for
one child: `RecoverTopologyNode` names the tree, the snapshot revision, the
agent and the attempt. The host refuses a different tree (`this session has a different tree`), a revision that
has moved (`stale tree revision`), a closed tree (`tree is closed`),
a root, a child that is not `failed` or `recovery_required` (or has no attempt),
or a different attempt
(`attempt mismatch`) before it calls this port. Failed-idle is eligible: the
same predicate the Agents list uses, with no model-grant check, and the same
tree-state guard as the JSONL retry (`live`, `interrupted`, or terminal
cleanup). A matching call is
`ProductChildRecoverer::recover` with that attempt. It is not the root
`Recover` intent. The call
runs on the host executor without holding the tree mutex across the lease
wait. The hub tracks that task with the tree's other `recover_node` runs.
Shutdown waits up to `HOST_SHUTDOWN_RELEASE_TIMEOUT`, then aborts it, before
any lease is released. Quit or `/reload` uses that same shutdown.

The operator's retry is the one trigger that waits on a lease: it waits up to 35
seconds for a lapsed owner's lease and then refuses. It polls: each try holds
the child for one recovery and never waits on the lease, and the child is
released between tries, every two seconds, so a held lease never stalls a
recycle or another recovery of it. What it settles,
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
`recovery_required`; the operator's retry of such a quiescent reopen
interrupts the child with cause `owner_lost`, attempt kept (law L, §14.1).
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
The operation receipt retains `{turn_token, output_sha256, output_bytes}`,
not the output itself. Its request digest is SHA-256 of the full canonical
request. Every topology tool replays by that digest and tool name, including
refused calls; a differing retry of the same operation id requires recovery.

### 4.7 `adopt_candidate`

`{ candidate_id, expected_target?, evidence?: [attempt_id], accept_conflicts?: bool, revert?: bool }`.
`accept_conflicts` defaults false. It explicitly allows text markers into the
caller's checkout, including the root's launch checkout. Receipt:
`{ candidate_id, target_before, target_after, files_changed, files?,
files_truncated?, reverted? }`. `reverted:true` identifies a reversal (§7.6).
`files` names at most 64 paths as `"M src/lib.rs"` or
`"R old.rs -> new.rs"`; `files_changed` is the full count and
`files_truncated` is set when the list was cut. A receipt from an older build
omits both. Non-UTF-8 file names use replacement characters in the receipt;
the landing still preserves the original name bytes. Section 7 owns the semantics.
JJ receipts currently omit `files` and `files_truncated` even when
`files_changed` is nonzero; listing changed paths is currently Git-only.

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
unadopted candidate (the checks above), is removed the
same way: marked `discarding`, published, removed, then `discarded`. An adopted
candidate's content already lives in its adopter, so its copy is disposable
once its occupants close; an open finished node retains its copy and base for
followup attempts. Removal drops only the copy's base pin (§6.1), not a retained
candidate's pins. Dependent copies pin their own bases. A
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
removed; only its `plak/<tree8>/<agent_id>` branch is then dropped.

### 4.9 Error codes

The complete `TopologyErrorCode` set, in wire spelling:

`unknown_field`, `schema_mismatch`, `bound_exceeded`, `grant_denied`,
`self_send`, `recipient_closed`, `recipient_attempt_terminal`,
`recipient_recovery_required`, `reply_invalid`, `mailbox_full`,
`spawn_budget`, `capacity`, `storage_low`, `request_key_conflict`, `depth_exhausted`,
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
`recovery_vcs_ambiguous`, `recovery_session_unavailable`, `runner_mismatch`,
`cross_runner_copy_unavailable`.

`lease_saturated`, `class_not_pinned`, and `merge_not_owner` are not codes and
MUST NOT deserialize.

## 5. Attempts

**The attempt, not the node, is the unit of work.** `status`, `turn_token`,
`turn_id`, `submit_harness_nudge`, the `blocked` reason, the
`failure`, the per-attempt usage row, `retry_of`, `request_key`, and
`candidate_id` all live on `agent_attempts`, keyed `(tree_id, attempt_id)`.
`attempt_id` is `<agent_id>-a<seq>`.
The in-memory attempt's exact `output_json` is joined from its submitted
intent on load and fold; it has no duplicate attempt-table column.

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

The tree store is schema **8**, retaining this admitted identity, the nested `spawns` roster, and grant bounds up to the 64-child and 65-node ceilings. Another schema lives
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

Root and child idle wakes use one hosted coalescer with role policies. A root
fires only for process notices and Result or Question mail after a 2 s quiet
window, restarted when its arrivals change. A child coalesces for a fixed 1 s
after its trigger, with a separate 1 s safety tick behind source notifications;
claimed mail delays that trigger. Source reads, ownership guards and product
suppression remain role-owned. Shared identity formatters preserve both command
prefixes and the exact prompt bytes. Root preparation reuses the ordinary queue
command preparation; its admission and settlement remain on that queue. Child
admission and wake settlement remain in the child adapter. A child sleeps on
Wait, re-observes its sources and admits the wake only on Fire.

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

Before opening the new attempt, a Git fork minted from its parent's live state
merges the parent's current state into the copy when its whole-worktree merge
is clean (§6.8). `refresh:false` opts out. Copies minted over a candidate,
including reviewers and resolvers, keep their pinned base;
snapshot and inherit copies are not refreshed. A conflicting catch-up is refused
without applying anything: followup proceeds at the old base and both parent and
child receive the paths. Running jobs, detached/wrong HEAD, independent staged
content and backend errors skip catch-up with a bounded reason.

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

On a successful landing the copy's base advances to the **adopted
state** only when still equal to that candidate's recorded base. An older
candidate sealed before catch-up remains adoptable over its own base without
moving the refreshed copy base or its pin. Otherwise the adopted candidate's
own snapshot is the new base on both backends. The next candidate is
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

Copy operations other than mint and the adopt target dispatch by artifact
layout, not by the launch checkout's current VCS. The exact root
`<launch>/.plak/worktrees/<id>` is git; `<launch>/.plak/workspaces/<id>` is jj.
Only the leaf's parent and grandparent classify a copy, through one shared
layout helper; nested or unrelated paths are detected from that path.
After `jj git init --colocate`, a git copy
still seals, unpins, unmints, and reports dirty and ahead/behind through git.
Adopting a git candidate into a checkout that now detects as jj is
`mint_unavailable` (`git candidate into a jj target: not yet supported`).

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
`<tree8>-<agent_id>` and `tree8 = sha256(tree_id)[..8]`. One shared naming
function maps this identity to the `plak/<tree8>/<agent_id>` branch and
`plak-<tree8>-<agent_id>` JJ workspace. Consequently two trees sharing a checkout may both
use the logical child id `c1`, and delayed cleanup from a crashed or partially
pruned tree cannot address the other tree's path or branch. The base is the
supplied candidate, or `snapshot(<parent cwd>)` — never the launch HEAD. The
argv is `git worktree add` plus `-B plak/<tree8>/<agent_id>` for a `fork` (a stale
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
plak/<tree8>/<agent_id>` when that branch exists. Child-id allocation therefore does not
skip a `.plak/worktrees/<copy_id>` the tree has no row for.

**Seal.** `snapshot(<child path>)`, then one `git update-ref --stdin`
transaction publishes `refs/plak/<namespace>/candidates/<candidate_id>` and
`refs/plak/<namespace>/candidate-bases/<candidate_id>` for the candidate and
its recorded base. Both updates prepare before either is committed, inside the
child, so discarding the worktree and the `plak/<tree8>/<agent_id>` branch cannot
orphan the candidate. The child's own `HEAD` is deliberately **not** advanced;
adoption never needs a checkout.
Model-visible candidate ids stay short: `cand-<attempt_id>` (the node id
when no attempt exists). Physical pins are `refs/plak/<namespace>/candidates/<id>`,
where the namespace is the first 16 hex digits of sha256(tree_id). Before
pinning, a private immutable ownership file under the Git common directory
records the full digest; a short-prefix collision refuses the seal and retains
existing artifacts. Reservation and ref publication hold one repository
namespace-directory lock. Pruning drops an owned namespace's immutable evidence
only after its last pin and branch are gone, under that same lock; concurrent seals cannot
publish across the empty-ref check. Tickets resolve the short id in their own tree namespace,
so one tree's cleanup cannot unpin another tree's adoptable work. Worktree paths
and fork branches use the eight-hex prefix. Before minting, the same lock
protects `plak-namespaces/<tree8>` containing the full digest. A different owner
refuses with `mint_unavailable`, `tree prefix collision with another tree in
this checkout; existing artifacts retained`, before touching a copy or branch.
Cleanup removes this evidence with the 16-hex evidence only when both
`refs/heads/plak/<tree8>/` and `refs/plak/<namespace>/` are empty.
The durable 16-hex witness precedes short-claim publication and outlives its
deletion, so an interrupted reservation or cleanup remains discoverable.

**Discard.** `git worktree remove --force <dest>`, then the directory, then
`git branch -D plak/<tree8>/<agent_id>`. Only a copy whose candidates are all adopted,
or whose settled owner explicitly gives up its candidates, is removed (§4.8),
and once it is gone the tree drops `refs/plak/<namespace>/bases/<agent>`.
Candidate records retain their candidate, candidate-base and adopted-before/
after pins until record collection. A retained adopted candidate remains a
spawn input after its checkout is removed. Explicit discard relinquishes
unadopted candidate pins. Cleanup retains durable ownership until all of a
collected record's refs have been removed successfully.

Mint creates the copy's `bases/<agent>` pin after worktree creation and before
installing the fence. Reclaiming an orphan mint removes its stale base pin
before the new create-only publication. Adoption pins the target snapshot at
`adopted/<id>-before` and the exact merged tree's wrapper at
`adopted/<id>-after` in one transaction after applying the patch. That wrapper
has the target snapshot as its parent; it does not race a second working-tree
snapshot or advance the target branch. Native JJ publishes the same candidate
base and adoption ref families through its Git backend.
For a producing copy that is still present, the adoption transaction also
advances its base pin to the candidate, matching the workspace row's new base.
Collecting the candidate record therefore cannot orphan an open copy's base.
Reopen queues copy-base reconciliation for interrupted `adopting` rows once per
host generation. Copies with missing bases or unknown producers are skipped,
and so is a copy the VCS port answers is absent; a port that cannot answer
fails that workspace's reconciliation with the `reopen to retry` remedy rather
than skip it. Attaching the tree to the hub drains and completes this dedicated
queue before the tree is published there; ordinary mutation effects cannot
take or discard it. A queued
reconciliation must complete before that workspace can admit an adoption retry;
no other settle queues it again. Attachment acknowledges the reopened generation
before changing refs; a refused publication changes no pin and fails that
workspace's reconciliation with the `reopen to retry` remedy, never a silent
pending hold. A failure records a
workspace-local VCS fault and holds its retries until reopen, without failing
unrelated topology effects. A successful reconciliation republishes the pin
from the committed workspace row before hosted fence construction. It does not
decide whether the landing applied: the VCS recovery hold remains and no files
are changed or retried. A retried landing cannot race a queued reconciliation.
A provenance-publication failure after applying files is
`workspace_recovery_required`: retained target files must be inspected before
another landing; it is not reported as an unavailable operation that did nothing.
Publication failure on a zero-file landing is `mint_unavailable` unless this
is a retry of an unsettled interrupted adoption. That retry retains its
`workspace_recovery_required` hold: earlier target files may already have landed.
Any failed retry of an unsettled interrupted adoption — conflict, target changed,
unavailable or recovery required — retains the `adopting` row and recovery hold.
The operator inspects the target, then uses `close_agent`; the copy and its
provenance pins remain until tree deletion.

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

Plak defaults to Git-only copies. Its opt-in `jj` feature enables the native
`jj-lib` backend (also enabled by the maintainer build). A build without JJ
refuses JJ-only repositories with `this build has no jj support; use a build
with the jj feature`. In a colocated repository it uses Git copies and reports
that choice in doctor and once when creating a session. With JJ enabled, JJ
takes precedence over colocated Git.

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
working-copy commit over an immutable mint base. With no candidate input, the
parent is snapshotted and a frozen base F is written with the parent's tree
and parent IDs, described `plak base <copy_id>`: F is a sibling of the parent's
working-copy commit, not its descendant. With candidate input, that exact
candidate is F. The child's empty working-copy commit has F as its parent,
and the recorded base is F. F and the child are published together by
`plak mint workspace`; F is pinned at `refs/plak/<namespace>/bases/<agent_id>`.
For JJ occupancy, ahead counts non-empty commits in `base..@`; behind counts
`base-..parent@` excluding empty commits and commits whose tree equals F
(the copied mint state is not new parent work). The frozen base and empty working-copy commit add no noise to
a fresh fork's `(0, 0)` count. The recorded row base is the authority.
Parent edits, snapshots, `describe`, `commit` and `new` do not rebase running
forks or stale their copies. Ancestor rewrites can still stale a copy and
must refuse sealing without changing its modified or untracked files.
In-process mints serialize per launch repository; library locking coordinates
working-copy mutations with external JJ callers. An incomplete mint retains
its directory and reports recovery with its path instead of silently deleting
possibly published state.

**Discard.** The host removes the workspace registration through the library
before deleting its directory. If registration removal fails, the directory
is retained. Its base pin is removed after successful workspace forgetting;
candidate pins are independent. A Plak-created frozen base is also abandoned
when no visible descendant remains; input candidates are never abandoned by
this cleanup. Reused copy IDs replace a stale base pin atomically on mint.
Cleanup returns an explicit acknowledgement or typed failure;
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

Snapshotting uses the native JJ library or a Git subprocess and can fail: a stale jj copy, a full disk, a
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
settled by a `recover_node` run or by the operator's retry; the operator
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

A refused automatic reclamation backs off independently per workspace: 30 seconds,
doubling to a one-hour cap. The process-local retry state resets on an external generation
change, follow-up, explicit discard or terminal lifecycle success (removal or
restoration), not an intermediate seal or reclamation's own generation bump. Size probes
cache a copy's walk for 30 seconds and reuse it throughout a refusal's backoff;
a changed generation, path or trim-result epoch invalidates the cache, even
during backoff. A bounded `plak.workspace` INFO records each trim attempt's
closed result and refusal class. VCS refusal detail is bounded
and logged under `plak.workspace` only when it changes, and is included in the
reported topology error.

Only when the tree is over its disk budget, a retained copy may have its root
`target/` trimmed under the acknowledged reclamation intent, with occupant jobs
stopped and outside the tree mutex. Trimming is allowed only after a typed static
coverage refusal in the pre-seal stage, never after a changed-after-seal refusal
or an operational seal/removal failure that may indicate a concurrent writer.
New budget pressure does not bypass an existing backoff, so a newly eligible
trim can wait until that copy's next retry (up to one hour).
The VCS port proves it a real (non-symlink), untracked, ignored directory with
no unignored or secret leaves. Nested repositories or worktrees and case-aliased
root names are retained. Tracked caches,
symlinks, JJ and uncertain inspections are retained. The operation is idempotent
and preserves the checkout, its seal and all candidate pins. There is no shared
`CARGO_TARGET_DIR` default: cargo's shared lock serializes builds and absolute
checkout paths affect fingerprints.

These owned intents are recorded in `agent_operations`. Reopen resumes their
seal/remove/restore stages rather than treating them as an uncertain explicit
discard. Removing an already absent checkout is idempotent. Whether the
checkout is present is asked of the VCS port; a port that cannot answer fails
the stage, so a checkout that only looks absent is neither removed without
its coverage proof nor minted over. A follow-up first
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

Retention never deletes work. Unadopted, nonsuperseded, nonempty candidate refs never expire
automatically: only explicit discard or user deletion relinquishes them.
After conversation retirement, keep each such candidate plus a minimal listable
closed child record naming all retained candidates, and minimal ancestor rows
needed to retain their tree membership. It says the conversation
was retired, accepts no follow-up, and remains adoptable and usable as spawn
inputs. Each collected candidate releases its candidate, candidate-base and
adopted-before/after refs together. Adopted, superseded and empty refs may be collected; their exact unpin intents stay
until cleanup succeeds. Unknown ignored/secret or uncertain checkout contents
prevent sealing, reclamation and retirement, regardless of either bound.
Bounds are soft where safe collection is unavailable or protected work remains.
Collection publishes a `candidate_unpin` operation with the exact candidate-id
array in the same save that removes the records. The unlocked VCS phase drops
all four ref families; only a successful cleanup acknowledges and removes that
intent. A failure or process loss leaves an idempotent obligation for reopen.
Maintenance inventories these pending refs even after the records are gone.
Failed-turn rescue snapshots have `rescue_pin` ownership rows in
`agent_operations`, published before sealing and containing the exact pin-id
array under the producing agent. They are not adoptable candidates. Their
candidate and candidate-base refs remain owned until that child's conversation
is collected; collection transfers them into the same durable `candidate_unpin`
cleanup intent. Maintenance and deleted-session inventory include both live
rescue ownership and pending cleanup, so removing a tree also releases its
rescue refs. A refused snapshot can leave an owner with no refs, which cleans
up through the same idempotent rule.

### 6.8 Live-fork catch-up

Catch-up runs only before a followup into an idle Git fork with no candidate
input. The child must have no running background jobs, an attached tree-owned
branch, no unmerged index entries and no independently staged version of a file.
Rehydration, when needed, finishes first. Candidate-minted, snapshot and inherit
copies are not refreshed; explicit `refresh:true` on them is `schema_mismatch`.
JJ copies report a skipped catch-up until backend parity.

The unlocked preparation snapshots parent `P` and child `C` over the recorded
logical base `B`. No new parent tree means no refresh. Only the whole-worktree
merge `merge-tree --merge-base=B P C` decides conflicts: `B` may contain adopted
uncommitted work that is absent from HEAD. A conflict retains the exact old
copy, branch, index and base; both parent receipt and child Start name the paths. Other preparation
failures skip catch-up with a bounded reason and followup continues.

A clean preparation creates a fixed-identity merge commit `Mc` whose tree is
`P`'s tree and whose parents are old HEAD and `P`. This checkpoints the new
parent base without committing the child's delta, regardless of HEAD ancestry.
Before preparing a refresh, admission checks the recipient's mailbox record
and queued-byte budgets including a reserved maximum 1024-byte Start line.
Before any copy write, the tree publishes a `workspace_refresh:<workspace>` operation containing
the exact original base, parent and child snapshots, whole result, merge commit,
old HEAD and branch. The operation also binds the original followup request
digest, so a retry cannot change its brief or refresh policy. The unlocked apply
uses an atomic worktree-only patch from `C` to the whole result, CAS-updates the
owned branch to `Mc`, resets only the index with `read-tree Mc`, and CAS-updates
the copy base pin from `B` to `P`. Only then does the row advance its base and
generation, settle the intent and open the new attempt. Existing uncommitted
child work stays uncommitted. Pending refresh ownership blocks adoption, explicit
discard, reclamation, retirement and another followup of that copy.

Rehydration restores the copy's base pin to the row's logical base, not the
reclamation snapshot used to remint its files. Before its first copy write,
apply checks the attached branch, HEAD in `{old HEAD, Mc}`, content in `{C, M}`
(HEAD `Mc` requires `M`), and the base pin in `{B, P}`. A failed fresh
precondition abandons the intent without writing the copy; recovery can do the
same when old HEAD and original content prove no write occurred. Thus an
unreachable plan cannot create a partially applied refresh. Existing divergent
post-write states retain their recovery intent until the operator restores one
of the exact known states; no blind intent-clearing operation discards evidence.

Attachment or the next followup reconciles a published apply intent before
admitting further work. An effect still owned by this process cannot be retried
concurrently:
old HEAD plus original child content abandons it at the old base; old HEAD plus
the exact merged content finishes the branch/index/pin updates; `Mc` plus the
merged content finishes index and base pin. If original and merged content are
identical, completing the branch/base update is safe. Any post-write divergent
branch or content is `workspace_recovery_required`, retaining the copy and intent for
inspection. A refresh fault records the prior idle node status; once exact-state
recovery settles that fault and no other VCS fault remains, projection restores
that same status without changing the sealed attempt or its result.
A preparation interrupted before a plan is published simply skips
catch-up at the old base. A completed `workspace_refresh_result:<send operation>`
retains the bound request digest and report even if another followup replaces
the workspace's current intent before the original send is admitted.
A completed outcome is part of the original send
receipt; replay never refreshes a copy twice or appends its Start line twice.

## 7. Adoption

`adopt_candidate { candidate_id, expected_target?, evidence?, accept_conflicts?, revert? }` lands a sealed
candidate into the **caller's own copy**, which is canonical for its subtree;
the root lands into the launch checkout. Integration is three-way, with the
candidate's recorded base as the merge base, against the *current* target
snapshot.

**Adoption is acceptance.** There is no `Accepted`/`Rejected` state anywhere.

Three rules govern every landing:

- **No checkout, no child mutation, no commit.** The child's work arrives in the
  caller's working tree as ordinary changes; new files arrive untracked.
- **The root checkout MUST NEVER receive a Plak-authored commit.** Children
  commit freely on `plak/<tree8>/<agent_id>` branches inside `.plak/`. Commit, push, and
  pull request happen only when the user instructs the root, which then uses the
  `git`, `gh`, or `jj` CLIs itself. **Plak ships no commit or PR tool.**
- **A conflict never destroys the only copy or lands implicitly.** A textual
  Git conflict pins a separate conflict candidate and fails without changing
  the target. Markers land only with explicit `accept_conflicts=true`.
  Non-textual or mixed conflicts remain plain failed results; all child files
  remain untouched.

Admission runs under the tree lock, the VCS landing runs unlocked under an
`adopting` workspace status, and the outcome is recorded on re-lock. A reopen
observing `adopting` is `recovery_required` and MUST NOT auto-retry.

Receipt: `{ candidate_id, target_before, target_after, files_changed, files?,
files_truncated? }`, plus `warning` when running processes still use the
checkout. `files` lists at most 64 changed paths (`"M src/lib.rs"`,
`"R old.rs -> new.rs"`); `files_changed` remains the full count. The warning is JSON
inside the stored receipt and replays verbatim, not a fresh observation.
The candidate durably records `target_before` and `merged_at`, the commit component of
`target_after`, alongside its adopter and evidence. `list_agents` exposes this
field and `plak session show` says `merged at <after> (before <before>)`. On Git this is the
internal target snapshot, never a new commit on the root branch. A failed or
conflicted adoption records no merged-at commit. Cleanup does not erase this
provenance when its worktree or branch is removed; the record owns its pins
until collection.

### 7.1 Base identity and expectations

A snapshot's identity token is `<commit>:<content>`. `expected_target`, when
supplied, is compared against a fresh snapshot of the target and matches on the
full token, the commit alone, or the content alone. A mismatch is
`target_changed{current}` with nothing applied.

### 7.2 The git recipe

1. Snapshot the target. A failure here is `mint_unavailable`.
2. Check `expected_target`.
3. `git merge-tree --write-tree -z --merge-base=<base_commit> <target_commit>
   <candidate_commit>`.
4. `git diff --no-ext-diff --no-textconv --name-status -z -M <target_tree>
   <merged_tree>` gives `files_changed` and the receipt's `files`. A rename is
   `R <old> -> <new>`. A zero count lands no files but retains exact before/after
   provenance through the ordinary wrapper publication.
5. `git diff --no-ext-diff --no-textconv --binary -M <target_tree> <merged_tree>`
   produces the patch; a repository-configured diff driver or textconv filter
   never runs and never alters the patch.
6. `git apply <patch>` in the caller's own path, the patch a `0600` file in a
   private directory — atomic by default,
   worktree-only, binary-safe, no commit.
7. Wrap the exact merged tree, parented by the target snapshot, and atomically
   publish before/after provenance plus the producing copy's advanced base.
   The child copy is untouched.

**Failure classification is typed, never collapsed.** `merge-tree` exiting
**1** is `merge_conflict{paths, child_path}`. Each path carries the kind git
named: the first non-`contents` `CONFLICT` type on that path (`binary`,
`modify/delete`, and the rest), otherwise `contents`. A binary path is
reported as both and is `binary`. The message lists `path (kind)`. `N` is the
full count. When the 512-byte cap would split an entry, the list stops at a
whole entry and says `+N more`. Duplicate entries are counted once and path
whitespace is preserved. An overlong child-copy path is shortened on a UTF-8
boundary, reserving room for the omission marker. The adopting call's facts keep every path,
uncut. Any **other** non-zero exit — a bad base object, a corrupt ref — is
`mint_unavailable` carrying a 240-byte stderr excerpt, so a broken repository
is never reported as a content conflict.

On the `git apply` leg, only stderr matching *"does not match index"*, *"patch
does not apply"*, or *"already exists"* yields `target_changed`: those mean the
worktree moved under the adopt. Anything else — modes, line endings,
permissions — is a typed VCS failure rather than a silent "target moved". That
closes the snapshot-to-apply race without taking a second snapshot.

**Textual conflict candidates.** When every conflicting path is `contents`
or `add/add`, an ordinary Git adopt pins the conflicted tree as
`<source>-x<ordinal>`, together with its target-snapshot base. The ordinal is
reserved under the tree lock; only one adopt may own a source copy at a time.
Publication uses a fixed-identity/date wrapper parented by that target snapshot,
under the namespace lock, and one transaction for candidate and candidate-base
refs. The target's files, index and HEAD are unchanged. The refusal names the
new candidate and the resolver/accept recipes. Mixed or non-textual conflicts
pin no candidate and retain the path/kind refusal.

A conflict inherits its source's workspace, producer, attempt and retention
authority, and records the exact target node and every conflict path/kind.
The source remains directly adoptable until some acceptance succeeds.
`spawn_agent {workspace:"fork", inputs:[conflict_id]}` starts a resolver over
the marker-bearing snapshot with the original target snapshot as its merge
base. A later landing therefore computes `current target + resolution - original
target`, preserving later, nonconflicting target edits.
Before landing a resolver, Git checks only its recorded conflict paths for
lines starting `<<<<<<<` or `>>>>>>>` followed by a space or end of line.
Remaining markers fail `merge_conflict` naming those paths, unless explicitly
accepted. A conflict candidate itself requires `accept_conflicts=true`.
A second merge conflict after the target moved is a plain refusal, not another
conflict candidate.

Acceptance through a conflict or resolver marks the source adopted with
`adopted_via` naming that resolution, marks the conflict adopted, and advances
the source copy's base/pin to the original source commit, never to the markers.
A resolver's own base advances to its accepted work. Both workspaces hold
`adopting` through the unlocked phase. The original source, its conflicts and
other resolvers cannot be adopted again after acceptance; refusals name the
accepting candidate when applicable. No commits enter the receiving branch.
Every acceptance, including a direct source adoption, supersedes all remaining
conflict candidates for that source and every unadopted resolver candidate,
including candidates sealed later by an already-running resolver. Late seals
derive the accepting id from the source's stored acceptance. A conflict whose
source is accepted, or which is itself adopted or superseded, cannot mint a
new resolver: spawn refuses `adopt_not_applicable` with
`resolution already accepted via <id>`.
Superseded rows record the accepting id in `adopted_via` without `adopted_by`;
they leave `list_agents`' `candidates` and no longer protect retention or count
as unadopted maintenance artifacts, except that a live resolver input still
protects its conflict record and source acceptance record. Neither is collected
or unpinned while a non-discarded, non-reclaimed copy names the conflict as its
`input_candidate_id`. Live-tree maintenance is deliberately more conservative:
it keeps this protection until the workspace is `discarded` or `clean_removed`,
without interpreting intermediate reclamation intents. For a deleted tree there
are no live input owners: maintenance classifies work only by `adopted_by` and
`adopted_via`. Normal producer collection queues their
candidate and base refs for unpinning, just like other spent candidate records.
A follow-up in the accepted resolver copy seals an ordinary delta over that
advanced base, not another resolution of the original source.

At mint the workspace row stores the resolver's `resolves`, `conflict_base`
(mint-time base commit), `conflict_target` and `conflict_paths`, independently
of candidate-record lifetime. Sealing and marker inspection use this immutable
workspace lineage, never a lookup of the input conflict record. A non-resolver
copy (`resolves` absent) seals ordinary work even after its adopted input was
collected. A resolver with inconsistent persisted lineage fails sealing closed.
An accepted resolver whose base advanced beyond `conflict_base` seals ordinary
follow-up deltas, including after reclamation, source collection and rehydration.
Even a spent resolver's pinned snapshot is inspected for unresolved markers
before returning its acceptance refusal; it can never apply a second landing.

### 7.3 The native JJ recipe

Adoption merges the immutable recorded base tree, sealed candidate tree, and
current target tree through `jj-lib`. It does not rebase or squash the child's
history, and it never includes edits made after sealing. Conflicts return
`merge_conflict` before any target checkout or transaction publication. A jj
conflict names every path with kind `contents`; git is what names `binary`
and `modify/delete`.
JJ does not create conflict candidates. A JJ conflict with explicit
`accept_conflicts` refuses `adopt_not_applicable` with
`conflict candidates are not yet available for jj`; a clean ordinary JJ adopt
still succeeds with that flag.

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

`adopt_not_applicable` covers:

- an already-adopted candidate;
- a conflict or resolver belonging to a different `conflict_target`;
- a resolution whose source was already accepted or explicitly discarded;
- a node adopting its own candidate;
- a copy whose recorded base is a previous candidate adopted by a **different** node,
  unless the caller already holds an adopted carrier of that base —
  refused by name rather than silently dropping the earlier delta;
- a candidate built on another candidate this caller has not adopted. The
  refusal names that input. When the input landed in another node, it names
  that node: adopt the input first (or that node's candidate), or the landing
  would drop the input's changes. On both backends the producing workspace's
  `input_candidate_id` records this identity at mint, independent of live
  ancestry, later seals and later base advancement. This check performs no
  repository probes or candidate scan.
  The caller's own earlier adoption of the input passes.
  An inherit adopter in the caller's resolved checkout has already landed the
  input there. Otherwise carriers are followed transitively, including later
  candidates from the input's own copy and candidates that the input's adopter
  began sealing after landing the input, as proven by the existing adoption
  and seal fold orders rather than wall-clock timestamps. For an inherit
  adopter in a different checkout, the remedy is to ask that adopter to land
  the stacked candidate there, not to ask for a candidate it cannot seal.
  An input whose workspace was explicitly discarded
  or relinquished is not a blocker; automatic clean-copy reclamation does not
  relinquish the candidate. A candidate from the same workspace is
  not an input of itself;
- a jj copy (`.plak/workspaces/`) over which an **unfinished child's copy was
  minted**, because the stale recovery that follows adoption would reset that
  child's uncommitted files. A git copy is not refused: the landing does not
  rewrite it.

### 7.5 Review evidence

`[orchestration] require_review_evidence` ships **off**. When it is on, an
adopt MUST pass `evidence` naming an attempt **in the caller's subtree whose
`output_json` contains the candidate id**; anything else is
`evidence_mismatch`.

`evidence` is recorded on the candidate and persisted either way — validated
and dropped would lose what stood behind a landing. Because each review node's
`output_json` links the candidate commit it reviewed, `list_agents` can show
"candidate X, reviewed by Y: reject" with no new state.

### 7.6 Reverting an adoption

The root host does not turn a successful `reverted:true` receipt into a second
`adopted` milestone. It uses the existing tree-info notice vocabulary to name
the reversal, keyed by tree, candidate and revert operation; the tool row
still records the reversal itself. No new persisted row kind is introduced.

`adopt_candidate {candidate_id, revert:true}` removes exactly one adoption's
delta from the caller's current checkout, preserving later unrelated edits.
Only the original adopter may revert it. The accepting candidate and its
before/after provenance pins must still be retained and match the recorded
objects; missing or changed pins refuse `adopt_not_applicable`. A later-sealed,
still-adopted candidate from any participating copy must be reverted first;
the refusal names that candidate. `revert` cannot combine with `accept_conflicts`.
JJ refuses `adopt_not_applicable` until backend parity.

Git snapshots the current target and computes a three-way merge with the pinned
adopted-after commit as base and adopted-before commit as the incoming side.
A conflict refuses `merge_conflict`, lists paths and changes nothing; reversals
never mint conflict candidates. A clean result uses the same atomic worktree-only
patch and exact-result receipt as adoption. No commit enters the receiving branch,
and the original before/after pins are not overwritten.

Success clears adoption ownership and `adopted_via` on the accepting candidate
and its adopted source/conflict aliases, recording `reverted_at` (the resulting
target snapshot) and `reverted_by` on those rows. Superseded, unadopted alternatives
stay superseded. If their accepting candidate has since been reverted,
adoption and spawn refuse with `superseded by <id>, since reverted; adopt
<source> again or let a new conflict pin a fresh candidate`, rather than claiming
that resolution remains accepted. A second revert by the original caller
refuses `already reverted at <snapshot>; adopt it again to restore it`.
A successful non-revert adoption clears reversal stamps on the candidate and
its adopted aliases. Reversal conflicts retain their path/kind list and say
`reversal conflicts with later edits; nothing was reverted`.
Copy bases and their pins return to the candidate's original
base only when still equal to that candidate's accepted commit. The original
candidate pins remain until record collection; Git can re-adopt through the
launch repository even if the producing copy has been reclaimed. A reverted
resolver whose accepted source record was collected uses its own retained
candidate and workspace lineage; it does not recreate retired source records.
`adopting`
holds every participating copy through reversal; an interrupted reversal reopens
as `recovery_required` and is never automatically retried.

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
mail, and replies never wake the root; they ride the next admission. This
idle-root rule is distinct from the watched-mail await rule (§4.4): a question
or urgent mail wakes a wait, and a reply also wakes a wait. Deferred
finding/coordinate mail never ends a wait by itself unless it is the only
pending item of a bare wait.

A submitted or queued user prompt always takes precedence. User interrupt or
cancel disables waking until the next user prompt. So does a root turn, of any
origin, whose terminal failure is typed `QuotaExceeded`: a wake would start a
paid turn the exhausted route refuses again. The suppression is the turn's
terminal fact (chapter 02 §2.6), so it survives reopen; the failed turn's error
row names the limit and its reset when the provider gave one, the idle
indicator and its one `auto-continue paused until the next prompt` line show
the pause, and waiting mail stays unread for the operator's next prompt, which
ends it. A reset time does not end it on its own. Recovery fences,
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
`auto-continue paused until the next prompt` transcript warning. A wake that
does not fit at preflight (`Sub-agent auto-continue preflight paused until the
next prompt`) or fails (`Sub-agent auto-continue paused until the next prompt:
<cause>`) writes its own pause row instead, and that episode gets no second
line, live or after reopen (chapter 02 §5.2). A crash between the
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

Root and child admissions use one hosted write-ahead command ledger port.
Its role adapters have separate storage authority: a root records its tree
batch before its product submission; a child records only its tree intent.
Fresh intents name their command kind and batching rule explicitly. Duplicate
settlement projects through the role's reader, then applies one rule: settle
from an acceptance fact, or consume an absent fact only below a positive
retained feed boundary; a rejected fact is never consumption. Child mail never
enters the product queue. Product-store refusals and interrupted tree-capacity
refusals retain their typed classification through preparation and settlement;
root batch recording and withdrawal reach the host as
`RecordStoreError::refused(StorageFull)` on an interrupted capacity refusal.
Attached root and child tree publications park until written or interrupted.
A stored-only root batch has no attached parking owner: its capacity refusal
pauses the queue and retains input and unread mail without ending the host,
even when no interrupt was requested. The operator may retry after storage
recovers; no background retry or inference replay is scheduled.
An automatic root continuation whose product preparation refuses capacity is
cancelled with its notice before the host retains the storage pause; it creates
no submission and is not retried in the background.
A one-shot run stages each wake queue item on a candidate record and installs
it only after submission preparation succeeds. A retained storage outage pauses
its queue and ends the wait without another wake attempt or undurable item.
Child publication waits belong to the command recipient. A refused root batch
leaves the queued input and unread mail retryable. Retrying a batch written
before its product submission reuses the same command identity and bytes
without adding a second tree command. The hub's guarded mutation preserves
queued effects on a refused publication; its typed result reaches the ledger
without prose conversion. Command identities use these pinned spellings:
`plak.queue.`, `plak:child-wake:`, `plak:resume-transient:root:`,
`plak:park-wake:`, `plak:resume-transient:{agent}:`, `plak:submit-harness:`,
`plak:spawn-start:`, `plak:mail:{id}:{start|follow-up|steer}`, and
`plak.send-now.{id}.{revision}`. A live-root send-now steer uses this ledger's
root adapter with its exact target turn and the stored `send_now_steer` kind.
Preparation saves the pending intent in place on the live session, including
an exact retry, without a session touch, a clone, or an existence pre-read.
The rebuilt submission must match the session's joined pending submission
field for field; a mismatch is an integrity refusal rather than a new write.
Consumed settlement discards the intent; an unrecorded definite refusal
discards it with its pause cause, while uncertain outcomes retain it.

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
prompt can keep delegating. The transcript says how many children are interrupting;
release may still be in progress.
A child whose cancel does not release is a retirement failure, as for a close.

Hosted cancellation records the descendant interrupt and dispatches retirement
on its existing async executor, without joining release on the requesting path.
Shutdown still joins its owned releases under its separate bound. Only a
synchronous application edge drives these futures with its application
executor. The interrupt marker never constructs a nested executor.
An explicit headless-run exit joins child retirement within the same two-second
allowance as shutdown, after stopping jobs and recording their terminals. This
does not close admission on an otherwise reusable host. Expiry reports bounded
cleanup failure without cancelling the release owner; the run never waits for
the 65-second joiner ceiling before teardown.

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
by `close_agent` and cannot be re-tasked; the host's recovery (or the
operator's retry, section 4.5a) settles it as section 14.2 describes: `failed` and idle, after
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
is no spawn queue.** The bound counts executing children only — the root's own
turn is free — and because `MAX_DIRECT_CHILDREN` and `MAX_RUNNING_CHILDREN` are
both 64, `capacity` is reachable only across several hubs, never from one
parent alone. What paces a wide tree is the per-account request budget
(§10.4), which delays model requests and never refuses a spawn.

The separate `storage_low` refusal is the product-home admission floor
(chapter 02 §2.2), not running saturation. It carries no running set and is not
remedied by interrupting children, deleting files or repeated model-side retries.

Children stop before the root on the shared runtime store too. Every child
session is created, replayed and reopened through a subordinate handle
(`AgentSessions::with_subordinate_admission`, chapter 01 §5.2), whose new work
stops one store reserve below the ordinary threshold: at `L - 2 * ceil(L / 8)`
of each aggregate bound `L` (3 GiB of the 4 GiB object-byte default, where the
root's new work stops at 3.5 GiB). Reads and recovery-class writes of child
sessions (effect-evidence recovery, the accepted-create lookup) may use the
root's handle. Past the mark `spawn_agent` refuses with `storage_low` before a
node is admitted or anything is minted: the spawn admission checks the
process's last store reading (at most one host poll old) before the template is
resolved, and a fresh reading before the binding is admitted; the message names
the bound, where child agents' new work stops and where the root's does. A child's Start or
follow-up is refused as storage pressure (mail delivery refused, nothing
admitted), a running child's Code Cell meets the mark at its next nested call
with the catchable `Backpressure` error, and a child turn already running ends
at its next ordinary write with the ordinary storage-pressure failure (`Failed`
of kind `Session`, never `recovery_required`), whose result reaches its parent
like any failed attempt. Settlement and recovery writes of children are not
held to the mark (chapter 01 §5.2 bounds what still lands past it). The root
therefore keeps the last reserve before the ordinary threshold for its own
turn, compaction and cleanup.

| Bound | Value | Configurable |
| --- | --- | --- |
| `MAX_TREE_DEPTH` | 4 | `[orchestration] max_depth`, 1–4 |
| `MAX_DIRECT_CHILDREN` | 64 | `[orchestration] max_children`, 1–64, default 8 |
| `MAX_TREE_NODES` | 65 live nodes, root included | `[orchestration] max_nodes`, 1–65, default 32 |
| `MAX_RUNNING_CHILDREN` | 64 | — |
| `MAX_PENDING_SPAWNS` | 64 | — |
| `MAX_TREE_ATTEMPTS` | 256 per root prompt | `[orchestration] max_attempts`, 1–256, default 64 |
| `MAX_WAIT_TARGETS` | 64 ids per `wait_agent` | — |
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

The ceilings exist so a root can hold a full row of 64 children (`max_nodes`
65 is the root plus 64); the defaults stay at 8, 32 and 64. The `spawn_agent`
description states the caller's granted `max_nodes` and `max_children`, so a
model sees the bounds it is actually under.

### 10.2 Blocked and failed attempts

One hosted transient-continuation service plans and gates root and child
continuations; both roles admit its instruction through the command ledger
(§8). Its classifier uses only canonical model-error
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

The service derives the role's exact command identity and model-visible text
under the owner's budget. Roots count continuations in memory per user task,
use `plak:resume-transient:root:{failed_turn}`, and cancel a pending wait on
new input or reload. Children count durable Resume decisions per attempt and
recheck attempt ownership, turn capacity and cancellation on tree notifications,
gate readiness and a child-only safety tick. The full admission predicate is
checked again when readiness resolves, including startup cancellation; a lost
owner releases without failed settlement. Their mailbox remains tree-owned.

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

### 10.4 Per-account request budget

A child waiting for a native credential remains running in the same attempt.
The Agents activity says `waiting for login: run plak login <account>` (with
command formatting). The live transcript notice includes that command when
the account is already known, or says `waiting for login` otherwise. No
per-face reducer is created: the admission-owned hosted turn projection reduces
the wait once, and faces share its progress and retained updates. The tree's
account label supplies the snapshot login remedy, never dispatch authority;
labelling a snapshot never rewrites an already-retained update. No
failed terminal or transient-resume attempt is manufactured. Result-mail wakes
continue through the normal queue while a root waits. Readiness resumes the
same undispatched request; cancellation/shutdown and different-account refusal
use the ordinary single-terminal paths (chapter 01 §5.4).

Every session the process builds — the root and every descendant — draws its
model requests from one budget per local account reference, sized by the home-scoped
`[orchestration] max_concurrent_requests_per_account` (1–64, default 8; read
at launch). Distinct local names never share the product request budget, even
when authenticated as one upstream account and sharing upstream limits. A request takes a slot before its operation deadline starts and
holds it until its response stream ends; a compaction summary is a request
too. A full budget never fails a request: the request waits in arrival order,
publishes `ModelRequestQueued` once (TUI status `waiting for a <account>
slot`, the child's Agents-tab activity, the JSONL `model_request_queued`
line, and a `model.request_queued` session-log event), and proceeds when a
slot frees. Interrupt, close, shutdown and a lost lease end the wait without
taking a slot. A request never holds a slot while its tools run, so a
parent waiting on its children cannot starve them.

A credential wait also relinquishes its slot and reacquires it in FIFO order
after readiness. Neither credential nor slot waiting spends the request's
operation deadline, and compaction uses the same rules.

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
`list_agents`, `close_agent`, `interrupt_agent`,
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

A worktree copy's git shim refuses, with exit 128:

- `core.worktree` set through `GIT_CONFIG_PARAMETERS`, `-c`, or `--config-env`;
- `git init` and `git init-db`;
- `git clone` whose destination resolves to the copy root;
- `git worktree add`;
- an effective working tree — cwd after `-C`, overridden by `--work-tree` or
  `GIT_WORK_TREE` — outside the copy;
- a `--git-dir` or `GIT_DIR` that is not this copy's git directory.

Global parsing records the command's argument index once. Every guard starts
after that index, even when a global option's value equals a command name.
Separate-value globals include `-C`, `-c`, `--git-dir`, `--work-tree`,
`--namespace`, `--config-env`, `--attr-source`, and `--super-prefix`;
stuck `-C`/`-c` and `--config-env=...` are inspected too. Abbreviations of
guarded long options are refused with a full-spelling remedy; they cannot
fall through as unknown options. Checkout/switch short clusters are scanned
for their branch-creation option and attached name. Optional `--color` takes
no following value.

After that admission, and before exec, the same shim refuses a write to Plak
coordination state. Exit 128, `plak: git-guard refused: <op> would change Plak
coordination state (<name>); <remedy>`. The copy's own branch is
`plak/<tree8>/<agent_id>`, derived from `<tree8>-<agent_id>`, when that branch ref exists (a fork; the mint records
it, and a later `switch` does not drop it) and empty for a detached snapshot.
Names are compared case-insensitively. Protected names are `refs/plak/**`,
`refs/stash` and `stash`, and `refs/heads/plak/*`, `heads/plak/*`, `plak/*`
other than that own branch. The guard refuses:

- `update-ref` of a protected ref (including `main-worktree/*` and
  `worktrees/*`), or of `refs/heads/<b>` checked out in
  another worktree. `update-ref --stdin` is copied to completion, then
  inspected in both newline and NUL modes, including `symref-create`, `symref-update`, `symref-delete` and
  `symref-verify`, and, when admitted, exec'd from that copy so the caller's
  bytes are unchanged. Buffering waits for EOF before Git starts; interactive
  transaction acknowledgements are unavailable through the shim. Both scratch
  files use private `mktemp` names, and buffering failure refuses the call;
  worktree registrations are listed once for the entire invocation, and quoted
  textual ref names are refused with a remedy to use NUL mode;
- `symbolic-ref` writes whose name or target is protected or checked out in
  another worktree; the message value
  of `-m` is skipped (a one-argument read passes);
- `branch` delete, move, copy, force, or a bare create of a protected name.
  A start point is not a written name. `--merged` and `--no-merged` list.
  Verbosity (`-v`/`--verbose`) alone does not make a branch command a read.
  Unrecognized branch `--no-*` combinations are refused rather than allowing
  cancellation of a listing action to expose a write; the explicit
  `--no-merged` and `--no-contains` filters still list.
  Only a single-dash argument is a cluster of short options (listing passes);
- `checkout` and `switch` creating a protected name, including
  `-b`/`-B`/`-c`/`-C` (separate or stuck to the name), `--orphan`,
  `--create` and `--force-create`, and the `=` forms;
- checkout/switch of another copy's protected branch (including a branch not
  currently checked out), explicit rebase branch targets, and
  `--ignore-other-worktrees`. `checkout <tree> -- <paths>` remains a read;
  an empty trailing `--` does not exempt a branch switch. A non-empty pathspec
  after `--`, or a regular non-empty `--pathspec-from-file` resolved against
  the effective cwd, exempts only a tree read, never
  explicit branch creation. Rebase skips values
  of `--empty` and `--whitespace` when locating its branch target, and
  `--update-refs` is refused because it can rewrite other protected branches.
  Effective `rebase.updateRefs=true`, including repository config and `-c`,
  is also refused unless `--no-update-refs` is explicit;
- `pull` with effective rebase mode and `rebase.updateRefs=true`, because its
  internal rebase does not invoke the shim. Explicit `--rebase`/`-r` modes and
  their ordering against `--no-rebase` take precedence over the current
  branch's `rebase` setting, then `pull.rebase`. The remedy is
  `git fetch then git rebase --no-update-refs`;
- a destination refspec of `fetch` or `pull`, including the value of
  `--refmap` and configured remote fetch mappings. The repository positional
  is not a refspec; with `--multiple`/`--all`, all positionals are remotes.
  Without a repository positional, the current branch's configured remote
  (otherwise `origin`) supplies the mappings. `remote update` checks named
  remotes or groups, the default group, or all non-skipped remotes;
  `remote prune <name>` checks that remote's mappings (dry-run stays a read).
  Effective `fetch.prune` and per-remote `prune` apply to the pruning scope
  check, with the per-remote setting and explicit command options taking
  precedence over the global setting.
  `fetch --stdin` is residual;
- wildcard destinations whose literal prefix before `*` covers a protected
  namespace, including `refs/*`, `refs/heads/*` and `*`. Pruning with a glob
  destination outside a named `refs/remotes/<name>/` namespace is refused;
- `push` of a destination refspec, and under `--delete`/`-d` every refspec,
  only when the remote resolves to this copy's or the launch checkout's git
  common dir (`.`, `./`, an absolute path, or a `file://` URL). Named remotes,
  push URLs and `url.insteadOf` are resolved with effective command-line
  configuration before comparing locality, including remotes defined only by
  `-c` (whose push URLs require a config query, not `remote get-url` alone);
  local mirror refusals name the invoked command, including `send-pack`.
  `--force-with-lease` takes no following argument.
  The first positional repository wins over `--repo`; only its absence uses
  `--repo`, then the configured push remote. Push to any other remote is not guarded;
- `send-pack` uses the same local-repository and destination checks. Local
  `--all`, `--mirror` and `--stdin` are refused; an explicit destination on
  another repository stays admitted;
- `stash` with no subcommand, or `push`, `save`, `pop`, `drop`, `clear`,
  `store`, `branch`, or a leading option (`list`, `show`, `apply`, `create`
  pass; the remedy is `commit WIP on <own branch>`, or `git stash create`
  followed by `git show <object>`, or retaining the file, when the snapshot
  has no branch);
- `gc --prune=<value>` except `--prune=never`, `prune` other than `-n`/`--dry-run`,
  `reflog expire`/`delete` with `--all` or the shared stash, `reflog drop`,
  and `repack -a/-A -d` (including clusters). Ordinary named reflog expiry
  and deletion pass because provenance objects have independent pins;
  `--updateref` still checks its written branch against protected refs and
  branches checked out elsewhere.
  `gc` and `maintenance` are refused when `-c`, `--config-env` or
  `GIT_CONFIG_PARAMETERS` or indexed `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/
  `GIT_CONFIG_VALUE_n` explicitly sets `gc.pruneExpire` or `gc.reflogExpire*`
  to `now` (including separately quoted key/value encoding). Indexed config
  also participates in the `core.worktree` admission refusal;
- `worktree remove`, `move`, `lock`, `unlock`, or `repair` of any path other than this
  copy (`repair` with no path would touch every copy), and mutating
  `worktree prune` (`-n`/`--dry-run` passes).
  A later `--no-dry-run` cancels dry-run admission for either prune command.
  A separate `--reason` value is a reason, not a worktree path.

A pin is read with `git show <ref>`. Any other protected branch is answered
with `work on <own branch>`. Push to a real remote and PR creation are not guarded.
Explicit object expiry (`gc --prune=<value>` other than `never`, and mutating
`prune`) is refused because it races in-flight object writes. Bare `gc --prune`
uses configured expiry. Shared stash and global reflog expiry/deletion are refused.
Reftable repositories, or deleting the minted branch after switching away,
can make OWN_BRANCH empty and reclamation conservatively treat the old shim
as unmanaged. This retains the copy; it does not grant another branch.

A JJ copy has no Git directory: its Git shim admits only `git --version`,
`git version` and `git help`; everything else exits 128 and directs the child
to JJ instead of discovering the launch checkout. Hosted construction also
requests the neutral core discovery ceiling (chapter 01) for JJ copies.

The JJ shim consumes value-taking global options before locating the command,
including `--color`, `--config`, repository and operation options and their
`=` forms. Every clustered or glued single-dash token is refused (including
`-GR`, `-nR` and `-mRelease`); pass short options and their values separately.
This fail-closed rule prevents repository redirection hidden in a cluster,
including valueless short flags before `R`. The other value-taking globals
(`--at-op`/`--at-operation`, `--config`, `--config-file`, `--color`) have no
short aliases and their long separate/equals forms are checked explicitly.
It admits only built-in top-level command names and the pinned
default aliases `b`, `ci`, `desc`, `op`, `st`; configured aliases and configured
default commands fail closed. The default aliases cannot be redefined through
user config. `init`, `undo`, `redo`, `gerrit`, `-R`/`--repository`, cwd outside
the copy, `--ignore-immutable`, `--config-file`, and `--config` naming aliases,
`immutable_heads` or `snapshot` are refused with exit 1.
Before execution, the shim requires the copy's `.jj` to be a real directory,
not a symlink, and walks from the physical cwd up to the copy root. Any nested
or linked `.jj` below that root is refused, including a dangling symlink, so
workspace discovery cannot redirect an invocation to another copy.

Repository-wide command groups are allowlists; any unlisted subcommand is
refused, including future additions:

- `workspace`: `list`, `root` only (in particular, never `remove`).
- `tag` and `bookmark|b`: `list|l` only (never `track`/`untrack`).
- `operation|op`: `log`, `show`, `diff` only.
- `config`: `get|g`, `list|l`, `path|p` only.
- `git`: `root` only.
- `util`: `completion`, `config-schema`, `markdown-help`, `install-man-pages`,
  `exec`, `snapshot` only.

`file track` accepts only explicit existing non-secret paths matching
`^[A-Za-z0-9._/@+,-]+$`, with no leading `-`. Tokens after `--` are checked as
paths even when they start with `-`. Quotes, `&`, whitespace and other fileset
syntax are refused rather than interpreted. It accepts only non-secret files; secret-policy
basenames, directories, omitted arguments and fileset expressions are refused
because a broad fileset could start tracking a secret. The `.env.example`
exception remains. Under `--at-op`/`--at-operation`, only `log`, `show`, `diff`,
`status|st`, `evolog`, `interdiff`, `file list|show|annotate|search`, and
`operation|op log|show|diff` are admitted.

Every admitted invocation injects `plak_base()` naming the recorded mint base,
`immutable_heads() = (all() ~ plak_base()+::) | (working_copies() ~ @)`, and
`snapshot.auto-track` excluding every secret-policy path (case-insensitive,
including the `.env.example` exception). Thus only the copy's own descendants
of its base are mutable and every other workspace's ancestry is immutable.
The shim is regenerated from the recorded base at hosted construction; no
user config is written. Other user settings still apply. At hosted
construction the copy's `refs/plak/<namespace>/bases/<agent_id>` pin must match
the recorded row base. A missing or mismatched pin refuses with `jj copy
base provenance is missing or differs; inspect the retained copy`. An admitted copy
may have moved its `@` off that base through `new`/`rebase`; follow-up and
reload still regenerate its shim. Sealing records `history_left_base` in the
canonical `seal_outcome` payload and result-mail replay emits `history left
plak_base(); the candidate is the whole tree` when true. The transient snapshot
observation uses the sealed commit and recorded base under the unlocked seal
phase, never a later working-copy observation. Tree schema 10 includes this
payload; the versioned state key identifies its store tuple. A JJ copy
with no owning tree row refuses with `jj copy has no recorded tree row; launch
from its owning session, or discard or close it and respawn`.

Repo creation inside a copy is refused because a child re-initialising its copy
turns it into a standalone repo the launch checkout cannot see, and its seal
then faults.

**What the shim cannot enforce.** It is basename-only. An absolute
`/usr/bin/git` or `PATH=/usr/bin git` with an explicit repository path or
deliberately cleared discovery ceiling, raw `gix::discover` in a custom
binary (only environment-aware gix discovery honors the ceiling), a
`git worktree add` outside the copy, `git replace`, `git filter-branch --all`,
`git repack --unpack-unreachable`, `git fetch --stdin`, an alias that expands
to a guarded verb, and a hostile overwrite of the shim itself are all residual.
Isolation remains host-level; the shim is a guard rail, not a boundary.
Retention driven by repository/global config, or ordinary
`git maintenance run --task=gc` without an explicit `now` override, remains
residual; the guard does not evaluate every effective expiry policy or date
spelling. Direct-URL `url.pushInsteadOf` rewrites and pre-existing symbolic-ref
indirection beyond the explicitly guarded worktree refs are also residual.
Named operator-branch reflog expiry, such as
`git reflog expire --expire=now refs/heads/main`, is also residual: it can remove
the operator's recovery history without changing the protected ref itself.
These are not covered by the lexical ref and explicit-option checks above.

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
- `PLAK_COPY_ID`: `<tree8>-<agent_id>`, the new copy's tree-scoped VCS identity,
  not its logical agent id.

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

Product state lives in `product.sqlite3` at **schema 36**: intents and
feed-derived views (chapter 02 §2.6). Its `submissions` bind queued commands to
their exact bytes; the result-mail batch a root command carries is a tree
intent. The tree lives separately in `subagents.sqlite3` at **schema 10**, owned
by `plakous-recursive-subagents-store`. Its twelve domain tables and its custody
record are below; §14.3 classifies every column family as an intent, a view of
the feed, or an external-effect record.

A tree save is one `Immediate` transaction under WAL and `synchronous=FULL`.
It first proves the writer's custody claim against the tree's custody record
(below) inside that transaction, then compare-and-swaps the `agent_trees`
header with `UPDATE ... WHERE revision = ?` on the revision the handle last
had acknowledged, requiring
exactly one changed row, and then writes only the rows that differ from the
persisted tree at this revision and grant: in-place upserts for new or changed
keys, nodes first, and keyed deletes for removed keys, nodes last. The image
is keyed by this tree's persisted revision and full custody grant, both
checked inside the `Immediate` transaction. Database-wide `data_version` is
diagnostic only: another tree's write does not invalidate this tree. A fresh
grant invalidates the image even at the same revision, preserving the
same-revision receipt contract.

Within one process all store handles for the same physical database file
share one writing connection and its images. Thus two same-grant handles
cannot publish divergent private baselines. Registry entries are weak;
release, break-glass release and deletion discard that tree's image so a
dormant tree is not retained by another resident tree's connection.
Load always refreshes the baseline from current rows and the current grant
in one `Immediate` transaction. An image may therefore originate from a load
as well as an acknowledged save. A cold, refused, failed or truly stale image
is rebuilt from current rows inside the save's existing transaction; the
save then writes deltas, never deleting and reinserting unchanged rows.
Only an absent tree needs an initial insert. The header CAS, exact
`StaleRevision` refusals and old-or-new crash outcome are unchanged; a
quiet save writes no WAL commit, including while another resident tree works.

All production writes of tree domain rows enter the fenced save or delete
port. Recovery and feed-view repair, maintenance/GC/pruning, and imported
trees use those same ports. `plak tree release --force` releases the custody
record, not domain rows; the old grant cannot write, and a new grant advances the fence and
rebuilds its baseline. Schema admission initializes new files only; it does
not migrate existing trees across cohorts. Raw row codecs are not an
unfenced production write path.

Tree-store transaction begin and commit failures report the static phase and
SQLite category, such as `disk_full` or `database_busy`, without backend text.
A failed commit remains ambiguous until exact durable state is reconciled; an
operator must not repair it by guessing a new tree locator.


| Table | Key | Holds |
| --- | --- | --- |
| `agent_trees` | `tree_id` | root agent, revision, host generation, admission state, `writer_mode` (`single_lease` only), selected tuple, launch checkout, the frozen orchestration snapshot (epoch 0), INTEGER `current_epoch`, nullable JSON `forked_from` provenance (`node` with source tree, agent, line, branch and the source epoch whose bytes seeded it, or `import` with provenance text) |
| `agent_nodes` | `(tree_id, agent_id)` | ancestry, depth ≤ 4, brief, template, bind tuple, `NodeStatus`, workspace id, tier, current `attempt_id`, session locator, path, branch, active-turn flag, nullable INTEGER `fold_order` view position, nullable `line_id` (the node's L0 agent line, canonical UUID text, unique across the store: a line maps to one node for life and never moves to another node; a fork or import makes a new line), INTEGER `epoch_no`, nullable `runner_id` (NULL is the local runner), paired nullable `workspace_root` / `workspace_cwd` on that runner (NULL is the node's path), nullable `parent_composition` |
| `agent_grants` | `(tree_id, agent_id)` | `may_spawn`, `may_adopt`, depth and node and child ceilings, and the visible / peer / managed / interrupt / close id sets |
| `agent_operations` | `(tree_id, operation_id)` | the canonical request, its digest, the terminal code and response for replay, and paired nullable INTEGER `fold_anchor` / `fold_order` for host fold intents |
| `agent_messages` | `(tree_id, message_id)` | body, `purpose` (including `followup` and the host-authored `result`), priority, reply link, disposition, durable state |
| `agent_attempts` | `(tree_id, attempt_id)` | `AttemptStatus`, turn token and plain turn id, the 0–2 harness nudge count, `candidate_id`, `retry_of`, `request_key`, start and end stamps, the blocked reason, the failure `{kind, message}`, and the five usage counters; submitted output is derived from its intent |
| `agent_candidates` | `(tree_id, candidate_id)` | workspace, producing agent and attempt, base commit / content / change id, candidate commit / content / change id, seal stamp, adopter, merged-at target commit, target-before snapshot commit, evidence, `kind` (`work\|conflict`), source candidate, conflict target and JSON path/kind array, `resolves`, `adopted_via`, reversed target snapshot `reverted_at` and `reverted_by` |
| `agent_workspaces` | `(tree_id, workspace_id)` | owner, path, `mode` (`inherit`, `snapshot`, `fork`), `status` (`active`, `adopting`, `discarding`, `clean_removed`, `adopted`, `discarded`), base identity, nullable `input_candidate_id`, immutable resolver `resolves` / `conflict_base` / `conflict_target` / JSON `conflict_paths`, generation, and the in-flight adopter (`adopter_agent_id`, set only while `adopting`) |
| `agent_commands` | `(tree_id, command_id)` | every runtime command the tree prepares — a root admission's result-mail batch, a child Start, wake, mail Start, followup or corrective round — with its recipient, attempt, kind, exact canonical text and message ids, all immutable; the tree's withdrawal of a command no fact settled; and, as a view, the fact the recipient's feed recorded for it (`accepted`, `rejected`, or `returned` by a withheld terminal) with the plain turn id it bound and nullable INTEGER `accepted_seq` |
| `agent_feed_cursors` | `(tree_id, runtime_session_id)` | the last feed sequence the tree projector applied for that session, root or child |
| `agent_tree_epochs` | `(tree_id, epoch_no)` | a successor epoch (`epoch_no` ≥ 1): its frozen snapshot JSON and digest |
| `agent_tree_runner_ancestors` | `(tree_id, runner_id)` | the JSON array of commits that runner already holds for the tree, for incremental bundles |
| `agent_tree_custody` | `tree_id` | the custody record: `fence` ≥ 1, the last holder's `host_id`, typed `host_instance` (`<pid>-<16 hex nonce>`), nullable `host_binary` (build id) and `host_name` (machine), `claimed_at`, `renewed_at`, and `released` |

One in-flight status per row: a node seals, a workspace adopts, and neither
status appears on the other.

Today's flows write the schema-10 defaults except where §1.2 says otherwise:
no provenance outside a fork, and for every node no line, the local runner and
no workspace pair. A spawn writes its caller's epoch and composition; a
profile switch or a roster apply writes successor epochs. The ancestor table
stays empty. `append_epoch` (§1.2) is the only writer of
`agent_tree_epochs` rows and of a non-zero `current_epoch` or `epoch_no`; a
save after it writes the new row, moves `current_epoch` and the root's
`epoch_no`, and deletes the rows it folded, in the tree's one save
transaction. The state CHECK still admits `restored_paused`, a spelling no
current tree writes; it reads as `live` (§14.1).

**Custody.** `agent_tree_custody` holds one fenced claim per tree, naming one
host process by its stable `host_id`, its typed process instance, its binary id
and its machine. That `host_id` is the home's `host-id` (chapter 02 §2.1),
the identity runners pair with: a Plak process binds
it as its host identity before it opens its first tree store, creating it on
first use, and a process that cannot read or create it opens no tree store;
the refusal names the file and its remedy (fix or remove it, then pair the
home's runners again). A process whose custody was already taken under the
unbound default `local` (through a store opened some other way) keeps it for
its life and logs that it did. Every process of
one home presents the same `host_id`, so custody never tells two processes
apart by it: the process instance does, and liveness compares only the
machine and the instance. A runner's owner leases are per host instance too,
and a Plak process binds every runner connection with its custody instance,
drawn once for the process (before custody is bound when a runner is reached
first), so a line's holder on a runner and a tree's custodian are the same
process. A process binds the host id of the first home it opens a tree store
for. A record written before this binding names `local`; it reads as any other
holder, and the next take under the next fence names the home's id. It is not
tree content: no tree save or whole-tree replace changes it, and it has no
foreign key, so it may precede the tree's rows. A take
of an absent or released record claims it under the next fence. The holder's
own take returns its claim unchanged. Another host process is refused with the
typed `tree_hosted_elsewhere (<binary id>)` refusal naming the holder, its
machine and its fence, unless it presents a `HolderGone` for exactly that
holder at exactly that fence; it then takes over under the next fence.
`HolderGone` is minted only through the sealed `CustodyLiveness` interface, so
no crate outside the tree can forge one (a negative compile pin holds this,
chapter 05). Renew, release and prove succeed only for the stored
holder at the stored fence while the record is held, so a superseded claim can
never act again. A release keeps the fence and the last holder; repeating the
last holder's release succeeds, and any other claim, including a stale
lower-fence one, is not held. Each step is one `Immediate` transaction, so
concurrent processes serialize and exactly one of them takes. Both tree stores
implement these primitives, and the fenced save and delete, with the same
transition law, held by one conformance suite (chapter 05).

**Fenced writes.** Every tree write presents a custody claim, and the store
proves it against the stored record in the same atomic step as the write:
`TreeStore::save(tree, claim)` and
`TreeStore::delete(tree_id, expected_revision, claim)`, in the SQLite store's
one `Immediate` transaction; the memory store under its custody lock. A claim that does not
hold the tree (none was taken, it was released, or a later take superseded
it) writes nothing and is refused `NotInCustody`, or `HostedElsewhere` with
the typed `tree_hosted_elsewhere` refusal when another host process holds the
record. A delete removes the custody record with the rows; tree ids are
session ids and are never reused. There is no other tree write: the store's
capacity probe writes only its schema row.

A host process takes custody as one `CustodyHost`, fixed at its first take,
and counts its holders per store and tree, so every holder in the process
shares one claim and only the last one releases it. The tree driver takes
custody when it binds a tree to its store, before its first save (the
attachment publication included), and every save it makes presents that
claim; a driver whose take was refused refuses every save the same way, and a
hub does not attach it. A hub attach takes custody before it loads or
bootstraps the tree, so that first save is fenced too, and gives its own share
back on every exit: an attach that fails after its take leaves the record
released, not held by the live process. The driver gives its
share up when its last clone drops, so every writer that can still write (an
owed release, a waiter that captured the driver) writes under it. A planned
handoff's release is the exception: it gives the driver's share up
explicitly at its end, whether or not it cut a running child short, because
a child it let go of while its turn ran keeps a driver clone until its task
ends, and when that happens is the task's timing. Every later save of that
driver is refused as not in custody. The child's runtime branch lease still
guards its session until its task ends or the process exits, so a next host
that takes custody and reopens the child is refused (`lease is held`) and
retries, as a handed-off child the release cut short is always continued
(§14.1). A
write outside a driver (a headless flow, a bootstrap before the hub attaches,
a session deletion) uses the process's claim when it holds one, otherwise one
taken for the write and released after it (`TreeStore::save_in_custody`,
`delete_in_custody`). A release the store refuses leaves the record naming
the live process, which its next take joins. Every share ends in a release,
or is forgotten when its record went with the tree's rows; a last share
dropped otherwise would keep the record held until the process exits, and
debug builds refuse it.

**Liveness.** Custody's holder-gone proof is its own, not the runtime lease's:
a custody holder can hold a dormant tree with no lease. Before its first take
in a store, a host process takes an exclusive, close-on-exec advisory lock on
`<instance>.lock` in the store's `custody-hosts` directory (beside
`subagents.sqlite3`, created 0700) and holds it for the rest of its life; the
kernel releases it when the process ends, however it ends. The instance is the
typed `<pid>-<16 hex nonce>` drawn at process start, never free text, and the
process's host is fixed at its first take (Plak names the local host, this
machine's node name and its binary id when that is known without hashing the
image, from the install manifest or the identity cache; otherwise none).
Taking the lock sweeps the free locks
of other instances that no held custody record names; it takes those locks
before it reads the records, so an instance it sweeps has ended and can claim
nothing more, and a crashed holder's free lock stays until its record is
taken over or released. When a take finds another instance holding the tree,
the process asks the sealed liveness module, which proves that holder gone
only when it ran on this machine and this process can take its lock, and only
while the path still names the file it locked (the inode check after the
lock); a link is never read as a lock. The process's probes of other
instances' locks (this proof, a sweep's, a running-holder read) are
serialized, since a lock belongs to an open file description and two probes
of one dead holder's lock would each see the other's, and a lock found held
is asked again twice, 5 ms apart, before the holder reads alive. A
`holder_elsewhere` read (a host's session directory) names a tree's holder
only when this proof does not prove it gone. A missing lock file or directory proves
nothing, since it may have been removed under a live holder: such a holder
waits for the break-glass. With that
`HolderGone` the take supersedes the holder under the next fence. A holder on
another machine, or one that cannot be proven gone, is never taken over by a
clock: there is no time-based expiry. The operator's break-glass,
`plak tree release --force <tree-id>` (chapter 02), releases such a record with
a warning: `break_custody` marks it released, keeping its fence and holder, so
the next take supersedes that holder and every write it still offers is
refused. It never releases a holder proven to be running on this machine (its
lock is held); a holder on this machine whose lock is missing is unknown, not
proven running, and is released with the warning.

**The hub holds custody.** Only the custody holder materializes a tree's
nodes. A process's hub entry for a tree exists while that process holds the
tree's custody, and it follows custody rather than root leases. Any node may
bring the hub up: a root attach loads or bootstraps the tree, while a child,
or a root that is not attaching, loads only rows that are already durable
(and refuses a tree that is absent or whose snapshot digests do not match);
custody is taken first in every case. The tree stays in custody while any
node is resident or has work in flight: a root session is attached, a root
runtime built against the hub without an attachment (a rebind's successor
before it is opened) lives, or a child turn is live, starting or retiring.
Root dormancy alone never ends it. Only a rebind's hand-off to its successor
detaches the last root attachment (a profile switch whose published successor
did not project or attach on its one in-process retry hands its tree off the
same way, chapter 02 §5.5): from a tree that is not dormant the hub
then keeps the tree and its children running and ends it once the whole tree
goes dormant, woken whenever a child leaves the live table or a resident root
goes. Every other release of a session is its host's shutdown of that session
and runs the tree's joined shutdown at the last attachment, retiring live
children: quit, a headless exit, a failed open, and the session a fork,
`/new`, a session switch or a recovery leaves behind. So does a hand-off
after its host's shutdown is latched, and a host's idle release (chapter 02
§4.4), which happens only while no child is live, so the tree is dormant
and its custody goes last. At its quit the host releases the
attachments its switches left behind, then ends every tree a hand-off left in
custody that no root session holds again with its joined shutdown, and joins
the dormant trees' shutdowns already under way. Either way the hub entry goes first and
custody is released when the tree driver's last clone drops, after every
writer that can still write has returned, except at a planned handoff's
release, which gives it up explicitly at its end (above). A starting or retiring slot is work
in flight until its owner clears it, so an owner cancelled before it clears
its slot keeps the tree from going dormant; such a tree ends at a shutdown
release or at its host's quit. A tree brought up without a root attachment (by
a child, or by a root that is not attaching) arms no dormancy watch and is not
ended at the host's quit either: it stays resident until a root attaches and
releases it, or the process ends (an open residency item). A takeover needs `HolderGone` or a
released record; another process's attach is refused with the typed
`tree_hosted_elsewhere` refusal. After a takeover the reopen law and the
custody take's `recover_node` runs apply unchanged (§14.1).

**There is no migration.** A schema change moves the product's state directory
(chapter 02 §2.5); an older store stays in its own directory and is never read. The store fails closed on a **DDL
fingerprint**, not the version integer alone, so a store written by an earlier
build of the same version number is refused too. At the current version, a
shape that differs from the descriptors is refused as
`tree store schema shape does not match its exact revision`, a recorded
fingerprint that differs as
`tree store schema fingerprint does not match its exact revision`, and a
missing one as `tree store schema fingerprint is missing`, each followed by
`; current-schema tree state is not authoritative`. Another version is refused
as unsupported persistence (`subagents schema <n>, expected <current>: ...`).

Ordinary tree-store open validates version, exact shape and DDL
fingerprint, without scanning page integrity or foreign keys. Plak doctor owns
the read-only integrity and foreign-key diagnostics (chapter 02 §7.3); writes
still enforce foreign keys. The tree and product stores run the same writer
configuration and admission steps from `plakous-recursive-subagents-store`'s
`admission` module, each over its own descriptors (noun, file stem, archive
hint, version and objects); the refusal templates are shared. The product store
already depends on that crate.

### 14.1 Reopen

Root and child adapters share a small exact-retry state and reopened-turn
classification. It permits one stale-head adoption and bounds child lease waits;
child lease polls retain their
existing operation identities. The root reprojects before its second exact
open rather than blindly adopting the offered locator. Projection debt alone
never fences a current head. Shared reads detect terminal publication for the
root within its projected feed range and read committed wait evidence for both
roles. Role adapters retain their short open loops, ownership guards,
projectors and release/settlement order. No command or provider call is replayed.

A pending child Resume is re-derived from its immutable decision and the exact
canonical failed terminal, including when death occurred before that terminal
was projected into the tree. No saved decision means conservative ordinary
failure settlement. The new owner uses the same transient-continuation service
to reconstruct the saved round, re-enters its gate and retries the same
`plak:resume-transient:{agent}:{attempt}:{round}` identity. A prepared exact
Resume keeps its original prompt/mail batch rather than being withdrawn like a
generation-dependent corrective command. Accepted commands are never replayed.
The reopened gate holds a cancellable hub startup reservation; interruption,
close and hub shutdown release the session.
While waiting for reachability, live list/wait `waiting_on` includes
`network: true`, and the host tree view includes `network`. This label is not
persisted recovery authority.

A root or child, including the API lane, reads its own exact stored evaluator
artifact and process bounds before compiling its runtime attachment. Current
installation siblings are construction inputs for new sessions, not reopen
authority. Child reopen uses the child's locator, never its parent's artifact;
new children explicitly clear any parent reopen pin. Root reopen follows
chapter 02 §4.2's lease-free projection then exact-open order and repeats that
projection once on a stale answer. A child instead adopts the store-named head
once, re-reading its created output bound and recorded evaluator before opening
it. A second child stale answer is `recovery_session_unavailable`, not a held
lease or another adoption opportunity. No artifact lookup advances or repairs
a checkpoint.

A restored tree opens live (the reopen law below); nothing pauses it. A
stored `restored_paused` or `interrupted` admission from an earlier build reads
as `live` and is not written again. Root rebind follows the settled-tree
predicate. `NodeRecord::presentation_status` is the node's own status.

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

**The reopen law.** What the lost process left in flight was its lost owner's
(design laws L and R), and no turn is rejoined in place after a custody take.
A reopen marks only what the tree rows prove:

- In-flight VCS is faulted as above.
- A running child the rows show parked between turns is interrupted with
  cause `owner_lost` by an anchored interrupt intent, its attempt, subtree and
  surviving processes kept. Parked between turns means a `running` node whose
  current attempt is open and unsubmitted, with no bound or active turn, a
  session, and at least one turn of that attempt spent (an accepted command of
  the attempt, or a pending transient Resume, proves one when a retained-head
  seed no longer counts it). An unstarted spawn or fresh followup, even one
  whose Start was prepared, has none and is not marked.
  A parked child with a Wake or mail Start command the lost process prepared
  for its current attempt that no fact names and the tree has not withdrawn is
  marked all the same, and is owed a `recover_node` (below): that start may
  have been admitted, so its turn is cancelled, or it was not, so it is
  withdrawn with its mail released unread and unclaimed.
- A running child with a bound or active turn is owed a `recover_node`; so is
  an interrupted one, whose interrupt was written before its turn's terminal
  was projected.
- An interrupted child with a command that no fact names and the tree has
  not withdrawn is owed a `recover_node` too, which finishes the withdrawal.

An owed child takes no turn until its `recover_node` ran in this process:
mail for it is not delivered (the send waits as for a busy slot, `SlotBusy`,
and the mail stays durable), and an interrupt of it takes effect. The same holds for the operator's
mail and interrupt from a face attached to the node (chapter 02 §4.4); the
operator acts there as the tree's root, so its mail is the root's
`send_agent_message`, recorded and read with the root as its sender, and
reaches only the root's message peers. An operator with its own reach and
sender identity needs a sender kind the mail record does not have, and
waits for a format cut. The owed set is process-local; every
custody take rebuilds it from the tree rows. The first root attach after a
custody take runs `recover_node` for every owed child, descendants first,
before the root actor's first open; the root actor's open then starts one more
background run over the tree (below). Each run holds a child for its whole
recovery; another run skips a child that is held, a live turn, startup or
retirement owns, or a turn waiter's recycle owns.

`recover_node` opens the child's session exactly and lets the open decide:

- **A decision barrier is closed, never restored:** every child open closes
  effect-decision barriers (chapter 01 §5.5), so a turn the previous owner
  left waiting at an effect decision ends abandoned, nothing started, its
  decision never made: with cause `process_lost`, or `interrupted` when a
  planned handoff let go of that turn (no turn admitted after the handoff). Its history carries the
  abandonment note, never a requested cancel's, so neither the assignment nor
  the mail the turn carried is marked as something not to act on; that mail
  was read once and stays read, and the continuation acts on it. The same
  option closes a promoted server pause (the paused response already in
  history) the same way, with the same cause, claim `tool_round_completed`
  and the model-round note.
- **Restored, never rejoined:** an open that still restores the lost turn
  (`RestoredWorking`: a hosted tool-batch continuation, or a server pause
  the lost owner never promoted) does not rejoin it. The restored turn is
  abandoned first, within the host's cancel bound, with `ProcessLost`, or
  `Interrupted` when a planned handoff let that turn go. Its terminal is
  `Cancelled` with that cause's abandonment report, the history note is the
  abandonment note for the claim the ledger supports (chapter 01 §5.5),
  never the cancelled-prompt note, and the hosted baton is finalized in
  that same terminal commit. A restored child turn with a root queued
  behind it has no report (no closure row admits the queued root), so it
  falls back to a plain cancel's terminal and the operator note, and the
  host logs a warning naming the turn. The feed then holds that one terminal, and the
  child is marked as below. The continuation is not told the operator
  cancelled the request, so it does not treat the assignment as something
  not to act on; the mail that turn carried stays read once. A child
  already interrupted (a continuation, wake or mail Start the lost owner
  admitted before binding it, so not the turn its attempt binds, which its
  latest interrupt already covers) has its restored turn lost again: the
  `owner_lost` interrupt is recorded anew, anchored on that turn, and the
  child's continuation is decided on it under the policy (§14.1, continuation
  after a loss).
- **Marked:** every open (the open restored the lost turn and it was
  abandoned, closed it as abandoned, or found no turn) marks the child right
  after the open: the interrupt with cause
  `owner_lost` is written first, then the reopened session is released and the
  feed its open left is projected, so the abandoned turn's terminal folds as
  the interrupted turn's. The open itself projects nothing: a lost turn's
  terminal folded before the mark would interrupt the child with no cause, and
  so with no continuation. A prepared command that feed does not name never ran
  and is withdrawn, its mail unread; a turn binding no terminal released is
  released. The owed mark settles last, so no delivery sees the child between
  the mark and the projection.
- A refused open leaves the child owed, takes no new turn, and records the
  refusal as its fault.

Inside a feed fold, law L applies in place: the terminal of a turn an exact
open closed (or a resumable turn a recovery reopen cancelled) interrupts its
`running` node, keeps the attempt open with no failure, and lifts the
retryable `recovery_required` the old law left; the abandonment is recorded
once on the node. A rebuild of the same feed applies it again. Neither path
ends the attempt, seals the copy, or sends a result: nothing is reported to the
parent for a lost turn. An attempt that already ended, or that submitted a
result, has no lost turn: its unresolved release settles as before, a submitted
result completing the attempt. A child whose copy is in an unknown VCS step
keeps `recovery_required` and leaves only by `close_agent`; its session part
still settles.

*Durable before and after.* The parked mark: before the reopen the rows hold a
running parked child and no interrupt intent; after it they hold one anchored
`owner_lost` interrupt intent for its current attempt, nothing else. The bound
mark: before `recover_node`, the bound turn and the child session's head the
lost process left; after it, the interrupt intent, the feed records the exact
open wrote (an abandoned terminal or none), the projection of those records,
and the withdrawn unadmitted commands. A restored turn is abandoned before the
mark; a crash after that abandon leaves its terminal in the feed, which the next
`recover_node` folds after its mark. A crash between any two of these leaves rows the next
reopen classifies the same way: an intent written is folded again, never
duplicated (it is keyed by its fold position); a child marked but not yet
projected is interrupted and still bound, so it is owed again and its next
`recover_node` projects the feed, withdraws what never ran and releases the
binding; a child whose mark projected its feed but did not reach the
withdrawal is owed again for the commands still unresolved; an owed child is
owed again.

*The reopen is idempotent.* The mark reads durable rows only and its intent is
keyed, so two reopens of the same rows mark the same children once; a reopen
of rows an earlier reopen already marked finds those children `interrupted`,
not running, and marks nothing. Deleted views prove nothing until the feed
rebuilds them: such a child is neither marked nor owed at reopen, and its
`recover_node` judges the rebuilt rows by the same law.

**Continuation after a loss.** Recovery spends no model call; paid work after
a loss runs only through the host's continuation service. A child's
continuation is **held** while all of these hold: the node is `interrupted`,
the latest interrupt intent of its current attempt has cause `owner_lost` (or
`handoff`), the attempt is open, unsubmitted and has turns left under
`max_turns` (a spent attempt is never held, so a continuation never runs past
it, after any number of losses; a spent attempt a loss stopped stays
`interrupted`, as an interrupted spent attempt does, until its parent closes
it), the node is not closed, no
continuation decision names what the loss stopped, and no turn ran on the node
since. The held state is derived from those rows; nothing records it. What the
loss stopped (the decision's anchor) is the turn of the attempt's latest
accepted command, which is the lost turn for a bound mark and the last turn for
a parked one; with no accepted command on record it is the loss's interrupt
intent. A held continuation ends through actions that already exist, with no
record of its own: a close, another interrupt (whose cause then reads; a root
stop records one on each held child, so stops stay stopped), or any later turn
on the node (mail, a follow-up, a Resume).

Consent is the one durable continuation decision intent of kind
`continuation` for the attempt, cause, host generation, round 1 and anchor,
deciding a new turn under its own command id (`plak:resume-loss:`). It is
written and published before anything is dispatched; a refused publication
rolls it back and nothing runs. With consent recorded and the turn not yet
admitted the continuation is **pending**: every recovery run resubmits it
under the same command id, and an owner-lost mark keeps that command rather
than withdrawing it. Only a still-pending continuation's command is kept: once
the continuation ended, its prepared command is withdrawn like any other (the
reopen law owes the child for it, and a Start refused because the hold ended
withdraws it at once), so its batch returns to unread and the child takes
commands again. *Durable before and after:* before consent the rows hold
the held node and no decision; after it, the decision and nothing else; the
Start then writes the command intent before it submits. A crash between the
decision and the submission leaves it pending, and the next run submits it. A
crash after the runtime admitted the command leaves either a turn the child's
exact open restores, which is cancelled and lost again (the child is marked
anew on it, and its continuation decided on that turn), or an accepted
command the next projection names, which is no longer pending; so a
continuation is never dispatched twice.
Its Start re-checks that it is still pending when it reserves the child and
when it binds the turn, so a close, an interrupt or another turn that ended it
refuses the Start. A Start the runtime already admitted when its bind is
refused is released at once, and the release cancels its turn. The node then
keeps no turn bound, whatever that turn's terminal: a turn that reached its
provider before the release stopped it can end needing recovery. Its command
settles from its record, since its batch is already in history. Once its command is accepted and its turn runs, the
continuation is **running**. The turn is a new host instruction to continue
the assignment from the last completed step without repeating completed
actions; it never replays the lost turn, carries the child's unread mail, and
counts against `max_turns` like any turn.

Under `[host] resume_after_loss = "hold"` (the default) a held continuation
waits for the operator's Resume: the TUI's `r` on the held child in the Agents
tab, or JSONL `resume` (chapter 02). Resume with no agent resumes every held
child. Under `"continue"` the `recover_node` run the root's open starts
resumes each held child once by itself; the attached root sets the policy on
the tree's hub when it opens. Each run ends with that continuation pass, after
every recovery it made, and reports the children still held in one live tree
notice that is never stored.

**What the parent learns.** A loss sends no result mail. Besides its
`wait_agent` rows (§4), a parent's next command after a loss names each of its
children a lost host interrupted (cause `owner_lost`, with a continuation
held, pending or running), once per host generation, in the process-notice
encoding: a `[harness]` block appended after the command's text and mail, one
`<agent_notice agent_id="c1" cause="owner_lost" continuation="held">` element
per child (`held`, or `continuing` when consent is recorded or the
continuation runs) saying the host was lost, the attempt and its work are
kept, no result was sent, and whether it waits for the operator or continues
by itself; sixteen are named and the rest counted. A handed-off child
continues by itself and is not named. The root's next command is an
operator prompt, a wake that carries mail (a wake with no mail takes
nothing and parks), or its own continuation after a loss; a child parent's is
any command the host prepares for it. Nothing is stored: the host's tree
driver holds which children it already named, in memory, so the next host
generation names those still interrupted again. A child counts as named only
once the command that carries the notice is recorded and committed, so a
command that is never recorded (a refused admission, an interrupted wake, a
refused child command) loses nothing and the next command carries it; one
with no room left under the prompt bound does not carry it either. Once
recorded, the notice is part of the command's exact text, so an exact
resubmission carries it again. A child whose turn was running when its host
died is marked owner-lost by the open's recovery run, so a command prepared
before that run marked it does not name it yet; the next one does.

**The root's continuation after a loss.** A root turn its lost host left
running is closed by the root's next exact open, whose terminal is an
abandonment with cause `process_lost`. The root records that loss as an
interrupt with cause `owner_lost` in the product store's root interrupt,
anchored on the lost turn, whether or not its tree has rows. A turn a live
owner fenced ends recovery-required instead and is not a loss of its owner:
its in-place recovery records no interrupt. The open finds the loss in the
root's feed, not in a view a projection may already have moved past: before it
projects anything it reads where to start, the earlier of the views' cursor
and the anchor of the root's latest accepted submission (the lost turn's
submission is never newer), and after its projection it scans the feed from
there. The loss is the last root terminal in that range when no root turn was
admitted after it. A scan start below the feed's retention restarts at the
retained start, since only the last root terminal matters; a lost turn whose
terminal is itself no longer retained is not found, so it is not held. The
open records it once the views applied that terminal, at the applied cursor,
once per lost turn; a projection still behind leaves it to the next open. The root is never blocked: it stays promptable and its
restored queue stays paused. Its continuation is **held** while that loss is
the root's latest interrupt, no continuation decision names it, and no root
turn was admitted since (a submission written at or after the loss's cursor
whose command was accepted; the lost turn's own submission is older, so a new
operator prompt ends the hold, which is the later-turn rule applied to the
root). Consent is one product-store root continuation naming the loss's stop
anchor, deciding a new turn under a `plak:resume-loss:root:` command id
derived from the loss intent's position. It records host generation 1: a
loss names the one root turn a lost host left, so one decision per loss is
already one per host generation. It is written before the turn is
prepared; the turn is a host instruction (a host-wake submission under that
id) to continue the current task from the last completed step, never a
replay. A pending decision whose submission exists is that queued item, which
a reopen keeps rather than discarding like an unadmitted wake (once the
decision is no longer pending the reopen discards it like any other); one
whose submission does not exist yet is prepared again under the same id; a
decision whose command was accepted is no longer pending.

*Durable before and after.* The closure: before it the feed holds the lost
turn unfinished; after it, the turn's abandonment terminal, and the product
store holds nothing new. The loss: before it the terminal is in the feed after
the scan's start; after it, one root interrupt and nothing else. Consent: one
decision and nothing else. The preparation: the submission with its queue
item, in one record write. The dispatch: the runtime's command record under
the decision's id. A crash between any two leaves rows the next open reads the
same way, and the command id is the same, so the root's continuation runs
once: an unrecorded loss is found again by the next open's scan; a decision
without a submission is prepared again; a submission the runtime did not admit
is the queued item the next open keeps and runs; an admitted one is either a
turn the next exact open restores, which runs on, or one it closes, which is a
new loss of the continuation turn with its own hold, never a second dispatch of
the first.

The operator's one Resume (`ResumeLossContinuation` with no agent: JSONL
`resume`, the TUI's `Ctrl+R` on the paused queue, and `/recover` or JSONL
`recover` on an attached face while the root holds one; `plak session recover`
only recovers) resumes every held child, records the
root's consent and runs its continuation, and lets the restored queue drain.
Under `[host] resume_after_loss = "continue"` the root's open does the same
for the root by itself: its held continuation runs once and its restored
queue drains; a decision a crash left pending runs at the next open
under either policy. The attached root sets the policy on its tree's hub
wherever the custody-take scan runs (every root attach) and again when it
opens.

**A planned handoff.** A reload (and, once they exist, a host restart or an
upgrade) releases its session as a planned handoff: the face requests the
release with the handoff armed (`request_handoff_and_release`), and the
release takes the flag first, so a refused or failed release leaves none
armed and a later plain release never inherits it. The release's cancel of
a running root turn is bounded by the host's cancel bound (5 s), not an
interactive quit's 300 ms, so a turn slow to settle under load is still
handed off; the reload's join waits that grace and the release after it
(a child's cancel inside the hub's release, and its two-second join) beyond a
quit's. A turn
that does not settle within it degrades the release into an ordinary loss:
the handoff is disarmed and nothing is recorded, the release abandons the
turn by lease and says so with a warning, never an error, and the next open
holds the turn (and marks the running sub-agents `owner_lost`). Inside its own release,
before any lease is released, the releasing host applies each running
child's feed to its tail, then records an interrupt with cause `handoff` on
each child that is running or parked and whose assignment started (a turn of
its attempt was admitted; a child reserved for startup whose Start was
never accepted is not handed off, and its startup is recovered instead; a
child held after a loss keeps its hold), and, when its quit reached a running
root turn, a product-store root interrupt with cause `handoff` anchored on
that turn, once its views applied the turn's admission (so the turn's own
submission is older than the interrupt; when they had not, nothing is written
and the release logs `session.handoff_unrecorded`). The projection first puts
the handoff's fold position past the acceptance of the turn it interrupts, so
that turn never reads as one admitted after the handoff. The hub's join retires
children by depth, deepest first, siblings concurrently, under its one
two-second bound (§16), so no node is released while a descendant of it is
still releasing or opening (a cancelled startup settles in its depth's level). A child whose release
outlasts the hub's two-second join (its turn ignores the cancel), and every
ancestor of it the spent bound then cuts short, keeps its
`handoff` row and records nothing more: the shutdown's joiner never writes a
retirement fault for a release its bound cut short, because that would end the
node as `recovery_required`, which no loss or handoff law reaches, and a
reload's orchestration carry would then drop it as finished; a retirement that
did finish inside the bound with an error still records its fault. The
retirement's own owner keeps its 65-second deadline (the cancel bound plus the
session release bound), past which it records the fault itself; a host that
exits or execs before then (a quit, `/reload`) leaves nothing. The next owner
reads the unsettled turn as its own loss, or as the handoff it was. For a
planned handoff such a release is no failure: the release says which children
it handed off with their turn still running (a live warning, `handed off c1
with its turn still running; the next host continues it once`) and exits
cleanly. A plain quit that cuts a child short reports the release as failed,
as before, and the next open reads that child as a loss. Only the releasing host writes it; the face never touches
the stores. The next host continues each handed-off node once by itself,
under either policy: a child through the continuation pass of its first
recovery run, the root at its open, whose restored queue then drains. A
handoff covers the turn it interrupted: a turn admitted after it and lost
again is an owner loss, held for consent, so a handoff continues by itself
once. A spent attempt is handed off but never continued. A handoff write that
fails leaves the release as it was: the next open reads an ordinary loss (or
nothing, when the release's cancel settled the turn) and holds, and nothing
continues by itself without a durable handoff row. *Durable before and
after:* before the record the rows hold the working nodes; after it, one
`handoff` interrupt per node and nothing else, then the release. A crash
after the record reads as the handoff it was meant to be; a crash before it
reads as a loss. `/reload` hands off a running root turn and running or
parked sub-agents when it resumes the same session; it still refuses while
background processes run (chapter 02 §5.1).

**Stops.** Every interrupt intent records its cause: a parent's
`interrupt_agent` records `parent`, the operator's interrupt of a node
`operator`, the root's interrupt reaching its descendants `cascade`, and an
embedded run's exit `stopped`. The operator's interrupt of a running root
turn records a product-store root interrupt with cause `operator`, anchored on
that turn, before its cancel is requested; a refused write is tried once
more, and a stop whose record still failed is reported and cancels, but is not
durable (a host lost before the turn's terminal then reads a loss). Any root interrupt anchored on a
turn is final for it: a stop recorded first keeps a later loss or handoff of
the same turn from being recorded, so a host lost before the stopped turn's
terminal leaves a stop that nothing continues, under either policy. The
operator's interrupt of an idle root records nothing and keeps a held root
continuation, so a stray interrupt never discards it: only the Resume, a new
prompt or a stop of the whole tree ends it. A stop of the whole tree while the
root holds a continuation after a loss, or has one pending, records `stopped`
anchored on the loss's turn, which ends the continuation and withdraws a
pending continuation's prepared submission with its queue item. A host's
`stop_session` (chapter 02 §4.4) is a stop of the whole session. Child-wake
suppression is written first. That suppression is what refuses a child
wake, until a later root turn is admitted: a prompt admits one and clears
it, so a child result after the prompt wakes the root, and until then a
child result does not. Then every running or held child is interrupted
with cause `stopped`, as an embedded run's exit does, except a child whose
rejected wake is already ending its attempt (§5.2).
This includes a child reserved for startup whose Start was never admitted:
it must not remain `running` after a stop merely because it has no turn owner,
park or loss continuation yet. Reopen retains the stopped interrupt and cannot
hand that assignment off. The additional stop selection applies only before
the assignment has started and never to a park-ending child. Planned handoff
still leaves unadmitted startup to its startup-recovery law rather than
inventing a continuation.
On a running root turn the children are signalled and not joined; the idle
boundary joins the retirement. A child that has not retired within the host's release
bound after the tree's stop records are durable does not fail the stop.
The root's `interrupt{stopped}` is recorded next, and skipped when that
turn's terminal is already applied, so a finished turn is not anchored
`stopped`. Background processes are stopped with the record, and the
cancel is requested only after it. Once the turn has ended, the idle
boundary repeats the suppression and the tree stop, pauses a queued
drain, stops background processes, and writes the root record last:
`stopped` on a held or pending root continuation, which ends it. That
root record is the commit point either way. A stop during a workflow is
observed as the operator's interrupt is, and the rest of the stop waits
until the workflow ends. The idle stop also joins tracked child recovery
before interrupting and retiring children, so recovery has released its
canonical leases before the daemon stop takes a fresh head lease. This join
uses the host release bound. If recovery exceeds that bound or fails, the stop
still records the descendants' `stopped` interrupts and the root interruption,
and signals child cancellation without joining their retirement. Unjoined
recoveries remain tracked. Only daemon stops for child lines still owned by
recovery are skipped; other child lines are attempted. The root stop record
may commit, but the request reports failure rather than claiming full completion.
The running-turn stop signal
does not wait for recovery. Failed joins retain only unjoined recovery handles;
a completed or failed handle is consumed once, never polled again on retry.
*Durable before and after:* a crash before the
root record is an ordinary loss, held or continued under the loss policy.
After it, with no root turn admitted since, the whole tree reads stopped
and nothing continues it under either policy. In `plak host`, the returning
process finishes recorded stops of daemon-held lines after its re-adoption
sweep and journal recovery, before serving. It takes a fresh head lease and
transfers the line before stopping remaining jobs, keyed by the recorded
stop intent; a re-cut repeats the key, not the process effect. Child stops
use the current attempt's existing anchored interrupt operation, and find
the line through its recorded session locator; a held child's daemon job
therefore stops even when no child runtime is materialized. A later root
attachment that rebuilds a held child transfers its daemon jobs on ordinary
release rather than stopping them. Each child stop is attempted even when
an earlier child refuses; failures are collected and logged. A later root
admission ends an old root stop, and a stop never follows an old product
record onto a successor head. Terminals are
recorded under that lease. Only this serving process finishes those stops,
never an inspecting or embedded process. Once a child line's recorded stop
succeeds, the host retires any process-registry port below the stop's owner
fence: that port no longer has control authority. A newer attachment's port
is retained, and the job terminals stay durable on the line.
An idle host can hand off its
resident daemon jobs without rebranding a cancelled turn from a previous
attachment as newly handed off: the root handoff record belongs only to a
root turn this attachment quit while driving. A planned handoff or a
later cascade does not replace `stopped` on a child while no turn of
that attempt was admitted or bound after the stop; a child resumed onto
a later turn is interrupted again. A child wake does not prompt that
root while the suppression holds. The request is answered only once the
stop is durable; the settle wakes the connection that asked, before the
notice.

A child whose own turn a live owner fenced is recovered at once, without
waiting for the parent: its turn waiter releases and reopens the session as
soon as the turn settles and records the outcome itself (section 14.2). The
parked set and `waiting_on` are process-local.

A child turn's start, whether its assignment's first turn, a turn its
waiter starts again (an instruction or continuation turn) or a parked
child's wake, projects the child's feed only through the record that
accepted its command, which binds the turn. What the running turn commits
after that is folded at the tree's next record of the child: its waiter's
terminal, a handoff or a release. The waiter's fold at a turn's end goes
only through the record that ended the turn; what the child commits after
it (a parked job's records, its next turn) is folded by the tree's next
record of the child. A turn the child's retirement cancelled (a close, the
host's release) ends while that retirement still publishes its release
page, so the waiter first waits, within the retirement's own deadline, for
that retirement to finish: the release page is always folded before the
turn's end, whichever of the two the scheduler ran first. The tree's writes
therefore do not depend on how far the child got by the time a turn's start
or end was projected.

Before a parked child's wake dispatches, the tree projector applies the
child's feed to its tail (§14.3), which settles the child's await receipts
(§4.4), and a spent `max_turns` ends the attempt `blocked`, its subtree closed
and processes stopped. Every child command is an `agent_commands` intent written
before submission, and what became of it is the child feed's
`CommandAccepted` or `CommandRejected` fact with that command id:

- A command with `CommandAccepted` binds the attempt it names to the fact's turn
  id; the binding is a view, so a turn admitted before a process loss is bound
  however far the lost process got. Its batch is acknowledged. If that turn has
  no `TurnTerminal` yet, it was in flight when its owner was lost: it is never
  rejoined after a take, nor left `recovery_required`. The take's
  `recover_node` cancels it and marks the child; a mail delivery whose reopen
  finds one does the same and fails closed, its mail staying unread for the
  child's continuation or next wake.
- A command with `CommandRejected` is settled rejected: its mail returns to
  unread and nothing is retried.
- The live submission site settles the same outcome from its acknowledgement,
  so the live host never waits on a feed pass: an admitted command takes the
  accepted fact (the later pass over its record changes nothing), and a
  command the runtime definitely refused before admitting it, which no fact
  will name, is withdrawn with its mail unread. An accepted command binds no
  turn onto a node an interrupt or close settled; its batch, already in
  history, is still acknowledged.
- A command with neither fact was never recorded. After a process loss
  nothing submits it again: the owner-lost mark withdraws it with its mail
  unread (the reopen law above).

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
Start and the parked-wake submission sites apply this. The pass
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
command id, not a spent turn.

A turn binds to at most one attempt. An accepted command whose attempt already
binds another turn fails closed: the binding is untouched, the recovered turn is
cancelled and released, and the operation is `operation_recovery_required`,
leaving the child `recovery_required` (§14.2). The send itself never binds a
turn or marks one active; active-turn is the view "bound turn with no
`TurnTerminal`", so a turn that ended or was replaced before the delivery
returned is never marked active again. A running turn that no tree command
admitted is left to recovery.

### 14.2 Settling `recovery_required`

Conflict pinning and acceptance run under ordinary `adopting` workspace holds.
An interrupted pin may exist without a candidate record: a later explicit
conflicting adopt reuses that unrecorded ordinal and overwrites its pin pair,
or orphan-reference maintenance removes it. Reopen never retries the landing.
Resolution acceptance also holds the source workspace so its advanced base pin
can be reconciled from the committed row after a cut.

A failed retry of an unsettled interrupted adoption retains the `adopting` row
and recovery hold for every failure outcome, including conflict, target changed,
unavailable and recovery required. The operator inspects the target and then
uses `close_agent`; the copy and its provenance pins remain until tree deletion.

`ProductAgentTree::apply_host_recovery`, called only by the tree
projector's application of a child's feed records (§14.3) and by the explicit
settlements below, is the only place that settles a **recovered** unresolved
session failure (`turn_recovery_required`, `session_retirement`, or a startup
or materialization failure) to a terminal outcome. The ordered session-binding
intent can instead clear its exact materialization failure and continue running
(§14.3); close, seal completion, adoption and replayed binding never clear it.
A lost turn is never settled here: law L (§14.1) interrupts its child and keeps
the attempt open. Terminal recovery settles only an attempt with no lost turn:
one that already ended or submitted and whose release alone was uncertain, and
one whose startup never produced a session, which ends `failed` with kind
`spawn_refused` (see the replay below). A quiescent reopen of retirement-only
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
caller through the operator retry's receipt or the pass note below. The host
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

The host settles a child by reopening its session exactly (§14.1). Here (for
the operator's retry and an unresolved session outcome; an owed child's
`recover_node` cancels it the same way and marks the child, §14.1) a head that
restores a resumable working turn is cancelled and released, never rejoined, and the child is interrupted
(law L). A head that requires effect evidence is
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
record (§14.3). A child's `TurnTerminal` abandonment fact applies law L, the
abandonment recorded once with its operator line, in this process or any later
one; so does the closure record of a turn whose terminal was `RecoveryRequired` (it
finalizes the turn's operations and appends its history with no second
terminal), when its `feed_terminal` read reports the abandonment. If
the tree transaction that would record it is refused, the tree cursor stays
behind and the refusal is `recovery_storage_unavailable`; the next attempt
applies the same record and settles the same way. There is no in-process memory
of an observed abandonment. When the child's feed, applied to its tail, holds
no terminal for the attempt's bound turn and its head is quiescent, the child is
interrupted the same way; its note says no abandonment evidence is available,
never that nothing was abandoned. A locator advance is a view and is
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
For an already-settled, failed-idle child this retry is the operator's (section
4.5a), not `/recover`: a `recover_node` run only selects unresolved child session outcomes.
A dirty whole-tree revision alone does not identify which child's recovery
outcome has not yet been reported; it is not automatic child recovery authority.

Settlement has three triggers:

- the operator's retry (section 4.5a), for one child, with its bounded lease
  wait: the Agents list's confirmed `u` (`RecoverTopologyNode`) or a JSONL
  `invoke_tool` of `recover_agent`, which use that port and no other;

- a `recover_node` run over the tree: the custody take's (the first root
  attach after this process takes the tree's custody runs `recover_node` for
  every owed child, and the first root open after it starts one background
  run; a later open of a tree already in custody starts none), one per
  returned child startup or retirement release with recorded fault or
  remaining recovery need (including release/publication failure), one per
  next-prompt or `/recover` recovery of an interrupted root turn, and one per
  explicit `/recover` with no interrupted root turn. A run tries the owed
  children first, then each child with an unresolved session outcome, each
  held for its own recovery; it never waits on a lease (a held lease is
  `recovery_lease_held` at once) and has no deadline. The host loop never
  waits on it. Runs may overlap: a run skips a child another holds, and that
  run reports it. The hub tracks every run; quit and `/reload` wait briefly
  for running ones at hub shutdown and then abort them, before any lease is
  released (below). Release-triggered runs publish the recovered tree view and
  log their report. An explicit `/recover` says so, and one that starts a run
  with children to try says so; one that starts a run with no child to try
  answers `nothing to recover` itself, before the next intent, rather than from
  the run, which a quit right after it would abort before it reported.

A fenced child's turn waiter releases and reopens the child session itself
(section 14.1). Its reopen may wait out a lease. The node is marked as owned by
the waiter before its live entry goes, and the mark is cleared only once the
outcome is recorded; it is held by a guard, so a recycle that panics or is
cancelled clears it too. A recovery reads the live entry, then the mark, so it
treats the child as owned (`recovery_owner_live`) throughout, and the custody
scan leaves the node to the waiter.

Each host-loop-requested run that settled or refused anything reports once to
the user as a system note naming the children it recovered (interrupted, attempt
kept) and each refused child with its `recovery_*` code. A refusal note names
the operator's retry and explains that `/recover` only selects unresolved
child sessions, not an already-settled child's pending publication. A child
whose session settled but
whose seal or adopt is still ambiguous is listed as refused
(`recovery_vcs_ambiguous`), never as recovered. A VCS-ambiguous child stays
`recovery_required`; `list_agents` and `/tree` show it.

`list_agents` lists `recovery_required` rows even without
`include_completed`, and a full row carries a `recovery` note saying the host
is recovering its session, with no remedy, when a session recovery would
settle it, or `remedy: "close_agent"` with a
`recovery` reason when only a workspace ambiguity remains or a terminal
refusal holds it. A failed copy removal (`workspace_cleanup`) is a workspace
ambiguity, not a session recovery. Interrupted seals,
adopts and copy removals are never settled automatically or by the
operator's retry: their outcome is a VCS question, and the tree records no
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

A `Resume { round }` decision from the shared continuation plan is durable
before the failed terminal is projected.
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
| Tree identity, selected tuple, launch checkout, frozen snapshot, admission state (`live`, `interrupted`, `restored_paused`, `tree_terminal`, `closed`), host generation | intent | the root session; interrupt and close requests |
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
| Failed-turn rescue snapshot ownership | external-effect record | `agent_operations` of kind `rescue_pin`, keyed by rescue id and owned by its producing agent, captures pin ids before the VCS write; conversation collection transfers cleanup ownership to `candidate_unpin` (§6.7) |
| Tool and root descendant interruption | intent, joined into views | anchored `agent_operations` of kind `interrupt`, per target attempt; captures whether live ownership or a non-ending park made the request effective, and derives only that agent's interrupted status |
| Session bound after materialization failure | intent, joined into views | `agent_operations` of kind `session_bound_after_failure`, keyed by the exact materialization intent, ordered with the other host intents |
| Submitted output and submitted attempt status | intent, joined into views | anchored `agent_operations` of kind `submitted`, per attempt, owns the validated exact `output_json`; store load joins output onto the in-memory attempt even without refolding, and its agent-local fold derives output and status, including historical attempts. `agent_attempts` has no output column and submit tool receipts keep no payload copy; provisional wait receipts retain their exact returned views |
| Topology operation receipts (`agent_operations`), wait receipts | intent | the runtime call id; terminal responses replay for the same full canonical request digest; `submit_result` stores only turn token, output SHA-256 and byte length in its canonical request column |
| Mail body, sender, purpose, priority, reply link, disposition | intent | the sending call |
| Mail durable state beyond `accepted` (`acknowledged`, `delivered`), returned admissions | view | wait receipt settlement (§4.4) and `agent_commands` batches against `CommandAccepted`, `CommandRejected` and withheld `TurnTerminal` facts (§8) |
| Child commands and root result-mail batches (`agent_commands`) | intent | the runtime command id |
| Below-retention command consumption, undeliverable mail | intent, joined into mail views | `agent_operations` kinds `consumed_command` (exact resubmission's command id), `failed_wake_mail` (agent, attempt and ended turn; immutable mail ids and notice), and `undeliverable_mail` (a terminal or cancelled send's envelope id) |
| Session retirement settled | view | the child's `LeaseReleased` record after its last terminal |
| Seal state, candidates, workspaces and their `sealing`, `adopting` and `discarding` statuses, `on_mint` | external-effect record | the tree's own VCS and copy operations, recovered by §14.1 and §14.2, never from a feed |
| Seal reservation | external-effect record | `agent_operations` of kind `seal_effect`, keyed by agent and attempt; captures `reserved_at` and `resume_status` before dispatch, including a seal with no divergent candidate |
| Seal outcome | intent, joined into views | anchored `agent_operations` of kind `seal_outcome`, with attempt, completion stamp, failure, candidate id and `history_left_base` flag; the fold derives settled status and `seal_failed` at that position and fences result mail once |
| Orphaned VCS operation and its settlement | intent, joined into views | anchored `agent_operations` of kinds `vcs_fault` and `vcs_fault_settled`, per affected agent and attempt; reopen records seal, adopt and removal ambiguity, and acknowledged workspace completion records settlement |
| Spawn refusal, adoption acceptance and failed workspace removal | intent, joined into views | anchored `agent_operations` of kinds `spawn_refused`, `attempt_adopted` for the producer's attempt, and `workspace_cleanup` per occupant; captured failure, diagnostic and spawn end stamp derive agent-local consequences |
| Feed cursors (`agent_feed_cursors`), node fold order | view metadata | cursor per runtime session and last applied intent order at the agent cursor; NULL node order restarts the fold |
| Node line, epoch number, runner placement, workspace root and cwd, parent composition | intent | the spawn: the caller's epoch, composition and runner; the root's runner is the one the tree's VCS port runs on (none for the local runner); written today as no line and no workspace override |
| Successor epochs, current epoch | intent | an epoch append: a profile switch's settlement of its pending epoch, or a roster apply (§1.2) |
| Tree provenance (`forked_from`) | intent | the fork or import that created the tree: a `/fork` genesis writes `node` (source tree, root agent, line, branch, source epoch); none for an import yet |
| Runner ancestors | external-effect record | each bundle apply that completes adds its tip to its target runner's set, at most 8, a full set giving up its first commit; a lost record only makes a later bundle carry more |
| Custody | ownership record, never tree content | the custody steps above |
| Interrupt cause | intent | an optional `cause` field (`operator`, `parent`, `cascade`, `owner_lost`, `handoff`, `stopped`) of the anchored `interrupt` intent, omitted when the writer records none: the operator's interrupt (the navigator's, and a child face's on wire v2) records `operator`, the reopen law `owner_lost`, and every other interrupt none; such a row keeps its earlier bytes |
| Root and child continuation decisions | intent | `agent_operations` of kind `continuation`, keyed `plak:continuation:<agent>:<attempt or root>:<cause>:<host generation>:<round>:<turn or command>:<anchor id>`, so root decisions in different turns and decisions after another host loss never collide; with cause `transient`, `owner_lost` or `handoff`, a one-based 32-bit round, the anchoring stopped turn or unfacted command, the decision stamp, and a decision: `new_turn` (its command id and the unfacted commands it replaces), `resubmit` (the one unfacted command) or `withdrawn` (at least one command); none today, where child transient decisions remain `child_finalization` `Resume` intents and root decisions are process-local |
| Terminal recovery refusals | intent | kind `terminal_refusal`, keyed `plak:terminal_refusal:<agent>:<attempt>`, with the refusal and its observation stamp; none today, where they remain process-local |
| VCS stages | external-effect record | kind `vcs_stage`, written before the stage runs: the stage (`mint`, `seal`, `adopt`, `unmint`, `refresh`, `base_snapshot`, `bundle_create`, `relay`, `bundle_apply`), its class (`reproducible`, `verifiable`, `ambiguous`), the node it serves, and exactly the inputs its kind takes (below), plus for exactly the verifiable class the expected `refs/` reference and commit; written for each VCS effect of a port on a runner (below), none for an in-process port |
| Pending epoch | intent | kind `epoch_pending`, with the epoch number it installs as (from 1), the workflow whose published settlement validates it, and the successor snapshot's exact JSON bytes and digest; the bytes must decode to a snapshot whose digest matches. Keyed `plak:epoch_pending:<workflow>`, at most one per tree, written by a profile switch after its product `Switch` intent and before its runtime publication (chapter 02 §5.5). Publish or validate: once the publication lands, the append from these bytes and the row's removal are one tree revision, taken in process right after the product projection and, after a crash, by the successor's open before its tools compile; a request withdrawn without its fact withdraws the row, and the line head's open withdraws any row left pending by a switch that never published onto it |

Each of the last five payloads is decoded only through its own codec, which
also requires the row's request digest to match its canonical request and a
keyed row to sit under its own identity. These payloads and `forked_from` are
frozen as complete: they refuse unknown fields, so a later field is a tree
schema change and moves the state key like any other.

A VCS stage's inputs by kind (R required, O optional, — absent). Every stage
names its node; commits are lowercase SHA-1 or SHA-256 hex, the bundle digest
is lowercase SHA-256 hex, runner ids carry no whitespace, and a stage naming
both runners names two different ones. Base snapshot, bundle create and relay
are reproducible only; bundle apply is never ambiguous.

| Stage | Candidate | Source runner | Target runner | Base | Tip | Bundle digest |
| --- | --- | --- | --- | --- | --- | --- |
| `mint` | O | — | O | O | — | — |
| `seal` | R | — | O | R | — | — |
| `adopt` | R | — | O | O | — | — |
| `unmint` | — | — | O | — | — | — |
| `refresh` | — | — | O | R | — | — |
| `base_snapshot` | — | R | — | R | — | — |
| `bundle_create` | O | R | R | O | R | — |
| `relay` | O | R | R | O | R | R |
| `bundle_apply` | O | R | R | O | R | R |

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
A later `recover_node` run exact-opens and owns the child session. If its
authoritative head is quiescent with no admitted Start or abandonment, the
child is interrupted with cause `owner_lost` and the opened attempt kept
(§14.1); nothing is reported to the parent. It does not leave a clean opening
requiring an explicit retry. The opened attempt's unread
opening envelope stays unread and durable: it reaches the child with its next
turn, ahead of any later followup's mail.
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
The driver publishes the intent, then folds due consequences inside
`fold_and_commit`; work that rebuilds the last views of a subtree also runs
the intent folds a reopen deferred over them. That boundary requires an acknowledged
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
settlement uses the same feed-to-view pass as an initial Start or a wake.
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

Root and child quiescent reopen and explicit recovery read retained wait-call evidence as
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
sessions they depend on at their tails), on every open and `recover_node` run, and in
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

The host reduces each child turn through one `TurnProjection`, which the
admission creates with the turn's admission-created event receiver before
acknowledgement delivery. The child's turn waiter, which the child's actor
runs, drives it beside the turn's result: one drive per turn takes the turn's
events and its result (chapter 02 §1). When the result comes the drive
reduces the events already delivered and closes the read model; a reducer
that reaches the turn's terminal first closes it then. Its live child handle
owns the read model, independent of attached faces. A follow-up admitted
mid-turn holds its receiver unreduced until the waiter takes it, so there is
never a second reducer; taking the follow-up and restarting a fresh turn
rebind the handle to the correct read model. A drive dropped before it
closed the read model (its waiter retired or rebound) hands the turn's
events back as a recovery with a gap marked, so the turn's next drive
reattaches; with no agent to recover them from, that drive closes the read
model. The read model dies with its
owning handle. This is process-local presentation, not persistence or reload
authority.

Observation delivery cannot reject an admitted child turn: if taking its
admission receiver fails, hosted recovers through a fresh atomic attachment
while retaining the acknowledgement, control and result. Recovered observation
closes immediately for a terminal turn snapshot. Throwaway cancellation/release
handles start no presentation task. On explicit source loss, the reducer marks
a gap; structural lag reattaches after a one-second backoff without replacing
the shared observation or its cursor. It never infers loss from the branch-wide
sequence of a turn-filtered receiver. A live source's observation failure does
not retire the child or its presentation capability.

`LiveChildHandle::host_observation` carries an opaque host-owned capability;
the tree hub never inspects its type. Hosted has one typed downcast helper,
and every face uses that helper rather than creating another reducer. Faces
read a snapshot and its cursor atomically, then follow already-reduced live
updates. At most eight child progress views are selected per host face. The complete
tree view carries optional `live_progress`: exact turn identity, current activity
verb (the same classification the root HUD uses), dispatched tool-call count,
last-observed activity time, and provider-reported live token counters. A progress
material change can publish a complete snapshot at the same durable tree revision,
with progress-only publications limited to about one per second. Timestamps alone
do not trigger a projection. Existing observers retain their slots; stable agent-id
order selects newcomers. Tool and usage counters come from the shared
projection, including for a late face after the earlier event prefix was
evicted. They are exact while the reducer has seen every turn event; after a
marked source gap (droppable loss or structural lag) they are approximate until
the terminal usage corrects them. No face replays a prefix or reads
presentation counters from L0.
This does not manufacture a tree mutation. Terminal observation or removal clears
the live progress. These counters and timestamps are observations, not billing,
terminal authority, or reconstructed history.

The open agent transcript (below) is the only child content view per face. Its
committed rows come from the live child's atomic transcript attachment; its
live updates come from the shared hosted projection, never a per-face raw-event
subscription or reducer. Updates at or before the history cursor cannot repeat
canonical items, and a turn ending during the history read refreshes the committed
baseline. The admission-created `TurnEvents` is owned by the reducer, not retained
for later viewing. A face attachment does not replay prior uncommitted
streaming text or reasoning: when prior activity is not covered, or the tail loses activity
or lags out, an addressed `earlier activity not shown` marker makes the gap
explicit. Closing drops this subscription; a lagged tail reattaches, at most once
a second, with an explicit gap. Live content uses the same keyed row reducer
and bounded scoped publication channel as the root. A refused patch marks that
scope for a full resync baseline, including its current previews.

The admission-owned `TurnProjection` retains a bounded keyed tool read model
(at most 512 calls, evicting settled calls before running calls), captured
atomically with the hosted attach cursor.
A late face shows an in-flight call as running, even when its exchange is not
yet committed in canonical history. Its result settles the same row immediately,
without waiting for the turn terminal. Committed settled calls supersede the
corresponding retained observations; tool observations not covered by the
committed baseline are still applied when their source cursor precedes the
history attachment. A canonical `interrupted` placeholder does not cover a live
call: its retained running observation overrides it immediately at the same row
key. Reattachment after lag repeats the retained running state, and a result
arriving during the history read still settles that key. A start and its result
occupy one retained call slot, not two. Earlier streaming text is not replayed, and the gap marker
continues to describe activity outside the retained presentation. These rows
remain process-local observations, never checkpoint or recovery authority.

Agent-scoped content occupies at most 128 of the observation mailbox's 512
slots, leaving room for root content and terminal observations. Consecutive
same-scope patches merge into one slot; raw assistant/reasoning appends coalesce
within their scope and output location. Saturating the agent share marks only
that scope's resync debt. Closing the agent view drops its publisher baseline
and live-row indexes as well as the subscription, even when no session or tree
hub is attached and the pane contains only a fail-closed notice.
Dropping the host drops all of its child observers. Unread presentation debt
never backpressures the child or its terminal result.

Child text, reasoning (under the root's visibility gate), tool start/completion,
usage and terminal observations feed the shared row projector; content is sent
as `TranscriptPatch { scope: Agent(id), .. }` at stream boundaries, with scoped
`TranscriptStream` appends between them. They never enter the root's
`apply_event` or mutate its transcript, usage, queue, or turn control. Progress
and usage remain tree observations rather than transcript authority.

**Agent transcript.** `OpenAgentTranscript { agent_id }` opens one agent's
transcript for the surface's main view; at most one is open per host face, and
opening another replaces it. Its identity is the agent id alone: tree revisions
never reset, re-send or refuse it. The host resolves the agent as a non-root node
of the attached session's tree, in any status. A node without a live turn is read
from its own bound session through `AgentSessions::transcript_source`, at the
current head of that session and branch in one load. A node with a live turn is
served with two cursors: register its hosted observation tail first, then
capture canonical history through `SessionTranscriptSource::attach`. Reduced
updates at or before the history cursor are skipped; later updates remain in
the already registered tail. Thus no intervening live update is lost and no
committed item is sent twice. What the tail cannot replay is marked with the
same gap notice. Items map to the
row types the root's live path writes: a non-harness user prompt is `User`,
with inline images represented by the same metadata-only image chips as the
root transcript; assistant text is `Assistant`, reasoning is `Thinking` under the root's
visibility gate, each call joined with its result by call id is one `Tool` row
whose subject, detail, state and wait facts come from the functions the root's
live path calls (a restored bare pi `wait` that ran the await takes its row
subject from the result observation's `subject`, the receipt's bounded agent
ids, not from the 2 KiB preview; a settled process wait's subject from the same shared function,
reading the host's job registry and the commands of earlier process starts in
the same history), a compaction is the `compact` tool row, and delivered mail is
one `tree` notice, whose text is `<purpose> · #<agent> · <body>`.
Root feed reads and child atomic-history reads use one hosted
`SessionTranscriptPart` reader, preserving result subjects and composition
lineage before their row projection.
The prose, image, mail and visible-reasoning rows also use the root's canonical
row-order mapper; omitted reasoning does not split a prose run or emit a row.
Canonical call and result rows use that mapper's shared tool constructor and
settlement. Child process/wait context and explicit composition owners remain
inputs, not facts guessed from another scope or from row order.
Canonical-history read positions and
fragment offsets are defined in chapter 01 §12; they are not bounded-window
indices and remain identical for retained items across reopen and window slides.
Child keys use that canonical position as `anchor`, the runtime lane, and the
derived-row ordinal within the fragment's reserved part-offset range. Each
offset reserves 16,384 row ordinals; omitted nested effects keep their slots
from durable acceptance order and call identity, independent of decode budgets.
This stability is within the retained composition-binding window (chapter 01
§12); aging out older binding evidence may rekey synthetic nested fragments.
Tail reads retain whole canonical position groups,
including after fragment injection and byte accounting, so sliding the window cannot restart that ordinal
inside an item. The zero key is reserved for truncation; neither row indices in
a bounded window nor process-local sequence numbers participate. A compaction marker's call
identity and a reasoning location also use the canonical position and fragment
offset rather than the tail index.
The derived notice retains the carried body's full text up to a UTF-8-safe
2 KiB bound, using the existing `… cut at N KiB of M KiB` marker when cut;
the TUI owns first-line summaries and expansion (chapter 02 §5.4).
Existing stored notices retain their earlier first-line-only text until the
projection is rebuilt; their persisted row format does not change. Local nested calls retain their canonical cell identity and
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
`interrupted` in the canonical baseline. A live call still in flight is shown
by the retained running observation described above and settles at its result.
One read projects at most the newest 512 items, newest first,
each string over at most the history redaction window (chapter 01), as
synchronous work on the blocking pool behind one permit per host face. Rows are
bounded like stored rows and the newest that fit the presentation bounds are
kept; one `Truncated` row leads them only when something older was dropped, and
rows that exactly fit are all kept. The attachment is a schema-3
`PresentationSnapshot` scoped to `Agent(id)`; live content and canonical
settlement use `TranscriptPatch`. Settlement reads the canonical history anew
and replaces provisional rows through one patch, not a full snapshot. When a
new turn starts, a new atomic attachment patches its canonical baseline and
follows its tail; it does not send a full snapshot to an already attached face.
`Sender` is the content boundary for host and product producers, including
background catalog-refresh hooks and opening recovery rows. Producers do not
receive its raw observation mailbox. Keyed snapshot
and patch variants exist only in the received vocabulary; the host's generic
sender cannot accept them. Explicit presentation publication owns revisions
and the conversion to those received frames.
Reopening the open agent on the same live turn is a no-op. A failed later
read keeps the last good rows beside a notice rather than empty rows; only
a send that passes the generation check below replaces the last good rows. An
unknown agent, the root, a node without a bound session, a host without a hub or
session store, and an unreadable first read answer at once with a notice row
and open nothing. Every scoped snapshot or patch is checked, under one lock,
against the face's view generation it
was started under; close, replace, a new turn and a session switch bump that
generation under the same lock, so nothing of an earlier serve arrives after
them. `/new`, `/resume`, fork and a profile rebind close the view before the
switch emits its `ClearView` or new session snapshot. This view does not count
against the eight progress observers. A history row reads both names from the
committed tool call (chapter 01 §8.2): it is labelled by the exposed name the
model called, so pi's `edit` and `write` over one patch tool stay two distinct
rows, and its activity kind and subject derive from the canonical name, exactly
as a live row's do.

Production consumers access tree state through `TreeDriver` and hub closures.
Each tree has one writer at a time, in arrival order. A mutation is an owned
(`Send + 'static`) closure that the driver runs on the blocking pool, with the
live tree as its guard; reads and `try_read` take the live tree directly and
never queue. A capacity-refused save inside a writer parks in the caller's
async task, not on the worker: an async sleep at the tree's retry interval
raced against the writing agent's cancellation, the tree's shutdown and the
shutdown of the host that attached the tree last. These are latched signals,
selected on directly; each writer takes the host signal current when it asks
for its turn. A queued writer gives up only while the tree cannot publish and
its agent or the tree or host is stopping. Dropping a mutation's future stops
its park, but the closure itself runs to its end. An abandonable mutation's
result that its dropped caller never reads is undone exactly once: the drop
first closes the result's channel, then undoes a result already sent as the
tree's next writer, and a writer whose result the closed channel refuses
undoes it in place. A caller whose writer has run reads its result at once,
without awaiting it, so no budget-deferred read can leave it unread. The
dropped caller's undo runs on a thread of its own, so it needs no runtime and
a runtime shutting down does not skip it; when no thread can be started it is
logged as an error and the claim holds until the tree is next opened. A wait
dropped while its admission runs therefore never leaves its parked claim
holding a send to the waiter's recipient.

Owed work never gives up its turn: releasing a process-local claim (mail
consumers, turn waiter registrations) and admitting and completing effects
wait for the tree whatever was cancelled, so a completed effect is always
recorded and only its publication may be refused. A claim is released by an
explicit writer; one dropped instead is released by an owed writer run as a
release owner. Where no runtime will run one, a turn waiter registration
leaves on a thread of its own, and a mail claim is logged as an error and
holds until the tree is next opened.
Effects never depend on their caller staying: a mutation whose caller is
gone before its effects reach it hands them to a release owner, and a settle
or a hub mutation that publishes and settles runs as a release owner the
caller awaits. A caller that awaits other work between taking effects and
settling them holds them so that an early return or an abort hands them to a
release owner; dormant-copy reclamation instead runs its job stop and the
dispatch it gates as one release owner, since its refused job stop abandons
the dispatch to recovery.
The VCS port (`VcsOps`) is async. Every repository operation and every
question about a copy's or checkout's files is an awaited port call made
outside the tree mutex and outside every writer. A question an admission needs
is asked before its writer: a spawn lists the JJ copy root, so the id of a
copy directory a crashed mint left without a row is skipped; an adopt asks
whether the adopter's checkout is JJ (for a conflict, resolution or revert)
and which running JJ copies were minted over the candidate. A question the port
cannot answer refuses the operation where its answer is needed, and an answer
about rows the writer no longer finds refuses with `target_changed`; neither is
read as "absent". A settle runs its admitted batch as a task of its own that
its release owner awaits: once dispatched, the batch makes its port calls one
at a time, in a fixed order, to its end even if that owner is aborted. A spawn
runs its mint the same way, inside one release owner that also runs the writer
committing or rolling back its reservation, so a spawn whose caller is dropped
mid-mint still records its child or releases its reservation. A failed-worktree
rescue seal is likewise a task of its own. These tasks are plain Tokio tasks,
outside the tree's task sets and the `Runtime` interface: they outlive an
aborted owner but not the Tokio runtime. When that runtime shuts down, a task
is dropped between two port calls; a port call already running on a blocking
worker is not interrupted, but nothing after it is recorded, so the next
open's recovery settles that batch as it settles a crash.
On a runner (chapter 01 §8.5) every VCS effect is a stage. Before a settle
dispatches a batch, or a spawn its mint, the tree records one `vcs_stage`
intent per effect, with the runner as its target, and publishes it; the
port then sends the effect's steps as tree operations keyed by that intent's
id, so the runner runs each step at most once and a host that lost the link
replays it by key. The revision that records an effect's answer removes its
stage. Seals, mints, removals, reclamations, base pins and refresh
applications are reproducible; adopts and `on_mint` hooks are ambiguous; no
single-runner stage is verifiable. A stage whose step lost its outcome (the
link was lost past its window, or the runner restarted under it), and a stage
a crashed process left recorded, settle by their class when the tree reopens
(the reopen law, §14.1):
a reproducible stage stays recorded while its node is open (it is removed once
the node is closed or gone) and its effect's own failure stands; its workflow
runs the effect again when that effect is next dispatched (the reopen and
`recover_node` do not rerun it by themselves), and a stage recorded with exactly the inputs of an
unfinished one takes that one's id again, so each step reaches the runner
under the key it was first sent with. The runner answers a step the lost run
still runs once that step ends, and a step it finished with its stored
answer, so a rerun never runs beside the step it repeats; a step whose
answer was already taken ran to its end and runs again under a key of its
own. An ambiguous stage records a `vcs_fault` for its node, as an in-flight
effect at reopen does; one of the root records it for the root, naming its
checkout, since nothing recovers the root itself. A verifiable stage stays
recorded like a reproducible one, and its rerun checks its expected outcome
first. That reading is one function, which node recovery replaces. Effects run outside a settle or a
spawn (attachment refreshes and base pins, a candidate's unpin, a failed
worktree's rescue seal) each declare an unstaged scope and go under a key of
their own: each is re-derived by its next attach or settle. A runner port
treats an effect in neither a stage nor an unstaged scope as a defect.
A copy on another runner than its parent's crosses as git bundles, each step
a stage recorded through the stage ledger the driver runs the spawn's mint
and each settle's batch under (`cross_runner::with_ledger`). The mint runs a
base snapshot on the parent's runner (the parent's HEAD and its non-secret
uncommitted work, pinned under `refs/plak/<ns16>/transfers/<agent>`), a bundle
create of that ref leaving out each of `tree_runner_ancestors` the target
still holds, the host's relay of the bundle in chunks of at most 1 MiB into
an upload named by its SHA-256 (the host stores none of it), and a bundle
apply that checks the digest and fetches the ref into the tree's mirror on
the target, `<copies root>/<runner id>/<tree id>` (keyed by runner, so runners
sharing a copies root keep apart mirrors); each is reproducible. The mint itself,
the spawn's own stage, then mints the copy from the mirror over the
snapshot, so the copy lives under the mirror's `.plak/worktrees`. The tree's
port for several runners (`RoutedVcs`) answers every later call about a path
under a runner's mirror from that runner's port, with the mirror as its
launch checkout (two runners whose mirrors nest are both left out); a copy there is not refreshed, and its ahead/behind against
a parent on another runner is unknown. Its seal runs on its runner by the
usual rules and is carried back the same way: bundle create of its candidate
reference less the copy's base, relay, and a verifiable bundle apply into the
parent's checkout that fetches the objects and creates the candidate
reference and its base pin, only where neither is, whose intent records that
reference and the sealed commit. A seal-back first reads the parent's
reference: naming the expected commit, it carries nothing; naming another
commit, it moves nothing and records a `vcs_fault` for the node; absent, it
carries. An apply whose answer was lost is checked the same way. An apply
removes its upload whatever its outcome, and the reclamation pass removes
transfer files older than a day from the launch checkout and every mirror. A
verifiable stage left unfinished stays recorded while its node is open, the
next one for the same node, candidate and outcome replaces it, and the
revision that records the seal settles it. Adoption runs on the parent's
runner as before. A copy crosses only from a git checkout. A crash at any
stage boundary leaves a reproducible or verifiable record and no fault: the
reopened tree reruns the spawn or the seal, and both converge
(`cross_runner_fork_stages`).
A mutation retains the store-acknowledged revision from its explicit publish;
settling an empty effect batch does not itself publish a mutation. Every queued
effect is drained after the tree mutex is released, including when the initial
publish fails. Chained cleanup is recorded before the original failure returns.
Raw tree handles are available only to test fixtures.

Follow-up delivery snapshots an owned operation under the live-map lock and
awaits it without that guard. The owned delivery runs to completion independently
of the delivering send, so admission and result registration finish even if
that send is cancelled. A per-child barrier reserves the exact live
session until admission and follow-up result registration finish. Deliveries
for that child serialize; terminal handoff, restart, and release drain them
before rebinding or releasing the session. Terminal handoff and restart drain
delivery without the map lock, then recheck turn identity and admissibility
under the map before consuming a queued result or starting a new turn. Steer,
list, and close admission remain available during slow follow-up delivery;
close retains its retiring-slot reservation until release completes, so a
concurrent reopen cannot replace the targeted child mid-delivery.

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
After publication and slot removal the host starts a `recover_node` run over the
tree only if a fault was recorded or session recovery remains required. Clean,
fast retirement starts none; a tree-read failure starts one rather than
skipping it. Joiner deadline expiry is a typed refusal, separate
from owner-confirmed failures through the explicit bounded cancel and batch
cancel interfaces. It does not authorize another fault publication: the release
owner records its deadline evidence. Tool and navigator requesting paths use
the background join instead of waiting for that deadline.
Caller publications of owner-confirmed failures, including child-start,
reopen reconciliation and attempt-end work, are scoped to the owning tree and
the attempt captured before awaiting, and skip an already settled or replaced
attempt. Dropping a startup caller requests only its exact reservation or the
turn that reservation transferred, never a replacement live turn. Navigator
and root-interrupt joiners do not republish owner failures. Caller-layer faults,
including job-stop failure, request recovery after publication. A late joiner
cannot re-fence, re-push or re-seal an already settled attempt.
Process-local failure reports carry the owning tree, attempt and exact retired
turn (or no turn for an unadmitted startup). A caller refuses a report from
another tree, attempt or bound turn before recording any host decision; identical
child ids in different trees are not interchangeable. Operation receipt
completion retains the original operation id and request, never a current call's;
caller fault publication is limited to that operation's captured target set.
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

A capacity-refused tree publication (`SQLITE_FULL`, or an explicitly typed
capacity cause from the store port) retains the exact prepared tree and its
pending effects under the current owner. The driver retries only its fenced
save at five-second intervals until an authoritative write succeeds or the
operator cancels/shuts down, with one retained mutation per owner and no
outage deadline. The live tree mutex is not held while parked, and mutation
contenders yield executor workers. It does not rerun mint, seal, adopt, model calls or tools.
Publications are cancelled by their agent identity, never by an unrelated
child cancellation or root interrupt. Ordinary guarded access holds the live
tree without cloning it; only a capacity-refused save detaches the retained
candidate. Public callback mutations which may publish detach before entering
the callback. All mutations, including scripted children, use this driver
ownership boundary rather than writing a raw shared tree handle. Child
wake and retirement publication wait at this boundary rather than recording
another fault through the same outage, entering retirement, or marking divergence.
The acknowledged tree revision advances only on a successful save. A
foreign revision, corruption or ambiguous backend error remains fail-closed;
cancellation/shutdown while still full returns `recovery_storage_unavailable`,
never the preventive definite `storage_low` refusal.
The tree's live capacity observation blocks new paid starts on colocated
runtime branches until an authoritative save succeeds. This observation is
not persisted and cannot replace the tree revision CAS or a child lease.
Admission retries a flagged store's pending or minimal authoritative write.
Acquiring a runtime lease to wake a parked child uses the same indefinite
capacity wait (chapter 01 §5); it does not end the old attempt or replay its
previous turn merely because the first acquire write was refused.

Process-exit abandonment is separate from joined retirement: the hub closes
admission, aborts and joins its `recover_node` runs before collecting root/child
runtime capabilities, cancels startups and aborts retirements. Live and retiring
child leases are included in the root's batched release, without waiting for
provider settlement or tree publication. A one-second product exit bound is
mandatory; ordinary retirement owners retain the joined cleanup contracts below.

Explicit bounded retirement joiners have a 65-second ceiling (the five-second cancellation
settle bound plus `HOST_SESSION_RELEASE_TIMEOUT`, 60 s). The worker continues awaiting
release after this ceiling, records slow-retirement recovery evidence, and logs
the returned cleanup result and total duration. Driver telemetry separately
times tool drain, job-terminal publication, heartbeat join, lease release and
backend flush as cleanup kinds in `durability_attempts`, with joined/failed
outcomes; these observations are not recovery authority.
The background operation-receipt owner waits for actual release completion,
not the joiner ceiling; expiry does not settle its prepared receipt prematurely.

Shutdown closes admission (no `recover_node` run starts after that), then gives
running runs one two-second allowance; at expiry it aborts them and joins each
for at most 250 ms, so no reopen of a child session outlives the leases
released next. It
cancels every child still opening, so none becomes resident, and then retires
the children by depth, deepest first, within one two-second allowance for the
whole join: a level's active releases start, together with any retirement of
that level already under way, and the next level up starts only once each of
them has released, its turn waiter has folded the released turn into the tree,
and each of that level's cancelled startups has settled. Siblings at one depth
retire concurrently. So no node retires while a descendant of it still
releases or opens, and a tree's shutdown commits in one order run to run.
Until its level starts, a shallower node keeps running: its turn is neither
cancelled nor released, so it can finish a provider round or run a tool in
that window (admission and live mail are already closed), and only within the
one allowance. A deaf leaf can spend the allowance; the
levels above it then start at once without a wait and are reported cut short
like any release the bound cut short (for a planned handoff, a warning, and the
next owner continues each of them once). Tracked tool-side retirement tasks are
joined within the same allowance. Those
tasks own job stops, effect settlement and operation-receipt completion even
after the live slot clears. Headless run exit joins both kinds of owner under
the same allowance, including when no live child remains. At expiry it returns
a bounded failure and retains
fault evidence, while release workers retain ownership until they return and
may publish their released locator afterward. It then lets the turn waiters
of released children, which fold each released turn's terminal into the tree,
finish within one more two-second allowance (only when it released children),
so a shutdown's last tree saves never depend on whether a waiter beat the end
of custody; and a release page that would only move a released child's feed
cursor (its waiter already folded the rest) is not saved, the next open reading
it again. No new
`recover_node` run starts on the closed hub. Explicit process-exit abandonment, not ordinary joined shutdown,
aborts these workers.
Every task the tree runs for its children belongs to the tree, never to a
root, so no root's release touches it. Work that owns a child's session or
runtime authority until it returns (startup and resume owners and their
cleanups, each live child's actor, release workers and the retirement tasks
they own) is tracked as the tree's release owners, which no
shutdown aborts. Shutdown bounds its join only on retirement completions and
on cancelled startups, as above, and leaves the other owners running
detached. Process-exit abandonment aborts release workers; the other owners
end with the exiting process's executor. The rest (projections, join
observers, `recover_node` runs) is the tree's task set: after the joins above,
the tree's shutdown closes it, aborting what still runs and joining it for
at most 250 ms.
A live child's actor is the one owner of its attached session. The hub's
follow-up, restart, release and cancel-and-release are messages it answers in
arrival order, and a reply nobody awaits any more stops that work. An
operator decision its turn waits on is a message too: the actor is that
decision's single decider (spec 02 §4.3.1). The actor
drives the child's turn waiter beside its mailbox, so a waiter that restarts
or retires its own child is answered by the actor it runs in. The actor ends
once its waiter has ended and its session is gone: released, or dropped
with its last handle when the hub drops that handle without a release.
Each child node has one residency in its tree's live table: opening (a
startup owns the slot), resident (a live turn, the only state that takes
new work) or releasing (its release is in flight, and the slot stays taken
until the release returns). A node with no entry is dormant; the tree is
dormant when no child node has an entry and no root runtime is resident.
Leaving opening classifies what arrived meanwhile in one order: a close or
interrupt cancels the startup before it can become resident; mail is taken
by the turn it starts, or answered `SlotBusy` and retried once the slot is
vacant; a wake starts a turn only from a park, after the turn ends. No
trigger is queued in process memory: each one is tree mail or a wake, or
joins the startup or retirement it found.
Admission checks active and retiring slots under the same mutex as the closed
flag. Batch cancellation starts and joins siblings concurrently; its report retains
input order, while independent retirement workers may publish in completion order.
Shutdown returns typed child/tree failures as well as retaining tree fault evidence.
The owning attachment always attempts canonical parent release afterward and keeps
a successful parent locator available for final publication even when a child failed.

Tool interrupt/close receipts remain prepared until cancellation, release, and
locator publication have joined successfully in the independently owned
background task. The caller returns after the intent is saved and exact owners
are signalled; it does not wait for the 65-second release deadline. The release
owner emits one recovery-required detach and one transcript notice for its fault,
regardless of whether a tool, navigator action or root interrupt initiated it;
concurrent joiners and a late failure after deadline evidence do not repeat them.
Navigator close detaches for live, starting or retiring children are deferred
until their release owner establishes the outcome, so an eager closed observation
cannot hide a retirement failure. The startup owner emits the closed detach after
successful release publication, using the same detach dedupe as the tool path.
A bound child without a live owner instead emits its terminal detach at
cancellation admission, including when live siblings
are still retiring. Faces subscribe to owner notices when they register the
runtime, before the first snapshot; overflow logs its lost-observation count.
Background tool errors log only captured agent/attempt ids and the typed error
kind, never error content. Replaying
a prepared cancellation
returns `operation_recovery_required` without another dispatch; failed retirement
records a failed receipt. Completed receipts replay without touching any later
turn. If the requesting future is cancelled after dispatch, its independently
owned retirement still settles its operation receipt. A process crash instead
leaves an unfinished receipt fail-closed and the existing close/interrupt intent,
child feed and retirement recovery reconcile the child; no release is replayed
against another turn. No domain-intent response is presented
as proof that cancellation completed.

## 17. Known limits

These are shipped gaps, stated so nothing above reads as a promise it does not
keep.

Conflict candidates, resolver marker checks and explicit marker acceptance are
Git-only. JJ retains its plain conflict refusal; no non-textual conflict
candidate is manufactured on either backend.

1. **No depth-remaining term on the template card**, and no observed duration.
2. **A snapshot copy minted over a candidate can answer `git status` with "no
   usable git dir".** Cosmetic; the seal and adopt paths are unaffected.
3. **Schema fail-closed message.** A DDL fingerprint drift is refused, but the
   message reads as an ordinary version mismatch.
4. **Storage cost is real.** Each `workspace=fork` `mutate` copy is a true
   worktree; a non-mutate fork is a detached snapshot. Inherit mints nothing.
   Workspace crates fingerprint on absolute path and rebuild per
   worktree, and cargo's exclusive target lock serializes a reviewer behind a
   worker build, so a cold build per copy is the expected cost — roughly
   1–3 GB per worker-and-reviewer pair on this repository.
