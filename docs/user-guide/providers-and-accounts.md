# Providers and accounts

## Accounts

| Account | How you connect | Notes |
| --- | --- | --- |
| `codex` | `plak login codex` | ChatGPT / Codex subscription |
| `grok` | `plak login grok` | SuperGrok subscription. Also answers to `supergrok` |
| `claude` | `plak login claude` | Claude subscription |
| `openrouter` | `plak key set openrouter` | OpenRouter API key |
| `openai` | `plak key set openai` | Direct OpenAI API |
| `anthropic` | `plak key set anthropic` | Direct Anthropic API |
| `xai-api` | `plak key set xai-api` | Direct xAI API; uses the `xai` key slot |

Subscription and API accounts have separate credentials and billing.
Use `grok` for the SuperGrok subscription and `xai-api` for separately billed
API sessions. The ambiguous bare spelling `xai` is refused.

Login and key entry are CLI-only. The TUI's Providers tab is read-only: per
vertical it names the account, its status, and the command that connects it.

Usage never refreshes a token. An expired Codex, Grok or Claude access token
shows that the credential needs a token refresh: the next turn refreshes it,
or run `plak refresh <account>`. An ended usage window means stale provider
data, not an expired local token.

API-key environment variables override stored key files. In particular,
`OPENROUTER_API_KEY` wins over the OpenRouter key file even if the variable's
key is invalid. Usage names that variable or the file's `sha256:` fingerprint,
never a key prefix or suffix. To return to a stored key, run
`unset OPENROUTER_API_KEY` and start Plak again. Remove the stale export from
your shell startup configuration too. `/reload` retains the running process's
environment; it does not pick up an unset in another shell.

Claude OAuth is a first-class account route. Sonnet 5 and Sonnet 5.5 have
live CLI, JSONL and TUI evidence; Opus 5.5 also has a captured TUI soak and a
high-context profile-fork check. None of this is release qualification; the
[live status](../../tests/live/STATUS.md) records the current state. Sonnet 5.5
offers low–max effort; its multi-turn thinking replay has not been separately
qualified.
Haiku 5.5 (`claude-haiku-5-5`) is enrolled on Claude OAuth only, with that
same low–max effort and the shared 1,000,000/128,000 window. It has no live
acceptance yet. Disabled thinking is refused before dispatch. Forced tool choice is admitted
and unqualified: Claude Code demotes it to auto while thinking is active.
The OAuth
inference request uses the pinned Claude Code 2.1.295 client identity,
headers, system identity text and billing attestation, including Claude Code's
per-turn prompt and turn numbers and its previous-response diagnostics. The beta header and
`max_tokens` follow the selected model. An OAuth request sends the
`clear_thinking_20251015` context-management edit when thinking is enabled
or adaptive, and omits it when reasoning is disabled.
Claude sessions see
Claude Code's native tools `Bash`, `Read`, `Edit`, `Write` and `TaskStop`;
search runs through the shell, so the host needs `grep`, `rg` or `find`. The separately billed
`anthropic` API-key route does not use that OAuth identity.

## OAuth

`plak login <account>` opens a browser on this machine.

- **Codex** waits for its loopback callback by itself. Do not paste the
  localhost URL back into the terminal. It reminds you at 30 seconds and times
  out at 120; press `c` to re-copy the URL while it waits.
- **Grok** opens a browser and asks you to paste the code from the page.
- **Claude** uses an HTTPS redirect with no loopback; paste the authorization
  code shown on the callback page.

`--remote` is for a browser that is not on this host. Plak prints the URL and
takes the pasted code or callback URL; `c` then Enter re-copies. An SSH session
refuses a local login outright and prints
`plak login <account> --remote` rather than switching mode silently.

`plak logout <account>` revokes a usable credential at the provider and retires
it; a credential this build cannot use is retired locally, and the output says
the remote grant may remain active. `--local` never contacts the provider.
Claude OAuth has no pinned remote revoke, so its logout is always local.

Credentials live under `$XDG_STATE_HOME/plak/subscriptions/<provider>/`
(falling back to `~/.local/state`), mode `0700`, with the credential file at
`0600`. That is deliberately outside `~/.plak`, so wiping your Plak home does
not log you out.
On macOS, subscription storage rejects symlinked ancestors including `/tmp`
and `/var`; use a direct path such as one beneath `$HOME` for `XDG_STATE_HOME`.

## API keys

`plak key set <account>` prompts for the key and writes
`~/.plak/secrets/<account>.key` at mode `0600`. `plak key clear` removes it.
`plak key status` reports presence without printing anything secret.

Environment variables win over files — `OPENAI_API_KEY`, `XAI_API_KEY`,
`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY` — and Plak never loads a `.env` file.
While one of those is set, `plak key set` and `plak key clear` refuse to touch
the file store rather than pretending to work:

```
OPENROUTER_API_KEY is set in the process environment and still wins over the
file store; unset OPENROUTER_API_KEY (Plak never loads .env); no file change
```

Codex OAuth enrolls `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6-sol`, `gpt-6-luna`,
`gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`.
Other ChatGPT/Codex catalog rows, including retired
`gpt-5.3-codex-spark`, are not executable on this path.
Claude OAuth enrolls `claude-sonnet-5`, `claude-sonnet-5-5`,
`claude-haiku-5-5`, `claude-fable-5-1`, `claude-opus-5`, and `claude-opus-5-5`.

Direct API models come from each provider's own model list, cached for six
hours under `~/.plak/cache/api-models/`. Offline, Plak uses the last cache,
else the table built into Plak. Anthropic reads `GET /v1/models` and admits
the Claude 5.x models and Opus/Sonnet 4.6 and later, including
`claude-haiku-5-5`; Claude 4.5 models are listed as `needs review`. OpenAI
reads `GET /v1/models` (ids only) and admits `gpt-6-luna`, `gpt-6-sol`,
`gpt-5.6-luna`, `gpt-5.6-sol`, and `gpt-5.6-terra`. Other text models are
`needs review`; speech, image, embedding, realtime, moderation and video
models are `not a coding model`. xAI reads `GET /v1/models` and admits
`grok-4.3`, `grok-4.5`, `grok-4.6`, and `grok-4.7`. Non-reasoning and
multi-agent variants, and `grok-build-0.1`, are `needs review`; image and
video models are `not a coding model`.

Effort is always explicit and must be on the model's published ladder
(OpenAI profiles: `none`, `low`, `medium`, `high`, `xhigh`, `max`; `none`
means "no reasoning", it is not what you get by leaving effort off;
Anthropic 5.x: `low` through `max`; Sonnet/Opus 4.6: `low`, `medium`, `high`,
`max`; `grok-4.7`, `grok-4.6` and `grok-4.5`: `low`, `medium`, `high`,
`xhigh`; `grok-4.3` also `none`). A model id the provider does not list
is refused before anything is sent, with the closest listed ids. Each response
may use the model's whole output limit (128k on the OpenAI and current Claude
rows). xAI publishes no output limit, and input and output share the model's
context (500k for `grok-4.7`/`grok-4.6`/`grok-4.5`, 1M for `grok-4.3`), so
Plak reserves 128k of that context for each response (never more than half
of it) and the rest is input. The input window is the provider's price
threshold where it has one (Haiku 5.5: 100k tokens; the OpenAI profiles: 272k
tokens; the xAI profiles: 200k tokens; longer prompts are billed at a higher
rate), else the full window (1M for the other Claude rows, 922k for those
OpenAI profiles, 372k for `grok-4.7`/`grok-4.6`/`grok-4.5`, 872k for
`grok-4.3`). A live xAI list that publishes a smaller context, threshold or
fewer efforts than Plak's reviewed table narrows the model to them. Windows are provider
tokens, not request bytes: after the first reply Plak compacts against the
input count the provider reports, so a 100k-token Haiku window holds about
100k tokens of conversation however many bytes that is. Set
`[api] full_window = true` to admit new API sessions under the full window
instead.

Because each response may use the whole output bound, the worst case for one
model round is that many output tokens at the provider's output price: 128k
output tokens on the OpenAI, current Claude and xAI rows. A turn with many
tool rounds can pay that per round.

A session keeps the window and the facts that change what it sends (effort
ladder, output limit) as it was created; if the provider later changes those,
reopening it says so and asks you to rebind with **Fork now** in the Agents tab,
or start a new session.
An edit to a price threshold alone does not. For example:

```sh
plak run --account openai --model gpt-5.6-luna --effort low -- hello
plak agent --account anthropic --model claude-haiku-5-5 --effort medium
plak run --account xai-api --model grok-4.7 --effort low -- hello
```

These commands spend API credits. `plak models <account>` lists every model
the provider lists without making an inference request: its status (`usable`,
`needs review` or `not a coding model`, with the reason), efforts, working
and full window and output bound, and for a usable model its accepted
compositions and what `plakous-default` picks there: `native-ptc` (Codex Code
Mode) on the OpenAI API's `CodeModeOnly` models, `direct` on the Anthropic and
xAI APIs. `--composition js-ptc` and
`py-ptc` select the local workers. `plak run`, `plak agent` and a new TUI
session from your root template all make the same choice for the same model.
API sessions persist their exact credential binding. If you replace a key,
Plak refuses to resume the old binding under it; select the new target explicitly.

When a Plak build changes what an API route shows the model (its tools, base
instruction or default composition), sessions from the older build are not
re-presented under the new tools. The new build keeps its state in a new
directory; carry a session across with `plak session export` and import, or
start a new one.

## Reading `plak status`

`plak status` reads local state only; it makes no provider request and prints no
secrets. Up to two tables, `Ready` and `Setup needed`, with four columns. A
table with no rows is omitted.

**CREDENTIAL**, for an OAuth account: `Ready`, `Missing` (signed out),
`Refresh required`, `Unavailable`, `Revoked`, `Login required · plak login <account>`
(an earlier-build login, or a payload this build cannot parse), `Unsupported`.
When the local read itself fails, the cell is the state and the repair instead,
for example `Profile changed · plak logout claude, then plak login claude`
or `Unsafe storage · …` with the path and chmod. A repair too long for the
column is `Unsafe storage · run plak status <account> for the fix`; that
command prints the full path and chmod. The other rows still print, and
bare `plak status` exits 0. For an API-key account: `Environment`, `Key file`,
`Missing`.

**PROFILE**, for an OAuth account: `Accepted`, `Action required`, `Inactive`,
`Unsupported`.

`plak doctor` adds the credential's refresh history per account, e.g.
`ready · generation 13 · record 22h old · last refresh completed none 2xx 22h
ago`.

## Refresh and quarantine

Ordinary commands refresh their own tokens; `plak refresh <account>` forces one
immediately, which is mostly useful for testing.

A refresh by one Plak process does not interrupt another: running Codex and
Grok sessions and children renew their credential before their next model
request, as long as the refresh kept the same login. A new `plak login` of the
same provider, even for the same account, is a different login: running
sessions on the old one stop before sending anything and need a reopen or
rebind.

A Codex login made by an earlier Plak build must be redone once: run
`plak login codex`, which replaces it locally. Until then `plak status` shows
`Login required · plak login codex` and Codex commands refuse it. This build cannot
remotely revoke such a login: to revoke it at the provider, run
`plak logout codex` with the earlier build first.

When a refresh comes back with a result Plak cannot interpret unambiguously, the
credential is **quarantined** rather than used — Plak fails closed instead of
guessing. You see it as `Unavailable` in `plak status`.

An ambiguous refresh is recorded **on disk**, so restarting the process does not
clear it. Codex and Claude then need `plak login` again; SuperGrok retries under
a new credential generation on your next ordinary command, so its quarantine
usually clears itself. (Plak also keeps a separate, process-local publication
quarantine that does go away on restart; the durable one is what `plak status`
reports as `Unavailable`.)

## Compositions

A composition decides how the model gets its tools: as ordinary tool calls,
or through programmatic tool calling (PTC), where the model writes a program
that calls the tools.

| Composition | Available on | Meaning |
| --- | --- | --- |
| `direct` | every account | Ordinary tool calls |
| `plakous-default` | every account | The per-model recommended composition (the default) |
| `native-ptc` | where the vertical enrolls one: today **Codex and the OpenAI API** | The vertical's own PTC system; today only Codex Code Mode is enrolled |
| `js-ptc` | every account | Plak's JavaScript harness PTC |
| `py-ptc` | every account | Plak's Python harness PTC |

The default follows the catalog: Codex `CodeModeOnly` rows take `native-ptc`,
and so does every OpenAI API model (Plak enrolls them as `CodeModeOnly`, as the
Codex catalog lists the same models); Grok, Claude, OpenRouter and the
Anthropic and xAI API accounts take `direct`. The OpenAI API shows the model the
Codex tools and the xAI API the Grok tools, without the subscription client's
identity. Override with `--composition` on
`plak run` / `plak agent`, or `composition = "…"` in a template.

Python cells use `exec_py {code, yield_time_ms?}` and may return while still
running (default and maximum initial wait: 10 seconds). Resume observation with
`wait_py {cell_id, yield_time_ms?, terminate?}`; waits default to 10 seconds
and clamp to 600 seconds. Termination resets a running interpreter without replay; a finished, unread
cell returns its result and keeps Python globals. Printed output beyond the
per-cell cap has an explicit truncation notice.
Steers yield observation without killing the cell, and nested agent/process
awaits are allowed.

This build runs Monty 1.1.0 under adapter `plakous.monty-python-adapter.v6`. It
refuses to reopen py-ptc roots and children recorded under adapter
`plakous.monty-python-adapter.v4` or `plakous.monty-python-adapter.v5` (the
Monty 0.0.21 builds), profile `plakous.python-ptc-profile.v2`, or
presentation `plakous.python-ptc-presentation.v3`. There is no migration; use
session carry/export to reference them.

`js-ptc` and `py-ptc` are one Plak-owned harness class: custom, non-default, one
engine per agent. `exec_js` takes a JSON field `source` (an async module; emit
with `text(...)`); `exec_py` takes `code`. Neither is Codex-native Code Mode,
and an unsupported tuple fails before inference.

Both `native-ptc` and `js-ptc` need the `plakous-js-host` sidecar — `PLAK_JS_HOST`
or a `plakous-js-host` beside the `plak` binary, which the separate worker build
produces beside `plak`. `py-ptc` needs `PLAK_MONTY_WORKER` or a sibling `monty` worker. The normal build
produces both workers, including the pinned upstream Monty runtime. A missing
sidecar fails closed.

Grok, Claude, OpenRouter and the Anthropic and xAI APIs have no native PTC
enrolled, so `--composition native-ptc` there (or on a Codex or OpenAI API
model that is not `CodeModeOnly`) is refused before inference, naming the
vertical and the alternatives: for example `Claude has no native programmatic
tool calling (native-ptc) enrolled; use js-ptc, py-ptc or direct`.

## Effort

The ladder is `none`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`.
`none` is an explicit API effort (no reasoning). Leaving `--effort` off is
still a missing effort, not `none`.

There is no default effort. Each catalog row decides: `--effort` is **required**
for a reasoning model and **refused** for everything else. `plak models
<account>` is how you find out which is which before you run.

Which levels a given model accepts comes from that model's catalog row, not from
a fixed per-provider list. Codex and Claude OAuth offer `low` through `max`,
never `ultra`. Codex also refuses an explicit `ultra` bind; OpenRouter keeps
`ultra` where its model admits it.

OpenRouter is the exception to the requirement: its model ids are literal
dynamic selections, every model/wire tuple is custom and experimental, and it is
not a subscription catalog or a qualification claim. `plak agent` on any other
account refuses to start without one:

```
plak agent requires --effort or an effort in the [root] agent template
```

Put `effort = "low"` in the template `[root] agent` names and the flag stops
being mandatory for the sessions that use it.

OpenRouter sessions remain resumable when endpoints are added, removed or
reordered without changing the selected wire's admission guarantees. Changes
to those guarantees still refuse before inference; endpoint-list drift never
changes your route, account, protocol or credentials and never enables fallback.

## Protocol

`--protocol auto|chat|responses|messages` applies to OpenRouter only, default
`auto` → `chat`. OAuth accounts reject the flag at parse time.
