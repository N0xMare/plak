use plakous_auth::oauth_pins::{CompatibilityProfilePins, SubscriptionOAuthPins};

const CODEX_LOOPBACK_PORTS: &[u16] = &[1455, 1457];

pub(crate) const CODEX_OAUTH: SubscriptionOAuthPins = SubscriptionOAuthPins {
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    auth_origin: "https://auth.openai.com/",
    oidc_discovery_url: None,
    scopes: "openid profile email offline_access api.connectors.read api.connectors.invoke",
    authorize_path: "/oauth/authorize",
    token_path: "/oauth/token",
    revoke_path: "/oauth/revoke",
    jwks_path: "/.well-known/jwks.json",
    userinfo_path: "/api/accounts/oauth/userinfo",
    device_authorization_path: "/api/accounts/deviceauth/usercode",
    device_token_path: Some("/api/accounts/deviceauth/token"),
    device_verification_path: Some("/codex/device"),
    device_redirect_path: Some("/deviceauth/callback"),
    browser_callback_path: "/auth/callback",
    loopback_redirect_uri: "http://localhost:1455/auth/callback",
    loopback_authority: "localhost:1455",
    loopback_ports: CODEX_LOOPBACK_PORTS,
};

pub(crate) const CODEX_PROFILE: CompatibilityProfilePins = CompatibilityProfilePins {
    profile_id: "openai.codex.compat.2026-07-31.r1",
    source_revision: "c4f2746c43a3ab87c2978657d7f2c274546e97fe",
    source_locator: None,
    notice_revision: "openai.codex.compat.notice.2026-07-31.r1",
    kill_switch_revision: "openai.codex.compat.kill.2026-07-31.r1",
    reviewed_at_unix: 1_791_072_000,
    expires_at_unix: 1_793_750_399,
    killed: false,
};

#[cfg(test)]
mod tests {
    use super::{CODEX_OAUTH, CODEX_PROFILE};
    use plakous_auth::oauth_pins::{CompatibilityProfilePins, SubscriptionOAuthPins};

    #[test]
    fn codex_exact_provider_pins_are_owned_here() {
        assert_eq!(
            CODEX_OAUTH,
            SubscriptionOAuthPins {
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
                auth_origin: "https://auth.openai.com/",
                oidc_discovery_url: None,
                scopes: "openid profile email offline_access api.connectors.read api.connectors.invoke",
                authorize_path: "/oauth/authorize",
                token_path: "/oauth/token",
                revoke_path: "/oauth/revoke",
                jwks_path: "/.well-known/jwks.json",
                userinfo_path: "/api/accounts/oauth/userinfo",
                device_authorization_path: "/api/accounts/deviceauth/usercode",
                device_token_path: Some("/api/accounts/deviceauth/token"),
                device_verification_path: Some("/codex/device"),
                device_redirect_path: Some("/deviceauth/callback"),
                browser_callback_path: "/auth/callback",
                loopback_redirect_uri: "http://localhost:1455/auth/callback",
                loopback_authority: "localhost:1455",
                loopback_ports: &[1455, 1457],
            }
        );
        assert_eq!(
            CODEX_PROFILE,
            CompatibilityProfilePins {
                profile_id: "openai.codex.compat.2026-07-31.r1",
                source_revision: "c4f2746c43a3ab87c2978657d7f2c274546e97fe",
                source_locator: None,
                notice_revision: "openai.codex.compat.notice.2026-07-31.r1",
                kill_switch_revision: "openai.codex.compat.kill.2026-07-31.r1",
                reviewed_at_unix: 1_791_072_000,
                expires_at_unix: 1_793_750_399,
                killed: false,
            }
        );
    }
}
