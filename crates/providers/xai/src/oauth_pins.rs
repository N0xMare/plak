use plakous_auth::oauth_pins::{CompatibilityProfilePins, SubscriptionOAuthPins};

const XAI_LOOPBACK_PORTS: &[u16] = &[56121];

pub(crate) const XAI_OAUTH: SubscriptionOAuthPins = SubscriptionOAuthPins {
    client_id: "b1a00492-073a-47ea-816f-4c329264a828",
    auth_origin: "https://auth.x.ai",
    oidc_discovery_url: Some("https://auth.x.ai/.well-known/openid-configuration"),
    scopes: "openid profile email offline_access grok-cli:access api:access",
    authorize_path: "/oauth2/authorize",
    token_path: "/oauth2/token",
    revoke_path: "/oauth2/revoke",
    jwks_path: "/.well-known/jwks.json",
    userinfo_path: "/oauth2/userinfo",
    device_authorization_path: "/oauth2/device/code",
    device_token_path: None,
    device_verification_path: None,
    device_redirect_path: None,
    browser_callback_path: "/callback",
    loopback_redirect_uri: "http://127.0.0.1:56121/callback",
    loopback_authority: "127.0.0.1:56121",
    loopback_ports: XAI_LOOPBACK_PORTS,
};

pub(crate) const XAI_PROFILE: CompatibilityProfilePins = CompatibilityProfilePins {
    profile_id: "xai.grok.compat.2026-07-31.r1",
    source_revision: "19231fce4b70aa5f7894a0a0eb20ff29bd417db5",
    source_locator: Some(
        "https://github.com/anomalyco/opencode/blob/19231fce4b70aa5f7894a0a0eb20ff29bd417db5/packages/opencode/src/plugin/xai.ts",
    ),
    notice_revision: "xai.grok.compat.notice.2026-07-31.r1",
    kill_switch_revision: "xai.grok.compat.kill.2026-07-31.r1",
    reviewed_at_unix: 1_785_456_000,
    expires_at_unix: 1_801_353_600,
    killed: false,
};

#[cfg(test)]
mod tests {
    use super::{XAI_OAUTH, XAI_PROFILE};
    use plakous_auth::oauth_pins::{CompatibilityProfilePins, SubscriptionOAuthPins};

    #[test]
    fn xai_exact_provider_pins_are_owned_here() {
        assert_eq!(
            XAI_OAUTH,
            SubscriptionOAuthPins {
                client_id: "b1a00492-073a-47ea-816f-4c329264a828",
                auth_origin: "https://auth.x.ai",
                oidc_discovery_url: Some("https://auth.x.ai/.well-known/openid-configuration"),
                scopes: "openid profile email offline_access grok-cli:access api:access",
                authorize_path: "/oauth2/authorize",
                token_path: "/oauth2/token",
                revoke_path: "/oauth2/revoke",
                jwks_path: "/.well-known/jwks.json",
                userinfo_path: "/oauth2/userinfo",
                device_authorization_path: "/oauth2/device/code",
                device_token_path: None,
                device_verification_path: None,
                device_redirect_path: None,
                browser_callback_path: "/callback",
                loopback_redirect_uri: "http://127.0.0.1:56121/callback",
                loopback_authority: "127.0.0.1:56121",
                loopback_ports: &[56121],
            }
        );
        assert_eq!(
            XAI_PROFILE,
            CompatibilityProfilePins {
                profile_id: "xai.grok.compat.2026-07-31.r1",
                source_revision: "19231fce4b70aa5f7894a0a0eb20ff29bd417db5",
                source_locator: Some(
                    "https://github.com/anomalyco/opencode/blob/19231fce4b70aa5f7894a0a0eb20ff29bd417db5/packages/opencode/src/plugin/xai.ts",
                ),
                notice_revision: "xai.grok.compat.notice.2026-07-31.r1",
                kill_switch_revision: "xai.grok.compat.kill.2026-07-31.r1",
                reviewed_at_unix: 1_785_456_000,
                expires_at_unix: 1_801_353_600,
                killed: false,
            }
        );
    }
}
