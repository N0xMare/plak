//! Generic OAuth and compatibility pin values for subscription dialects.
//!
//! These tables are credential-free immutable facts only. They deliberately
//! contain no OAuth workflow, HTTP client, secret material, or manager logic.
//! Provider crates own every concrete pin table. This module owns only the
//! closed value shapes and provider-neutral validation laws consumed by the
//! shared auth workflow.

/// Pure OAuth client, endpoint, and loopback pins for a subscription dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionOAuthPins {
    /// Public OAuth client identifier (not a secret).
    pub client_id: &'static str,
    /// Authentication origin / issuer base URL in the dialect's exact form.
    pub auth_origin: &'static str,
    /// Optional OIDC discovery document URL when the dialect pins one.
    pub oidc_discovery_url: Option<&'static str>,
    /// Exact space-delimited scope string in pinned wire order.
    pub scopes: &'static str,
    /// Authorization endpoint path.
    pub authorize_path: &'static str,
    /// Token endpoint path.
    pub token_path: &'static str,
    /// Token revocation endpoint path.
    pub revoke_path: &'static str,
    /// JWKS endpoint path relative to the auth origin.
    pub jwks_path: &'static str,
    /// Userinfo endpoint path.
    pub userinfo_path: &'static str,
    /// Device authorization (or user-code) endpoint path.
    pub device_authorization_path: &'static str,
    /// Optional second device token path (Codex-style dual endpoints).
    pub device_token_path: Option<&'static str>,
    /// Optional device verification UI path when distinct from authorization.
    pub device_verification_path: Option<&'static str>,
    /// Optional device redirect path when distinct from the browser callback.
    pub device_redirect_path: Option<&'static str>,
    /// Browser callback path component.
    pub browser_callback_path: &'static str,
    /// Exact primary loopback redirect URI registered for the public client.
    pub loopback_redirect_uri: &'static str,
    /// Host:port authority of the primary loopback redirect (no scheme/path).
    pub loopback_authority: &'static str,
    /// Loopback ports the dialect may bind, in preference order.
    pub loopback_ports: &'static [u16],
}

impl SubscriptionOAuthPins {
    /// Returns the primary loopback port from the pin table.
    #[must_use]
    pub const fn primary_loopback_port(self) -> u16 {
        self.loopback_ports[0]
    }
}

#[cfg(test)]
pub(crate) const fn browser_test_pins() -> SubscriptionOAuthPins {
    SubscriptionOAuthPins {
        client_id: "browser-test-client",
        auth_origin: "https://browser.example/",
        oidc_discovery_url: None,
        scopes: "openid offline_access",
        authorize_path: "/oauth/authorize",
        token_path: "/oauth/token",
        revoke_path: "/oauth/revoke",
        jwks_path: "/.well-known/jwks.json",
        userinfo_path: "/oauth/userinfo",
        device_authorization_path: "/oauth/device/code",
        device_token_path: Some("/oauth/device/token"),
        device_verification_path: Some("/device"),
        device_redirect_path: Some("/device/callback"),
        browser_callback_path: "/callback",
        loopback_redirect_uri: "http://localhost:1455/callback",
        loopback_authority: "localhost:1455",
        loopback_ports: &[1455, 1457],
    }
}

#[cfg(test)]
pub(crate) const fn oidc_test_pins() -> SubscriptionOAuthPins {
    SubscriptionOAuthPins {
        client_id: "oidc-test-client",
        auth_origin: "https://oidc.example",
        oidc_discovery_url: Some("https://oidc.example/.well-known/openid-configuration"),
        scopes: "openid profile offline_access",
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
}

/// Pure compatibility-profile identity and lifecycle pins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompatibilityProfilePins {
    /// Immutable subscription-integration profile identifier.
    pub profile_id: &'static str,
    /// Immutable reviewed source revision the profile was pinned from.
    pub source_revision: &'static str,
    /// Optional human-readable source locator (URL or path).
    pub source_locator: Option<&'static str>,
    /// Exact operator notice revision required for acceptance.
    pub notice_revision: &'static str,
    /// Locally reviewable kill-switch revision bound into acceptance.
    pub kill_switch_revision: &'static str,
    /// UTC review timestamp (Unix seconds).
    pub reviewed_at_unix: u64,
    /// Hard compatibility expiry (Unix seconds, exclusive).
    pub expires_at_unix: u64,
    /// Compiled local kill-switch state.
    pub killed: bool,
}

impl CompatibilityProfilePins {
    /// Returns whether the profile is active (not kill-switched, before hard expiry).
    #[must_use]
    pub const fn is_active(self, now_unix: u64) -> bool {
        !self.killed && now_unix < self.expires_at_unix
    }

    /// Returns whether an acceptance is current against this profile at `now_unix`.
    ///
    /// Fail-closed when killed, past hard expiry, before the reviewed lower
    /// bound, or when `accepted_at_unix` is in the future relative to `now_unix`.
    #[must_use]
    pub const fn is_acceptance_current(self, accepted_at_unix: u64, now_unix: u64) -> bool {
        self.is_acceptance_current_at(accepted_at_unix, now_unix, self.killed)
    }

    /// Like [`Self::is_acceptance_current`] with an explicit kill-switch override.
    ///
    /// Dialects use this for test injection without mutating pin tables.
    #[must_use]
    pub const fn is_acceptance_current_at(
        self,
        accepted_at_unix: u64,
        now_unix: u64,
        killed: bool,
    ) -> bool {
        !killed
            && now_unix < self.expires_at_unix
            && accepted_at_unix >= self.reviewed_at_unix
            && accepted_at_unix <= now_unix
    }

    /// Returns whether an acceptance is current with an explicit hard expiry.
    ///
    /// Used when a manager may shorten the compiled expiry (test injection or
    /// operator policy) without changing pin table values.
    #[must_use]
    pub const fn is_acceptance_current_until(
        self,
        accepted_at_unix: u64,
        now_unix: u64,
        expires_at_unix: u64,
    ) -> bool {
        !self.killed
            && now_unix < expires_at_unix
            && accepted_at_unix >= self.reviewed_at_unix
            && accepted_at_unix <= now_unix
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{CompatibilityProfilePins, SubscriptionOAuthPins};

    const TEST_PORTS: &[u16] = &[1455, 1457];

    const fn oauth_pins() -> SubscriptionOAuthPins {
        SubscriptionOAuthPins {
            client_id: "test-client",
            auth_origin: "https://issuer.example/",
            oidc_discovery_url: None,
            scopes: "openid offline_access",
            authorize_path: "/oauth/authorize",
            token_path: "/oauth/token",
            revoke_path: "/oauth/revoke",
            jwks_path: "/.well-known/jwks.json",
            userinfo_path: "/oauth/userinfo",
            device_authorization_path: "/oauth/device/code",
            device_token_path: Some("/oauth/device/token"),
            device_verification_path: Some("/device"),
            device_redirect_path: Some("/device/callback"),
            browser_callback_path: "/callback",
            loopback_redirect_uri: "http://localhost:1455/callback",
            loopback_authority: "localhost:1455",
            loopback_ports: TEST_PORTS,
        }
    }

    const fn profile_pins() -> CompatibilityProfilePins {
        CompatibilityProfilePins {
            profile_id: "test.compat.r1",
            source_revision: "test-source-r1",
            source_locator: None,
            notice_revision: "test-notice-r1",
            kill_switch_revision: "test-kill-r1",
            reviewed_at_unix: 100,
            expires_at_unix: 200,
            killed: false,
        }
    }

    #[test]
    fn primary_loopback_port_is_the_first_pinned_port() {
        let pins = oauth_pins();
        assert_eq!(pins.primary_loopback_port(), 1455);
    }

    #[test]
    fn profile_acceptance_current_checks_kill_expiry_reviewed_and_future() {
        let pins = profile_pins();
        let now = pins.reviewed_at_unix + 1;
        assert!(pins.is_active(now));
        assert!(pins.is_acceptance_current(pins.reviewed_at_unix, now));
        assert!(!pins.is_acceptance_current(pins.reviewed_at_unix.saturating_sub(1), now));
        assert!(!pins.is_acceptance_current(now + 1, now));
        assert!(!pins.is_acceptance_current(pins.reviewed_at_unix, pins.expires_at_unix));
        assert!(!pins.is_acceptance_current_at(pins.reviewed_at_unix, now, true));
        assert!(!pins.is_acceptance_current_until(
            pins.reviewed_at_unix,
            now,
            now.saturating_sub(1).max(1)
        ));
        assert!(pins.is_acceptance_current_until(pins.reviewed_at_unix, now, pins.expires_at_unix));
    }
}
