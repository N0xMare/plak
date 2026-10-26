//! The built-in native providers and their pinned route tuples.

use super::{
    CredentialSelectionId, ModelAdapterProfileId, ModelId, NativeSubscriptionError, NativeToolMode,
    PRIMARY_SLOT, ProtocolId, ProviderAuthorityClassId, ProviderProfileRevisionId,
    ProviderRouteSelection, RouteId,
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
    /// Whether this exact native route was compiled into the facade.
    #[must_use]
    pub fn is_compiled(self) -> bool {
        super::vertical::vertical(self).is_some()
    }

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
        super::vertical::vertical(self)
            .map_or_else(Vec::new, super::vertical::NativeVertical::enrolled_models)
    }

    /// The source-backed native tool mode an enrolled literal is pinned to.
    /// Only Codex rows carry one.
    #[must_use]
    pub fn enrolled_tool_mode(self, model: &str) -> Option<NativeToolMode> {
        super::vertical::vertical(self).and_then(|vertical| vertical.enrolled_tool_mode(model))
    }

    /// Whether an exact literal is an enrolled coding model for this integration.
    #[must_use]
    pub fn enrolls(self, model: &str) -> bool {
        super::vertical::vertical(self).is_some_and(|vertical| vertical.enrolls(model))
    }
}

/// Materializes the complete pinned route/auth tuple for one explicit native
/// provider and literal model selection.
///
/// This does not choose a provider, credential slot, or model. It exposes the
/// immutable integration facts needed to construct a complete
/// [`SelectedProviderConfiguration`](super::SelectedProviderConfiguration)
/// without duplicating provider pins in an application.
///
/// # Errors
///
/// Returns [`NativeSubscriptionError::Internal`] if a pinned identifier does
/// not validate.
pub fn native_provider_route_selection(
    provider: NativeProvider,
    requested_model: ModelId,
) -> Result<ProviderRouteSelection, NativeSubscriptionError> {
    let ids = super::vertical::require_vertical(provider)?.route_ids();
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
