# CLI reference

Everything below is what `plak help` and `plak <command> --help` print in the
binary you just built. Run those first if anything here looks stale.

## Shape of the grammar

```
plak                              open the TUI (new session in the cwd)
plak <command> [options]
plak help                         the command list
plak <command> --help             one command's help
```

A bare command that needs an argument prints that command's help instead of
failing — `plak run`, `plak login`, `plak key set`, `plak session show` — and
that is a successful exit, not an error. `plak version` prints, for example, `plak 0.0.1 (release; binary 0123456789ab)`.
The binary ID identifies the executable in `/reload` and handoffs.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Anything else, including an invalid `config.toml` |
| 2 | Usage or argument error, including a model, effort or composition the account does not offer, an unknown or ambiguous session prefix, a confirmation that needs `--yes`, an invalid config value, and `key status` for an OAuth account |
| 3 | Authentication failure: a credential that is not ready, an unaccepted or inactive subscription compatibility profile, or an unsafe credential store |
| 4 | Interrupted before completion, or stopped by `SIGTERM`/`SIGHUP` |
| 5 | Agent/runtime failure, or explicit recovery required |
| 6 | Run reached `--max-turns` or `--timeout` |

Every subscription failure ends with what to do about it, for example:

```
$ plak run --account codex --model gpt-5.6-luna --effort low -- 'hi'
plak: codex is signed out; run `plak login codex`
```

`plak doctor` exits 5 when any check fails. An unknown command exits 2:

```
$ plak nonesuch
plak: unknown command; run `plak help` for the command list
```

## Pipe versus terminal output

Table-shaped commands (`status`, `doctor`, `session list`, `key status`) render
column-aligned to a terminal and tab-delimited to a pipe, so
`plak status | cut -f4` is stable. There is no `--json` on those commands;
`plak session log --json` and `plak telemetry sql --json` are the two with an
explicit JSON mode.

`plak agent` writes machine-only JSONL on stdout; its diagnostics go to stderr.
Send `{"v":1,"kind":"background","id":"b1"}` to background foreground commands
already running. It is a no-op otherwise, does not cancel or steer, and never
affects later commands or nested code-cell calls. Unsolicited `job_started`,
`job_promoted` and `job_terminal` events identify jobs with `owner` and `job_id`
(`p<N>`), without a command `id`. Terminal events include status, elapsed time
and the bounded output tail; they do not consume the model's completion notice.
The protocol version remains 1.

`plak run` never lets model or tool text drive your terminal: escape and other
control sequences in stderr lines, and in the answer when stdout is a
terminal, are shown as `�`, while newlines and tabs are kept. Piped stdout is
the answer byte for byte, so `plak run ... > answer.txt` keeps exactly what the
model wrote.

---

## Accounts and credentials

### `plak login <codex|grok|claude> [--remote]`

Interactive browser OAuth for a subscription account. By default the browser
opens on **this** machine. Codex waits for the loopback callback on its own — do
not paste the localhost URL back; it reminds at 30s and times out at 120s, and
`c` re-copies the URL while it waits. Grok and Claude open a browser and then
ask you to paste a code from the callback page.

`--remote` is for SSH and headless hosts. For Codex it is a device-code login:
Plak prints a verification URL and a one-time code, you open the URL in a
browser on any device and enter the code, and Plak finishes on its own (nothing
is pasted back; Ctrl-C cancels; the code expires after 15 minutes). Continue
only if you started that login yourself; if someone gave you a code, cancel.
If the provider does not offer device-code login for your account, run
`plak login codex` on a machine with a browser, or forward the callback port
(`ssh -L 1455:localhost:1455 <host>`) and run `plak login codex` on the host.
For Grok and Claude, Plak prints the URL and takes the pasted code or callback
URL. An SSH session refuses a local login outright and tells you to re-run
with `--remote` rather than switching mode silently.

The SuperGrok account answers to `grok` and `supergrok`; bare `xai` is refused
as ambiguous. API keys use `plak key set` instead. Login is CLI-only; the TUI's Providers tab shows status
and names this command.

### `plak logout <codex|grok|claude> [--local]`

Retires the credential. A usable credential is revoked at the provider first.
A stored credential this build cannot use — quarantined, login required, from a
changed profile or a retired format, or under an inactive compiled profile — is
retired locally, and the output says the remote grant may remain active. With
nothing stored, logout says there is nothing to log out. `--local` never
contacts the provider: it retires the local copy without a remote revoke, for
example when the provider cannot be reached. Claude OAuth has no pinned remote
revoke, so its logout is always local.
If a stored login was written by an older build or under another native
profile, `plak status` says so and names `plak login <account>`. That login
replaces the old record itself, says so in one line, and authorizes a fresh
grant (the old grant may remain active at the provider).
If a newer plak build wrote the stored login, this build does not use or
change it. `plak login <account>` first asks
`This login was written by a newer plak build; replacing it means that build must log in again. Continue? [y/N]`;
anything but `y` leaves it untouched, and without a terminal the command
refuses without asking. Prefer running the newer build.

### `plak refresh <codex|grok|claude>`

Claims a new refresh grant now, even when the access token is still fresh, and
prints the new access-token expiry. Ordinary commands refresh on their own.

A subscription refresh that fails before sending names the network phase (DNS,
connect or TLS) and leaves the credential unchanged. Retry when online; an
unknown refresh outcome still requires credential recovery rather than replay.

### `plak status [account]`

Local credential readiness, with no provider request and no secrets. Both
status forms warn when any compiled provider review expires within 14 days,
or is already expired, naming the surface and UTC date even if that account
is not configured. Install a newly reviewed build before expiry; the warning
does not extend a route's lifetime. Bare `plak status` groups every account:

```
$ plak status
Ready

PROVIDER            KIND     PROFILE   CREDENTIAL
Claude Subscription OAuth    Accepted  Ready
Codex Subscription  OAuth    Accepted  Ready
Grok Subscription   OAuth    Accepted  Ready
OpenRouter API      API key  —         Key file

Setup needed

PROVIDER            KIND     PROFILE   CREDENTIAL
Anthropic API       API key  —         Missing
OpenAI API          API key  —         Missing
xAI API             API key  —         Missing

Subscription compatibility profiles
PROVIDER            REVIEW VALID UNTIL (UTC)
Codex Subscription  …
Grok Subscription   …
Claude Subscription …
An expired profile requires a newly reviewed build; logging in again does not renew it.

OAuth credentials live in <XDG_STATE_HOME>/plak/subscriptions, outside the product home, so emptying it does not log you out. API keys live in <PLAK_HOME>/secrets.
Local credential state only; no provider request was made.
```

Naming an OAuth account prints one line with compatibility, credential state
and a repair when needed. `plak status grok` reports the SuperGrok subscription;
`plak status xai-api` reports the API key. Bare `xai` is ambiguous and refused.

A section with no rows is omitted. When every account is ready there is no
Setup needed heading, and when none are there is no Ready heading.

A provider whose local credential cannot be read is still listed, under Setup
needed, and bare `plak status` still exits 0. CREDENTIAL then names the state
and the repair, for example `Unsafe storage · …` with the chmod to run. A
login this build cannot use is a normal row naming its repair:
`Profile changed · run plak login claude` for a Grok or Claude login from an
older build or another profile, `Login required · plak login codex` for an
earlier-build Codex login. A repair too long
for the column is `Unsafe storage · run plak status <account> for the fix`;
that command prints the full path and chmod. The other accounts,
including API keys, still print. `plak status <account>` still exits 3 when
that one account cannot be read. The four columns stay put, so
`plak status | cut -f4` is still the credential cell.

### `plak key set|clear|status <openai|xai-api|anthropic|openrouter>`

`set` prompts for the key and writes it to the secrets store; `clear` removes
the file; `status` reports presence without printing the key. Environment
variables override files, `plak key clear` cannot unset a process environment
variable, and Plak never loads a `.env` file. Key entry is CLI-only.

### `plak secret set|rm <name>`, `plak secret list`

Named secrets that `config.toml` refers to by name — an MCP server's
`secret_env`, `bearer_secret` or `secret_headers`. `set` prompts with hidden
input (the value is never an argument) and writes
`<PLAK_HOME>/secrets/named/<name>.secret`, mode 0600; `rm` removes it; `list`
prints names only. A name matches `[a-z][a-z0-9-]{0,31}`. There is no
environment override.

### `plak mcp add|list|show|test|login|logout|prompts|remove|enable|disable`

MCP server setup without editing `config.toml` by hand.

```sh
plak mcp add everything -- npx -y @modelcontextprotocol/server-everything
plak secret set github                        # the token; never an argument
plak mcp add github --url https://api.githubcopilot.com/mcp/readonly --bearer-secret github
plak mcp add ghlocal --secret-env GITHUB_PERSONAL_ACCESS_TOKEN=github \
  -- docker run -i --rm -e GITHUB_PERSONAL_ACCESS_TOKEN ghcr.io/github/github-mcp-server stdio --read-only
plak mcp list
plak mcp show github
plak mcp test github
plak mcp add linear --url https://mcp.linear.app/mcp --oauth
plak mcp login linear                                  # or --remote over SSH
plak mcp logout linear
plak mcp prompts github                                # the server's prompts
plak mcp prompts github review --arg target=main       # what one expands to
plak mcp enable github --template root-agent             # every allowed tool
plak mcp enable github --template root-agent --tool get_me   # or one at a time
plak mcp disable github --template root-agent
plak mcp remove github
```

- `add <id> [options] -- <command> [args...]` adds a stdio server;
  `add <id> --url <url>` an HTTP one. Options: `--scope user|project` (default
  `user`; `project` writes the trusted project's `.plak/config.toml`),
  `--env KEY=VALUE`, `--secret-env KEY=SECRET_NAME` (stdio), `--bearer-secret
  SECRET_NAME`, `--header NAME=VALUE`, `--header-secret NAME=SECRET_NAME` (http),
  `--oauth` with `--oauth-issuer <url>`, `--oauth-client-id <id>`,
  `--oauth-client-secret SECRET_NAME` and `--oauth-callback-port <port>` (an
  http server that you log in to instead of giving a bearer secret),
  `--tools a,b` (the server allowlist), `--resources` (give templates the
  server's `read_resource` tool), `--startup-seconds`, `--call-seconds`
  and `--replace` to overwrite an existing id. The table is written in place,
  keeping the rest of the file, and anything a config load would refuse is
  refused before the file changes. Secrets are named, never given; `add` tells
  you which are not stored yet.
- `list` prints each server's transport, scope and target, the templates that
  enable it, the secrets it names that are not stored, whether resources are
  enabled, and for an OAuth server whether you are logged in (with the access
  token's remaining lifetime and scope), read without contacting the server.
  `show <id>` prints the full table as a session freezes it, plus the same
  facts.
- `test <id>` starts or connects to the server the way a session does (same
  spawn, environment, secret and login path), lists the tools `mcp:<id>` would
  bind (with `read_resource` when resources are enabled) and the ones left out
  with the reason, with resources enabled says how many resources and resource
  templates the server lists, then stops it. A failure exits non-zero with the
  cause. It calls no model.
- `login <id> [--scope "<scopes>"] [--remote]` logs in to a server added with
  `--oauth`. It finds the server's authorization server, prints it and the
  scope it will request, and opens the authorize URL in your browser; the
  browser returns to `http://127.0.0.1:<port>/callback` (the table's
  `oauth_callback_port`, else a free port). `--scope` requests these scopes
  instead of the discovered ones, which is how you grant more when a tool
  call says a scope is missing. `--remote` is for a browser on another
  device, as with `plak login --remote`: when the authorization server offers
  the device grant you enter a one-time code there, otherwise you paste back
  the URL the browser was sent to. A refused device request prints its HTTP
  status and says it is falling back to paste-back. An advertised device endpoint
  skipped by policy prints a policy-skip notice and uses paste-back without
  sending a device request. Over SSH, `login` without `--remote` is
  refused with that hint. The login is stored under
  `<PLAK_HOME>/secrets/mcp-oauth/<id>/` and used only for that server's URL.
  A session never starts a login: when the login expires and cannot be
  renewed, is revoked, or lacks a scope, the tool call fails and names the
  `plak mcp login` command to run.
- `logout <id> [--local]` revokes the login at the authorization server and
  removes it here. When a dispatched revocation fails (including HTTP 503), the login is
  quarantined because the server may already have changed the grant; calls
  refuse it until a new login or local logout. No record is removed; `--local`
  removes it here without revoking, and the grant may stay active at the
  authorization server.
- `prompts <id>` starts the server the way `test` does and lists its prompts
  with their arguments. `prompts <id> <prompt> [--arg NAME=VALUE]...` gets one
  prompt and prints the single user message it expands to: a first line
  naming the prompt and marking its content as coming from an untrusted
  server, then each of the server's messages under a `user:` or `assistant:`
  label. A required argument that is missing, or one the prompt does not
  take, is refused before the server is asked. It calls no model; only you
  invoke prompts, never the model.
- `enable <id> --template <name> [--tool <t>]...` adds `mcp:<id>` (or one
  `mcp:<id>/<t>` per `--tool`) to that template file's `tools`; `disable`
  removes every entry for the server. Only the `tools` line changes. New
  sessions pick it up; a session that already exists keeps its tools.
- `remove <id>` refuses while a template would be left listing a server or
  tool nothing serves; `--force` removes those entries first.
- Every edit is checked against your home configuration alone and with the
  current trusted project over it, so a home template cannot list a server
  that only a project defines. Only your home's templates and the current
  project's are checked; another project's templates are not seen.

---

### Subscription compatibility review deadlines

The first time you use a subscription account — `plak login`, `plak refresh`,
`plak logout <account>`, `plak models` or `plak run` — and again whenever the build's profile revision
changes, Plak prints a notice on stderr and asks once:

```
codex uses an experimental revision-pinned compatibility integration.
profile: <profile id>
reviewed: 2026-10-21 UTC
expires: 2026-11-20 UTC; after that this build refuses codex until a newer build replaces the profile
Continue? [y/N]
```

Answer `y`. The answer is read from stdin, so a non-interactive
`plak run` accepts it with `echo y | plak run --account codex ... -- '<prompt>'`
(the prompt itself follows `--`), or you accept it once beforehand with
`plak login <account>`. With stdin closed (`</dev/null`, a CI job) `plak run`
refuses with exit 3 and names both ways (`plak login` itself asks for the
answer on a terminal or a pipe); any other answer declines, exits 3 and sends
nothing.

When a newer build renews a profile's review, `plak status` shows the
acceptance as required while the credential stays ready. Accepting the renewed
notice (for example through `plak models <account>`) keeps the existing login;
no new `plak login` is needed.

`plak status` lists the UTC expiry of each compiled subscription compatibility
profile. These dates govern this build's integration review, independently of
credential expiry. Once a profile expires, obtain a build whose profile was
renewed after native client protocol review and regression testing. Logging in
again cannot renew a compiled profile. Maintainers must not extend dates without
that review. OpenRouter's static dialect review provenance is separate from the
fresh endpoint/model authority used for request admission.

## Running an agent

### `plak run`

Run until the task is idle. Finished background processes and child results
or questions trigger distinct continuation turns; running work alone waits
without a turn. The prompt goes after `--`.

```
plak run --account codex --model gpt-5.6-luna --effort low -- 'summarise src/'
plak run --account grok --model grok-4.6 --effort low -- 'run the tests'
plak run --account claude --model claude-sonnet-5 --effort low --composition js-ptc -- 'fix the lint'
plak run --session 0f2c… -- 'and now update the changelog'
plak run --account codex --model gpt-5.6-luna --effort low --image shot.png -- 'what is wrong here?'
```

| Flag | Meaning |
| --- | --- |
| `--account <id>` | `codex`, `grok`, `claude`, `openrouter`, `openai`, `anthropic` or `xai-api`. `--provider` is an accepted alias. |
| `--model <exact-id>` | An exact catalog model id. No prefixes, no inference, no fuzzy match. |
| `--effort <level>` | `none`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`. Required for a reasoning model, refused for the rest. `none` is explicit; omitting the flag is not `none`. There is no default; the catalog row decides. |
| `--composition <c>` | `direct`, `plakous-default`, `native-ptc`, `js-ptc`, `py-ptc`. `native-ptc` is the vertical's own programmatic tool calling; today only Codex Code Mode is enrolled (Codex `CodeModeOnly` rows and OpenAI API models enrolled as `CodeModeOnly`), and elsewhere it is refused naming the vertical. `js-ptc` and `py-ptc` are harness PTC on every account. `plakous-default` (the default when omitted) is the per-model recommendation: `native-ptc` where Codex Code Mode is enrolled, `direct` on Grok, Claude, OpenRouter and the Anthropic and xAI API accounts. |
| `--protocol <p>` | `auto`, `chat`, `responses`, `messages`. OpenRouter only, default `auto` → `chat`. Other accounts reject it at parse time. |
| `--session <id>` | Continue an idle session: the full UUID or the short id `plak resume` accepts, resolved among this directory's sessions. Its recorded binding wins, so `--account`, `--model`, `--effort`, `--composition` and `--protocol` are refused alongside it. |
| `--resume-tree` | Resume a `RestoredPaused` topology. Requires `--session`. |
| `--max-turns <n>` | Admitted turns including the first, 1..=100; default 25. |
| `--timeout <seconds>` | Whole-run wall clock, including authentication/catalog preparation and idle waiting; default none. |
| `--print-session-id` | Print `session_id=<uuid>` on stdout before the turn starts. |
| `--image <path>` | Attach an image to the prompt; repeat for more (at most 8). png, jpeg, gif or webp, detected from the bytes; an image over 2048 px on its long edge or 3.75 MiB is downscaled to fit (PNG when that fits, otherwise JPEG) and reported on stderr as `plak: attached [image: shot.png 2048x1324 1.9 MB, downscaled from 3456x2234]`; smaller ones are sent unchanged. With images, the prompt after `--` may be left out. A file that cannot be read, is not a decodable image or is over 64 MiB is refused (exit 2) naming the file. A model without image input refuses the run naming the route and model. |

`plak run` needs a ready OAuth login or key; an API account with no key exits 3
naming `plak key set <account>` and its environment variable, and a signed-out
subscription exits 3 naming `plak login <account>`. An invalid `config.toml`
refuses the run (exit 1) before anything else happens. It uses full
coding tools with no approval gates. `--session` reopens the recorded workspace locator, so your cwd
must match the one the session was created in.

Only the last turn answer is written to stdout, once after release, with
separate text blocks of the answer split by a blank line and a final line break;
terminal tool results continue on stderr. Set `PLAK_VERBOSE=1` to include
per-tool progress as well. An interrupted turn prints its recorded partial
answer and exits with code 4. `--print-session-id` still prints before the turn.

Waiting and continuation summaries go to stderr. A bound stops jobs,
interrupts children resumably, and exits 6. Ctrl-C does the same and exits 4.
Stopped root and live-child background processes keep their recorded stop
reason when you reopen the session.
An already-retired or busy child's control channel does not change exit 6/4;
actual terminal-recording failures are still reported as cleanup errors.
`turn.auto_continue` applies only to interactive idle wakes, not this command.

Model-launched process tools run with your full host authority on both
Linux x86_64 musl and macOS; there is no OS sandbox. See
[known limits and security model](known-limits-and-security-model.md).
`native-ptc` and `js-ptc` locate `PLAK_JS_HOST` or a
`plakous-js-host` beside the binary; `py-ptc` locates `PLAK_MONTY_WORKER` or a
sibling `monty` worker. That is worker isolation, not workspace isolation, and
it does not isolate Plak itself. Unsupported process-tool targets, including
Intel macOS, fail closed.

### `plak agent`

The same host loop the TUI drives, speaking newline-delimited JSON (`v=1`) on
stdin and stdout. Stdio is the pipe; there is no `plak agent stdio` subcommand.
Binding flags match `plak run` (`--account`, `--model`, `--effort`,
`--composition`, `--protocol`, `--session`, `--resume-tree`); an omitted flag
falls back to the `[root] agent` template's value, and `plak agent` refuses to
start when neither supplies an account, a model, or a required effort. A model
that is not a row of the account's catalog is refused before anything attaches,
naming the account, the catalog and the `plak models` command that lists it
(OpenRouter model ids are left to OpenRouter):

```
model `gpt-5.6-luna` is not in the SuperGrok subscription catalog of account `grok`; run `plak models grok` to list the models it offers
```

```
plak agent --account codex --model gpt-5.6-luna --effort low
plak agent --session 0f2c… --resume-tree
printf '{"v":1,"kind":"prompt","id":"1","text":"hi"}\n' | plak agent --account grok --model grok-4.6 --effort low
```

Stdin commands: `prompt`, `interrupt`, `compact`, `resume_queue`, `resume_tree`, `tree`,
`interrupt_node`, `close_node`, `stop_all`, `clear`,
`invoke_tool`, `fork`, `recover`, `background`, `quit`.
A `prompt` may carry `"images": ["shot.png", "~/..."]`: paths to png, jpeg,
gif or webp files (relative ones resolve against the agent's working
directory; `~` is not expanded), read with the same limits as `plak run
--image`. With images, `text` may be omitted.
`quit` gives an active turn 300 ms to settle cancellation and another 100 ms
to publish its result; a provider that ignores cancellation cannot hold the
host open until its response arrives. Unfinished work is recovered on reopen.
Idle or settled quits join child cleanup and report any cleanup failure.
Closing the control stream uses the same quit grace. Final projection stages
share one second from quit start; any remaining projection debt is applied on
the next open.

Stdout events: `hello`, `ready`, `status`, `busy`, `assistant`,
`assistant_done`, `tool`, `error`, `turn`, `tree`, `queue`, `bye`.

Each command's answer (`error`, a `status` note) carries that command's `id`,
even when commands are piped back to back; a manual `compact` always answers
with a note naming its outcome. Ctrl-C (`SIGINT`) interrupts a running turn and
exits 4 after `error` and `bye`; idle, it quits with exit 0.

See [agents and orchestration](agents-and-orchestration.md) for a worked
session.

### `plak resume [uuid]`

Opens a prior session in the TUI. With no argument you get a picker scoped to
the current directory; with an id you get that session — the full UUID, or the
short id Plak prints when you quit. `plak --resume [uuid]` is the same command.

---

## Inspecting things

### `plak doctor`

Local health checks. Everything is offline except one OpenRouter probe: when a
key and a default model are both set, Plak probes catalog keys with discovery
GETs only, never inference. Compiled provider reviews within 14 days of expiry
(or already expired) produce local warnings naming the surface and UTC date.
They count in the warning total; warnings alone do not change the exit code.

```
$ plak doctor
Plak doctor

CHECK              RESULT   DETAIL                                        FIX
Version            OK       available · 0.0.1                             —
Home               OK       available · /home/you/.plak                   —
Config             OK       valid · /home/you/.plak/config.toml           —
Product db         OK       available · /home/you/.plak/state/r25.e27.p33.t8/product.sqlite3   —
Runtime db         OK       available · /home/you/.plak/state/r25.e27.p33.t8/runtime.sqlite3   —
Runtime store      OK       available · checkpoints 412/524288 (new work stops at 458752), …   —
Telemetry db       OK       ok · 1189 rows                                —
Secrets dir        OK       available · /home/you/.plak/secrets           —
Sessions dir       OK       available · /home/you/.plak/state/r25.e27.p33.t8/sessions          —
Cwd                OK       available · /home/you/project                 —
Composition host   OK       included · not required by the selected …     —
Root template      OK       unset · no [root] agent; new sessions …       —
Spawn templates    OK       none · [roster] working_set is empty          —
Oauth codex        OK       ready · generation 13 · record 22h old …      —

Result: 21 passed · 0 warnings · 0 failures
Provider credentials: run `plak status`
```

The same 21 checks are always emitted. Every failing row carries its own FIX
column. `Runtime store` is an ERROR (`full`) once new work is refused for lack
of space and a WARNING (`nearly_full`) shortly before; its fix is
`plak session gc --all`, then `plak session delete <id>`. The row also shows
the database file size with its free bytes, and the change feed's retained
rows and their bytes (informational: feed rows never count toward the store
being full). `doctor` exits 5 when any row is ERROR; warnings alone do not
fail it.

### `plak usage [--account codex|grok|claude|openrouter|supergrok] [--session <uuid>]`

Usage, status, and the TUI Usage/Providers pages show the last failed credential
refresh when telemetry has it, with its local `HH:MM`, cause, and repair action.
A successful refresh clears that notice; starting another refresh does not.
`credential unchanged` means the old credential was restored, not an unknown
refresh outcome. These notices are display-only and never refresh a token or
change credential readiness. Without readable telemetry, ordinary status text
remains.

Subscription plan limits and, with `--session`, the context status of that exact
attached runtime. Without `--session` the plan limits come from the provider
dashboard. Plak never estimates tokens or model capacity, so a fact it does not
have is reported as absent rather than guessed. `--account` queries and prints
only that account; other accounts are not falsely shown as disconnected.
`--account` and `--session` conflict.

For this command, `grok` and `supergrok` select the same SuperGrok
subscription usage vertical. `openrouter` selects OpenRouter usage.

### `plak models <codex|grok|claude>`

Reads the authenticated catalog for one account and lists its models. Requires a
ready login; this is the command that tells you the exact `--model` ids
`plak run` will accept. One tab-separated line per model:

```
<id>	visibility=…	tool_mode=…	streaming=…	tools=…	reasoning=…	efforts=…	context_length=…
```

`reasoning=true` is the flag that decides whether `--effort` is required.
`tool_mode` is `n/a` when the catalog reports none, and `context_length` is
`unknown` when the provider did not report one.

### `plak models openrouter`

Reads the OpenRouter catalog using your configured OpenRouter API key and
prints one exact model ID per line. This only discovers identifiers: listing
does not establish capabilities, admission, or qualification. It makes no
inference request.

### `plak models <openai|anthropic|xai-api>`

Lists the provider's own model list with the stored API key (cached six hours;
offline, the last cache, else the table built into Plak; see
[providers and accounts](providers-and-accounts.md)). It makes no inference
request. A header line names the source (`live list`, `cached list, N min old`
or `bundled table (reason)`), then one tab-separated line per model:

```
<id>	<status>	efforts=…	window=…	full_window=…	max_output=…	composition=…	plakous-default=…
```

`<status>` is `usable`, `needs review`, `not a coding model` or `retired`. Only
a `usable` row runs; it lists its compositions and what `plakous-default`
picks. Every other row ends in `reason=…` instead. `efforts` is that model's
own ladder, `window` the working window, `full_window` the model's full input
window and `max_output` its output bound.

### `plak state prune-legacy [--yes] [--forget-missing-launches] [--prune-orphaned-references]`

`plak doctor` lists non-current state paths and sizes. Pruning lists the same
inventory and asks for confirmation; non-terminal stdin requires `--yes`.
It refuses unsafe paths, in-use locks and inventory changes. Current state,
carried-forward references, dirty copies and unadopted work are retained.
`--forget-missing-launches` forgets ownership only for launch directories that
no longer exist; it deletes no checkout. `--prune-orphaned-references` removes
private retained references not used by current sessions or their sources.
Safe independent entries may be removed while protected entries are kept.
Review `kept:` reasons before manually removing anything. Export files under
`carry/` are never selected. Implementation details live in spec 02 §2.5.

### `plak session list|show|log|recover|export|import|reference|delete|gc`

```
plak session list
plak session show <uuid>[.cN]
plak session log <uuid>[.cN] [--kind tool] [--tail N] [--json]
plak session recover <uuid>
plak session export <uuid> [--out <path>]
plak session import <path>
plak session reference <path-or-session> [--search <text>] [--tail N]
plak session delete <uuid> [--yes] [--abandon-children]
plak session gc <uuid> | --all
```

Every session id here may be the full id or an unambiguous prefix of at least
four characters among this workspace's sessions, as `plak resume` and the
exit line print it. For `show` and `log`, append `.cN` to select child `cN` in that root session's
agent tree, for example `plak session log <uuid>.c1`. These are read-only child
views; other session commands do not accept child selectors. `log <uuid>.cN`
is that child's committed history, not the root event log. A committed
compaction is a row of kind `compaction` (`compacted N earlier segments
(<pass>)` when the checkpoint recorded the pass); the summary itself is not
shown. `--kind compaction` selects those rows.

`show` labels a Working checkpoint `working (live owner present)` while the
runtime holder lock is held, and `recovery required` when it is unowned. Both
show the phase and recovery obligations; those details do not mean a live turn
needs recovery. Ownership and the head are separately observed, read-only facts.

`recover` reopens a session whose last turn was interrupted — a crash, a
killed process, or a turn that ended `recovery required` — records once what
that turn left unobserved, and releases it again. It never re-sends a model
request or re-runs a tool. It exits 0, printing the one line that says what was
recovered, or `nothing to recover`. You rarely need it: the next `plak run
--session` or TUI prompt does the same thing on its own.

`export` and `import` link a session to another plak build, one whose
stored format differs (each build keeps its state in its own
`~/.plak/state/<format key>/`). `export` writes a portable JSON document
(`"format": "plak-session-export"`), mode 0600, to `--out` or to
`~/.plak/carry/`, and prints its path. It refuses, naming the cause, while a
turn is running or parked, a sub-agent is still live or sealing (let it finish
or close it first; interrupted, recovery-required and finished sub-agents are
retained only as inert source facts), a queued prompt's
delivery is unresolved. `import` creates a new session from the document,
prints `session <new> linked from <old> (<old key>, <producer>)`, and the new
session continues from the full conversation, including tool exchanges and inline
image bytes. It does not restore live tools, processes, topology, children,
usage or prompt caches. Queued prompts stay paused; workflow facts stay
reference-only. The first request rebuilds the prompt cache. File import strips provider replay regardless of the document's declared
binding. Only a locally produced `/reload` handoff may retain replay, on the
identical account/model and after offline destination decoding.
Before creating the successor, import warns if the estimated history exceeds
the destination route's admitted input window, naming that route/window and
warning that the first prompt will refuse unless history compacts.
A smaller destination route schedules compaction at the first prompt when
needed; if its protected history or summary input still cannot fit, it returns
the normal context-limit refusal rather than silently truncating conversation.
An unknown destination window is reported but does not block carry.
This build writes version 8 and imports versions 8 and 7. Version 7 has only its
old bounded checkpoint, not a complete conversation; its archive remains
available through `session reference <path-or-session> [--search <text>]
[--tail N]` as untrusted reference data. New exports have no separate archive. Keep exports private: they contain full
conversation/tool contents, not a redacted transcript.
Documents larger than 256 MiB, histories larger than 128 MiB/100,000 messages,
and malformed content are refused, not truncated. A document already imported
into this workspace is refused, naming its existing successor.
`session list` and `session show` name the lineage link. A build imports its
own document version and the immediately previous one.

`delete` removes a session and its retained `~/.plak/references/<id>.json` archive,
plus all of its retained history, together with the
history of its finished sub-agents and its agent tree, and prints
`deleted session <id>`. It is how you free space when the session store is
full; `/clear`, compaction and `/recover` do not. It asks first unless `--yes`
is given, and refuses while the session is open in a running plak (or its
process died less than half a minute ago), while its agent tree has a
sub-agent that is still running or resumable, or one that needs recovery
(resolve it with `recover_agent` first, or explicitly abandon that obligation
with `--abandon-children`), or while other sessions continue from
it or share its history (forks, rebinds, `/clear` targets); the
refusal names them. These checks run before the question and again after it,
and nothing is removed until the session itself has been closed for deletion.
`--abandon-children` exempts only recovery-required children in this session's
own tree. It does not bypass live child leases, a live root, or fork dependencies.
A child closed before a late session-cleanup fault still needs recovery and is
listed as `recovery_required`. Recover it or use `--abandon-children`; recovery
keeps it closed and does not send another result to its parent.
If a delete fails part way, run it again: it finishes what is left. A session
carried forward with `import` does not block deleting the one it came from.

A physical disk-full error is separate from the store's retention limit:
`plak session gc --all`, deleting unused sessions, or freeing disk space are
the remedies. The retention reserve is logical, not reserved disk blocks;
reclamation may need some disk space before it can commit. A prompt rejected
before durable admission says `prompt was not admitted`, remains queued for
editing or explicit resubmission, and does not pause or recovery-fence the host.

`gc` drains deferred object reclamation in bounded transactions after pruning
superseded checkpoints. Automatic pruning and deletion enqueue this work without
waiting for the object sweep. Its object/byte totals count its explicit drain.
`gc` reclaims checkpoint history a session no longer needs: the
intermediate checkpoints every turn leaves behind. The conversation, resume,
forks, export and `session show` are unchanged, and it works even when the
session store is full. It prints `reclaimed <n> checkpoints, <n> objects, <n>
bytes (<n> sessions pruned, <n> interrupted deletes finished)`, then one
`skipped <id>: <reason>` line for each session it left alone: one open in a
running plak, one with an unfinished turn (run `plak session recover` first),
or one with a workflow (`/compact`, `/fork`, a model change) that a crash cut
short before it started (`workflow pending`: open the session, or run `plak
session recover`, first). With `--all` it covers
every plak session (product record) in this home, not just this workspace, and
also finishes deletes that a crash interrupted between archiving and deleting;
sub-agent runtime sessions are pruned by their own turns. Turns reclaim their
own history automatically once enough builds up, so `gc` matters mostly when
the store is full or for an imported session that arrived with long history. A
finished sub-agent's session is an ordinary session; nothing deletes it
automatically.

`list` is scoped to the current workspace:

```
$ plak session list
Sessions in /home/you/project

SESSION ID                            UPDATED               PROVIDER            MODEL          EVENTS  TITLE
0a1b2c3d-0000-4000-8000-000000000000  2026-09-09T12:30:04Z  Codex Subscription  gpt-6-astra    0       (empty)
```

`log` prints the agent-readable event log, last 100 events by default.
`show` includes a **Canonical recovery (root session)** section. It names the
session's current checkpoint, the one an open would find.
A Working checkpoint lists its exact recovery phase and payload-free operation
identities, actions, and request digests. Viewing this report leaves the session
unchanged; it does not retry a child or clear an uncertain effect. A fresh
standalone session has its own tree and does not inherit unresolved children
from the old one. The **Topology children (durable snapshot)** section shows
each child's node and attempt status alongside its own canonical checkpoint
and obligations. A quiescent root or child checkpoint does not establish that
the child node retired; a missing tree is reported as uninspected.
`show` does not run state admission or create logs, telemetry, or an event-count
index. If the event log is busy, its count is shown as unavailable and the
recovery report still appears.
`--kind tool` filters a namespace (it includes `tool.started` and friends) and
`--json` emits raw JSONL with no headers. The files live at
`~/.plak/state/<format key>/sessions/<uuid>.events.jsonl`.

### `plak telemetry status|sql|prune`

A local analytics store; nothing leaves your machine.
```
$ plak telemetry status
Telemetry store
  Path           /home/you/.plak/telemetry/telemetry-v17.sqlite3
  Schema version 17
  Size on disk    507904 bytes
  Retention       536870912 bytes, 90 days
  Write errors    0
  Dropped         0
  WAL checkpoint busy  0
TABLE                 ROWS
turns                 42
model_attempts        58
tool_calls            21
…
```

`plak telemetry status --views` prints the recommended read-only SQL to paste
into `plak telemetry sql "<query>"` (add `--json` for machine output). The
`v_replay_loss` view lists retry and declined-retry correlation ids, proposed
next attempts, cause/phase, and the closed `declined_reason` when no replay
was dispatched. A declined `ModelRetry` has zero delay and does not discard
output; delay and `output_discarded` are event facts, not SQL columns. The view
lists assistant-text/thinking bytes and completed items from the failed
generation (discarded only when replayed), broken-attempt
elapsed time and replay class, never the discarded content. The SQL
is read-only and attaches `product.sqlite3` and `runtime.sqlite3` as `product`
and `runtime`.

`plak telemetry prune` deletes whole sessions, oldest first, so it refuses a
bare run: pass `--max-age-days N`, `--max-bytes N`, or `--yes` to prune to the
configured retention. Defaults are 512 MiB and 90 days, both settable under
`[telemetry]` in `config.toml`. The byte cap counts this schema's telemetry file
plus its `-wal` and `-shm` sidecars; prune checkpoints and truncates the WAL, and
says so if a reader prevented the truncate. Retention is best effort: pruning
stops when a reader prevents reclamation and retains sessions with unfinished
turns.

Each telemetry schema has its own file. Older files and the legacy
`telemetry.sqlite3` stay untouched; `plak doctor` reports them. Prune applies
only to the current schema. Removing older files remains an explicit action.

### `plak config show|set|edit|init|trust|untrust`

```
plak config show          # prints `config_path=<path>` on its first line
plak config set <key> <value> [--scope user|project]
plak config edit          # uses $VISUAL, then $EDITOR; supports shell arguments
plak config init          # creates a commented template that pins no defaults
plak config trust         # admits the discovered project .plak
plak config untrust       # revokes project trust
```

`[roster] working_set` and `[orchestration]` are config-only — there is no
roster editor in the TUI. Agent templates are one file each under
`~/.plak/agents/<name>.toml` (or a project's `.plak/agents/`).
`config edit` uses `vi` only at a terminal; headless use requires `VISUAL` or
`EDITOR`. Bare set aliases `mouse`, `theme` and `compaction_enabled` are refused;
use `tui.mouse`, `tui.theme` and `compaction.enabled`.
See [configuration](configuration.md).

## Maintainer commands

`just build` and `just build-release` compile with `--features maintainer`, so
your binary also has `plak fork`, `plak compact` and
`plak compatibility` under a `MAINTAINER:` heading in `plak help`. They are not
part of the product surface; `plak <cmd> --help` documents each.

Maintainer inference capture selects its private run only when inference starts.
Read-only commands and state maintenance that send no inference create no empty
wire-capture run.
