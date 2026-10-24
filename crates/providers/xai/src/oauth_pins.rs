use plakous_auth::oauth_pins::{
    CompatibilityProfilePins, SUBSCRIPTION_REVIEW_WINDOW_SECONDS, SubscriptionOAuthPins,
};

/// Review date of the Grok compatibility profile (00:00Z).
const XAI_REVIEWED_AT_UNIX: u64 = 1_792_800_000;

const XAI_LOOPBACK_PORTS: &[u16] = &[56121];

/// Extra authorization-URL pairs of the Grok login, in wire order.
pub(crate) const XAI_AUTHORIZE_EXTRA_PAIRS: &[(&str, &str)] =
    &[("plan", "generic"), ("referrer", "plak")];

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
    reviewed_at_unix: XAI_REVIEWED_AT_UNIX,
    expires_at_unix: XAI_REVIEWED_AT_UNIX + SUBSCRIPTION_REVIEW_WINDOW_SECONDS,
    killed: false,
};

#[cfg(test)]
mod tests {
    use super::{XAI_AUTHORIZE_EXTRA_PAIRS, XAI_OAUTH, XAI_PROFILE, XAI_REVIEWED_AT_UNIX};
    use plakous_auth::oauth_pins::{CompatibilityProfilePins, SubscriptionOAuthPins};

    /// Every consent-bearing Grok OAuth fact, hashed. A digest change is a
    /// consent-contract change: under spec 01 §7.2 it requires a new profile
    /// id, notice revision and kill-switch revision in the same edit.
    #[test]
    fn xai_consent_facts_are_bound_to_the_contract_revision() {
        let mut facts = XAI_OAUTH.consent_fact_lines();
        for &(name, value) in XAI_AUTHORIZE_EXTRA_PAIRS {
            facts.extend(["authorize_extra=", name, "=", value, "\n"]);
        }
        assert_eq!(
            (
                XAI_PROFILE.profile_id,
                XAI_PROFILE.notice_revision,
                XAI_PROFILE.kill_switch_revision,
                plakous_model::sha256_hex(&[facts.as_bytes()]).as_str(),
            ),
            (
                "xai.grok.compat.2026-07-31.r1",
                "xai.grok.compat.notice.2026-07-31.r1",
                "xai.grok.compat.kill.2026-07-31.r1",
                "3bd089ce47db3b7fae2011bc0250a35c2c56aec773119ac311d4409e7662ba13",
            )
        );
    }

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
                reviewed_at_unix: XAI_REVIEWED_AT_UNIX,
                expires_at_unix: XAI_REVIEWED_AT_UNIX + 90 * 86_400,
                killed: false,
            }
        );
    }
}
