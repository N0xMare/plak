//! Stable identity for the one admitted xAI Responses dialect.

use plakous_responses::ResponsesDialectProfile;
use serde::{Deserialize, Serialize};

/// Research baseline for the shared Grok subscription Responses grammar.
pub(crate) const XAI_SUBSCRIPTION_RESEARCH_BASELINE: &str = "2026-08-22";

/// The versioned SuperGrok Responses grammar.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum XaiDialect {
    /// The xAI Platform API-key Responses grammar.
    PlatformResponses2026_08,
    /// The Grok subscription route's shared xAI Responses grammar.
    GrokSubscriptionResponses2026_07,
}

impl XaiDialect {
    /// Returns the stable protocol identity used in route and replay bindings.
    #[must_use]
    pub const fn protocol_id(self) -> &'static str {
        match self {
            Self::PlatformResponses2026_08 => "xai.platform.responses.2026-08",
            Self::GrokSubscriptionResponses2026_07 => "xai.responses.grok-subscription.2026-07",
        }
    }

    /// Returns the stream terminal convention for this exact grammar.
    #[must_use]
    pub const fn terminal_convention(self) -> XaiTerminalConvention {
        match self {
            Self::PlatformResponses2026_08 => XaiTerminalConvention::TypedResponseEvent,
            Self::GrokSubscriptionResponses2026_07 => XaiTerminalConvention::TypedResponseEvent,
        }
    }

    /// Returns the exact Responses-family wire profile used by the adapter.
    /// Profiles parameterize shared sequence/terminal/BOM policy; deterministic
    /// adapter coverage does not constitute live provider qualification.
    #[must_use]
    pub const fn responses_profile(self) -> Option<ResponsesDialectProfile> {
        match self {
            Self::PlatformResponses2026_08 => Some(ResponsesDialectProfile::xai()),
            Self::GrokSubscriptionResponses2026_07 => Some(ResponsesDialectProfile::xai()),
        }
    }
}

/// The terminal framing accepted by an xAI stream decoder.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum XaiTerminalConvention {
    /// The documented public stream framing ends with `data: [DONE]`.
    DoneSentinel,
    /// A typed `response.completed`, `response.incomplete`, or `response.failed` event is required.
    TypedResponseEvent,
}

/// One immutable xAI route family.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum XaiRouteKind {
    /// Separately billed xAI Platform API-key access.
    PlatformApiKey,
    /// Subscription-funded Grok product access.
    GrokSubscription,
}

impl XaiRouteKind {
    /// Returns the stable route identifier.
    #[must_use]
    pub const fn route_id(self) -> &'static str {
        match self {
            Self::PlatformApiKey => "xai.platform_api_key",
            Self::GrokSubscription => "xai.grok_subscription",
        }
    }
}

/// Credential and billing authority attached to a route.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum XaiAccessMode {
    /// Embedder-owned xAI Platform API-key authority.
    PlatformApiKey,
    /// A local transport fixture without production credential authority.
    LocalTransportOnly,
    /// OAuth bearer access funded by the selected Grok subscription account.
    GrokSubscriptionOAuth,
    /// Subscription access is blocked because no OAuth credential source was configured.
    NonRoutableSubscription,
}
