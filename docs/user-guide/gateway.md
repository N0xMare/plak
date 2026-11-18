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
matching relays already in flight. `enable` and `drain` are local only.
`drain` stops accepting, waits up to 30 seconds for in-flight work, then the
service exits.

## Client configuration

In the home `config.toml`:

```toml
[gateway]
address = "unix:/run/user/1000/plak/gateway.sock"
# key_file = "/home/user/.plak/secrets/gateway.key"
# allow_non_tailnet = false
```

The server's own file is `<home>/gateway.toml`, separate from `config.toml`.
`plakd gateway init` writes it and does not touch the store.

## What is not relayed yet

An authenticated relay is refused `internal` until the shared relay server
core is available. Facts, refresh and admin calls are served. A host whose
`[gateway] address` is set refuses to start, because this build cannot yet
send its model traffic through the gateway. If that host's environment sets
`OPENAI_API_KEY`, `XAI_API_KEY`, `ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY`,
the refusal names those variables.

An account command that stays in this process, and a host with no gateway
address, take the holder lock shared. `plakd gateway serve` takes it
exclusive, so the two cannot overlap. A host that finds the service already
holding the store refuses to start and tells you to set `[gateway] address`.
