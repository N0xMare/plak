//! The built-in native providers and their pinned route tuples.

use super::{
    CredentialSelectionId, ModelAdapterProfileId, ModelId, NativeSubscriptionError, NativeToolMode,
    PRIMARY_SLOT, ProtocolId, ProviderAuthorityClassId, ProviderProfileRevisionId,
    ProviderRouteSelection, RouteId, facade_native_tool_mode,
};

/// A built-in Plak-owned subscription integration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NativeProvider {
    /// `ChatGPT` subscription access through the pinned Codex compatibility path.
    Codex,
    /// `SuperGrok` subscription access through the pinned xAI compatibility path.
    Xai,
    /// Claude Max OAuth access through the pinned Messages Claude OAuth path.
    Claude,
}

impl NativeProvider {
    /// Returns the stable command-line identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Xai => "xai",
            Self::Claude => "claude",
        }
    }

    /// Every enrolled coding model id for this integration.
    ///
    /// The answer is the provider crate's own enrollment table; no other list
    /// of native model ids exists. Catalog observation is still required
    /// before a listed model can run.
    #[must_use]
    pub fn enrolled_models(self) -> Vec<&'static str> {
        match self {
            Self::Codex => plakous_openai::RELEASE_ENROLLED_CODEX_MODELS
                .iter()
                .map(|(model, _)| *model)
                .collect(),
            Self::Xai => plakous_xai::subscription_coding_model_ids().collect(),
            Self::Claude => plakous_anthropic::claude_oauth_enrolled_models().collect(),
        }
    }

    /// The source-backed native tool mode an enrolled literal is pinned to.
    /// Only Codex rows carry one.
    #[must_use]
    pub fn enrolled_tool_mode(self, model: &str) -> Option<NativeToolMode> {
        match self {
            Self::Codex => {
                facade_native_tool_mode(plakous_openai::release_enrolled_codex_tool_mode(model))
            }
            Self::Xai | Self::Claude => None,
        }
    }

    /// Whether an exact literal is an enrolled coding model for this integration.
    #[must_use]
    pub fn enrolls(self, model: &str) -> bool {
        match self {
            Self::Codex => plakous_openai::release_enrolled_codex_model(model),
            Self::Xai => plakous_xai::is_subscription_coding_model_id(model),
            Self::Claude => plakous_anthropic::claude_oauth_enrolled_model(model),
        }
    }
}

/// Materializes the complete pinned route/auth tuple for one explicit native
/// provider and literal model selection.
///
/// This does not choose a provider, credential slot, or model. It exposes the
/// immutable integration facts needed to construct a complete
/// [`SelectedProviderConfiguration`](super::SelectedProviderConfiguration)
/// without duplicating provider pins in an application.
pub fn native_provider_route_selection(
    provider: NativeProvider,
    requested_model: ModelId,
) -> Result<ProviderRouteSelection, NativeSubscriptionError> {
    let ids = match provider {
        NativeProvider::Codex => (
            plakous_openai::OPENAI_CODEX_RESPONSES_PROTOCOL_ID,
            plakous_openai::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
            plakous_openai::OPENAI_RESPONSES_ADAPTER_REVISION,
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
            plakous_openai::OPENAI_CODEX_SOURCE_REVISION,
        ),
        NativeProvider::Xai => (
            plakous_xai::SUPERGROK_RESPONSES_DIALECT.protocol_id(),
            plakous_xai::SUPERGROK_RESPONSES_DIALECT.route().route_id(),
            plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE,
            plakous_xai::XAI_RESPONSES_ADAPTER_REVISION,
            plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE,
            plakous_xai::XAI_GROK_BUILD_SOURCE_REVISION,
        ),
        NativeProvider::Claude => (
            plakous_anthropic::CLAUDE_OAUTH_PROTOCOL_ID,
            plakous_anthropic::CLAUDE_OAUTH_ROUTE_ID,
            plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            plakous_anthropic::CLAUDE_OAUTH_ADAPTER_REVISION,
            plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
            plakous_anthropic::CLAUDE_OAUTH_SOURCE_REVISION,
        ),
    };
    Ok(ProviderRouteSelection::new(
        ProtocolId::new(ids.0).map_err(|_| NativeSubscriptionError::Internal)?,
        RouteId::new(ids.1).map_err(|_| NativeSubscriptionError::Internal)?,
        ModelAdapterProfileId::new(ids.2).map_err(|_| NativeSubscriptionError::Internal)?,
        ProviderProfileRevisionId::new(ids.3).map_err(|_| NativeSubscriptionError::Internal)?,
        ModelAdapterProfileId::new(ids.4).map_err(|_| NativeSubscriptionError::Internal)?,
        ProviderProfileRevisionId::new(ids.5).map_err(|_| NativeSubscriptionError::Internal)?,
        ProviderAuthorityClassId::new("oauth-subscription")
            .map_err(|_| NativeSubscriptionError::Internal)?,
        CredentialSelectionId::new(PRIMARY_SLOT).map_err(|_| NativeSubscriptionError::Internal)?,
        requested_model,
    ))
}
