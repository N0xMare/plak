//! Route identity and catalog policy, independent of runtime construction.
//!
//! The binding vocabulary remains complete. This registry does not change
//! serialization or select a different route when an exact account is absent.

use plakous::subscriptions::NativeProvider;

use crate::binding::{PlatformProvider, ResolvedProtocol};

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
pub use openai::OPENAI;
pub use openrouter::OPENROUTER;
pub use xai::XAI;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteKind {
    Native(NativeProvider),
    Platform(PlatformProvider),
    OpenRouter,
}

#[derive(Debug)]
pub struct RouteCapability {
    pub account: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub kind: RouteKind,
    /// OpenRouter resolves its wire from the explicit selection instead.
    pub(crate) protocol: Option<ResolvedProtocol>,
    pub(crate) catalog_label: &'static str,
    pub(crate) native_ptc_refusal: &'static str,
    /// Whether `native-ptc` belongs to the route's binding vocabulary at all;
    /// model enrollment is a separate admission check.
    pub(crate) native_protocol_vocabulary: bool,
    pub usage_label: Option<&'static str>,
    pub(crate) subscription_label: Option<&'static str>,
}

impl RouteCapability {
    pub const fn native_provider(&self) -> Option<NativeProvider> {
        match self.kind {
            RouteKind::Native(provider) => Some(provider),
            RouteKind::Platform(_) | RouteKind::OpenRouter => None,
        }
    }

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

pub const fn for_account(account: &str) -> Option<&'static RouteCapability> {
    let mut index = 0;
    while index < ROUTES.len() {
        let route = ROUTES[index];
        let mut alias = 0;
        while alias < route.aliases.len() {
            if same_id(route.aliases[alias], account) {
                return Some(route);
            }
            alias += 1;
        }
        index += 1;
    }
    None
}

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

pub const fn for_platform(provider: PlatformProvider) -> &'static RouteCapability {
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

#[cfg(test)]
mod tests {
    use super::*;

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
