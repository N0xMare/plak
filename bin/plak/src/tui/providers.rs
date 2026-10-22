//! Credential status projection for the Providers tab (spec 02 §5.5).
//!
//! One value per configured vertical — Codex OAuth, Claude OAuth, `SuperGrok`
//! OAuth, and the API-key accounts — carrying the host-computed state, the
//! account label when the credential supplies one, and the exact CLI command
//! that connects or repairs it.
//!
//! The Providers tab is a **status view with no actions**: login, logout and
//! key entry are CLI-only (`plak login`, `plak logout`, `plak key set`,
//! `plak key clear`), so nothing here is an action and `Enter` on a row only
//! focuses it. Model choice is a profile, not a picker on this tab, so
//! Providers carries no model-readiness header of its own.
//!
//! Every command string the product prints is built here, by
//! [`oauth_login_remedy`] and [`api_key_remedy`], so no surface can invent a
//! command the CLI does not have. [`remedy_catalog`] enumerates them for the
//! test that parses each one through the real CLI.

pub(crate) use crate::tui::{AuthKind, ProviderDef, ProviderState};

/// Colour class of a status line on the Providers tab (spec 02 §5.5).
///
/// Three tones, so a status word and its colour can never disagree: green is
/// the only thing painted as connected, yellow is a credential the operator can
/// obtain with one CLI command, and red is a credential that exists but cannot
/// be used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusTone {
    /// `ready` — present and revalidated.
    Ready,
    /// `login required` — nothing stored yet.
    LoginRequired,
    /// `re-login required (expired)`, `refresh outcome unknown`, or a store
    /// Plak could not read.
    Blocked,
}

/// Providers-tab wording for one host-projected credential row.
pub trait ProviderRowPresentation {
    /// Status wording for the Providers tab (spec 02 §5.5).
    fn status_label(&self) -> String;
    /// Colour class of the status line.
    fn status_tone(&self) -> StatusTone;
    /// The exact CLI command that connects or repairs the credential.
    fn cli_direction(&self) -> String;
    /// Description-panel text behind the status word.
    fn status_description(&self) -> String;
}

impl ProviderRowPresentation for ProviderDef {
    /// Status wording for the Providers tab (spec 02 §5.5).
    ///
    /// The wording is closed: `ready`, `login required`,
    /// `re-login required (expired)`, and `refresh outcome unknown`. The nuance
    /// the host reported behind an expiry — `revoked`, `refresh required` —
    /// belongs in the description panel, not in the status word, so the states
    /// stay readable side by side. A store Plak could not read is neither ready
    /// nor repairable by logging in, so it says so instead of borrowing one of
    /// the credential states.
    /// A recorded refresh failure replaces generic expiry prose, without
    /// changing the readiness state or its colour.
    fn status_label(&self) -> String {
        if self.detail.starts_with("refresh failed at ") {
            return if self.state == ProviderState::Ready {
                format!("ready; {}", self.detail)
            } else {
                self.detail.clone()
            };
        }
        match self.state {
            ProviderState::Ready => "ready".to_owned(),
            ProviderState::NotLoggedIn | ProviderState::NoKey => "login required".to_owned(),
            ProviderState::Expired => "re-login required (expired)".to_owned(),
            ProviderState::RefreshUnknown => "refresh outcome unknown".to_owned(),
            ProviderState::Unavailable if self.detail.is_empty() => "unavailable".to_owned(),
            ProviderState::Unavailable => format!("unavailable ({})", self.detail),
        }
    }

    /// Colour class of [`Self::status_label`].
    fn status_tone(&self) -> StatusTone {
        match self.state {
            ProviderState::Ready => StatusTone::Ready,
            ProviderState::NotLoggedIn | ProviderState::NoKey => StatusTone::LoginRequired,
            ProviderState::Expired | ProviderState::Unavailable | ProviderState::RefreshUnknown => {
                StatusTone::Blocked
            }
        }
    }

    /// The exact CLI command that connects or repairs this vertical.
    ///
    /// Printed on every row, ready or not: the Providers tab is a status view,
    /// and the one thing an operator can do about any vertical is run this
    /// command in a terminal. It comes from the same catalogue the CLI parser
    /// test walks, so the tab cannot name a command `plak` does not have.
    fn cli_direction(&self) -> String {
        match self.kind {
            AuthKind::Oauth => oauth_login_remedy(&self.id),
            AuthKind::ApiKey => api_key_remedy(&self.id),
        }
    }

    /// One sentence explaining the state behind [`Self::status_label`], for the
    /// floating description panel (spec 02 §5.5).
    ///
    /// Quarantine is `refresh outcome unknown`, not an expiry nuance. Grok
    /// retries automatically; Codex and Claude still require `plak login`.
    fn status_description(&self) -> String {
        if self.detail.starts_with("refresh failed at ") {
            return self.detail.clone();
        }
        let command = self.cli_direction();
        match self.state {
            ProviderState::Ready => {
                "Connected and revalidated; profiles rooted on this account can be used."
                    .to_owned()
            }
            ProviderState::NotLoggedIn => format!(
                "No subscription credential is stored. Run `{command}` in a terminal; login is CLI-only."
            ),
            ProviderState::NoKey => format!(
                "No API key is stored or exported. Run `{command}` in a terminal; key entry is CLI-only."
            ),
            ProviderState::Expired if self.detail.is_empty() => format!(
                "The stored credential can no longer be used. Run `{command}` to replace it."
            ),
            ProviderState::Expired => format!(
                "The stored credential can no longer be used ({}). Run `{command}` to replace it.",
                self.detail
            ),
            ProviderState::RefreshUnknown if self.id == "grok" => {
                "The refresh outcome is unknown after an ambiguous refresh. The next refresh or catalog load will retry automatically.".to_owned()
            }
            ProviderState::RefreshUnknown => format!(
                "The refresh outcome is unknown after an ambiguous refresh. Run `{command}` to recover."
            ),
            ProviderState::Unavailable if self.detail.is_empty() => {
                "Plak could not read this vertical's credential state, so it reports nothing."
                    .to_owned()
            }
            ProviderState::Unavailable => format!(
                "Plak could not establish this vertical's credential state ({}).",
                self.detail
            ),
        }
    }
}

/// Independent oracle for the OAuth command contract tests.
#[cfg(test)]
pub const OAUTH_ACCOUNTS: [&str; 3] = ["codex", "grok", "claude"];

/// The API-key accounts `plak key set` / `plak key clear` accept.
#[cfg(test)]
pub const API_KEY_ACCOUNTS: [&str; 4] = ["openai", "xai-api", "anthropic", "openrouter"];

/// The Limits projection before any host read: every vertical of
/// the capability registry listed, none painted as connected.
#[must_use]
pub fn unread_usage_limits() -> crate::tui::presentation::UsageLimitsView {
    use crate::tui::presentation::{
        UsageLimitsView, UsageVertical, VerticalUsageState, VerticalUsageView,
    };
    UsageLimitsView {
        verticals: crate::config::usage_verticals()
            .map(|(account, label)| VerticalUsageView {
                vertical: UsageVertical::new(account, label),
                account_email: None,
                plan_label: None,
                observed_at: None,
                state: VerticalUsageState::NotConnected,
                balances: Vec::new(),
            })
            .collect(),
        ..UsageLimitsView::default()
    }
}

/// Placeholder for the OAuth account argument, as help text spells it.
pub const OAUTH_ACCOUNT_PLACEHOLDER: &str = "<codex|grok|claude>";

/// Placeholder for the API-key account argument, as help text spells it.
pub const API_KEY_ACCOUNT_PLACEHOLDER: &str = "<openai|xai-api|anthropic|openrouter>";

/// The canonical command that connects one OAuth subscription account.
///
/// `plak auth login` still parses as a legacy alias, but no product surface
/// names it: an operator reading a remedy gets the command the help pages and
/// the docs use.
#[must_use]
pub fn oauth_login_remedy(account: &str) -> String {
    format!("plak login {account}")
}

/// The canonical command that stores one provider API key.
///
/// API-key accounts never take `plak login`: the two auth routes do not cross,
/// and pointing an `openrouter` operator at an OAuth verb sends them to a
/// refusal.
#[must_use]
pub fn api_key_remedy(account: &str) -> String {
    format!("plak key set {account}")
}

/// The remedy for `account`, chosen by which auth route the account belongs to.
///
/// An account the product does not know is treated as an API key: the four
/// OAuth verticals are a closed set, and inventing an OAuth login for an
/// unknown name is the exact defect this dispatch exists to prevent.
#[must_use]
pub fn account_remedy(account: &str) -> String {
    if crate::config::oauth_accounts().any(|known| known == account) {
        oauth_login_remedy(account)
    } else {
        api_key_remedy(account)
    }
}

/// Every concrete remedy command any product surface can print.
///
/// The CLI-facing regression test parses each of these with the real argument
/// parser, so a remedy that names a command `plak` does not have fails the
/// build rather than reaching an operator.
#[must_use]
#[cfg(test)]
pub fn remedy_catalog() -> Vec<String> {
    let mut catalog = Vec::new();
    for account in OAUTH_ACCOUNTS {
        catalog.push(oauth_login_remedy(account));
        catalog.push(format!("plak logout {account}"));
    }
    for account in API_KEY_ACCOUNTS {
        catalog.push(api_key_remedy(account));
        catalog.push(format!("plak key clear {account}"));
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::{
        API_KEY_ACCOUNTS, AuthKind, OAUTH_ACCOUNTS, ProviderDef, ProviderRowPresentation as _,
        ProviderState, StatusTone, account_remedy, remedy_catalog,
    };

    fn def(id: &str, kind: AuthKind, state: ProviderState, detail: &str) -> ProviderDef {
        ProviderDef {
            id: id.to_owned(),
            label: id.to_owned(),
            kind,
            state,
            detail: detail.to_owned(),
            account_label: None,
        }
    }

    /// Spec 02 §5.5: the status wording is a closed set of three, and the tone
    /// follows the word so a colour can never contradict it.
    #[test]
    fn providers_render_last_failed_refresh_instead_of_expired() {
        let message = "refresh failed at 05:48: network unreachable (DNS lookup timed out); credential unchanged; retry when online";
        let row = def("claude", AuthKind::Oauth, ProviderState::Expired, message);
        assert_eq!(row.status_label(), message);
        assert_eq!(row.status_description(), message);
        assert_eq!(row.status_tone(), StatusTone::Blocked);
        let ready = def("claude", AuthKind::Oauth, ProviderState::Ready, message);
        assert_eq!(ready.status_label(), format!("ready; {message}"));
        assert_eq!(ready.status_tone(), StatusTone::Ready);
    }

    #[test]
    fn every_state_maps_to_its_exact_status_word_and_tone() {
        let ready = def("codex", AuthKind::Oauth, ProviderState::Ready, "");
        assert_eq!(ready.status_label(), "ready");
        assert_eq!(ready.status_tone(), StatusTone::Ready);

        for absent in [
            def("grok", AuthKind::Oauth, ProviderState::NotLoggedIn, ""),
            def("openrouter", AuthKind::ApiKey, ProviderState::NoKey, ""),
        ] {
            assert_eq!(absent.status_label(), "login required");
            assert_eq!(absent.status_tone(), StatusTone::LoginRequired);
        }

        // The nuance behind an expiry never leaks into the status word.
        for detail in ["", "revoked", "refresh required"] {
            let expired = def("claude", AuthKind::Oauth, ProviderState::Expired, detail);
            assert_eq!(expired.status_label(), "re-login required (expired)");
            assert_eq!(expired.status_tone(), StatusTone::Blocked);
        }

        let unknown = def(
            "grok",
            AuthKind::Oauth,
            ProviderState::RefreshUnknown,
            "quarantined",
        );
        assert_eq!(unknown.status_label(), "refresh outcome unknown");
        assert_eq!(unknown.status_tone(), StatusTone::Blocked);

        // An unreadable store is neither ready nor fixable by logging in.
        let unknown = def(
            "codex",
            AuthKind::Oauth,
            ProviderState::Unavailable,
            "credential store unreadable",
        );
        assert_eq!(
            unknown.status_label(),
            "unavailable (credential store unreadable)"
        );
        assert_eq!(unknown.status_tone(), StatusTone::Blocked);
    }

    /// Spec 02 §5.5: every vertical prints its CLI direction, ready or not, and
    /// the direction follows the auth route rather than the state.
    #[test]
    fn every_vertical_names_its_exact_cli_command() {
        for state in [
            ProviderState::Ready,
            ProviderState::NotLoggedIn,
            ProviderState::Expired,
            ProviderState::RefreshUnknown,
            ProviderState::Unavailable,
        ] {
            let grok = def("grok", AuthKind::Oauth, state, "");
            assert_eq!(grok.cli_direction(), "plak login grok");
        }
        let openrouter = def("openrouter", AuthKind::ApiKey, ProviderState::NoKey, "");
        assert_eq!(openrouter.cli_direction(), "plak key set openrouter");
        assert!(remedy_catalog().contains(&openrouter.cli_direction()));
    }

    /// Spec 02 §5.5: `refresh outcome unknown` is explained in prose, since the
    /// status word deliberately does not carry quarantine nuance.
    #[test]
    fn a_quarantined_credential_is_explained_rather_than_merely_labelled() {
        let quarantined = def(
            "grok",
            AuthKind::Oauth,
            ProviderState::RefreshUnknown,
            "quarantined",
        );
        let description = quarantined.status_description();
        assert!(description.contains("unknown"), "{description}");
        assert!(description.contains("retry automatically"), "{description}");
        assert!(
            !description.contains("plak login grok"),
            "grok retries without re-login: {description}"
        );

        let codex = def(
            "codex",
            AuthKind::Oauth,
            ProviderState::RefreshUnknown,
            "quarantined",
        );
        let codex_description = codex.status_description();
        assert!(
            codex_description.contains("plak login codex"),
            "{codex_description}"
        );

        let plain = def("grok", AuthKind::Oauth, ProviderState::Expired, "");
        assert!(
            !plain.status_description().contains("quarantined"),
            "a plain expiry is not described as a quarantine"
        );
    }

    #[test]
    fn an_api_key_account_is_never_given_an_oauth_remedy() {
        for account in API_KEY_ACCOUNTS {
            assert_eq!(account_remedy(account), format!("plak key set {account}"));
        }
        for account in OAUTH_ACCOUNTS {
            assert_eq!(account_remedy(account), format!("plak login {account}"));
        }
        // An account outside both closed sets falls to the key route rather
        // than manufacturing an OAuth login the CLI would refuse.
        assert_eq!(account_remedy("mystery"), "plak key set mystery");
    }

    #[test]
    fn the_catalog_covers_both_auth_routes_and_names_no_legacy_alias() {
        let catalog = remedy_catalog();
        assert!(catalog.contains(&"plak login codex".to_owned()));
        assert!(catalog.contains(&"plak logout claude".to_owned()));
        assert!(catalog.contains(&"plak key set openrouter".to_owned()));
        assert!(catalog.contains(&"plak key clear anthropic".to_owned()));
        assert!(
            catalog.iter().all(|command| !command.contains("plak auth")),
            "no product remedy names the legacy alias: {catalog:?}"
        );
    }
}
