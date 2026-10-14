//! OpenRouter-owned Responses dialect identities and closed host data.

use plakous_model::{ModelFamilyId, ModelId, ProtocolId, ReplayKindId};
use plakous_responses::{LifecycleDialectPolicy, ResponsesDialectProfile};
use plakous_responses_dialect::{
    ResponsesReplayProjection, ResponsesRequestPolicy, ResponsesStreamDialectData,
    ResponsesUsagePolicy,
};
use serde::{Deserialize, Serialize};

/// Maximum freshness window accepted for authenticated dynamic discovery.
pub const OPENROUTER_EVIDENCE_MAX_AGE_SECONDS: u64 = 14 * 24 * 60 * 60;
/// Fixed OpenRouter Responses resource.
pub const OPENROUTER_RESPONSES_PATH: &str = "/api/v1/responses";
/// Dynamic Custom/experimental Responses protocol identity.
pub const OPENROUTER_RESPONSES_PROTOCOL_ID: &str = "openrouter.responses.v1";
/// Current OpenRouter Responses host-table revision.
pub const OPENROUTER_RESPONSES_DIALECT_REVISION: u32 = 3;
/// Completed output-item replay envelope retained by the Responses artifact.
pub const OPENROUTER_OUTPUT_ITEM_REPLAY_KIND: &str = "openrouter.responses.output-item";
/// Revision of [`OPENROUTER_OUTPUT_ITEM_REPLAY_KIND`].
pub const OPENROUTER_OUTPUT_ITEM_REPLAY_VERSION: u32 = 3;

/// One current OpenRouter Responses grammar.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OpenRouterDialect {
    /// Stateless OpenRouter Responses at the fixed `/api/v1/responses` resource.
    ResponsesV1,
}

impl OpenRouterDialect {
    #[cfg(test)]
    #[allow(
        non_upper_case_globals,
        reason = "temporary name retained only for an older cross-family unit assertion"
    )]
    pub(crate) const StatelessResponsesBeta2026_07: Self = Self::ResponsesV1;

    /// Returns the stable protocol identity used in target and replay bindings.
    #[must_use]
    pub const fn protocol_id(self) -> &'static str {
        match self {
            Self::ResponsesV1 => OPENROUTER_RESPONSES_PROTOCOL_ID,
        }
    }

    /// Returns the shared Responses stream grammar selected by this dialect.
    #[must_use]
    pub const fn responses_profile(self) -> ResponsesDialectProfile {
        match self {
            Self::ResponsesV1 => ResponsesDialectProfile::openrouter(),
        }
    }

    /// Returns the closed shared request policy for this stateless dialect.
    #[must_use]
    pub const fn request_policy(self) -> ResponsesRequestPolicy {
        match self {
            Self::ResponsesV1 => ResponsesRequestPolicy::openrouter_stateless(),
        }
    }

    /// Returns lifecycle deltas consumed by the shared stream host.
    #[must_use]
    pub const fn lifecycle_policy(self) -> LifecycleDialectPolicy {
        match self {
            Self::ResponsesV1 => LifecycleDialectPolicy::openrouter(),
        }
    }

    /// OpenRouter response usage requires a provider-reported exact total.
    #[must_use]
    pub const fn usage_policy(self) -> ResponsesUsagePolicy {
        ResponsesUsagePolicy::RequireTotal
    }

    /// Builds complete shared stream data for one dynamically admitted literal.
    #[must_use]
    pub fn stream_data(self, expected_model: &ModelId) -> ResponsesStreamDialectData {
        match self {
            Self::ResponsesV1 => ResponsesStreamDialectData::openrouter(expected_model.as_str()),
        }
    }

    /// Returns the item-level replay projection owned by the Responses family.
    pub fn replay_projection(
        self,
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        model: ModelId,
    ) -> Result<ResponsesReplayProjection, plakous_responses_dialect::ResponsesProjectError> {
        ResponsesReplayProjection::new(
            protocol,
            model_family,
            model,
            ReplayKindId::new(OPENROUTER_OUTPUT_ITEM_REPLAY_KIND).map_err(|_| {
                plakous_responses_dialect::ResponsesProjectError::ProjectionIdentity
            })?,
            OPENROUTER_OUTPUT_ITEM_REPLAY_VERSION,
        )
    }
}

/// One immutable OpenRouter billing/auth family.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OpenRouterRouteKind {
    /// Separately billed OpenRouter API access.
    Platform,
}

impl OpenRouterRouteKind {
    /// Returns the stable route identifier.
    #[must_use]
    pub const fn route_id(self) -> &'static str {
        match self {
            Self::Platform => "openrouter.platform",
        }
    }
}

/// Credential and billing authority attached to a route.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OpenRouterAccessMode {
    /// An origin-bound OpenRouter Platform API key.
    PlatformApiKey,
    /// A loopback-only deterministic transport fixture; never production access.
    LocalFixture,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_dynamic_dialect_has_no_dated_or_beta_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let dialect = OpenRouterDialect::ResponsesV1;
        assert_eq!(dialect.protocol_id(), OPENROUTER_RESPONSES_PROTOCOL_ID);
        assert!(!dialect.protocol_id().contains("beta"));
        assert!(!dialect.protocol_id().contains("2026"));
        assert_eq!(
            dialect.request_policy(),
            ResponsesRequestPolicy::openrouter_stateless()
        );
        assert_eq!(
            dialect.responses_profile(),
            ResponsesDialectProfile::openrouter()
        );
        assert_eq!(
            dialect.lifecycle_policy(),
            LifecycleDialectPolicy::openrouter()
        );
        let model = ModelId::new("vendor/dynamic-model")?;
        assert_eq!(
            dialect.stream_data(&model).usage_policy(),
            ResponsesUsagePolicy::RequireTotal
        );
        Ok(())
    }
}
