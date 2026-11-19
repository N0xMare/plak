# Credential gateway

`plakd gateway serve` is the only plakd process that opens the provider
credential store. Every host uses a separate gateway, even on one machine.
Hosts require `[gateway] address`; account commands can also reach the local
service without host configuration. If the gateway is down, start it before
retrying. There is no local fallback.

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

On a machine that also runs the host, use [`plakd host install`](host-service.md#install)
to install both services. On a gateway-only machine, use
`plakd gateway install` and follow the printed start command.
The gateway installs as `plak-gateway.service` under the Linux user manager
or `com.plakd.gateway.plist` under macOS LaunchAgents. On Linux you can put
provider API-key variables in a private `~/.config/plak/gateway.env`; on either
platform you can use `plakd key set` after starting the gateway.

The unit runs `~/.local/bin/plakd-gateway gateway serve`. A missing link is
seeded from the plakd executable that ran the installer, so run it from a
qualified snapshot. Neither binary installation nor service reinstallation
moves an existing gateway link. Neither service installer starts a process.

## Migrate an existing service once

If the old host uses in-process credentials or the old gateway unit points
directly at a snapshot, settle work first. Stop the old host, drain the old
gateway if present, then disable or unload both units. On Linux run
`systemctl --user disable --now plakd-host.service plak-gateway.service` for
the units that exist. On macOS run
`launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.plakd.host.plist"`
and the corresponding command for `com.plakd.gateway.plist` if loaded.
Back up the old definitions outside the service manager's search path, then
remove them. The installers refuse to replace edited or pinned definitions and
print the exact stop and removal commands.

Keep the credential stores and use the same `PLAK_HOME` and `XDG_STATE_HOME`;
no secret copy or re-login is needed. For a co-located gateway, remove a
custom `socket` override from `gateway.toml` and a conflicting
`[gateway] address` from the host home config before running
`plakd host install` with the original web/UI arguments. On a gateway-only
machine, run `plakd gateway install` and keep the intended listener settings.
Start the gateway, run `plakd gateway check`, then start or upgrade the host.

## Upgrade with the host

For 0.0.5, follow the [cross-key upgrade order](host-service.md#upgrade-reload-and-logs):
rewrite named account references in templates to kinds, settle turns, live
children and daemon jobs, then upgrade paired runners and advance their
dedicated links. Upgrade the gateway **before** carrying the host:

1. Run `plakd gateway drain` on the gateway machine and wait for the old
   process to exit.
2. Create a temporary link to the new immutable snapshot in the same
   directory as `~/.local/bin/plakd-gateway`, then rename it over that link.
   Leave the host link and the service definition alone.
3. Run `systemctl --user restart plak-gateway.service` or
   `launchctl kickstart "gui/$(id -u)/com.plakd.gateway"`, then run
   `plakd gateway check`. No service-definition reload is needed. An
   explicit Linux service stop can also stop the dependent host; after
   the gateway check, start the host if needed for the coordinated upgrade.

The new gateway serves v1 to the old host and v2 to the new host. Then run
the destination's `plakd host upgrade --expect-binary <id>` and re-pin clients
to host wire protocol 12; protocol 11 receives a typed `VersionMismatch`.
Once the carry commits, recovery is fix-forward only.
If the host was carried first, upgrade the gateway, then restart the host.
The running host keeps its negotiated protocol; Codex and SuperGrok refuse
until it negotiates v2.

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
directory: API keys remain in the existing home `secrets/` files.

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

The host sends API-key and subscription
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

Missing host configuration or an unreachable gateway produces a
`gateway_unavailable` remedy. Run `plakd gateway install` if the local
gateway is missing; otherwise start the service at the configured address.
