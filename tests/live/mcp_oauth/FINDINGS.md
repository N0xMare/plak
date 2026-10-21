# Product Findings

These are product defects, not waived passes. No product code is changed by
this lane. The complete matrix intentionally exits nonzero while they remain.
Use the JSON report's per-row commands and safe AS events for fresh evidence.

## Keycloak Callback Extension Rejected

`dcr_loopback_keycloak` registers successfully and the real Keycloak browser
login reaches Plak's callback. Keycloak includes `session_state` along with
`state`, `iss` and `code`. The browser's safe callback witness records the key
names (never their secret values), exact redirect/state matches and the issuer.
Plak refuses with `the login callback is invalid`, before a token request.

`crates/common/auth/src/oauth_workflow/callback.rs`'s standard callback parser
rejects unknown parameters; `crates/harness/mcp/src/oauth/login.rs` removes
`iss` but uses that standard parser for the remaining MCP callback. RFC 6749
section 4.1.2 requires clients to ignore unrecognized response parameters.
This prevents a default Keycloak authorization-code login, not just a malformed
or malicious callback.

The separate pre-registered fixture clients use Keycloak's supported
`exclude.session.state.from.auth.response` attribute so device, confidential,
refresh/revocation and restart coverage can proceed independently. The DCR
row is never normalized or patched to pass.

## DCR Client Missing Device Grant

`dcr_device` advertises a real RFC 8628 endpoint but Plak's registration asks
for only `authorization_code` and `refresh_token`. node-oidc-provider's DCR
response confirms those grants; its device authorization endpoint then refuses
the client and Plak falls back to paste-back. The row requires the advertised
device flow, so it fails rather than silently treating fallback as device
coverage. `dcr_paste` separately exercises that legitimate fallback.

`device_confidential` uses a client registered for the device grant and succeeds
through the same product device-flow implementation and Playwright user-code
approval. Auth-code PKCE enforcement is not applied to that separate device
client: RFC 8628 does not supply a PKCE challenge.
