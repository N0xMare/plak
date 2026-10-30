use plakous_auth::oauth_pins::{
    CompatibilityProfilePins, SUBSCRIPTION_REVIEW_WINDOW_SECONDS, SubscriptionOAuthPins,
};

// Review date is the single release-cut input; the compatibility contract is unchanged.
const CODEX_REVIEWED_AT_UNIX: u64 = 1_793_318_400;

const CODEX_LOOPBACK_PORTS: &[u16] = &[1455, 1457];

/// Truthful `originator` Plak sends on the authorization URL and every
/// Codex request header.
pub(crate) const CODEX_ORIGINATOR: &str = "plak";

/// Extra authorization-URL pairs of the Codex browser login, in wire order.
pub(crate) const CODEX_AUTHORIZE_EXTRA_PAIRS: &[(&str, &str)] = &[
    ("id_token_add_organizations", "true"),
    ("codex_cli_simplified_flow", "true"),
    ("originator", CODEX_ORIGINATOR),
];

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
    reviewed_at_unix: CODEX_REVIEWED_AT_UNIX,
    expires_at_unix: CODEX_REVIEWED_AT_UNIX + SUBSCRIPTION_REVIEW_WINDOW_SECONDS,
    killed: false,
};

#[cfg(test)]
mod tests {
    use super::{
        CODEX_AUTHORIZE_EXTRA_PAIRS, CODEX_OAUTH, CODEX_ORIGINATOR, CODEX_PROFILE,
        CODEX_REVIEWED_AT_UNIX,
    };
    use plakous_auth::oauth_pins::{CompatibilityProfilePins, SubscriptionOAuthPins};

    /// Every consent-bearing Codex OAuth fact, hashed. A digest change is a
    /// consent-contract change: under spec 01 §7.2 it requires a new profile
    /// id, notice revision and kill-switch revision in the same edit.
    #[test]
    fn codex_consent_facts_are_bound_to_the_contract_revision() {
        let mut facts = CODEX_OAUTH.consent_fact_lines();
        facts.extend(["originator=", CODEX_ORIGINATOR, "\n"]);
        for &(name, value) in CODEX_AUTHORIZE_EXTRA_PAIRS {
            facts.extend(["authorize_extra=", name, "=", value, "\n"]);
        }
        assert_eq!(
            (
                CODEX_PROFILE.profile_id,
                CODEX_PROFILE.notice_revision,
                CODEX_PROFILE.kill_switch_revision,
                plakous_model::sha256_hex(&[facts.as_bytes()]).as_str(),
            ),
            (
                "openai.codex.compat.2026-07-31.r1",
                "openai.codex.compat.notice.2026-07-31.r1",
                "openai.codex.compat.kill.2026-07-31.r1",
                "c438a6e48849cb042a2384a1ed63be13ea5ae901ac822c6973ebdf9222c320f8",
            )
        );
    }

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
                reviewed_at_unix: CODEX_REVIEWED_AT_UNIX,
                expires_at_unix: CODEX_REVIEWED_AT_UNIX + 90 * 86_400,
                killed: false,
            }
        );
    }
}
