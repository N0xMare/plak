# MCP OAuth E2E

Opt-in, no real accounts, no inference, no changes to product code. Run on
a native Linux Docker host. Requirements: Python 3.11+, Docker Compose v2+, Rust
1.99.0 and network access to pull the pinned images/npm lockfile. On macOS,
Docker Desktop 4.34+ must have host networking enabled. All fixtures bind
loopback, including Keycloak; fixed ports 18080-18085 and 18765 must be free.
A cross-checkout file lock prevents concurrent runs. Do not expose the fixture
ports: admin and test-user passwords are intentionally public test data.

```sh
just mcp-oauth-e2e
python3 tests/live/mcp_oauth/run.py --list
python3 tests/live/mcp_oauth/run.py --binary target/release/plak --only dcr_loopback_strict,forced_401
python3 tests/live/mcp_oauth/run.py --previous-binary /path/to/other-state-key/plak
```

The recipe builds a `test-hooks` release binary. Direct Python invocation
requires one already built **from this checkout**. No sidecars are needed for
the scripted direct composition. The driver refuses a non-test-hooks binary;
the offline selector cannot fall back to a provider. `PLAK_TEST_FAULTS` injects
revocation 503 and a calibrated `tools/call` 401; a separate MCP fixture injects
a real HTTP 401 at `tools/call` so its
request log can prove no resend. Inherited credentials, SSH markers, proxy
settings and subscription state are not passed to Plak. A browser opener shim
hands local authorize URLs to Playwright. Remote login captures the failed
callback and pipes it to Plak; device login enters the printed code.

`target/live/mcp-oauth/report.json` is private (0700 parent/0600 file under a
fresh umask). It records source revision, binary hash/version, per-scenario
results, redacted CLI output, session logs and safe AS/MCP events. It never
records bearer/refresh tokens, authorization codes, device user codes, cookies,
client secrets or full authorize URLs. Fresh scratch homes and Compose volumes
are removed in `finally`; no server is intentionally left running. Raw Docker
logs are not evidence artifacts. Playwright stderr is omitted on failure because
its locator diagnostics can contain entered device codes. Interrupt the driver with Ctrl-C, not SIGKILL.
A failed teardown is a failing run. Browser logins exec into one owned
Playwright container with a fresh browser/context per login; timed-out Compose
CLI/plugin process groups are joined before resource teardown. Reports are overwritten by default; use
`--report PATH` to retain separate runs. Do not commit private homes/carries.

## Coverage And Oracles

The matrix includes Keycloak and strict-AS DCR loopback login, remote
paste-back and device login, confidential client + issuer pin, 60-second TTL
cadence, rotation/predecessor reuse, forced 401 (one refresh/no automatic
resend), cross-process refresh (an in-flight contender may fail closed, then explicitly
retry without a second refresh), safe/hostile 403 step-up, admin revocation,
logout (remote, 503 quarantine, local), wrong audience, same-origin tenant
metadata, callback issuer mismatch, Keycloak restart, and state-key carry.
`FAIL` is an unmet product/AS expectation pending triage; `HARNESS` is a driver
or setup exception. Neither is silently skipped. `BLOCKED` is missing supplied
build evidence. Read the detail and independent events before calling a product
defect. No assertion treats the scripted model's final `done` as tool success.

Keycloak does not implement RFC 8707. Its realm has a fixed audience mapper;
only node-oidc-provider proves exact `resource`/JWT audience binding. The strict
AS has ephemeral runtime-generated signing keys and in-memory grants: its own
restart is not a persistence claim. The restart scenario uses Keycloak's real
persisted realm. Keycloak's raw DCR login retains its default `session_state`
callback parameter. The pre-registered client disables that optional parameter
using Keycloak's native client attribute (never browser callback rewriting),
so other rows can qualify independently if Plak rejects that extension.
The confidential, revocation, split-origin and restart rows use that client.
Paste-back and callback-policy rows omit the optional device endpoint from
metadata so remote device preference cannot replace callback coverage. Device uses a
separate confidential client without an auth-code PKCE requirement (RFC 8628
does not send a PKCE challenge); public DCR and confidential auth-code clients
require S256.
The AS proxies count actual token/revocation requests and keep
only a predecessor refresh token in process memory for the explicit AS reuse
probe. The MCP server verifies JWKS signature, issuer, audience and expiry
before SDK dispatch, with per-tool scope enforcement outside the SDK.

Resource-rule policy is `resource-rules.json`, overridable with
`--resource-rules PATH`. `tenant_metadata` accepts `refuse` or `accept`; the
current location-bound rule requires `refuse`: metadata served at strict's
well-known location cannot claim sibling tenantb. This keeps the parallel interoperability
branch's resource-rule changes out of fixture implementation.

State-key carry requires `--previous-binary` with test-hooks and a
*different* state key. It uses the public session export/import commands, not
SQLite edits or moving a state directory. The older build creates/exports a
plain scripted session with an older-compatible config; the new build restores
the MCP template, imports the session and calls MCP using the pre-existing
home-scoped login. The older build need not implement MCP OAuth. Without that
binary the row is
`BLOCKED` and the complete matrix exits nonzero. Same-key import is not evidence
for a state-key move.

Offline tooling tests (no Docker/browser):

```sh
python3 -m unittest tests/tooling/test_mcp_oauth_e2e.py -v
```

Historical fixed product findings and their independent witnesses are in [FINDINGS.md](FINDINGS.md).

Additional rows qualify verified split-origin token/device/revocation secret
dispatch, refusal after metadata removes a stored endpoint, extension callback
parameters, and duplicate-state refusal before token exchange.
