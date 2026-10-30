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
pub(crate) use crate::capabilities::OAUTH_ACCOUNTS;

/// The API-key accounts `plak key set` / `plak key clear` accept.
#[cfg(test)]
pub(crate) use crate::capabilities::API_KEY_ACCOUNTS;

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
#[cfg(test)]
pub(crate) use crate::product_config::settings::remedy_catalog;
pub(crate) use crate::product_config::settings::{
    API_KEY_ACCOUNT_PLACEHOLDER, OAUTH_ACCOUNT_PLACEHOLDER, account_remedy, api_key_remedy,
    oauth_login_remedy,
};
#[cfg(test)]
mod tests;
