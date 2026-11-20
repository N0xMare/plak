//! Route identity and catalog policy, independent of runtime construction.
//!
//! The binding vocabulary remains complete. This registry does not change
//! serialization or select a different route when an exact account is absent.

use plakous::subscriptions::NativeProvider;

use crate::binding::{PlatformProvider, ResolvedProtocol};

mod build;
pub use build::{CapabilityKind, CapabilityUnavailable, PRODUCT_CAPABILITIES};

mod anthropic;
mod claude;
mod codex;
mod grok;
mod openai;
mod openrouter;
mod xai;

pub use anthropic::ANTHROPIC;
pub use claude::CLAUDE;
pub use codex::CODEX;
pub use grok::GROK;
pub(crate) use grok::tool_result_images as supergrok_tool_result_images;
pub use openai::OPENAI;
pub use openrouter::OPENROUTER;
pub use xai::XAI;

/// Credential and model-construction family, independent of build admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteKind {
    /// Subscription OAuth and the provider's native model implementation.
    Native(NativeProvider),
    /// Direct API-key authentication with reviewed provider model facts.
    Platform(PlatformProvider),
    /// Gateway API-key authentication with separately selected wire semantics.
    OpenRouter,
}

/// Static route registry entry.
#[derive(Debug)]
pub struct RouteCapability {
    pub(crate) review_deadline: fn() -> Option<u64>,
    pub(crate) hosted_retrieval: fn() -> Option<plakous::model::HostedRetrievalTable>,
    /// Canonical default-account prefix; named accounts retain this route.
    pub account: &'static str,
    pub(crate) aliases: &'static [&'static str],
    /// Authentication and model-construction family, never a fallback order.
    pub kind: RouteKind,
    /// `OpenRouter` resolves its wire from the explicit selection instead.
    pub(crate) protocol: Option<ResolvedProtocol>,
    pub(crate) catalog_label: &'static str,
    pub(crate) native_ptc_refusal: &'static str,
    /// Whether `native-ptc` belongs to the route's binding vocabulary at all;
    /// model enrollment is a separate admission check.
    pub(crate) native_protocol_vocabulary: bool,
    /// Subscription-usage presentation label, absent on routes without one.
    pub usage_label: Option<&'static str>,
    pub(crate) subscription_label: Option<&'static str>,
}

impl RouteCapability {
    /// Compiled review-window deadline as Unix seconds, absent when the route
    /// supplies no review deadline. Reading it neither refreshes nor admits it.
    #[must_use]
    pub fn review_deadline(&self) -> Option<u64> {
        (self.review_deadline)()
    }

    /// Native identity only for subscription routes, never an API alias.
    #[must_use]
    pub const fn native_provider(&self) -> Option<NativeProvider> {
        match self.kind {
            RouteKind::Native(provider) => Some(provider),
            RouteKind::Platform(_) | RouteKind::OpenRouter => None,
        }
    }

    /// Direct API identity only; gateway and subscription routes return `None`.
    #[must_use]
    pub const fn platform_provider(&self) -> Option<PlatformProvider> {
        match self.kind {
            RouteKind::Platform(provider) => Some(provider),
            RouteKind::Native(_) | RouteKind::OpenRouter => None,
        }
    }
}

/// Catalog/credential presentation order, including native refresh order.
pub const ROUTES: &[&RouteCapability] = &[
    &CODEX,
    &GROK,
    &CLAUDE,
    &OPENAI,
    &XAI,
    &ANTHROPIC,
    &OPENROUTER,
];

const fn same_id(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Parse an exact default or named account and return its route without
/// reading credentials. Invalid identities return `None`.
#[must_use]
pub fn for_account(account: &str) -> Option<&'static RouteCapability> {
    account
        .parse::<crate::account::AccountRef>()
        .ok()
        .map(|account| account.vertical().capability())
}

/// Find a known native identity without checking build availability. An
/// unregistered identity returns `None`, never another provider's entry.
#[must_use]
pub const fn for_native(provider: NativeProvider) -> Option<&'static RouteCapability> {
    let mut index = 0;
    while index < ROUTES.len() {
        let route = ROUTES[index];
        if let RouteKind::Native(candidate) = route.kind
            && same_id(candidate.as_str(), provider.as_str())
        {
            return Some(route);
        }
        index += 1;
    }
    None
}

pub(crate) const fn for_platform(provider: PlatformProvider) -> &'static RouteCapability {
    let mut index = 0;
    while index < ROUTES.len() {
        let route = ROUTES[index];
        if let RouteKind::Platform(candidate) = route.kind
            && candidate as u8 == provider as u8
        {
            return route;
        }
        index += 1;
    }
    // PlatformProvider is a closed, ungated identity vocabulary. Missing an
    // identity is a registry programming error, not a fallback to another API.
    unreachable!()
}

/// Compiled native routes in registry order; API routes are never included.
pub fn native_providers() -> impl Iterator<Item = NativeProvider> {
    ROUTES.iter().filter_map(|route| route.native_provider())
}

/// Complete, ungated account vocabulary used by frozen template bind hints.
pub fn account_ids() -> impl Iterator<Item = &'static str> {
    ROUTES
        .iter()
        .flat_map(|route| route.aliases.iter().copied())
}

/// Complete, ungated vocabulary, not a test for a model's native enrollment.
pub fn no_native_protocol_accounts() -> impl Iterator<Item = &'static str> {
    ROUTES
        .iter()
        .filter(|route| !route.native_protocol_vocabulary)
        .map(|route| route.account)
}

/// An account selection that cannot be admitted by this build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccountAdmissionError {
    /// The selection does not parse as a known exact account identity.
    UnknownAccount {
        /// Refused input, not a fallback credential reference.
        account: String,
    },
    /// The account's route disagrees with the explicit target.
    AccountTargetMismatch {
        /// Exact account that failed target validation.
        account: String,
    },
    /// The selected account is not in the ready credential state.
    AccountUnavailable {
        /// Exact account whose credential state prevents admission.
        account: String,
    },
    /// An account operation holds its exclusive lifecycle slot.
    AccountBusy {
        /// Exact account whose current operation must settle before retry.
        account: String,
    },
    /// A destructive account action requires sign-out first.
    AccountSignedIn {
        /// Account whose signed-in state prevents the action.
        account: String,
    },
    /// Default account identity cannot be deleted like a named account.
    DefaultAccountNotRemovable {
        /// Default account whose identity must remain readable.
        account: String,
    },
}

impl std::fmt::Display for AccountAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAccount { account } => write!(formatter, "unknown_account: `{account}`"),
            Self::AccountTargetMismatch { account } => write!(
                formatter,
                "account_target_mismatch: `{account}` disagrees with the selected target"
            ),
            Self::AccountUnavailable { account } => {
                write!(formatter, "account_unavailable: `{account}`")
            }
            Self::AccountBusy { account } => write!(
                formatter,
                "account_busy: `{account}` is busy; retry after the account operation finishes"
            ),
            Self::AccountSignedIn { account } => write!(
                formatter,
                "account_signed_in: `{account}`; log out first with `plakd logout {account}`"
            ),
            Self::DefaultAccountNotRemovable { account } => write!(
                formatter,
                "default_account_not_removable: `{account}`; use logout or key clear"
            ),
        }
    }
}

impl std::error::Error for AccountAdmissionError {}

/// Refuses invalid account syntax without choosing another route or reading credentials.
///
/// # Errors
/// Returns an unknown-account refusal for invalid syntax.
pub fn require(account: &str) -> Result<(), AccountAdmissionError> {
    account.parse::<crate::account::AccountRef>().map(|_| ())
}

/// Admit a native route by identity without resolving subscription credentials.
///
/// # Errors
/// Returns a capability refusal for an unregistered identity.
pub fn require_native(provider: NativeProvider) -> Result<(), CapabilityUnavailable> {
    if for_native(provider).is_some() {
        return Ok(());
    }
    Err(CapabilityUnavailable {
        id: provider.as_str(),
        kind: CapabilityKind::Route,
        feature: provider.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_accounts_never_acquire_a_route() {
        for account in [
            "openai/work/extra",
            "unknown",
            "",
            "xai",
            "codex/",
            "OPENAI",
        ] {
            assert_eq!(
                require(account),
                Err(AccountAdmissionError::UnknownAccount {
                    account: account.into()
                })
            );
        }
        for account in account_ids() {
            assert!(require(account).is_ok());
        }
    }

    #[test]
    fn registered_accounts_and_aliases_are_unique_and_exact() {
        let mut accounts = std::collections::BTreeSet::new();
        for route in ROUTES {
            assert!(route.aliases.contains(&route.account));
            if route.native_provider().is_some() {
                assert!(route.usage_label.is_some());
                assert!(route.subscription_label.is_some());
                assert!(route.protocol.is_some());
            }
            for alias in route.aliases {
                assert!(accounts.insert(*alias), "duplicate alias {alias}");
                assert!(std::ptr::eq(for_account(alias).unwrap(), *route));
            }
        }
        assert!(for_account("xai").is_none());
        assert!(for_account("CODEX").is_none());
        assert!(for_account(" codex").is_none());
    }

    #[test]
    fn native_order_and_platform_identity_coverage_are_complete() {
        assert_eq!(
            native_providers().collect::<Vec<_>>(),
            [
                NativeProvider::Codex,
                NativeProvider::Xai,
                NativeProvider::Claude,
            ]
        );
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            assert_eq!(for_platform(provider).platform_provider(), Some(provider));
        }
    }
}
