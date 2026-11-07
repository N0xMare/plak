//! Route identity and catalog policy, independent of runtime construction.
//!
//! The binding vocabulary remains complete. This registry does not change
//! serialization or select a different route when an exact account is absent.

use plakous::subscriptions::NativeProvider;

use crate::binding::{PlatformProvider, ResolvedProtocol};

#[cfg(not(all(
    feature = "codex",
    feature = "grok",
    feature = "claude",
    feature = "openai",
    feature = "xai-api",
    feature = "anthropic",
    feature = "openrouter"
)))]
mod route_unavailable;

mod build;
pub use build::{
    BUILD_CAPABILITIES, BuildCapability, CapabilityKind, CapabilityUnavailable, compiled_features,
    omitted_features, require_composition, require_feature,
};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteKind {
    Native(NativeProvider),
    Platform(PlatformProvider),
    OpenRouter,
}

#[derive(Debug)]
pub struct RouteCapability {
    pub compiled: bool,
    pub feature: &'static str,
    pub(crate) review_deadline: fn() -> Option<u64>,
    pub(crate) hosted_retrieval: fn() -> Option<plakous::model::HostedRetrievalTable>,
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
    pub fn review_deadline(&self) -> Option<u64> {
        (self.review_deadline)()
    }

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
    ROUTES
        .iter()
        .filter(|route| route.compiled)
        .filter_map(|route| route.native_provider())
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

/// Admits the compiled tool packs selected by an unchanged binding vocabulary.
///
/// # Errors
///
/// Returns the exact omitted tool-pack capability without selecting a substitute.
pub fn require_tools(
    selection: &crate::tool_plan::TemplateToolSelection,
) -> Result<(), CapabilityUnavailable> {
    use crate::tool_plan::{OptionalTool, SelectedTool};
    if selection
        .entries()
        .contains(&SelectedTool::Optional(OptionalTool::FetchUrl))
    {
        require_feature("web-fetch")?;
    }
    if selection.mcp_servers().next().is_some() {
        require_feature("mcp")?;
    }
    Ok(())
}

/// An account selection that cannot be admitted by this build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccountAdmissionError {
    UnknownAccount { account: String },
    AccountTargetMismatch { account: String },
    CapabilityUnavailable(CapabilityUnavailable),
}

impl std::fmt::Display for AccountAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAccount { account } => write!(formatter, "unknown_account: `{account}`"),
            Self::AccountTargetMismatch { account } => write!(
                formatter,
                "account_target_mismatch: `{account}` disagrees with the selected target"
            ),
            Self::CapabilityUnavailable(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AccountAdmissionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnknownAccount { .. } | Self::AccountTargetMismatch { .. } => None,
            Self::CapabilityUnavailable(error) => Some(error),
        }
    }
}

/// Refuses unknown or omitted accounts without choosing another route.
pub fn require(account: &str) -> Result<(), AccountAdmissionError> {
    let selection: crate::account::AccountRef = account.parse()?;
    if selection.is_named() && !selection.vertical().is_api() {
        return Err(AccountAdmissionError::UnknownAccount {
            account: account.to_owned(),
        });
    }
    let route = selection.vertical().capability();
    require_route(route).map_err(AccountAdmissionError::CapabilityUnavailable)
}

fn require_route(route: &RouteCapability) -> Result<(), CapabilityUnavailable> {
    if route.compiled {
        return Ok(());
    }
    Err(CapabilityUnavailable {
        id: route.account,
        kind: CapabilityKind::Route,
        feature: route.feature,
    })
}

pub fn require_native(provider: NativeProvider) -> Result<(), CapabilityUnavailable> {
    if let Some(route) = for_native(provider) {
        return require_route(route);
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
            assert_eq!(
                require(account).is_ok(),
                for_account(account).unwrap().compiled
            );
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
