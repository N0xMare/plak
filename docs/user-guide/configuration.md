# Configuration

Plak keeps editable preferences in config.toml and agent templates under
agents/. These are ordinary TOML files; credentials and durable session state
are separate and must not be copied as configuration. Comments, multi-line
strings and arrays are supported, and configuration saves preserve unrelated
formatting.

For the exhaustive list of keys — type, default, constraint, meaning — see
[`../spec/07-config-reference.md`](../spec/07-config-reference.md), or read
`configs/reference/config.toml` in this repo, which is the same list as a
commented file you can copy.

## Your Plak home

```
~/.plak/
  config.toml          # preferences
  agents/<name>.toml   # one agent template per file
  secrets/             # credentials; nothing here is in config.toml
```

Optionally create a commented reference template (no defaults are pinned):

```sh
plak config init          # writes a commented template; pins no defaults
plak config show          # prints the resolved file, its path, and origins
plak config edit          # opens it in $VISUAL, then $EDITOR
plak config set <key> <value>
```

`PLAK_HOME` overrides the location and must be absolute, with no operator-placed
symlinked ancestor. macOS system aliases such as `/tmp` and `/var` are allowed. That is how you keep a throwaway home for experiments:

```sh
PLAK_HOME="$HOME/plak-scratch" plak config init
```

`config edit` chooses `VISUAL`, then `EDITOR`, as a shell command (for example
`EDITOR="code --wait"`). Without either, it uses `vi` only at a terminal;
headless callers must set an editor explicitly.

Deleting a key from the file restores its default. Deleting `config.toml`
restores all of them. A key you never wrote stays out of the file: saving one
setting does not fill the rest of the schema in around it.

Plak reads the file and refuses to start on anything it did not fully
understand. A typo, a duplicate key, a value of the wrong type and a key that
does not exist are all errors that name the file, and the line and column
wherever there is one to name.

## Idle Auto-Continue

`[turn] auto_continue` defaults to `true`. An idle interactive
root continues when background processes finish or child results or questions arrive after a 2-second quiet
window. User prompts take precedence; Esc pauses auto-continue until your next
prompt. There is no limit on consecutive wake turns; each needs a new completion
or unread child result or question. Recovery and
paused restored queues prevent waking. JSONL remains explicit-prompt only; `plak run` follows its own until-idle
policy and ignores this key. A `plak run` wake that does not fit the window still
pauses auto-continue on the session, so opening it later in the TUI starts paused
until the next prompt. Set `plak config set turn.auto_continue false`
to disable it immediately, including while children are running. The attached
host re-reads this home-only flag; no tree quiescence or reattachment is needed.

## Process Job Bounds

`[jobs]` controls how long process tools run in the foreground and background:

```toml
[jobs]
foreground_seconds = 30
max_runtime_hours = 10
```

`foreground_seconds` accepts 1..=600 seconds (default 30). Reaching this budget
promotes the process to the background; it does not kill it.
`max_runtime_hours` accepts 1..=24 hours (default 10) and is the hard runtime cap
for a background process. Set them with
`plak config set jobs.foreground_seconds 60` or
`plak config set jobs.max_runtime_hours 12`.

These bounds bind when a session is created or explicitly rebound. An exact
resume keeps its stored bounds; editing `[jobs]` does not change an existing
session's process tool descriptions or running jobs. Observation hints on tool
calls control how long to wait for output, not these lifetime bounds. Ctrl+B
and JSONL `background` promote one process, not future calls.

## API Input Window

`[api]` chooses the input window direct API sessions (`openai`, `anthropic`,
`xai-api`) are admitted under:

```toml
[api]
full_window = false
```

By default (`false`) a model with a published price threshold runs under it
(Claude Haiku 5.5: 100k tokens, above which input costs more); every other
model runs under its full window. `true` admits the full window on every model.
Both are provider input tokens: once a reply reports its input count, Plak
measures the conversation against the window with that count, not with request
bytes.
Set it with `plak config set api.full_window true`. Like `[jobs]`, it binds when
a session is created or explicitly rebound; an exact resume keeps the window
it was created under. `plak models <account>` shows both windows.

Non-interactive commands receive null stdin, so a command such as `cat` without
arguments receives EOF rather than waiting for input. Codex commands
that need `write_stdin` input use `tty: true`; non-PTY sessions still support
waiting for output and stopping. If stdin is closed, an input refusal explains
how to rerun `exec_command` with `tty=true`. Claude, Grok and pi (OpenRouter) shell tools do not accept stdin
input.

## Choose a root

The root is the agent your session starts as. It is a **template**: a file in
`~/.plak/agents/` naming an account, a model, and what that agent is allowed
to do.

```toml
# ~/.plak/agents/root-agent.toml
account = "codex"
model = "gpt-5.6-sol"
effort = "high"
description = "Root operator: drives the session and delegates bounded work."
access = "full"
```

```toml
# ~/.plak/config.toml
[root]
agent = "root-agent"
```

`account` must be one you are logged in to (`plak status`), and `model` must be
one that account offers. Neither is checked when the file is read — they are
checked when a session binds — so a typo shows up at launch, not at save.

The Agents tab writes a profile's templates and points the root and the
working set at them. Nothing else in a session changes the live root: there is
no slash command that switches its account, model or effort.

## Build a working set

The working set is the list of templates the root may spawn. Up to five names,
each of which must be a template file that carries a `description` — that
description is the card the root reads when it decides who to hand work to, so
write it as "use this one when …".

```toml
[roster]
working_set = ["worker", "reviewer", "explorer"]
```

or, from the shell:

```sh
plak config set roster.working_set worker,reviewer,explorer
plak config set roster.working_set ""      # clear it
```

The array form the file uses works there too, quoted for the shell:
`plak config set roster.working_set '["worker", "reviewer"]'`.

A change to `[root]`, `[roster]` or `[orchestration]` is accepted only when no
tree is mid-run. Plak tells you which happened: `applied`, or `saved; applies
at the next session genesis`.

## Write a template

One file per agent, named by the file: `~/.plak/agents/reviewer.toml` is the
template `reviewer`. A project can override one for everybody working in it by
putting the same name in `<project>/.plak/agents/` — see *Per-project
configuration* below.

The fields worth deciding on:

- `description` — the spawn card. Required for anything in the working set or
  in a `spawns` list.
- `access` — `full` or `read-execute`. A reviewer or explorer can use
  `read-execute` to omit dedicated file-edit tools while retaining shell
  diagnostics. Shell commands still run with host authority; this is not an OS
  write prohibition. A request for more is clipped, not refused. (This field
  was called `tools`; a template that still says `tools = "full"` is refused
  with a hint to rename it.)
- `tools` — optional tools beyond the core set, by name:
  `tools = ["web_search", "fetch_url", "mcp:github"]`. See *Optional tools and
  MCP servers* below.
- `spawns` / `may_adopt` — which templates this agent may spawn when it runs
  as a child (`spawns = ["worker", "reviewer"]`; omitted means a leaf), and
  whether it may pull a finished child's work into its own checkout.
  `may_adopt` requires `spawns` naming another template. The root spawns `[roster]
  working_set`, not its own `spawns`.
- `effort` — `none|low|medium|high|xhigh|max|ultra`, as the model admits. `none` is an explicit effort, not an omitted one. A direct
  API account (`openai`, `anthropic`, `xai-api`) needs an effort on the
  model's published ladder (`plak models <account>`); the template is refused
  at load without it.
- `cost_tier` — a 0–255 label *you* choose, shown on the spawn card. It is not
  measured; it is your hint about what this agent costs to run.
- `[env]` — environment for `on_mint` only, not the copy's tool calls.

`configs/reference/agents/` has all four of these written out with every field
documented in place, including the ones most templates leave alone.

## Optional tools and MCP servers

Every template gets its provider's core coding tools. A template's `tools`
list adds more, by name:

| Name | What it is |
| --- | --- |
| `web_search`, `web_fetch`, `x_search` | Search and fetch run by the provider of the route the agent binds. A route that cannot serve one refuses the session or spawn with a remedy (for example, put `x_search` on a grok template and spawn it). |
| `fetch_url` | One HTTPS fetch Plak runs itself, on every route: HTML comes back as Markdown, text and JSON as text, small PNG/JPEG/WebP as images. Only public internet addresses are reachable; see *fetch_url* below. |
| `image_gen` | Generates or edits one image through the route's provider and saves it as a new workspace file; it never overwrites a file. Each call is a paid request. Every route refuses it in this release: OpenAI API and xAI API (and Codex and Grok, which have no image transport yet) until a live check qualifies them, Claude, Anthropic and OpenRouter because they have no image endpoint. |
| `mcp:<id>` | Every tool an MCP server allows. |
| `mcp:<id>/<tool>` | One tool of an MCP server. |

None is on by default and no shipped profile turns one on. Options live in
`config.toml`; a table there enables nothing by itself:

```toml
[tools.web_search]
max_uses = 5
allowed_domains = ["docs.rs"]

[mcp_servers.github]
transport = "stdio"
command = "github-mcp-server"
args = ["stdio"]
secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = "github" }
tools = ["get_file_contents", "create_issue"]

[mcp_servers.linear]
transport = "http"
url = "https://mcp.linear.app/mcp"
bearer_secret = "linear"
```

`fetch_url` takes its limits from `[tools.fetch_url]`:

```toml
[tools.fetch_url]
max_bytes = 2097152      # response body cap, 1..=8388608; a call may only lower it
timeout_seconds = 30     # whole fetch including redirects, 1..=120
```

It sends one `GET` with no cookies or credentials, upgrades `http://` to
`https://`, follows up to 5 redirects (never from https to http) and refuses
loopback, private, link-local and cloud-metadata destinations on every hop. It
does not read `robots.txt`: each call is one fetch you directed, not a crawl.
Text over 256 KiB is saved to a file the result names on a `full output:`
line. The model chooses the URL, so anything it has read can leave in a query
string; leave `fetch_url` off templates that must not send workspace content
out.

Secrets are never written in `config.toml`. Store one under a name and refer
to the name:

```sh
plak secret set github     # prompts; the value is never echoed or taken from argv
plak secret list           # names only
plak secret rm github
```

A stdio MCP server starts when the first session that names it binds, is
shared by every session in the same workspace, and stops five minutes after
the last one ends. Its tool list is frozen when the session is created; a
server that later changes a tool answers that tool with an error telling you to
start a new session. The root's tools are frozen the same way: editing the
`[root] agent` template's `tools`, a `[tools.*]` table or an `[mcp_servers.*]`
table changes new sessions only, never one you resume, clear or switch models
in. A tool listed through `mcp:<id>` whose input schema Plak cannot represent
is left out with a notice in the transcript; naming it as `mcp:<id>/<tool>`
refuses the session instead. MCP results reach the model marked as untrusted
data. An `http` server is reached at its `url` (HTTPS, or plain HTTP only to
`127.0.0.1` or `[::1]`); `bearer_secret` and `secret_headers` name stored
secrets that are sent as `Authorization: Bearer` and as those headers.

A child gets at most its parent's tools: a working-set template may not list
`fetch_url`, `image_gen` or an MCP tool its root template lacks, and the same
holds for every `spawns` entry. `web_search`, `web_fetch` and `x_search` are
exempt, so a root can hand X search to a Grok child it could not run itself.
The check runs when the configuration loads and names the edge and the extra
tools. The full key reference is in spec 07 §3.4 and §3.5.

### Rust build caches

`on_mint` runs once in a new copy before its first turn. It receives
`PLAK_LAUNCH_CHECKOUT` (the original checkout), `PLAK_PARENT_PATH` (the
spawning parent's workspace), and `PLAK_COPY_ID` (the tree-scoped copy id).
These values override same-named `[env]` entries; the hook otherwise receives
only host `PATH` and `HOME` plus its template's `[env]`.

A shared `CARGO_TARGET_DIR` serializes builds through Cargo's build lock.
Setting it in `[env]` affects only the hook, not later tool calls. To warm a
copy's own default `target/` without sharing that lock, add this to the
template, before any `[env]` table:

```toml
on_mint = '''
if [ -d "$PLAK_LAUNCH_CHECKOUT/target" ]; then
  case "$(uname -s)" in
    Darwin) cp -cR "$PLAK_LAUNCH_CHECKOUT/target" ./target || true ;;
    Linux) cp -a --reflink=always "$PLAK_LAUNCH_CHECKOUT/target" ./target || true ;;
  esac
fi
'''
```

Seed from an idle launch checkout. Warm-up is best-effort: on Linux,
`--reflink=always` skips copying where reflinks are unsupported, and the copy
then builds cold. On macOS, `cp -cR` uses APFS clones; on a non-APFS volume it
falls back to a full copy. For a large `target/`, that fallback is slow and
disk-heavy and can hit the hook's 300 s ceiling, failing the spawn. Use this
recipe on APFS to keep macOS warm-up copy-on-write rather than a full copy.
Do not share one `CARGO_TARGET_DIR` across concurrent copies. Cargo may rebuild
path-dependent artifacts, but each copy builds independently.
`sccache` is an optional additional compiler cache; configure it separately for
tool commands, since hook `[env]` does not carry over.

## Per-project configuration

A repository can carry its own Plak configuration. Launching anywhere inside a
project that has `.plak/config.toml` or `.plak/agents/` resolves the nearest
such directory as the project root. Product homes are never projects: neither
`$PLAK_HOME` nor, when `PLAK_HOME` points elsewhere, the default `~/.plak`. A project home holds:

```
<project>/.plak/
  config.toml          # overrides only the keys it carries
  agents/<name>.toml   # replaces the whole global template of that name
```

A repository's configuration can name models, efforts and an `on_mint` hook,
so it does **not** apply just because you `cd` into the repo. The first time
you see Plak report `project <path> is not trusted; … not applied`, decide
whether you trust it:

```sh
plak config trust       # admit this project's .plak configuration
plak config untrust     # take it back
```

Trust is recorded once per project in `~/.plak/trusted-projects.toml` — a
list you own, in your home, that a repository cannot edit for you. Until
trusted, the project config is skipped entirely and `plak config show` marks
it `untrusted — ignored`.

Orchestration controls (`[orchestration]`) always come from your home config.
Project values in that section are ignored with a warning, even after trust.
Use `plak config set orchestration.<key> <value> --scope user` to change them.

Once trusted, the merge is asymmetric by design: your global config can never
accidentally name a project template (the global file is validated before the
project is even read), while the project's config may name global or project
templates. A same-named project template replaces the global one wholesale —
it is a whole file, not a field-level merge.

Writes pick a side too. `plak config set` writes `~/.plak/config.toml`; add
`--scope project` to write the override into `<project>/.plak/config.toml`
instead — Plak stores only the keys that differ from your global file, so the
project config stays a readable delta. Setting a key the project already
decides asks you for `--scope project` rather than silently absorbing the
project's value into your global file. Settings you change inside the TUI
follow the same rule: in a trusted project they land in the project config.

Project configuration never moves your state: sessions, credentials,
databases, logs and cache all stay under `~/.plak`.

## Presets you can copy

```sh
# The annotated reference: every key, every field, with its meaning inline.
cp -r configs/reference/. "$PLAK_HOME"

# The same four roles pinned to one provider each.
cp -r configs/providers/codex/. "$PLAK_HOME"    # or claude, grok
```

The Agents tab lists `codex-default`, `claude-default`, and `grok-default`
even when `~/.plak/profiles/` is empty. Choosing one writes `{name}-root`,
`{name}-worker`, `{name}-reviewer`, and `{name}-scout` under `~/.plak/agents/`
(a file you already have under one of those names is kept as it is) and points
the root and the working set at those files. With **Next session** that happens
right away; with **Fork now** it happens once the fork has landed, so a refused
fork changes nothing. Fork now branches the current session onto that profile,
briefs the new session as that profile with its own sub-agent cards, and
returns to the transcript; the previous session keeps its own cards. See
[the TUI guide](tui.md#agent-profiles).

A file you add at `~/.plak/profiles/<name>.toml` is listed after the built-ins.
It has exactly two keys:

```toml
root = "codex-high"
working_set = ["grok-worker", "grok-reviewer"]
```

The templates it names must already exist. A file with another key, a missing
template, or a built-in's name (`codex-default`, `claude-default`,
`grok-default`) is not listed; the Agents tab shows a line saying why. Copy into a **new, empty** `PLAK_HOME`:
copying over an existing home leaves your old templates beside the preset's,
and files written for an older grammar will fail the current loader.

## How long a turn may run

A turn has no time limit. It runs until the model answers, until you interrupt
it (Esc in the TUI, Ctrl-C for `plak run`), or until it reaches its round cap.
Every model call is one round, and polling a long build costs one round per
poll. At the cap, the model gets one more round with no tools and is told to
summarize what it did and what it could not finish. The default cap is 512
rounds:

```toml
# ~/.plak/config.toml
[turn]
max_model_rounds = 512   # 1..=1024
```

A change applies the next time a session attaches. A turn that is already
running keeps the cap it started with.

## When something is wrong

Plak refuses to start on a config file it did not fully understand, and the
message names the problem. The common ones:

- **`unknown config key: tui.reading_width`** — the key was deleted. Delete the
  line. The same goes for `tui.show_timestamps`.
- **`unknown config key: <key>; run plak config show for the valid keys`** — a
  typo, or a key that moved into a template file.
- **`[root] agent names no template file root-agent.toml`** — the pointer and
  the file disagree. Check `ls ~/.plak/agents/`; the file stem *is* the name.
- **`[roster] working_set template explorer needs a description`** — anything
  spawnable needs a card. Add a `description` line.
- **`template explorer may_adopt requires spawns naming another template`** —
  an agent cannot adopt work from a subtree it is not allowed to create. A
  list of only its own name does not count.
- **`template hub spawns helper, and no template file helper.toml exists`** —
  every name in `spawns` must be a template file with a `description`.
- **`templates are file-authored; edit ~/.plak/agents/explorer.toml`** —
  `plak config set` does not write template bodies, on purpose.
- **`[roster] working_set must be an array like ["a", "b"]`** — in the *file*,
  the working set is a TOML array of strings. The comma shorthand is a
  `config set` convenience only; a bare `working_set = a, b` in the file is not
  TOML at all and fails as a syntax error.
- **`[orchestration] max_depth must be an integer between 1 and 4`** — each
  orchestration bound is a closed range up to its built-in ceiling. A value
  outside it is an error naming the key and the ceiling, not a quiet cap.
- **`boolean config values must be true or false`** — booleans are TOML
  booleans. `1`, `yes`, `0` and `no` are not values, in the file or after
  `plak config set`.
- **`duplicate key (line 9, column 1)`** — the same key twice in one section.
  Plak will not guess which one you meant; delete the one you do not want.

Tool output also has a built-in per-round observation cap: about 8% of the
model's effective input window at four text bytes per token, bounded to
32-256 KiB and never exceeding available context headroom. This is automatic,
not a configuration key. Truncated process pages name their private output
file with `full output: <path>`, including foreground commands; the model can
read that file for details. Small untruncated foreground commands do not
create output files. File creation and retention are best-effort, as for
background job output.

Because the files are TOML, a `#` starts a comment everywhere except inside a
string, so `description = "fixes the #123 bug"` keeps its text. A description
with a quote or a newline in it is fine too — write it the way TOML spells it,
with `\"` or a `"""` multi-line string.
