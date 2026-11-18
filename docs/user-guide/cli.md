# CLI reference

Everything below is what `plakd help` and `plakd <command> --help` print in the
binary you just built. Run those first if anything here looks stale.

## Shape of the grammar

```
plakd                              print help and exit 0
plakd <command> [options]
plakd help                         the command list
plakd <command> --help             one command's help
```

A bare command that needs an argument prints that command's help instead of
failing -- `plakd run`, `plakd login`, `plakd key set`, `plakd session show` -- and
that is a successful exit, not an error. `plakd version` prints, for example, `plakd 0.0.4 (release; binary 0123456789ab)`.
The binary ID identifies the executable in build checks and host upgrades.

`plakd wire schema` prints this build's wire v2 JSON Schema, including the exact
protocol version and binary id. It is a supported release command; see the
[integration guide](integrating.md) for transports and protocol frame examples.

Placement is selected through the host wire operations, not top-level launch
flags. The host validates runner/workspace choices before session creation.

`plakd run` and `plakd agent` accept repeatable `--account <kind>=<ref>`
(`--account claude=claude/work`) in addition to one bare `--account` for the
root. A template names a provider kind (`claude`), never a named account
(`claude/work`); a named reference in a template file is a load error.
Without a choice a kind takes its default slot when that is ready, else its
only ready account; with several ready accounts and no choice the command is
refused with `account_choice_required`, naming the kind and its accounts.
Interactive clients read each kind's accounts and readiness from session
options and offer a picker when a used kind has more than one ready account.
"Continue on <account>" after a usage limit rebinds the root onto that
account and submits the failed prompt once more (the transcript then shows
it twice); a non-root kind only updates that session's choice. The root
cannot be switched while it is busy.

## A persistent host and the stdio bridge

`plakd host` serves the home's sessions in the foreground. Host/web serving is
part of the default build; `--web` adds the web endpoint and `--ui` its validated
static build. Run the host under its user service or a multiplexer. Stop/restart
it through that service, or send SIGTERM for planned handoff. Only one host may
serve a home, even across different state keys; use an explicit stop or
coordinated upgrade rather than silently replacing an older running build.

Programmatic clients can use `plakd host connect --stdio`. Stdout carries only
wire frames; stderr carries connection diagnostics. The client supplies the
handshake (host-control wire version 11, reported by `plakd wire schema` and
distinct from `plakd agent`'s JSONL
version 1). Closing stdin detaches that client without stopping host sessions.
An absent or incompatible host refuses; there is no embedded fallback and no
automatic replay after a broken connection.

The explicit local `--auto-start` option may start an absent unmanaged host.
It refuses a remote-shell start and an installed user service (Linux
`plakd-host.service` or macOS `com.plakd.host.plist`), naming the service-start
command instead. The detached host uses the public home as cwd,
has no implicit startup workspace, and writes a private bounded
`host/auto-start.log`. It exits after five minutes without connections or kept
work; ordinary foreground and unit hosts persist. Auto-start uses a curated
OS/tool environment, not the caller's API-key variables. Configure stored keys
with `plakd key set`, or set variables in the environment of an explicitly
started host/unit. Missing-key diagnostics name the resolving process.
`key set`/`key clear` themselves still run locally and obey their own process's
environment-first precedence; the bridge does not perform target-host writes.

Bare `plakd` prints help and exits 0 without starting a host or session.

`plakd host install --web unix:<absolute-socket> --ui <absolute-directory>
[--dry-run]` writes a Linux systemd user unit or a macOS per-user LaunchAgent.
Linux installation reloads the user manager; macOS installation never invokes
`launchctl`. Neither enables or starts the service. Follow the printed
commands; see [service setup](host-service.md#install).

`plakd host upgrade --expect-binary <id> [--wait <seconds> | --drain <duration|idle>]`
runs from the destination build. Same-key upgrades drain roots and children at
joined runtime boundaries before handing off, with a 60-second natural-drain
budget by default; `--drain` changes that budget. Cross-key upgrades carry
settled roots into new successor sessions. The separate settling wait defaults
to 0 and accepts at most 3600 seconds; explicit `--drain` and `--wait` cannot
be combined. Before surrender, a refusal leaves the old owner
authoritative; after surrender, recover through the destination's receipts,
never by reopening the old owner. Retry the same destination command after an
unknown outcome. `plakd host upgrade --abandon <successor-uuid-or-runner-id>`
irreversibly abandons pending remote successors only. Read
[upgrade, reload and logs](host-service.md#upgrade-reload-and-logs) before
changing a host or its paired runners.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Anything else, including an invalid `config.toml` |
| 2 | Usage or argument error, including a model, effort or composition the account does not offer, an unknown or ambiguous session prefix, a confirmation that needs `--yes`, an invalid config value, and `key status` for an OAuth account |
| 3 | Authentication failure: a credential that is not ready, an unaccepted or inactive subscription compatibility profile, or an unsafe credential store |
| 4 | Interrupted before completion, or stopped by `SIGTERM`/`SIGHUP` |
| 5 | Agent/runtime failure, an answer truncated at its output limit (`turn_failed:output-limit`; the answer is still printed), or explicit recovery required |
| 6 | Run reached `--max-turns` or `--timeout` |
| 7 | `tree_hosted_elsewhere`: another running `plakd` holds the session's agent tree (see `plakd tree release`) |
| 130 | Quit escalated because the turn's terminal did not join and the host abandoned by lease only; clean idle or terminal-joined quit remains 0 |

Every subscription failure ends with what to do about it, for example:

```
$ plakd run --account codex --model gpt-5.6-luna --effort low -- 'hi'
plakd: codex is signed out; run `plakd login codex`
```

`plakd doctor` exits 5 when any check fails. An unknown command exits 2:

```
$ plakd nonesuch
plakd: unknown command; run `plakd help` for the command list
```

## Pipe versus terminal output

Table-shaped commands (`status`, `doctor`, `session list`, `key status`) render
column-aligned to a terminal and tab-delimited to a pipe, so
`plakd status | cut -f4` is stable. There is no `--json` on those commands;
`plakd session log --json` and `plakd telemetry sql --json` are the two with an
explicit JSON mode.

`plakd agent` writes machine-only JSONL on stdout; its diagnostics go to stderr.
Send `{"v":1,"kind":"background","id":"b1"}` to background foreground commands
already running. It is a no-op otherwise, does not cancel or steer, and never
affects later commands or nested code-cell calls. Unsolicited `job_started`,
`job_promoted` and `job_terminal` events identify jobs with `owner` and `job_id`
(`p<N>`), without a command `id`. Terminal events include status, elapsed time
and the bounded output tail; they do not consume the model's completion notice.
The protocol version remains 1.

`plakd run` never lets model or tool text drive your terminal: escape and other
control sequences in stderr lines, and in the answer when stdout is a
terminal, are shown as `�`, while newlines and tabs are kept. Piped stdout is
the answer byte for byte, so `plakd run ... > answer.txt` keeps exactly what the
model wrote.

---

## Accounts and credentials

### `plakd login <codex|grok|claude> [--remote]`

Interactive browser OAuth for a subscription account. By default the browser
opens on **this** machine. Codex waits for the loopback callback on its own — do
not paste the localhost URL back; it reminds at 30s and times out at 120s, and
`c` re-copies the URL while it waits. Grok and Claude open a browser and then
ask you to paste a code from the callback page.

`--remote` is for SSH and headless hosts. For Codex it is a device-code login:
plakd prints a verification URL and a one-time code, you open the URL in a
browser on any device and enter the code, and plakd finishes on its own (nothing
is pasted back; Ctrl-C cancels; the code expires after 15 minutes). Continue
only if you started that login yourself; if someone gave you a code, cancel.
If the provider does not offer device-code login for your account, run
`plakd login codex` on a machine with a browser, or forward the callback port
(`ssh -L 1455:localhost:1455 <host>`) and run `plakd login codex` on the host.
For Grok and Claude, plakd prints the URL and takes the pasted code or callback
URL. An SSH session refuses a local login outright and tells you to re-run
with `--remote` rather than switching mode silently.

The SuperGrok account answers to `grok` and `supergrok`; bare `xai` is refused
as ambiguous. API keys use `plakd key set` instead. Host clients can also
implement the confirmed Accounts login operations.

### `plakd logout <codex|grok|claude> [--local]`

Retires the credential. A usable credential is revoked at the provider first.
A stored credential this build cannot use — quarantined, login required, from a
changed profile or a retired format, or under an inactive compiled profile — is
retired locally, and the output says the remote grant may remain active. With
nothing stored, logout says there is nothing to log out. `--local` never
contacts the provider: it retires the local copy without a remote revoke, for
example when the provider cannot be reached. Claude OAuth has no pinned remote
revoke, so its logout is always local.
If a stored login was written by an older build or under another native
profile, `plakd status` says so and names `plakd login <account>`. That login
replaces the old record itself, says so in one line, and authorizes a fresh
grant (the old grant may remain active at the provider).
If a newer plakd build wrote the stored login, this build does not use or
change it. `plakd login <account>` first asks
`This login was written by a newer plakd build; replacing it means that build must log in again. Continue? [y/N]`;
anything but `y` leaves it untouched, and without a terminal the command
refuses without asking. Prefer running the newer build.

### `plakd refresh <codex|grok|claude>`

Claims a new refresh grant now, even when the access token is still fresh, and
prints the new access-token expiry. Ordinary commands refresh on their own.

A subscription refresh that fails before sending names the network phase (DNS,
connect or TLS) and leaves the credential unchanged. Retry when online; an
unknown refresh outcome still requires credential recovery rather than replay.

### `plakd status [account]`

Local credential readiness, with no provider request and no secrets. Both
status forms warn within 14 days of a subscription profile's expiry, or after
it, and note within 14 days of an API route's review-due date, or after it,
naming the surface and UTC date even if that account is not configured. An
expired subscription profile refuses its route; an API route past its review
date keeps working. Install a newly reviewed build before expiry; the warning
does not extend a route's lifetime. Bare `plakd status` groups every account:

```
$ plakd status
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
and a repair when needed. `plakd status grok` reports the SuperGrok subscription;
`plakd status xai-api` reports the API key. Bare `xai` is ambiguous and refused.

A section with no rows is omitted. When every account is ready there is no
Setup needed heading, and when none are there is no Ready heading.

A provider whose local credential cannot be read is still listed, under Setup
needed, and bare `plakd status` still exits 0. CREDENTIAL then names the state
and the repair, for example `Unsafe storage · …` with the chmod to run. A
login this build cannot use is a normal row naming its repair:
`Profile changed - run plakd login claude` for a Grok or Claude login from an
older build or another profile, `Login required - plakd login codex` for an
earlier-build Codex login. A repair too long
for the column is `Unsafe storage - run plakd status <account> for the fix`;
that command prints the full path and chmod. The other accounts,
including API keys, still print. `plakd status <account>` still exits 3 when
that one account cannot be read. The four columns stay put, so
`plakd status | cut -f4` is still the credential cell.

### `plakd key set|clear|status <openai|xai-api|anthropic|openrouter>`

`set` prompts for the key and writes it to the secrets store; `clear` removes
the file; `status` reports presence without printing the key. Environment
variables override files, `plakd key clear` cannot unset a process environment
variable, and plakd never loads a `.env` file. Host clients can also implement
masked, confirmed key operations.

### `plakd secret set|rm <name>`, `plakd secret list`

Named secrets that `config.toml` refers to by name — an MCP server's
`secret_env`, `bearer_secret` or `secret_headers`. `set` prompts with hidden
input (the value is never an argument) and writes
`<PLAK_HOME>/secrets/named/<name>.secret`, mode 0600; `rm` removes it; `list`
prints names only. A name matches `[a-z][a-z0-9-]{0,31}`. There is no
environment override.

### `plakd mcp add|list|show|test|login|logout|prompts|remove|enable|disable`

MCP server setup without editing `config.toml` by hand.

```sh
plakd mcp add everything -- npx -y @modelcontextprotocol/server-everything
plakd secret set github                        # the token; never an argument
plakd mcp add github --url https://api.githubcopilot.com/mcp/readonly --bearer-secret github
plakd mcp add ghlocal --secret-env GITHUB_PERSONAL_ACCESS_TOKEN=github \
  -- docker run -i --rm -e GITHUB_PERSONAL_ACCESS_TOKEN ghcr.io/github/github-mcp-server stdio --read-only
plakd mcp list
plakd mcp show github
plakd mcp test github
plakd mcp add linear --url https://mcp.linear.app/mcp --oauth
plakd mcp login linear                                  # or --remote over SSH
plakd mcp logout linear
plakd mcp prompts github                                # the server's prompts
plakd mcp prompts github review --arg target=main       # what one expands to
plakd mcp enable github --template root-agent             # every allowed tool
plakd mcp enable github --template root-agent --tool get_me   # or one at a time
plakd mcp disable github --template root-agent
plakd mcp remove github
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
  device, as with `plakd login --remote`: when the authorization server offers
  the device grant you enter a one-time code there, otherwise you paste back
  the URL the browser was sent to. A refused device request prints its HTTP
  status and says it is falling back to paste-back. An advertised device endpoint
  skipped by policy prints a policy-skip notice and uses paste-back without
  sending a device request. Over SSH, `login` without `--remote` is
  refused with that hint. The login is stored under
  `<PLAK_HOME>/secrets/mcp-oauth/<id>/` and used only for that server's URL.
  A session never starts a login: when the login expires and cannot be
  renewed, is revoked, or lacks a scope, the tool call fails and names the
  `plakd mcp login` command to run.
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

### `plakd runner serve|connect|allow|ps|pair|unpair|secret`

Runners run a placed session's tools on another machine you can ssh to.
These commands set one up and show what it holds. Local remains the explicit default.

On the runner machine, write `~/.plak/runner/runner.toml` with the workspace
roots a host may use (never `~/.plak` itself or anything inside it, which
`serve` refuses):

```toml
roots = ["/home/me/src"]
copies = "/home/me/plak-copies"   # optional: where copies from other runners go
```

On the host, name the machine in `config.toml` and pair with it:

```toml
[runners.build]
ssh = "build-box"          # a ~/.ssh/config alias or [user@]host
command = "/opt/plakd/bin/plakd"  # absolute stable executable on the runner
```

```sh
plakd runner pair build     # adds this host to the runner's allowlist over ssh
plakd runner ps build       # the lines and jobs it holds for this host
plakd runner unpair build   # drops this host's pin; --abandon-jobs leaves running jobs
```

Pairing needs a build that knows its source revision (a release or
maintainer install), matching the runner's protocol, source revision and flavor.
Pairing records machine identity and admitted roots; re-pair older pins without
machine evidence. On the same machine, runner roots must not overlap another
runner's roots or any non-deleted local session workspace or minted copy.
The host always dials, with fixed strict ssh options,
`BatchMode` and `StrictHostKeyChecking=yes`, so the ssh key must work without a
prompt and the runner's host key must already be in `known_hosts` (connect
once with plain `ssh` to accept it). On the runner machine,
`plakd runner connect --stdio` (what ssh runs) starts the daemon when none is
running, `plakd runner serve` runs it in the foreground, `plakd runner allow
<host-id>` pairs a host by hand and `plakd runner ps` lists every host's lines,
`held` or `orphaned`; a host's `plakd runner ps <name>` sees only its own. The daemon exits after `idle_exit_seconds` (default
300) with nothing to keep. `plakd runner secret set|rm <name>` and `plakd runner
secret list` manage the runner machine's own secrets, which an MCP server with
`placement = "runner"` reads; they work like `plakd secret`, on a separate store.
`plakd doctor` resolves each `[runners.<name>]` with `ssh -G` (no connection)
and shows the user, host and port it would dial.

A running session waits when its saved runner loses its link or is still
reconciling. It stays busy and needs attention; this is not proof that its
processes died. There is no default timeout that fails safe held work overnight.
After reconnection, the host completes the ownership and recovery barrier before
automatically releasing the same turn. Do not submit the work again or change
its runner to work around the wait.

The hold sends no new model work. A held child does not spend another turn,
send a result or wake its parent just because the runner is absent. Parked mail
waits unread for admission; an owed fork seal waits for its runner without
inventing a candidate. Other available placements can keep working. A child-only
hold needs attention without changing its root's own runner status.

Stop remains available while disconnected and still cancels the tree. Remote
cleanup may remain pending until the runner answers; a stopped row is not a
claim that an unreachable process was killed. A fixed unknown outcome, replaced
runner identity or permanent refusal still ends the affected work honestly;
reconnection does not make an uncertain effect or seal safe to repeat.

A host restart is different from a runner reconnect: a live hold is not durable.
Ordinary unpaid recovery and the host's `resume_after_loss` policy apply, with
hold as the default. Runner readiness alone is not consent for a paid
continuation. The product still places a tree's children on its selected
runner; these waits do not add a cross-runner spawn argument.

---

### Subscription compatibility review deadlines

The first time you use a subscription account -- `plakd login`, `plakd refresh`,
`plakd logout <account>`, `plakd models` or `plakd run` -- and again when the notice
revision or consent identity changes, plakd prints a notice on stderr and asks
once. Renewing only a profile's review and expiry dates does not ask again:

```
codex uses an experimental revision-pinned compatibility integration.
profile: <profile id>
reviewed: 2026-11-15 UTC
expires: 2027-02-13T00:00:00Z; after that this build refuses codex until a newer build replaces the profile
Continue? [y/N]
```

Answer `y`. The answer is read from stdin, so a non-interactive
`plakd run` accepts it with `echo y | plakd run --account codex ... -- '<prompt>'`
(the prompt itself follows `--`), or you accept it once beforehand with
`plakd login <account>`. With stdin closed (`</dev/null`, a CI job) `plakd run`
refuses with exit 3 and names both ways (`plakd login` itself asks for the
answer on a terminal or a pipe); any other answer declines, exits 3 and sends
nothing.

Consent binds the provider, profile id, primary and secondary source revisions,
provenance and kill-switch revision, together with the notice revision. An
existing valid acceptance survives a later review when those pins are unchanged:
the stored `reviewed_at` and `expires_at` are not compared. The current compiled
profile's active window and kill switch are still checked. If the notice
revision or consent identity changes, reaccept it through, for example,
`plakd models <account>`; reacceptance does not itself replace or refresh the
existing login credentials.

`plakd status` lists the UTC expiry of each compiled subscription compatibility
profile. These dates govern this build's integration review, independently of
credential expiry. Once a profile expires, obtain a build whose profile was
renewed after native client protocol review and regression testing. Logging in
again cannot renew a compiled profile. Maintainers must not extend dates without
that review. OpenRouter's static dialect review provenance is separate from the
fresh endpoint/model authority used for request admission.

## Running an agent

### `plakd run`

Run until the task is idle. Finished background processes and child results
or questions trigger distinct continuation turns; running work alone waits
without a turn. The prompt goes after `--`.

```
plakd run --account codex --model gpt-5.6-luna --effort low -- 'summarise src/'
plakd run --account grok --model grok-4.6 --effort low -- 'run the tests'
plakd run --account claude --model claude-sonnet-5 --effort low --composition js-ptc -- 'fix the lint'
plakd run --session 0f2c... -- 'and now update the changelog'
plakd run --account codex --model gpt-5.6-luna --effort low --image shot.png -- 'what is wrong here?'
```

| Flag | Meaning |
| --- | --- |
| `--account <id>` | `codex`, `grok`, `claude`, `openrouter`, `openai`, `anthropic` or `xai-api`. `--provider` is an accepted alias. |
| `--model <exact-id>` | An exact catalog model id. No prefixes, no inference, no fuzzy match. |
| `--effort <level>` | `none`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`. Required for a reasoning model, refused for the rest. `none` is explicit; omitting the flag is not `none`. There is no default; the catalog row decides. |
| `--composition <c>` | `direct`, `plakous-default`, `native-ptc`, `js-ptc`, `py-ptc`, `py-sdk`. `py-sdk` selects the typed, fresh-cell Python SDK for a core-tools-only single-agent root; it never follows from `plakous-default`. `native-ptc` is the vertical's own programmatic tool calling; today only Codex Code Mode is enrolled (Codex `CodeModeOnly` rows and OpenAI API models enrolled as `CodeModeOnly`), and elsewhere it is refused naming the vertical. `js-ptc` and `py-ptc` are harness PTC on every account. `plakous-default` (the default when omitted) is the per-model recommendation: `native-ptc` where Codex Code Mode is enrolled, `direct` on Grok, Claude, OpenRouter and the Anthropic and xAI API accounts. |
| `--protocol <p>` | `auto`, `chat`, `responses`, `messages`. OpenRouter only, default `auto` → `chat`. Other accounts reject it at parse time. |
| `--session <id>` | Continue an idle session: the full UUID or an unambiguous session prefix, resolved among this directory's sessions. Its recorded binding wins, so `--account`, `--model`, `--effort`, `--composition` and `--protocol` are refused alongside it. |
| `--max-turns <n>` | Admitted turns including the first, 1..=100; default 25. |
| `--timeout <seconds>` | Whole-run wall clock, including authentication/catalog preparation and idle waiting; default none. |
| `--leave-jobs-running` | Leave shell `&` leftover jobs alive and unowned at exit; the caller must clean them up. Budget-promoted jobs hold the run open while their leaders live. |
| `--print-session-id` | Print `session_id=<uuid>` on stdout before the turn starts. |
| `--image <path>` | Attach an image to the prompt; repeat for more (at most 8, and at most the 16 MiB per-message budget once encoded: three maximum-size images or eight typical screenshots; more is refused, exit 2, naming the budget). png, jpeg, gif or webp, detected from the bytes; an image over 2048 px on its long edge or 3.75 MiB is downscaled to fit (PNG when that fits, otherwise JPEG) and reported on stderr as `plakd: attached [image: shot.png 2048x1324 1.9 MB, downscaled from 3456x2234]`; smaller ones are sent unchanged. With images, the prompt after `--` may be left out. A file that cannot be read, is not a decodable image or is over 64 MiB is refused (exit 2) naming the file. A model without image input refuses the run naming the route and model. |

`plakd run` needs a ready OAuth login or key; an API account with no key exits 3
naming `plakd key set <account>` and its environment variable, and a signed-out
subscription exits 3 naming `plakd login <account>`. An invalid `config.toml`
refuses the run (exit 1) before anything else happens. It uses full
coding tools with no approval gates. `--session` reopens the recorded workspace locator, so your cwd
must match the one the session was created in.

`run` never asks you to approve an effect. If a coordination effect requests
approval, it denies it and prints the tool name with `run is non-interactive` on stderr.
This also applies to an approval restored by `--session`; the restored turn
finishes before your new prompt starts. Use an approval-capable wire client to allow an effect instead.

Only the last turn answer is written to stdout, once after release, with
separate text blocks of the answer split by a blank line and a final line break;
terminal tool results continue on stderr. Set `PLAK_VERBOSE=1` to include
per-tool progress as well. An interrupted turn prints its recorded partial
answer and exits with code 4. `--print-session-id` still prints before the turn.

Shell `&` leftovers become jobs but never hold `run` open. They stop at exit
unless `--leave-jobs-running` gives up ownership; redirect service output to
a file or `/dev/null` if it must survive output capture closing. Hosted and JSONL
jobs remain owned until stop or session release. A local daemon or paired
runner keeps background jobs across quit, including an escalated quit, and
across upgrade. `plakd session delete` and `plakd runner unpair` refuse while
a job is still running. Open the session and stop the jobs, or pass
`--abandon-jobs` to delete or unpair anyway and leave those processes running.

Waiting and continuation summaries go to stderr. A bound stops owned jobs,
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
`plakous-js-host` beside the binary; `py-ptc` and `py-sdk` locate `PLAK_PY_HOST` or a
sibling `plakous-py-host` worker. That is worker isolation, not workspace isolation, and
it does not isolate plakd itself. Unsupported process-tool targets, including
Intel macOS, fail closed.

### `plakd agent`

The hosted session loop, speaking newline-delimited JSON (`v=1`) on
stdin and stdout. Stdio is the pipe; there is no `plakd agent stdio` subcommand.
Binding flags match `plakd run` (`--account`, `--model`, `--effort`,
`--composition`, `--protocol`, `--session`); an omitted flag
falls back to the `[root] agent` template's value, and `plakd agent` refuses to
start when neither supplies an account, a model, or a required effort. A model
that is not a row of the account's catalog is refused before anything attaches,
naming the account, the catalog and the `plakd models` command that lists it
(OpenRouter model ids are left to OpenRouter):

```
model `gpt-5.6-luna` is not in the SuperGrok subscription catalog of account `grok`; run `plakd models grok` to list the models it offers
```

```
plakd agent --account codex --model gpt-5.6-luna --effort low
plakd agent --session 0f2c...
printf '{"v":1,"kind":"prompt","id":"1","text":"hi"}\n' | plakd agent --account grok --model grok-4.6 --effort low
```

Stdin commands: `prompt`, `interrupt`, `compact`, `resume_queue`, `resume`, `tree`,
`interrupt_node`, `close_node`, `stop_all`, `stop_processes`, `clear`,
`invoke_tool`, `fork`, `recover`, `background`, `resolve_approval`, `quit`.
`clear` starts a new session.
`resume` with `agent_id` resumes that loss-held child; without it, it resumes
all held work and the restored queue. It admits continuation instructions,
not a replay of the interrupted turn. `resume_queue` only resumes queue delivery.
A `prompt` may carry `"images": ["shot.png", "~/..."]`: paths to png, jpeg,
gif or webp files (relative ones resolve against the agent's working
directory; `~` is not expanded), read with the same limits as `plakd run
--image`. With images, `text` may be omitted.
`quit` gives an active turn 300 ms to settle cancellation and another 100 ms
to publish its result; a provider that ignores cancellation cannot hold the
host open until its response arrives. Unfinished work is recovered on reopen.
Idle or settled quits join child cleanup and report any cleanup failure.
Closing the control stream uses the same quit grace. Final projection stages
share one second from quit start; any remaining projection debt is applied on
the next open.

An `approval` event offers an opaque `approval_id`. Answer it once, choosing
`allow` or `deny`:

```json
{"v":1,"kind":"resolve_approval","id":"decision-1","approval_id":"<handle from approval>","choice":"allow"}
```

Acceptance returns a `status` note with that command's `id` and
`detail: "approval accepted: allow"` (or `deny`); `approval_resolved` reports
the decision's resolution with the original turn's correlation when known.
Allow authorizes the exact call once; deny never dispatches it. A stale,
expired, reused, or foreign handle returns a nonfatal `error`, as does an
invalid choice. Keep reading and send a corrected command, interrupt, or quit.
Backpressure and definite decision-receipt capacity refusals leave the offered handle
pending; retry that handle after capacity is available. Acceptance consumes it.
Ambiguous writes stay fenced and cannot be retried in place.
After a crash, `--session` emits `hello` before re-offering a pending approval;
answer the newly offered handle, not one retained from the old process.
`plakd run` still denies approvals because it has no inbound command channel.

Stdout events: `hello`, `ready`, `status`, `busy`, `assistant`,
`assistant_done`, `tool`, `approval`, `approval_resolved`, `error`, `turn`, `tree`, `queue`, `bye`.

Each command's answer (`error`, a `status` note) carries that command's `id`,
even when commands are piped back to back; a manual `compact` always answers
with a note naming its outcome. Ctrl-C (`SIGINT`) interrupts a running turn and
exits 4 after `error` and `bye`; idle, it quits with exit 0.

See [agents and orchestration](agents-and-orchestration.md) for a worked
session.

## Inspecting things

### `plakd doctor`

Local health checks. Everything is offline except one OpenRouter probe: when a
key and a default model are both set, plakd probes catalog keys with discovery
GETs only, never inference. Compiled provider reviews within 14 days of a
subscription expiry or an API review-due date (or past it) produce local
warnings naming the surface and UTC date. They count in the warning total;
warnings alone do not change the exit code.

```
$ plakd doctor
plakd doctor

CHECK              RESULT   DETAIL                                        FIX
Version            OK       available · 0.0.4                             —
Home               OK       available · /home/you/.plak                   —
Home free space    OK       available · product home volume has at least 5120 MiB available   —
Config             OK       valid · /home/you/.plak/config.toml           —
Product db         OK       available · /home/you/.plak/state/r28.e39.p39.t11/product.sqlite3   —
Runtime db         OK       available · /home/you/.plak/state/r28.e39.p39.t11/runtime.sqlite3   —
Runtime store      OK       available · checkpoints 412/524288 (new work stops at 458752), …   —
Subagents db       OK       available · /home/you/.plak/state/r28.e39.p39.t11/subagents.sqlite3   —
Telemetry db       OK       ok · 1189 rows                                —
Secrets dir        OK       available · /home/you/.plak/secrets           —
Sessions dir       OK       available · /home/you/.plak/state/r28.e39.p39.t11/sessions          —
Carry files        OK       none · /home/you/.plak/carry                  —
Cwd                OK       available · /home/you/project                 —
Jj cli             OK       missing · native workspace operations need no jj CLI; library not built   —
Stdin tty          OK       terminal · observed without reading or writing the stream   —
Stdout tty         OK       terminal · observed without reading or writing the stream   —
Composition host   OK       included · not required by the selected …     —
OpenRouter catalog OK       not configured · no OpenRouter key; catalog allowlist is checked at attach   —
Root template      OK       unset · no [root] agent; new sessions …       —
Spawn templates    OK       none · [roster] working_set is empty          —
OAuth codex        OK       ready · record ready · generation 13 · 22h old …   —
OAuth grok         OK       signed_out · no record · no refresh telemetry   —
OAuth claude       OK       signed_out · no record · no refresh telemetry   —

Result: 23 passed · 0 warnings · 0 failures
Provider credentials: run `plakd status`
```

The checks depend on the build, configuration and configured runners; the
example is illustrative. Every failing row carries its own FIX column.
`Runtime store` is an ERROR (`full`) once new work is refused for lack
of space and a WARNING (`nearly_full`) shortly before; its fix is
`plakd session gc --all`, then `plakd session delete <id>`. The row also shows
the database file size with its free bytes, and the change feed's retained
rows and their bytes (informational: feed rows never count toward the store
being full). `doctor` exits 5 when any row is ERROR; warnings alone do not
fail it.

`Home free space` checks the volume containing the product home, not the
workspace. It warns below 5 GiB and reports an error below 512 MiB, where new
turns and spawns are refused. Existing work is not cancelled. The operator can
inspect the volume and free space, then retry; plakd never deletes files to
make space. An unreadable probe warns without assuming zero free space.

### `plakd usage [--account codex|grok|claude|openrouter|supergrok] [--session <uuid>]`

Usage and status show the last failed credential
refresh when telemetry has it, with its local `HH:MM`, cause, and repair action.
A successful refresh clears that notice; starting another refresh does not.
`credential unchanged` means the old credential was restored, not an unknown
refresh outcome. These notices are display-only and never refresh a token or
change credential readiness. Without readable telemetry, ordinary status text
remains.

Subscription plan limits and, with `--session`, the context status of that exact
attached runtime. Without `--session` the plan limits come from the provider
dashboard. plakd never estimates tokens or model capacity, so a fact it does not
have is reported as absent rather than guessed. `--account` queries and prints
only that account; other accounts are not falsely shown as disconnected.
`--account` and `--session` conflict.

For this command, `grok` and `supergrok` select the same SuperGrok
subscription usage vertical. `openrouter` selects OpenRouter usage.

### `plakd models <codex|grok|claude>`

Reads the authenticated catalog for one account and lists its models. Requires a
ready login; this is the command that tells you the exact `--model` ids
`plakd run` will accept. One tab-separated line per model:

```
<id>	visibility=…	tool_mode=…	streaming=…	tools=…	reasoning=…	efforts=…	context_length=…
```

`reasoning=true` is the flag that decides whether `--effort` is required.
`tool_mode` is `n/a` when the catalog reports none, and `context_length` is
`unknown` when the provider did not report one.

### `plakd models openrouter`

Reads the OpenRouter catalog using your configured OpenRouter API key and
prints one exact model ID per line. This only discovers identifiers: listing
does not establish capabilities, admission, or qualification. It makes no
inference request.

### `plakd models <openai|anthropic|xai-api>`

Lists the provider's own model list with the stored API key (cached six hours;
offline, the last cache, else the table built into plakd; see
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

### `plakd tree release --force <tree-id>`

Each session's agent tree is held by one running `plakd` at a time. A second
`plakd` that opens the same session is refused with
`tree_hosted_elsewhere (<build>)` and exits 7; quit the other one first. When
the holding `plakd` on this machine has exited (even by a crash), the next open
takes the tree over by itself. Use this command only when the holder ran on
another machine sharing this home, or its exit cannot be proven: it prints a
warning naming the holder and releases the tree so the next open takes it. If
that `plakd` is in fact still running, every change it then makes to the tree is
refused. A holder still running on this machine is never released: quit it.
Without `--force` nothing changes.

### `plakd state prune-legacy [--yes] [--forget-missing-launches] [--prune-orphaned-references]`

`plakd doctor` lists non-current state paths and sizes. Pruning lists the same
inventory and asks for confirmation; non-terminal stdin requires `--yes`.
It refuses unsafe paths, in-use locks and inventory changes. Current state,
carried-forward references, dirty copies and unadopted work are retained.
`--forget-missing-launches` forgets ownership only for launch directories that
no longer exist; it deletes no checkout. `--prune-orphaned-references` removes
private retained references not used by current sessions or their sources.
Safe independent entries may be removed while protected entries are kept.
Review `kept:` reasons before manually removing anything. Export files under
`carry/` are never selected. Implementation details live in spec 02 §2.5.

### `plakd session list|rename|archive|unarchive|show|log|recover|export|import|reload|delete|gc`

```
plakd session list [--include-archived]
plakd session rename <full-id> <title>
plakd session rename <full-id> --clear
plakd session archive <full-id> [--yes]
plakd session unarchive <full-id>
plakd session show <uuid>[.cN]
plakd session log <uuid>[.cN] [--kind tool] [--tail N] [--json]
plakd session recover <uuid>
plakd session export <uuid> [--out <path>]
plakd session import <path>
plakd session reload <uuid> --expect-binary <binary-id>
plakd session delete <uuid> [--yes] [--abandon-children] [--abandon-jobs]
plakd session gc <uuid> | --all
```

Rename sets a home-local title override, even while busy; `--clear` restores the
generated title. Archive hides a session from ordinary lists and makes it
read-only until explicitly unarchived. It refuses running or uncertain work;
unarchive starts nothing. `--include-archived` includes hidden sessions.
Rename, archive and unarchive require full ids. Other session ids may be the
full id or an unambiguous prefix of at least
four characters among this workspace's sessions, as the other session commands and the
exit line print it. For `show` and `log`, append `.cN` to select child `cN` in that root session's
agent tree, for example `plakd session log <uuid>.c1`. These are read-only child
views; other session commands do not accept child selectors. `log <uuid>.cN`
is that child's committed history, not the root event log. A committed
compaction is a row of kind `compaction` (`compacted N earlier segments
(<pass>)` when the checkpoint recorded the pass); the summary itself is not
shown. `--kind compaction` selects those rows.

`show` labels a Working checkpoint `working (live owner present)` while the
process named by the branch's runtime holder record is still running,
`owner unknown (recorded on another host)` when another machine holds it, and
`recovery required` when it is unowned. All show the phase and recovery obligations; those details do not mean a live turn
needs recovery. Ownership and the head are separately observed, read-only facts.

`recover` reopens a session whose last turn was interrupted — a crash, a
killed process, or a turn that ended `recovery required` — records once what
that turn left unobserved, and releases it again. It never re-sends a model
request or re-runs a tool. It exits 0, printing the one line that says what was
recovered, or `nothing to recover`. You rarely need it: the next `plakd run
--session` or explicit host attach does the same thing on its own.

`export` and `import` link a session to another plakd build, one whose
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
binding. Only a locally produced, validated carry may retain replay, on the
identical account/model and after offline destination decoding.
Before creating the successor, import warns if the estimated history exceeds
the destination route's admitted input window, naming that route/window and
warning that the first prompt will refuse unless history compacts.
A smaller destination route schedules compaction at the first prompt when
needed; if its protected history or summary input still cannot fit, it returns
the normal context-limit refusal rather than silently truncating conversation.
An unknown destination window is reported but does not block carry.
This build writes version 13 and imports versions 13 and 12. Both versions preserve
the composition selection, including `py-sdk`, and session-owned named state.
Both versions name the runner the session ran on and its profile (root
template and working set); the imported session keeps that profile. Versions 11 and older are refused with `this build imports versions 12 and 13`.
Version 13 tags portable sessions explicitly; ordinary import refuses the reserved
`host_upgrade` kind. Dormant identity-preserving moves are not available yet.
New exports have no separate archive.
Keep exports private: they contain full conversation/tool contents and named
values, not a redacted transcript.
Documents larger than 256 MiB, histories larger than 128 MiB/100,000 messages,
and malformed content are refused, not truncated. A document already imported
into this workspace is refused, naming its existing successor.
`session list` and `session show` name the lineage link. A build imports its
own document version and the immediately previous one.

`reload` submits a same-user request for a running session when the home sets
`host.reload_requests = true`. The caller must match `--expect-binary` and a
live session listener must have announced the session. The outcome is recorded
in a private file. Hosted sessions record a refusal naming the host-restart
remedy; a client reload does not upgrade the host. Use the host upgrade workflow
in [host service setup](host-service.md) to change the serving build.

`delete` removes a session and its retained `~/.plak/references/<id>.json` archive,
plus all of its retained history, together with the
history of its finished sub-agents and its agent tree, and prints
`deleted session <id>`. It is how you free space when the session store is
full; a new session, compaction and recovery do not. It asks first unless `--yes`
is given, and refuses while the session is open in a running plakd (or its
process died less than half a minute ago), while its agent tree has a
sub-agent that is still running or resumable, or one that needs recovery
(let plakd recover it first, retry through explicit client operations, or abandon that obligation
with `--abandon-children`), or while other sessions continue from
it or share its history (forks, rebinds, new-session targets); the
refusal names them. These checks run before the question and again after it,
and nothing is removed until the session itself has been closed for deletion.
`--abandon-children` exempts only recovery-required children in this session's
own tree. It does not bypass live child leases, a live root, or fork dependencies.
Delete also refuses while this session or a finished child still has a background
job. Open the session and stop the job, or pass `--abandon-jobs` to delete anyway
and leave that process running. The store-full remedy is the same command.
A child closed before a late session-cleanup fault still needs recovery and is
listed as `recovery_required`. Recover it or use `--abandon-children`; recovery
keeps it closed and does not send another result to its parent.
If a delete fails part way, run it again: it finishes what is left. A session
carried forward with `import` does not block deleting the one it came from.
If deletion was interrupted after archive, `plakd session gc --all` cannot
recover the abandon choice; rerun `plakd session delete <id> --yes
--abandon-jobs` to finish it. Unreadable job labels are reported as `unknown`
and do not block an explicit `--abandon-jobs` choice.

A physical disk-full error is separate from the store's retention limit:
`plakd session gc --all`, deleting unused sessions, or freeing disk space are
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
running plakd, one with an unfinished turn (run `plakd session recover` first),
or one with a workflow (a compact operation, a fork operation, a model change) that a crash cut
short before it started (`workflow pending`: open the session, or run `plakd
session recover`, first). With `--all` it covers
every plakd session (product record) in this home, not just this workspace, and
also finishes deletes that a crash interrupted between archiving and deleting;
sub-agent runtime sessions are pruned by their own turns. Turns reclaim their
own history automatically once enough builds up, so `gc` matters mostly when
the store is full or for an imported session that arrived with long history. A
finished sub-agent's session is an ordinary session; nothing deletes it
automatically.

`list` is scoped to the current workspace:

```
$ plakd session list
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

### `plakd telemetry status|sql|prune`

A local analytics store; nothing leaves your machine.
```
$ plakd telemetry status
Telemetry store
  Path           /home/you/.plak/telemetry/telemetry-v22.sqlite3
  Schema version 22
  Size on disk    507904 bytes
  Retention       536870912 bytes, 90 days
  Write errors    0
  Dropped         0
  WAL checkpoint busy  0
TABLE                 ROWS
turns                 42
model_attempts        58
tool_calls            21
recorded_terminals    42
recording_evidence    1
…
```

`plakd telemetry status --views` prints the recommended read-only SQL to paste
into `plakd telemetry sql "<query>"` (add `--json` for machine output). Those
queries include repeated reads in a session (a salted digest, never the path),
the mix of programs started (a closed program label such as `cargo` or
`other`, never arguments or paths), occupancy at compaction, wait outcomes, and
each turn's first-request footprint with the idle gap before each request. The
`v_replay_loss` view lists retry and declined-retry correlation ids, proposed
next attempts, cause/phase, and the closed `declined_reason` when no replay
was dispatched. A declined `ModelRetry` has zero delay and does not discard
output; delay and `output_discarded` are event facts, not SQL columns. The view
lists assistant-text/thinking bytes and completed items from the failed
generation (discarded only when replayed), broken-attempt
elapsed time and replay class, never the discarded content. The SQL
is read-only and attaches `product.sqlite3` and `runtime.sqlite3` as `product`
and `runtime`.

`plakd telemetry prune` deletes whole sessions, oldest first, so it refuses a
bare run: pass `--max-age-days N`, `--max-bytes N`, or `--yes` to prune to the
configured retention. Defaults are 512 MiB and 90 days, both settable under
`[telemetry]` in `config.toml`. The byte cap counts this schema's telemetry file
plus its `-wal` and `-shm` sidecars; prune checkpoints and truncates the WAL, and
says so if a reader prevented the truncate. Retention is best effort: pruning
stops when a reader prevents reclamation and retains sessions with unfinished
turns.

Each telemetry schema has its own file. Older files and the legacy
`telemetry.sqlite3` stay untouched; `plakd doctor` reports them. Prune applies
only to the current schema. Removing older files remains an explicit action.

### `plakd config show|set|edit|init|trust|untrust`

```
plakd config show          # prints `config_path=<path>` on its first line
plakd config set <key> <value> [--scope user|project]
plakd config edit          # uses $VISUAL, then $EDITOR; quoted arguments, no shell
plakd config init          # creates a commented template that pins no defaults
plakd config trust         # admits the discovered project .plak
plakd config untrust       # revokes project trust
```

`[roster] working_set` and `[orchestration]` are configuration fields. Agent templates are one file each under
`~/.plak/agents/<name>.toml` (or a project's `.plak/agents/`).
`config edit` uses `vi` only at a terminal; headless use requires `VISUAL` or
`EDITOR`. Keys use their dotted names, for example `compaction.enabled`.
See [configuration](configuration.md).

### `plakd gateway`

Same-user credential gateway: listeners, caller keys, the kill switch, audit
and admin verbs. `serve` is the only foreground service and takes the
exclusive holder lock. `init` writes `<home>/gateway.toml` and does not open
the store. Keys come from the home `secrets/` directory; the server file
names no key directory. `serve` uses the host's state home and refuses a
`state_root` that is not that path exactly. `install` writes a user service
definition and does not start it.

```
plakd gateway init [--tcp <ip:port>]
plakd gateway serve
plakd gateway install [--dry-run]
plakd gateway uninstall
plakd gateway status [--dry-run]
plakd gateway caller add|list|rm|rotate
plakd gateway disable|enable
plakd gateway drain
plakd gateway audit [--since <rfc3339>] [--caller <id>]
plakd gateway check
```

A same-UID peer on the Unix socket is the local administrator and presents no
key. TCP requires a caller key. The service relays API-key accounts and the
subscription logins. A host with `[gateway] address` set relays API-key
and subscription routes through it. See [credential gateway](gateway.md).

## Maintainer commands

`just build debug|release|maintainer` and `just install maintainer` compile with
`--features maintainer`, so your binary also has `plakd fork`, `plakd compact` and
`plakd compatibility` under a `MAINTAINER:` heading in `plakd help`. They are not
part of the product surface; `plakd <cmd> --help` documents each.

Maintainer inference capture selects its private run only when inference starts.
Read-only commands and state maintenance that send no inference create no empty
wire-capture run.
