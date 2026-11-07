# Configuration reference

Every key `plak` reads from disk, its type, its default, its constraint, and
what it does. Code is the authority: the tables below are transcribed from
`CONFIG_KEYS` and `CONFIG_SECTION_DOCS` in
`bin/plak/src/product_config/keys.rs`, and `TEMPLATE_FIELD_DOCS` in
`bin/plak/src/product_config/templates.rs`, the same tables that render
`docs/reference-config/**` and the commentary in a saved `config.toml`. A key that
is not in that table does not exist.

Operator-facing instructions live in
[`../user-guide/configuration.md`](../user-guide/configuration.md).

## 1. Files and discovery

Build capabilities are not configuration keys. The persisted account,
composition and tool-selection vocabulary is unchanged in a feature-reduced
binary, so read-only session operations still decode it. Session admission
refuses only the selected Python-bound template/default/session when `python`
is omitted, or the selected template's MCP tools when `mcp` is omitted
(server definitions alone enable nothing), naming the unavailable feature
(exit 2). Unused templates never block a session. It never silently drops tools or
substitutes a composition. Doctor scans all templates for its config verdict.

A Plak home holds one `config.toml` plus one file per agent template:

```
$PLAK_HOME/            # default ~/.plak
  config.toml          # preferences; never secrets
  agents/<name>.toml   # one template per file
  secrets/             # credentials; never in config.toml
    named/<name>.secret  # `plak secret set`; config names these, never holds them
    mcp-oauth/<id>/      # `plak mcp login`; the server's OAuth login (spec 04 §3.1)
```

`PLAK_HOME` MUST be absolute when set; otherwise the home is `$HOME/.plak`
(`PlakPaths::resolve`). `plak config init` writes a commented reference template that pins no keys when
`config.toml` is absent and reports the path either way; it never overwrites.

Product config publications, including project-scoped settings and MCP edits,
serialize on the home's `config.lock` sidecar. The lock is held through the
atomic rename and parent-directory sync and is released by process exit.
Terminal preference writers re-read the home baseline under that lock and
apply only `[tui]`; host settings merge only the product fields changed from
their original view into the latest effective config. Neither writer republishes
the other's stale values. Raw external editor writes do not participate in
this advisory lock.

**Project roots.** Launching below a directory that carries `.plak/config.toml`
or `.plak/agents/` resolves that directory as the *project root*: the nearest
ancestor wins, `.plak` must be a real directory (a symlink or plain file is
ignored), an empty `.plak` is ignored, and `$PLAK_HOME` itself is never a
project. Launch discovery likewise skips the default home `$HOME/.plak` when
`PLAK_HOME` points elsewhere: it is a product home, not the project of
everything below `$HOME`. Only `config.toml` and `agents/` participate; sessions, secrets,
databases, logs, telemetry and cache always live under `$PLAK_HOME`.
Imports and attached maintainer actions use the same ancestor discovery from
the recorded workspace, not the invoking shell's cwd or the literal workspace
directory as a project root.

**Trust gate.** A discovered project applies nothing until it is trusted.
`plak config trust` records the canonical project root in
`$PLAK_HOME/trusted-projects.toml` — a non-secret global file published
atomically — and `plak config untrust` removes it. A missing or malformed
registry trusts nothing. An untrusted project's configuration is skipped
entirely: the load still succeeds on the global files and reports the blocked
source as a warning.

**Merge semantics.** The global file loads and validates alone first: a
global `root.agent` or `working_set` name must name a **global** template — a
project file can never satisfy a reference the global home made. A trusted
`project/.plak/config.toml` then overrides only the keys it carries, except
`[orchestration]`, `turn.auto_continue`, `tui.reload_requests` and
`mcp.client_id_metadata_url`: those controls are home-scoped. Project orchestration
values are ignored with a warning, and their displayed origins remain user
or default. The entire file still must satisfy TOML syntax and schema validation;
invalid values are load errors even in a home-scoped section.
A project-scope `config set orchestration.*` fails with a hint
to use `--scope user`. A
trusted `project/.plak/agents/<name>.toml` replaces the whole global template
of that name or adds a project-only one. References the *project* config
makes resolve against the merged set, so a project `root.agent` may name a
global template or a project one — but a name the merged set does not have is
the same load error as always.

**Template discovery order.** Every `*.toml` under `$PLAK_HOME/agents` is read
first, then every `*.toml` under `<project>/.plak/agents` when a trusted
project root is resolved. A project file of the same stem **wins**: it
replaces the user's template for that name. Files whose stem begins with `.`
and files without a `.toml` extension are skipped. **The file stem is the
template's name**; a stem that is not `[a-z][a-z0-9-]{0,31}`, or that collides
with a reserved name, is a load error naming the file.

A template is authored in its file. `plak config set` never writes a template
body; `agent.<name>.<field>` is refused with *templates are file-authored;
edit ~/.plak/agents/<name>.toml*. The one exception is `plak mcp enable` and
`plak mcp disable` (and `plak mcp remove --force`, which disables first): they
rewrite only the `tools` array of the one template file the name resolves to
(the project's when a trusted project supplies it), adding or removing
`mcp:<id>` or `mcp:<id>/<tool>` entries, keeping every other line and the
comments beside the remaining entries, and validating the result as both
layers would load it (§3.5) before publishing it. A session that
already exists keeps the tools it froze; new sessions pick the change up. The Agents tab selects a whole profile.
A user profile is `~/.plak/profiles/<name>.toml` with exactly two keys,
`root = "<template>"` and `working_set = ["<template>", ...]` (at least one
name, every entry a string); any other key, a wrong type, a missing template,
or a stem that is a built-in's name (`codex-default`, `claude-default`,
`grok-default`, `multi-default`) leaves that file out of the list with a line saying why.
Nothing in a session changes the live root's binding except a profile's Fork
now or Use now; no slash command rebinds it. A profile is per session: `[root]
agent` and `[roster] working_set` are only the default profile brand-new
sessions bind, written by the Agents tab's Set as default (or by `plak config
set` and the file); Fork now never writes them, and Use now, offered only
while a session has no model, also records its profile as the default. Changing the
default never changes an existing session, which keeps the root template and
working set its tree froze, re-resolved against the template files at its next
`/reload` (chapter 02 §5.5).

## 2. Grammar

`config.toml` and `agents/*.toml` are TOML. The `toml` crate parses the
document and `serde` deserializes it into the closed schema of §3 and §4; the
whole of TOML's own vocabulary is available, including basic and literal
strings, multi-line strings and escapes, multi-line arrays, dotted keys and
comments anywhere a comment is legal.

The schema is what narrows it. Each section is a table of the keys §3 lists and
no others, each key takes exactly the TOML type its row names, and a template
is one flat object plus an optional `[env]` table. Booleans are TOML booleans:
`true` and `false`, never `1`, `yes`, `0` or `no`.

Four things fail the load, all of them named:

- **malformed syntax** and **a duplicate key or table**, reported by the
  document parser with the line and column it stopped at;
- **an unknown section or key**, reported with the migration hint §5 lists when
  the name was once something else;
- **a wrong TOML type**, reported with the key and the line and column of the
  value;
- **an invalid semantic choice** — a value outside a key's range or outside its
  closed value set — reported by the same rule set `plak config set` runs, with
  the line and column of the value.

Nothing is skipped silently and there is no second, more forgiving reader:
`plak` refuses to start rather than run on a file it did not fully understand.
A load error names the file it came from.

Session policy and live topology admission distinguish admitted, disabled, and
unreadable configuration. Unreadable configuration refuses creation or a fresh
recompile with its named error; it never becomes a topology-disabled session.
Import checks the document's workspace configuration before creating a successor.

The bare account spelling `xai` is refused as ambiguous: use `grok` for
SuperGrok OAuth or `xai-api` for an API key. Other unambiguous aliases remain.

The hosted capability registry owns route identity aliases and catalog-policy
metadata; Plak's registry adds product presentation, doctor probes and bundled
profiles. Those registries add no configuration keys and do not change account,
model, composition or protocol admission. The template-account membership and
no-native-protocol vocabularies stay complete and ungated, as do serialized
session binding enums. Recommendation aliases are presentation policy, never
authority to admit an otherwise unknown or ambiguous account.

Deleting a key restores its default. Deleting `config.toml` restores every
default. `plak config show` prints the resolved file with its path; secrets are
never in it.

## 3. `config.toml`

A session-binding problem never edits configuration. An invalid stored
account/model selection is refused with the remedy
`plak config set root.agent <template>`; the configured pointer and file bytes
remain unchanged. Only explicit configuration actions (including Settings and
profile application) publish configuration changes; an incomplete bind cannot
silently clear `[root] agent`.

Thirteen key sections, in render order: `[root] [roster] [orchestration] [turn]
[jobs] [api] [tui] [compaction] [skills] [telemetry] [mcp] [host] [web]`, plus
three families of named tables, `[tools.<name>]` (§3.4), `[mcp_servers.<id>]`
(§3.5) and `[runners.<name>]` (§3.6), sixteen in all. Any other section is an error.
The families are not `plak config set` keys: they are authored in the file, a
save leaves them exactly as written, and `plak config show` prints the
canonical rendering of the tool and server tables after the keys.

`[host] resume_after_loss` decides whether the root and the sub-agents a lost
host interrupted hold for the operator's Resume or continue once by themselves
(chapter 03 §14.1); `[mcp_servers.<id>] placement` places a server (§3.5);
`[runners.<name>]` is read by `plak runner pair`, `plak runner ps` and `plak
doctor` (§3.6) and by nothing else yet; `[web] allow` and `[web] origin` are
read by `plak host` when it serves its web endpoint (`--web`, chapter 02 §4.5,
chapter 04 §2.10) and by nothing else. A
0.0.3 build refuses a `config.toml` that sets any of them (unknown section or
key), so `plak` writes them only when the operator sets them: `plak config
init` and a fresh save write the `[host]` and `[web]` headers commented out, a
save of another key never adds one, and a save that clears the last key of
`[host]` or `[web]` removes the empty table. `plak doctor` names each one the
home file or a trusted project file carries, and a `[host]`, `[runners]` or
`[web]` table present even empty (chapter 02 §7.3). `[host]
resume_after_loss`, `[runners.<name>]` and `[web]` are home-scoped: a project
file that sets them is loaded with a warning and its values are ignored, so a
repository can never grant web access.

| key | type | default | constraint | meaning |
| --- | --- | --- | --- | --- |
| `root.composition` | enum | `(unset)` | direct\|plakous-default\|native-ptc\|js-ptc\|py-ptc\|py-sdk | Root-only composition override for the exact root account/model tuple, validated against its account/model. Changing root.agent or applying a profile clears it. Children keep their own template composition. py-sdk uses the typed Python SDK and is not valid in child templates. |
| `root.agent` | string | `(unset)` | names an agents/<name>.toml file; [a-z][a-z0-9-]{0,31}, and never a section name | Template a new session binds at genesis; unset leaves the tuple to the CLI flags or the shell binding. |
| `roster.working_set` | array of string | `[]` | 0..=5 names, no duplicates, each a template file with a description | Templates the root may spawn this epoch; every other node spawns from its own template's spawns list. |
| `orchestration.subagents` | bool | `true` | true or false | Whether the topology tools are compiled into the root catalog at all. |
| `orchestration.max_depth` | integer | `1` | 1..=4 | Deepest node the tree admits, counting the root as depth 0. |
| `orchestration.max_children` | integer | `8` | 1..=64 | Active or undelivered direct children; dormant children release slots. |
| `orchestration.max_nodes` | integer | `32` | 1..=65 | Root plus active, undelivered or recovery-required nodes. |
| `orchestration.workspace_idle_seconds` | integer | `600` | 1..=65535 | Idle seconds before lossless dormant-checkout reclamation. |
| `orchestration.workspace_budget_mib` | integer | `4096` | 1..=65535 | Soft fork-checkout disk budget in MiB; retains uncovered work. |
| `orchestration.child_retention_days` | integer | `7` | 1..=65535 | Days before a sealed dormant child conversation is retired. |
| `orchestration.child_retention_count` | integer | `128` | 1..=65535 | Dormant conversations retained; protected work refs never expire. |
| `orchestration.max_attempts` | integer | `64` | 1..=256 | Attempts one root prompt may open across the tree. |
| `orchestration.transient_resumes` | integer | `1` | 0..=2 | New continuation turns after transient failure per root user prompt or child attempt; 0 disables automatic continuation. |
| `orchestration.max_concurrent_requests_per_account` | integer | `8` | 1..=64 | Model requests one account may have in flight across the root and every descendant; excess requests wait in order for a slot. Takes effect on the next launch. |
| `orchestration.require_review_evidence` | bool | `false` | true or false | When true, adopt_candidate refuses unless its evidence names a subtree attempt that referenced the candidate. |
| `turn.auto_continue` | bool | `true` | true or false | Wake an idle interactive root when background processes finish or child results or questions arrive. Takes effect immediately; JSONL and children do not auto-continue. |
| `turn.max_model_rounds` | integer | `512` | 1..=1024 | Model rounds one root turn may run; the next round must answer without tools. A turn has no wall clock, so this is what stops a runaway turn. Takes effect when a session next attaches; a running turn keeps the cap it was admitted under. |
| `jobs.foreground_seconds` | integer | `30` | 1..=600 | Default process foreground budget before promotion. Bound at session construction and rebind; exact reopen keeps the stored value. |
| `jobs.max_runtime_hours` | integer | `10` | 1..=24 | Hard runtime cap per background process. Bound at session construction and rebind; exact reopen keeps the stored value. |
| `api.full_window` | bool | `false` | true or false | Admit direct API sessions (openai, anthropic, xai-api) under the model's full input window instead of the provider's price threshold. Bound at session construction and rebind; exact reopen keeps the stored value. |
| `tui.mouse` | bool | `true` | — | Mouse capture; turning it off hands text selection back to the terminal. Takes effect on the next launch. |
| `tui.theme` | enum | `"default-dark"` | default-dark\|default-light\|catppuccin-latte\|catppuccin-frappe\|catppuccin-macchiato\|catppuccin-mocha; dark and light are accepted aliases that normalize on the next save | Color palette for the whole product shell. |
| `tui.scroll_lines` | integer | `3` | 1..=10 | Transcript lines moved per wheel tick. |
| `tui.show_thinking` | bool | `true` | subject to the provider's reasoning policy | Render reasoning blocks where the provider admits them. |
| `tui.group_tool_calls` | bool | `true` | — | Collapse consecutive tool activities to a summary when assistant prose resumes; false keeps action lines visible. |
| `tui.reduced_motion` | bool | `false` | — | Suppress the spinner, the pulse and camera easing. |
| `tui.reload_requests` | bool | `false` | true or false; home-scoped | Let this user's own processes, including an agent's shell, ask a running interactive session to reload onto the installed build (plak session reload <session> --expect-binary <id>) at its next idle boundary. Takes effect when a session next attaches. |
| `compaction.enabled` | bool | `true` | — | Automatic transcript compaction when a session approaches its context bound. |
| `skills.enabled` | bool | `true` | true or false | Discover Agent Skills (<workspace>/.agents/skills, then ~/.agents/skills) when a session is constructed, list them in its instructions, let its read tool read files under ~/.agents/skills, and give Claude-vocabulary sessions the Skill tool. Bound when a new session or spawn is constructed; exact reopen keeps the listing and tools it froze, and a fork, rebind or switch keeps the source session's on or off. |
| `telemetry.max_bytes` | integer | `536870912` | greater than 0; zero is refused as ambiguous. 536870912 is 512 MiB | Best-effort byte retention target for the main database, WAL and SHM; busy readers or unfinished sessions may keep it above target. |
| `telemetry.max_age_days` | integer | `90` | greater than 0; zero is refused as ambiguous | Retention cutoff for local analytics rows, in days. |
| `mcp.client_id_metadata_url` | string | `(unset)` | `https://` URL with a path, no query or fragment; home-scoped | The OAuth Client ID Metadata Document that identifies Plak to an MCP authorization server that supports one (spec 01 §8.5). Unset disables that registration path; the document lists the redirect URI `http://127.0.0.1/callback`, which a server matches on any port. Takes effect at the next `plak mcp login`. |
| `host.resume_after_loss` | enum | `(unset)` | hold\|continue; unset holds; home-scoped | Whether the root and the sub-agents whose host was lost (a crash, a lease loss) hold for the operator's Resume or continue once by themselves with a new instruction turn, the root's restored queue draining too (chapter 03 §14.1). Read when the root attaches and opens, and by plak host's start scan, which under continue opens a session whose root's loss is held; a released 0.0.3 build (or older) refuses a config.toml that sets it. |
| `web.allow` | array of string | `(unset)` | tailnet logins, no duplicates; unset admits no one; home-scoped | The tailnet logins (Tailscale-User-Login) plak host --web admits through tailscale serve. Read when plak host starts with --web; a 0.0.3 build refuses a config.toml that sets it. |
| `web.origin` | string | `(unset)` | https://<host>[:port], nothing after it; home-scoped | The exact origin tailscale serve publishes plak host --web at; every request must name its host and a WebSocket upgrade its origin. Read when plak host starts with --web; a 0.0.3 build refuses a config.toml that sets it. |

OpenRouter input admission uses the discovered context window minus the reserved
output bound, lowered by eligible endpoint limits. It has no fixed 65,536-token
input ceiling or implicit cost cap; `api.full_window` applies only to the direct
API routes named above.

### 3.1 Bounds are closed ranges

A configured foreground budget is not a kill timer. Both `[jobs]` values are
rendered numerically in every route's process tool descriptions, including
nested Code Mode, JavaScript and Python presentations. Exact descriptor
reconstruction uses the stored construction values, never a fresh config read.
Config changes apply to new sessions and explicit rebind. On resume, Plak reads
these bounds from the exact stored runtime facts before
compiling tools or composition attachments. Editing `[jobs]` does not change
an existing session's bounds or descriptions; invalid stored facts refuse
resume rather than falling back to today's policy.
Explicit process-start
observation hints on every route clamp to 0..600 seconds (first observation at
least 250 ms), independently of `[jobs].foreground_seconds`. Empty Codex
`write_stdin` polls first use 5..300 seconds. When completion notices are
earned, consecutive quiet repeats hold up to 900 seconds, interruptible by
output, exit, steer, another own promoted job's terminal or a host notice
(chapter 01 §8.5). Without notices every poll retains 5..300 seconds.
Nonempty writes use 250 ms..30 seconds.
Grok output awaits, pi `wait` and Claude blocking output waits cap at one hour. Ctrl+B and
JSONL `background` are one-shot actions, not configuration or future-call defaults.

The four `[orchestration]` bounds — `max_depth`, `max_children`, `max_nodes`,
`max_attempts` — take exactly `1..=` the built-in ceiling (4, 64, 65, 256). A
value outside the range is a load error naming the key and its ceiling, and
`plak config set` refuses it the same way; nothing is clamped at load or set.
The epoch finalize clamps in `OrchestrationSnapshot::from_config` remain as
defense in depth for values that arrive already resolved. Raising a ceiling
requires a change to this chapter and a code change, never a config edit.

`max_concurrent_requests_per_account` takes `1..=64` the same way. It is
home-scoped like the other `[orchestration]` keys but is not epoch state: it
sizes the process-wide per-account request budgets when Plak launches
(chapter 03 §10.4), so an edit applies to the next launch, never to a running
tree, and does not roll the epoch digest.

The workspace writer mode is fixed at `single_lease` and the Code Mode cell
spawn budget is fixed at 8; neither is a config key any more, and a file that
still carries `occupancy` or `max_agents_per_cell` gets the named removal
error in §5.

### 3.2 A turn has no wall clock

Tool observations have a built-in, model-window-relative per-round byte cap,
not another `config.toml` key: 8% of the effective input capacity at four text
bytes per token, clamped to 32-256 KiB. Each round receives the smaller of this
cap and the runtime's headroom-derived allowance (spec 01 observation policy).
The cap is derived when a hosted session is constructed or rebound. Large
process pages keep a `full output: <path>` line pointing into the private jobs
root, including foreground commands; clipping or withholding in L0 preserves
that line. Small untruncated foreground commands create no output file.

A root turn ends when the model answers, when the round cap's wrap-up round
answers, when the operator interrupts it (Esc in the TUI, SIGINT for `plak
run`), or when one bounded operation fails: a model operation has its own
five-minute bound, and each tool call has its own execution bound. There is no
turn-level deadline, so a turn that polls a long build runs as long as the
build does. `turn.max_model_rounds` is the only runaway stop. When a turn
reaches it, the runtime appends a visible note and runs one more round with
tool selection withdrawn, so the operator gets a summary of the progress made
instead of a silent cut (spec 01 §3.3). The default, 512, is twice the former
256 cap, which had been sized at about four times the observed polling burn
(66 rounds on one 40-second `cargo check`) while a 20-minute deadline also
bounded the turn. The ceiling, 1024, is the runtime's own bound, and the
subscription gate's per-turn allowance is sized for it. A child agent inherits
its parent's cap. Each turn keeps the cap it was admitted under, including a
turn resumed after a restart.

### 3.3 Theme aliases

`dark` and `light` are accepted spellings of `default-dark` and
`default-light`. They normalize on the next save: a file that says
`theme = "dark"` is rewritten as `theme = "default-dark"` the next time
anything persists the config. The four Catppuccin values have no aliases.

### 3.4 `[tools.<name>]`

Options for an optional tool a template lists. A table enables nothing: only a
template's `tools` list does, and a listed tool whose table is absent runs on
the defaults below. `<name>` is one of the three tools that take options; any
other name is an unknown section.

| key | type | default | constraint | meaning |
| --- | --- | --- | --- | --- |
| `tools.fetch_url.max_bytes` | integer | `2097152` | 1..=8388608 | Response body cap before conversion, in bytes. |
| `tools.fetch_url.timeout_seconds` | integer | `30` | 1..=120 | Whole fetch including redirects, in seconds. |
| `tools.web_search.max_uses` | integer | `5` | 1..=20 | Searches one call may run. |
| `tools.web_search.allowed_domains` | array of string | `[]` | at most 10 domain names, none twice; exclusive with blocked_domains | Search only these domains. |
| `tools.web_search.blocked_domains` | array of string | `[]` | at most 10 domain names, none twice; exclusive with allowed_domains | Never search these domains. |
| `tools.x_search.allowed_x_handles` | array of string | `[]` | at most 20 handles of 1 to 15 letters, digits or `_`, without `@` | Search only posts from these X handles; empty is every handle. |

A template or session selecting `fetch_url` requires the compiled `web-fetch`
capability. A build without it refuses config/session admission with the typed
`CapabilityUnavailable` (exit 2), before attach or writes; it never drops the
selected tool silently. Frozen fetch bounds remain readable for show/log/export.

The same capability verdict applies to a template, card or default selecting
an uncompiled account or evaluator. Execution admission returns the typed
`CapabilityUnavailable` (exit 2), without selecting another account. Existing
session show/log/export does not require its route to be compiled. Stored
binding validation checks the frozen vocabulary, not today's model enrollment;
attach, resume and rebind separately require the compiled route before writes.
The route feature uses the account spelling: `codex`, `grok`, `claude`,
`openai`, `anthropic`, `xai-api` or `openrouter`. Codex and OpenAI additionally
select JS; Python is an explicit independent selection requiring JS. A build
without a route omits it from login/model menus, presets and catalog refresh;
an explicit command selecting it still returns the typed refusal. The named
headless, API and minimal build variants are qualified in the full-only gate,
not alternative configuration profiles or distributed artifacts.

A set `allowed_domains`, `blocked_domains` or `allowed_x_handles` replaces
the list the model passes on a call. Each option travels in the route
provider's own parameter, and a bind on a route whose provider has none
refuses with `[tools.web_search] <option> is not available on <route>;
remedy: ...`:

| route | `allowed_domains` | `blocked_domains` | `max_uses` other than 5 |
| --- | --- | --- | --- |
| claude, anthropic | web search tool `allowed_domains` | `blocked_domains` | tool `max_uses` |
| codex | `search_query` `domains` | refused | honored: one search per call |
| openai | `filters.allowed_domains` | refused | refused |
| grok, xai-api | `allowed_domains` | `excluded_domains` | refused |
| openrouter | `web` plugin `include_domains` | `exclude_domains` | honored: one search per call |

`allowed_x_handles` is the `x_search` tool's `allowed_x_handles` on grok and
xai-api, the only routes that serve X search.
On openai, grok and xai-api the provider has no search count limit, so the
default `max_uses` bounds nothing there; only an unset (default) value binds.

### 3.5 `[mcp_servers.<id>]`

One MCP server a template may name as `mcp:<id>` or `mcp:<id>/<tool>`. `<id>`
matches `[a-z][a-z0-9]{0,15}(_[a-z0-9]+)*` within 32 bytes, so the exposed
`mcp__<id>__<tool>` name splits back unambiguously. A table configures how to
reach the server; it starts nothing until a template that lists it binds.

| key | type | default | constraint | meaning |
| --- | --- | --- | --- | --- |
| `transport` | string | `(required)` | stdio\|http | How the server is reached. |
| `command` | string | `(required for stdio)` | non-empty; stdio only | argv[0], resolved on the captured PATH at spawn to the first executable file of that name (a non-executable file earlier on PATH is skipped, as execvp does). |
| `args` | array of string | `[]` | stdio only | The rest of argv. |
| `env` | table of string | `{}` | keys `[A-Za-z_][A-Za-z0-9_]*`; stdio only | Literal, non-secret environment. |
| `secret_env` | table of string | `{}` | keys as `env`, none also in `env`; values are secret names; stdio only | Environment variables whose values are named secrets. |
| `url` | string | `(required for http)` | `https://`, or `http://` only to `127.0.0.1` or `[::1]`; no credentials or fragment; http only | The streamable HTTP endpoint. |
| `bearer_secret` | string | `(unset)` | a secret name; http only | Sent as `Authorization: Bearer <secret>`. |
| `headers` | table of string | `{}` | header names; never `Authorization` or `Proxy-Authorization`; no control characters; http only | Literal, non-secret headers. |
| `secret_headers` | table of string | `{}` | header names, none also in `headers` (case-insensitive), no `Authorization` beside `bearer_secret`; values are secret names; http only | Headers whose values are named secrets. |
| `auth` | string | `(unset)` | `oauth`; http only; not with `bearer_secret` | Authenticate with the server's OAuth authorization server through a `plak mcp login <id>` login instead of a static bearer secret. |
| `oauth_issuer` | string | `(unset)` | `https://` URL (or `http://` to a loopback literal when `url` is one), no query or fragment; requires `auth = "oauth"` | The authorization server to use; it must be one the server's resource metadata lists. Required with `oauth_client_id`. |
| `oauth_client_id` | string | `(unset)` | 1 to 255 bytes of visible ASCII; requires `oauth_issuer` | A client registered with that authorization server in advance; without it Plak uses a Client ID Metadata Document (when `mcp.client_id_metadata_url` is set and the server supports one) or dynamic registration. |
| `oauth_client_secret` | string | `(unset)` | a secret name; requires `oauth_client_id` | The pre-registered client's secret, sent only to that issuer's token and revocation endpoints. |
| `oauth_callback_port` | integer | `(ephemeral)` | 1024..=65535; requires `auth = "oauth"` | Fixed loopback port of the login callback `http://127.0.0.1:<port>/callback`, for a client registered with that exact redirect. |
| `tools` | array of string | `(every listed tool)` | MCP tool names, 1 to 128 bytes without whitespace, none twice | Server-level allowlist; `mcp:<id>` means exactly these. |
| `resources` | bool | `false` | either transport | Give templates that list the server its `mcp__<id>__read_resource` tool (spec 01 §8.5); the server must advertise resources. Without it a server's resources never reach a model. |
| `placement` | string | `(unset)` | host\|runner; `runner` needs `transport = "stdio"`; unset is host | Where the server's process runs: on the host beside the agent, or on the node's runner. A runner-placed server runs on the runner the process selected, as that runner's user, in the workspace as the runner names it, with its `secret_env` names resolved there from the runner's own secrets (`plak runner secret set`); no host secret value crosses. With no runner selected the host is the node's runner: the server runs here and its `secret_env` names resolve from this home's runner secrets, never its named secrets. A session freezes the placement each server ran under in its MCP tool records. |
| `startup_seconds` | integer | `20` | 1..=120 | Budget to start the server and list its tools. |
| `call_seconds` | integer | `120` | 1..=3600 | Hard per-call ceiling; progress does not extend it. |

Release-note compatibility notice: configuration using `[mcp]`, `auth`,
`oauth_*` or `resources` does not load in earlier builds (unknown sections or
keys). This is configuration forward-incompatibility,
independent of session export/import.

A key of the other transport is a load error, and so is an `oauth_*` key
without `auth = "oauth"` or `auth` beside `bearer_secret`. The canonical
rendering a session freezes spells out `auth`, the `oauth_*` keys and
`resources` only when they are set, so a table without them renders, freezes
and digests exactly as before they existed; `placement` is spelled the same
way. Secrets are never values in
TOML: a secret name matches `[a-z][a-z0-9-]{0,31}` and resolves from the home
(spec 04 §3.1) when the server is started, from the runner's secrets for a
server placed on a runner; whether it exists is a bind check (for a server on
a selected runner, the runner's spawn check), not a load check. `plak mcp add` writes a table into the home or (with
`--scope project`) the trusted project's `config.toml` in place, keeping the
rest of the file and spelling out only the keys given; `--replace` keeps the
old table's place and comments. Every `plak mcp` edit (`add`, `remove`,
`enable`, `disable`) is checked before any file is written against both layers
it leaves: the home alone, as every load outside a project reads it, and the
home with the current trusted project over it. A home template therefore
cannot list a server or a tool only the project serves. `remove` refuses while
a template file would be left naming what its layer no longer serves;
`--force` removes those entries, checks the config and every template file
together, then writes the templates and then the config. Only the home's
templates and the current trusted project's templates are checked: a
template of another project that lists the server is not visible and is left
as it is. A secret reference given to `plak mcp add` that is not a secret
name is refused without repeating it. A written file keeps its permissions,
and a symlinked `config.toml` is written at its target. A trusted project may add an `[mcp_servers.<id>]` or
`[tools.<name>]` table or replace one of the home's by name, whole (a stdio
server is a host-authority command, like `on_mint`); its secrets still resolve
only from the home.

### 3.6 `[runners.<name>]`

One machine a host may run a node's tools on, reached over ssh as
`ssh <strict options> -- <ssh> plak runner connect --stdio` (chapter 04 §2.9).
`<name>` matches `[a-z][a-z0-9-]{0,31}` and is never `local`, the name of the
host's own runner, which needs no table. Home-scoped. `plak runner pair <name>`
and `plak runner ps <name>` dial it; no session is placed on a runner yet.

| key | type | default | constraint | meaning |
| --- | --- | --- | --- | --- |
| `ssh` | string | `(required)` | 1 to 255 visible ASCII bytes, no whitespace, not starting with `-` | The ssh destination: a `~/.ssh/config` alias or `[user@]host`. |

Any other key is a load error. The runner's own roots, PATH and HOME live in
its `runner.toml` on that machine, not here (§3.7).

### 3.7 `runner.toml`

The runner home's configuration on a runner machine, `<home>/runner/runner.toml`
(chapter 02 §2.1). It is read when the daemon starts, again at every host bind
for `hosts`, and by `plak runner allow`, which edits `hosts` in place and keeps
the rest of the file as written. A missing file is the empty configuration,
with which `plak runner serve` refuses to start. Any other key is a load error.

| key | type | default | constraint | meaning |
| --- | --- | --- | --- | --- |
| `roots` | array of strings | `[]` | absolute paths; at least one to serve; none inside or around the product home | The workspace roots a host may name. A requested workspace, the first root's default one included, is served only when it resolves under one of them and neither lies inside nor contains the runner machine's product home (`PLAK_HOME`, with the runner home and every secret and credential in it); `plak runner serve` refuses to start with a root inside or around it, or one that is not absolute. The tree's version control answers under the same roots and `copies`, outside the product home. Consent, not containment (chapter 04 §2.9). |
| `copies` | string | `(unset)` | absolute path; not inside or around the product home or any root | Where copies carried from another runner are kept, one mirror repository per runner and tree (`<copies>/<runner id>/<tree id>`, chapter 03 §14). The tree's version control also answers under it. Unset, this runner takes no copies from other runners. |
| `path` | string | `(unset)` | | The `PATH` the daemon and its processes start with. Unset, a login shell (`$SHELL -l`) is asked once, bounded at 5 s, when `connect` starts the daemon: it runs in a process group of its own, which is killed once it answered or at the deadline, so a startup file that leaves a job in the background neither delays the answer nor outlives it. |
| `home` | string | `(unset)` | | The `HOME` the daemon and its processes start with. Unset, the user's. |
| `hosts` | array of strings | `[]` | lowercase uuids | The host identities that may bind (`plak runner allow`). |
| `idle_exit_seconds` | integer | `300` | | How long the daemon stays up with no connection, job, orphan, service or unacknowledged answer before it exits. |

## 4. `agents/<name>.toml`

One flat object plus an optional `[env]` table. The file stem is the name.

| field | type | default | constraint | meaning |
| --- | --- | --- | --- | --- |
| `account` | string | `(required)` | codex\|chatgpt\|grok\|supergrok\|xai-oauth\|claude\|openrouter\|openai\|anthropic\|xai-api | Credential lane the copy runs on. |
| `model` | string | `(required)` | non-empty; a direct API account must name an enrolled model, checked at load; every other account is checked at bind | Exact model id. |
| `effort` | string | `(unset)` | none\|low\|medium\|high\|xhigh\|max\|ultra; Codex offers low through max (never ultra); none is an explicit API effort, not an omitted one; a direct API account requires a level on the model's published ladder, checked at load; every other account is checked at bind | Reasoning effort; on a subscription account a typo surfaces as a bind failure, not a load error. |
| `fast_mode` | bool | `false` | true is only valid with account = "codex" | Fast mode: request the provider's faster processing lane for this Codex subscription template. It is not a model choice and does not grant additional tools. A child runs on its own template's setting, never its parent's. |
| `composition` | enum | `"plakous-default"` | direct\|plakous-default\|native-ptc\|js-ptc\|py-ptc; native-ptc is refused for openrouter, anthropic and xai-api, and on openai is valid only for a model enrolled as CodeModeOnly | How the model composes the product tools: direct calls one at a time, or a programmatic tool calling (PTC) cell. native-ptc is the provider's own PTC system (today only Codex Code Mode is enrolled); js-ptc and py-ptc are harness PTC on every account. plakous-default uses the versioned Plakous-recommended tool presentation for this provider/model; the resolved presentation is pinned for the session and does not select or substitute a model. |
| `protocol` | enum | `"auto"` | auto\|chat\|responses\|messages; anything but auto is OpenRouter-only | Wire protocol pin for an OpenRouter model whose family the gateway reports wrongly. |
| `description` | string | `""` | required and non-blank for a template named in [roster] working_set or in a spawns list | The spawn card a parent routes on: what this agent is for, in the parent's words. |
| `access` | enum | `"full"` | full\|read-execute | Edit ceiling: full includes file-edit tools; read-execute omits dedicated file-edit tools but permits shell commands. Neither is an OS sandbox. A spawn request above the ceiling is clipped, not refused. |
| `tools` | array of string | `[]` | web_search\|web_fetch\|x_search\|fetch_url\|image_gen\|mcp:<id>\|mcp:<id>/<tool>, none twice; each mcp:<id> names a [mcp_servers.<id>] table; a child gets exactly its own list, never its parent's | Optional tools enabled by name after the core tools; options come from config.toml [tools.<name>] and [mcp_servers.<id>]. Each is resolved against the route when a session or child binds, and a tool the route cannot serve refuses the bind. |
| `spawns` | array of string | `[]` | 0..=5 names, no duplicates, each a template file with a description | The templates a copy of this template may spawn; its own name is never on its menu. Empty is a leaf with no spawn_agent. The root spawns [roster] working_set instead; this list applies where the template runs as a child. |
| `may_adopt` | bool | `false` | requires spawns naming another template | Whether a copy may adopt a sealed candidate out of its own subtree. |
| `delegation` | enum | `"explicit"` | explicit\|proactive | Whether the model is told to delegate unprompted or only when asked. |
| `cost_tier` | integer | `(unset)` | 0..=255 | Static label a parent sees on the spawn card; never a measured cost. |
| `max_turns` | integer | `(unset)` | 1 or more | Turn budget for a copy of this template. |
| `on_mint` | string | `(unset)` | not validated at parse; runs in the copy's workspace | Command run once when a copy is minted, before its first turn. |
| `show_model` | bool | `false` | — | Put the model id on the spawn card next to the description. |
| `env` | table of string | `{}` | keys are not validated; values are strings | Environment for on_mint only; not passed to the copy's tool calls. |

`account` and `model` are required; every other field has a default. `on_mint`
is the only free string with no check at all. On a subscription account an
`effort` typo surfaces as a bind failure against the catalog, not as a load
error; on a direct API account both `effort` and `model` are checked when the
file is read. The file stem alone is the template's name; there is no `name`
key.

The hook receives tree-supplied `PLAK_LAUNCH_CHECKOUT`, `PLAK_PARENT_PATH`, and
`PLAK_COPY_ID` values, overriding same-named `[env]` entries. Chapter 03 §13
defines their meanings and the hook's cleared-environment lifecycle.

Cross-field rules, all checked at load and again before any save:

- `may_adopt` requires `spawns` naming another template. A list of only this
  template is still a leaf, because a node's menu excludes itself, and a leaf
  cannot adopt into its own copy.
- `spawns` names at most 5 templates, none twice. Every name MUST be a template
  file, and every template a `spawns` list names MUST carry a non-blank
  `description`, whether or not the working set reaches it. A list may name the
  template itself or close a cycle; a node's menu never carries its own
  template, and the depth bounds end a cycle.
- `fast_mode = true` requires `account = "codex"`. A template that sets it is a
  **Fast** template. Fast and a non-empty `spawns` are legal together anywhere,
  including the root and the working set: `spawns` decides only whether a node
  may be a parent, and every child runs on its own template's tier.
- `protocol` other than `auto` requires `account = "openrouter"`.
- `composition = "native-ptc"` selects the vertical's own programmatic tool calling system (spec 02 §6.5); today only Codex Code Mode is enrolled, on Codex `CodeModeOnly` rows and OpenAI API models enrolled as `CodeModeOnly`, and other verticals have none enrolled. It is refused for `openrouter`, `anthropic` and `xai-api`; on `openai` it is valid only for a model the API enrollment table marks `CodeModeOnly` (spec 02 §3.1), checked at load. On a subscription account it is checked at bind.
- `composition = "py-sdk"` in any agent template is a load error: `py-sdk is root-only and has no child result channel`. Select it with `[root] composition = "py-sdk"`, the root settings picker or `--composition py-sdk`. It requires the core single-agent surface and a Direct-capable route; the root opens no topology, and optional tools are refused before inference. `plakous-default` never chooses it.
- Direct API accounts require an enrolled model and an explicit effort on that model's published ladder (`plak models <account>`); the provider route fixes their wire protocol.
- `max_turns` must be 1 or more.
- `tools` entries are parsed in order: a name outside the closed set, a
  malformed `mcp:` name, a duplicate, or an `mcp:<id>/<tool>` beside the
  `mcp:<id>` that already includes it is refused at its location. Every
  `mcp:<id>` MUST name an `[mcp_servers.<id>]` table in the effective
  config.toml, and an `mcp:<id>/<tool>` MUST name a tool that table's `tools`
  allowlist admits (a table without one admits any name).
- A child's list is never compared with its parent's (chapter 03 §3.1): the
  spawn graph is the capability grant.
- A template named in `[roster] working_set` MUST carry a non-blank
  `description`.

## 5. Named load and save errors

The loader never fails anonymously. A name that used to mean something else
names its replacement; every other failure carries the line and column it
happened at, and a load names the file it read.

Every message below is exact as the loader writes it. Config messages, including
unknown names, are followed by ` (line <l>, column <c>)`,
and a template message is prefixed `template <name>: `.

| input | message |
| --- | --- |
| `[agent.<name>]` in config.toml | `[agent.<name>] in config.toml is no longer a template; move it to ~/.plak/agents/<name>.toml` |
| `[default]` | `unknown config section [default]; tuple fields moved to template files under ~/.plak/agents with [root] agent, and subagents moved to [orchestration]` |
| `[roster.<x>]` | `unknown config section [roster.<x>]; [roster] holds only working_set = ["<template>", ...]` |
| any other unknown section | `unknown config section [<name>]` |
| any other `[roster]` key | `[roster] only accepts working_set; move <key> into agents/<name>.toml` |
| any other unknown key | `unknown config key: <section>.<key>` |
| a duplicate key or table | `duplicate key` |
| malformed syntax | the document parser's own sentence, such as `unclosed table, expected \`]\`` |
| a string key given another type | `[<section>] <key> must be a string` |
| a bool key given another type | `boolean config values must be true or false` |
| `working_set` that is not an array of strings | `[roster] working_set must be an array like ["a", "b"]` |
| template `spawns` that is not an array of strings | `template <name>: spawns must be an array of template names` |
| template `spawns` with an invalid name | `template <name>: spawns: <name grammar error>` |
| unknown template key | `template <name>: unknown key <key>` |
| template `tools` written as the old string ceiling | `template <name>: tools is now a list of tool names; the edit ceiling moved to access` |
| template `access` outside its set | `template <name>: access must be full\|read-execute` |
| template `tools` that is not an array of strings | `template <name>: tools must be an array of tool names` |
| template `tools` entry refused by the grammar | `template <name>: tools: ` followed by `unknown tool "<x>"; expected one of …`, `tool "<x>" is listed twice`, `tool "<x>" is already included by mcp:<id>; list one or the other`, or the invalid server-id / tool-name sentence |
| `mcp:<id>` with no table | `template <name>: tools names mcp:<id>, and config.toml has no [mcp_servers.<id>]` |
| `mcp:<id>/<tool>` outside the allowlist | `template <name>: tools names mcp:<id>/<tool>, which [mcp_servers.<id>] tools does not allow` |
| `[tools.<x>]` for another name | `unknown config section [tools.<x>]; [tools.<name>] takes fetch_url, web_search or x_search` |
| a `[tools.*]` or `[mcp_servers.*]` value out of range or of the wrong shape | `[<table>] <key> must be an integer in <a>..=<b>` / `must be an array of at most <n> <what>` / `must be a string` / `must be a table of strings` / `must be an array of strings` |
| both domain lists set | `[tools.web_search] allowed_domains and blocked_domains cannot both be set; list one or the other` |
| invalid server id | `[mcp_servers.<id>] is not a valid MCP server id; an id matches [a-z][a-z0-9]{0,15}(_[a-z0-9]+)* within 32 bytes` |
| missing or unknown transport | `[mcp_servers.<id>] needs transport = "stdio" or "http"` / `[mcp_servers.<id>] transport must be "stdio" or "http"` |
| missing command or url | `[mcp_servers.<id>] needs command for transport = "stdio"` / `needs url for transport = "http"` |
| a key of the other transport | `[mcp_servers.<id>] <key> does not apply to transport = "<transport>"` |
| a url outside the rule | `[mcp_servers.<id>] url must be https://, or http:// to a loopback literal (127.0.0.1 or [::1])`; with userinfo, `url cannot carry credentials; name a secret with bearer_secret` |
| a secret reference that is not a name | `[mcp_servers.<id>] <key> names a secret: secret name "<x>" must match [a-z][a-z0-9-]{0,31}` |
| a literal `Authorization` or `Proxy-Authorization` header | `[mcp_servers.<id>] headers cannot carry <header>; name a secret with bearer_secret or secret_headers` |
| a template field of the wrong type | `template <name>: <key> must be a string` / `a boolean` / `an integer` |
| direct API template whose model is not a usable listed row, or whose effort is missing or off the model's published ladder | `template <name>: ` followed by the API admission refusal: the model is not listed (with up to three closest ids), is listed but not usable (with its status and reason), or `requires --effort <ladder>` |
| a template `[env]` value that is not a string | `template <name>: env <key> must be a string` |
| template section other than `[env]` | `template <name>: unknown section [<x>]; a template is one flat object plus an optional [env] table` |
| `[root] agent` naming no file | `[root] agent names no template file <name>.toml under ~/.plak/agents or .plak/agents` |
| `working_set` naming no file | `[roster] working_set names no template file <name>.toml under ~/.plak/agents or .plak/agents` |
| working-set member with no description | `[roster] working_set template <name> needs a description for its spawn card` |
| `spawns` naming no file | `template <name> spawns <child>, and no template file <child>.toml exists under ~/.plak/agents or .plak/agents` |
| `spawns` naming a template with no description | `template <child> is in template <name>'s spawns and needs a description for its spawn card` |
| `spawns` with a duplicate or more than 5 names | `template <name> spawns names "<child>" more than once` / `template <name> spawns names <n> templates; the maximum is 5` |
| `may_adopt` without `spawns` naming another template | `template <name> may_adopt requires spawns naming another template` |
| `telemetry.*` of zero | `telemetry.<key> must be a positive integer` |
| project-scope write to an untrusted root | ``project <root> is not trusted; run `plak config trust` before writing project configuration`` |
| project-scope save changing an on-disk project template | `template <name> is defined by the project (<path>); edit it there` |
| project config clearing `root.agent` | `a project config cannot clear root.agent; override it with a template name or clear it globally` |
| user-scope `config set` on a project-decided key | `<key> is set by the project config <path>; use --scope project to change it there` |

An untrusted project is a warning, not an error: `project <home> is not
trusted; its <config.toml and/or agents/> not applied (run `plak config trust`
to enable it)`. `plak config show` prints it as `project=<home> (untrusted —
ignored)` and still resolves the global configuration.

`tui.collapsed_edit_blocks`, `tui.reading_width` and `tui.show_timestamps` were
deleted. Each gets a named file-load removal error with a delete-this-line remedy;
`plak config set` instead says there is nothing to set. A user
file load error points at `plak config edit`; a project overlay error points at
the project file path. These remedies do not require a successful config load.

`tui.banner` was deleted too, along with the `tui.startup_animation` it was
briefly renamed from. The PLAK wordmark is drawn at the top of every transcript
the pane is wide and tall enough to hold it in, so there was nothing for the key
to switch. All four spellings — both names, dotted and bare — get the removal
message above; the fix is to delete the line.

The boolean spellings `1`, `yes`, `0` and `no` were accepted by the removed
grammar and are not values any more, in a file or on the command line. A file
that still carries one gets *boolean config values must be true or false* with
the line it is on.

Config publication has one commit point: the atomic replacement of
`config.toml`. What is published is the file that was already there with only
the keys whose value actually changed rewritten, so an operator's comments, key
order, alignment and blank lines survive a save of one other key, and a key the
file leaves out stays out and keeps its default. A theme alias is one such
change and normalizes on that save (§3.3). A home with no `config.toml` yet is
published with only changed preferences pinned. `plak config init` and
`config edit` instead create a reference template with every key commented
out and its section headers live, so uncommenting a key pins it in its own
section; omitted keys continue to follow defaults. Every root binding edit copies its current template body into a
fresh unshared name (`default`, `default-2`, and so on), preserving its other
fields and leaving the previous template file untouched. Saves preflight all
existing template bodies and refuse changes to them; edit user or project
files directly. Unrelated settings saves do not rewrite templates.

A private atomic-file operation exclusively creates a temporary file, writes
and `fsync`s its contents, publishes it, then syncs its parent directory. Fresh
templates are published with an exclusive hard link so a concurrent file cannot
be overwritten; the config pointer is published by rename. All prerequisite
template files and their directory entries are synced before the pointer write.
A staging failure can leave an unreferenced template, but the active binding
stays unchanged and the result is *before publication*. A failure syncing the
config directory after pointer rename is *published, durability uncertain*;
callers must not roll back as though the old pointer remained active. Invalid
configuration is refused before any publication. Saves never delete templates.

**Scoped writes.** When a trusted project is resolved, every write names its
scope. A **user**-scope save publishes only values and templates the global
home owns: a project-decided key is never absorbed into `config.toml`, and a
project template is never rewritten from a merged in-memory body. A
**project**-scope save requires trust, writes `<project>/.plak/config.toml`
containing only the keys that differ from the global baseline — an equal
value is dropped so the file stays a delta — and stages project templates
under `<project>/.plak/agents/` by the same exclusive-create rule as the
global home. A project file that ends up carrying no overrides is not
written; an existing one is still published through the atomic path.

## 6. Epoch and digest rule

The live `config.toml` is not bind authority for a running tree. At genesis the
resolved config is frozen into an `OrchestrationSnapshot`: the root bind, the
writer mode, the four clamped tree bounds, the epoch rules, the roster (one
`RosterBinding` per closure template: the working set plus every template
reachable from it through `spawns`), and a `templates_digest` over the
canonical rendering of every closure template plus the root template's body,
in name order. The root template has a card only when the closure reaches it.
Each card carries its template's `tools` list. The snapshot also freezes
`tool_sections`: the canonical rendering of every `[tools.<name>]` and
`[mcp_servers.<id>]` table a closure template's list, or the bound root
template's list, reaches — a listed tool with options and no table renders its
defaults, so spelling a default out changes nothing. The same renderings follow
the templates in the `templates_digest` body. The snapshot's own `digest`
covers all of it; an epoch that reaches no table carries no `tool_sections`
field and digests exactly as before the field existed.

Because the digest covers the resolved template **contents**, editing a
template file mid-epoch changes nothing: the running tree keeps binding what
was frozen. The same holds for a referenced tool table: editing an MCP server's
command mid-epoch changes nothing a running tree binds. Editing a table no
frozen list reaches changes no digest at all. The edit takes effect in a new
session at genesis, including the in-process history carry `/reload` performs
when these settings changed. Reload requires an idle root with no live children
or background processes; it creates a linked session under current config and
login, freezes the new tree's spawn bounds and roster, and recompiles root tool
descriptions. It records `orchestration settings changed; carried into a new
session to apply them`. The source epoch is unchanged. The decision compares
every frozen effective value against the current frozen snapshot, with the
same genesis bound clamping. Only `digest`, `templates_digest` and `bind_hints`
are excluded; reached tool tables compare as TOML values, not rendering bytes.
Changed descriptions, cost tiers, model visibility, template names and root
template selection count, including values supplied by built-ins or defaults
when not overridden by the user. Adding an always-rendered tool-option key
also counts as a changed default and carries each affected idle session once.
Hints or digest/render-recipe changes alone
keep exact reopen, even through multiple build reloads. `/rebind` explicitly performs the same carry even when
exact reopen cannot attach the root. Neither path relabels provider replay for
another account; no prompt is automatically submitted. A profile's
Fork now does not touch the running tree at all: the new session opens a tree
of its own whose genesis freezes the profile's roster (spec 03 §1.2). Two
homes with byte-different but semantically identical templates produce the
same digest, because the digest is taken over the canonical rendering, not the
file bytes.

`plak config set` on `root.agent`, any `roster.*`, or an epoch-scoped
`orchestration.*` key (all except `turn.auto_continue`) is accepted only with
quiescent persisted trees and reports `saved; applies to new sessions and at
the next /reload of an idle session`. It does not modify an attached epoch.
`turn.auto_continue` is home-only and Immediate:
it saves without tree quiescence, and the attached interactive host re-reads
it before waking. Every other immediate key saves immediately.

## 7. `plak config` commands

```
plak config show
plak config init
plak config edit
plak config set <key> <value> [--scope user|project]
plak config trust
plak config untrust
```

`--scope` defaults to `user` (`global` is an accepted spelling of the same
scope) and chooses which file a trusted project context writes (§5). With no
project resolved, or an untrusted one, only `user` exists: `project` fails
with the trust message in §5's table. `plak config trust` records the
discovered project root in `$PLAK_HOME/trusted-projects.toml` and fails when
no project is resolved; `plak config untrust` revokes it. Trust is recorded
per canonical root, never per `.plak` directory or session.

`plak config edit` opens the raw user file without first loading it, so it
remains usable when strict parsing refuses startup. Doctor checks the effective
configuration, including trusted project overlays and all template files; a
refusal points at this editor for the user file or at the named project/template
path, never at `plak config show`, which requires a valid configuration.

`plak config show` prints the effective config with its provenance: the
resolved `config_path`, the `project=` line naming the discovered project
home and its trust state, each `warning=`, and an `origin.<key>` /
`origin.template.<name>` line per value saying which file decided it —
`default`, `user`, or `project`.

`<key>` is the dotted `section.key` from §3. Values are unquoted on the command
line (`plak config set tui.theme catppuccin-mocha`); a bool key takes `true` or
`false` and nothing else. A rejected key or value leaves the file byte
identical: the same rule set the file load runs is run on a candidate copy
first, and nothing is written unless it validates. An accepted value rewrites
its one key and leaves the rest of the file as the operator wrote it. An unknown key exits with the usage
message *unknown config key: `<key>`; run `plak config show` for the valid
keys*.

Only dotted configuration keys are accepted. Bare `mouse`, `theme` and
`compaction_enabled` are refused with the corresponding dotted spelling
(`tui.mouse`, `tui.theme`, `compaction.enabled`). Invalid command-line values
exit 2; an invalid configuration file remains exit 1.
`config edit` selects nonempty `VISUAL`, then `EDITOR`, as a shell command with
the config path passed separately. It uses `vi` only with interactive stdin and
stdout; without a terminal or explicit editor it refuses with exit 2 before
creating the home. Template creation failures propagate instead of launching
an editor on an unavailable file.

Two keys take a value spelling the file grammar cannot:

- `root.agent ""` clears the root pointer. The template file stays: templates
  are file-authored and a save never deletes one.
- `roster.working_set` accepts the file's own array — `'["a", "b"]'` — and a
  shell shorthand splitting on commas or spaces: `a,b`, `a, b`, `a b`. An empty
  value clears the set. The **file** grammar remains the array only; a bare
  `working_set = a, b` in `config.toml` is not a TOML value at all and fails as
  malformed syntax, and `working_set = 3` is the named array error.

`agent.*` is refused (§1). Everything else is `unknown config key`.

## 8. Shipped presets

| path | what it is |
| --- | --- |
| `docs/reference-config/` | Every key and every template field, each with its meaning, type, default and constraint as a comment. It is the approved Codex provider preset annotated by the product's own renderer — regenerated by a test (`PLAK_REGENERATE_REFERENCE_CONFIGS=1 cargo test -p plak reference`), so it cannot describe a key the loader does not have or carry values the preset does not approve. It is a working home: `root-agent` bound as root, `worker`, `reviewer` and `researcher` in the working set. |
| `bin/plak/presets/` | The default role set, one home per built-in profile: `codex`, `claude`, `grok`, and `multi` (root and reviewer on `claude`, worker on `codex`, researcher on `grok`). They live inside the `plak` package because the binary embeds their agent files; the Agents tab embeds these four as `codex-default`, `claude-default`, `grok-default` and `multi-default`. |
| `tests/fixtures/config-scenarios/` | Complete scenario homes — single-agent, mixed-account trees, direct and PTC compositions, OpenRouter-only. |

Every built-in profile has the same four roles:

| role | `access` | `spawns` | `tools` |
| --- | --- | --- | --- |
| root (`root-agent`) | `full` | `[]` | none: core tools only; its menu is the working set |
| `worker` | `full` | `[]` | none |
| `reviewer` | `read-execute` | `[]` | none |
| `researcher` | `full` | `[]` | hosted retrieval the route serves plus `fetch_url`: `web_search, web_fetch, fetch_url` on `claude`, `web_search, fetch_url` on `codex`, `web_search, x_search, fetch_url` on `grok` |

The researcher's card names what it researches with its tools (the workspace
and the web, and posts on X (x.com) where it has `x_search`), asks for grounded findings
with their sources, and asks it to edit files only when the brief asks. The
capability line after the card lists the tools themselves. The root orchestrates; the worker, reviewer
and researcher are leaves, so the default role graph is one level deep
whatever a larger `max_depth` allows. A deeper tree is user configuration: a
template whose `spawns` names other templates (for example a worker with
`spawns = ["researcher"]`) and `[orchestration] max_depth` raised to admit it.
In a built-in profile each role is the template `<profile>-<role>`. No shipped preset or scenario carries a `[tools.*]` or
`[mcp_servers.*]` table: the researchers run their tools on the table
defaults. The reference `config.toml` ends with a fully commented example of
both table families.

Every preset under `docs/reference-config/`, `tests/fixtures/config-scenarios/` and `bin/plak/presets/` is loadable as a
`PLAK_HOME`; `bin/plak/tests/integration/product_config_fixtures.rs` and the
`plak` unit tests load them offline, so a schema change breaks them rather than letting them rot. They are
ordinary TOML files: copy one, edit it by hand, and a `plak config set` later
keeps the comments you left in it.
