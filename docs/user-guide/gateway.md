# Credential gateway

`plakd gateway serve` is the process that opens the credential store when a
deployment wants one holder. A machine that does not set `[gateway] address`
keeps account commands in the process that runs them.

## Listeners

The service listens on a Unix socket. A same-user peer on that socket is the
local administrator and does not present a caller key. An optional TCP
listener binds only to a loopback or tailnet address (100.64.0.0/10 or
`fd7a:115c:a1e0::/48`) unless `allow_non_tailnet` is set, and every TCP caller
presents a key.

## Caller keys

`plakd gateway caller add` mints a key, prints it once, and stores only its
hash. The default scope is `relay:*`. Add `--admin <account>` for admin scope.
`rotate` replaces the key. `rm` revokes the caller. Ids are not reused.

## Kill switch and drain

`disable` can be remote. It stops new calls and, with `--cut`, cancels
matching relays already in flight. `enable` and `drain` are local only. A
global disable still lets the local administrator run `enable`, `disable`,
`drain`, `caller list` and `status`, so it can always be cleared; relays, facts and
refreshes stay blocked for everyone until then.
`drain` stops accepting and waits up to 30 seconds for in-flight relays and
refresh claims; the service exits and only then releases the store.

`plakd gateway audit --since 2026-11-18T00:00:00Z` filters by an RFC 3339
time. `plakd gateway status` shows how many audit lines failed to append.

## Install

`plakd gateway install` writes `~/.config/systemd/user/plak-gateway.service`
running the executable that wrote it, with `UMask=0077`, the `PLAK_HOME` and
`XDG_STATE_HOME` of the shell you ran it from, `EnvironmentFile=-%h/.config/plak/gateway.env`
for API-key variables, `Restart=on-failure`, `RestartSec=2` and a stop timeout
longer than the drain. Point `ExecStart` at an immutable build before enabling
it.

## Client configuration

In the home `config.toml`:

```toml
[gateway]
address = "unix:/run/user/1000/plak/gateway.sock"
# key_file = "/home/user/.plak/secrets/gateway.key"
# allow_non_tailnet = false
```

A same-user unix gateway needs no key: without `key_file` the caller is the
local administrator. A `tcp:` address always sends a key, by default
`<home>/secrets/gateway.key`. The key file must be mode 0600.

A process reads `[gateway]` once. A running host keeps the custody it started
with; restart it after changing `[gateway]`. If `config.toml` does not parse,
the host refuses to start rather than opening the store itself. A host with a
unix address also refuses to start unless the gateway that holds this home's
state store advertises that exact socket; `plakd gateway check` and
`plakd doctor` report the same mismatch.

The server's own file is `<home>/gateway.toml`, separate from `config.toml`.
`plakd gateway init` writes it and does not touch the store. It names no key
directory: API keys are the home `secrets/` files the host already reads.

`state_root` in that file, when set, must be the host state home byte for
byte: `$XDG_STATE_HOME` when that variable is set, non-empty and absolute,
otherwise `$HOME/.local/state`. `plakd gateway serve` and
`plakd gateway status --dry-run` refuse any other spelling, including a
trailing slash or a symlink's other name, before taking the lock or opening
the store. Compare the strings; do not canonicalize.

## What the gateway relays

The listener relays `openai`, `xai-api`, `anthropic` and `openrouter` from the
gateway process's own key files and environment, and the `codex`, `claude`
and `grok` logins, and only to those providers' pinned upstreams. Facts and
refresh for those accounts are served from that custody. Other admin verbs
stay on the account bridge.

A host whose `[gateway] address` is set sends API-key and subscription
model traffic through the gateway and does not take the holder lock, read
`secrets/` or open the subscription store. Existing sessions reopen
unchanged. Login, logout, consent, key status and account listing use the
gateway's admin verbs; `plakd key status` prints each key's fingerprint so you
can compare it before and after moving to the gateway. `plakd run`,
`continue` and `models` work the same way through the gateway.

A login through a remote caller key with admin scope may sign in an empty
slot or log the same provider account in again. If the new login is a
different account, the gateway refuses it before storing anything
(`local_admin_required`); run that login on the gateway machine. If that host's environment sets `OPENAI_API_KEY`,
`XAI_API_KEY`,
`ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY`, the host refuses to start and
names those variables. OpenRouter allowance is `plakd` usage through the
gateway: the gateway reads `GET /api/v1/key` and returns the amounts, never
the key. Other API-key accounts report `not-observed`.

An account command that stays in this process, and a host with no gateway
address, take the holder lock shared. `plakd gateway serve` takes it
exclusive, so the two cannot overlap. A host that finds the service already
holding the store refuses to start and tells you to set `[gateway] address`.
