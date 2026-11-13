//! Synthetic discovery through the same bounded document parsers.

use super::{
    OpenRouterChatDiscoveryError, exact_literal, validate_endpoints_document,
    validate_models_document,
};
use crate::{OpenRouterChatModelEvidence, OpenRouterChatRoute};
use plakous_model::Timestamp;
use serde_json::Value;

impl OpenRouterChatRoute {
    /// Parses synthetic discovery documents without network or credential reads.
    /// Available only with the explicit fixture-clock feature; never a standard
    /// integration admission or production discovery substitute.
    ///
    /// # Errors
    /// Returns the production document/literal validation failure.
    #[doc(hidden)]
    pub fn fixture_model_evidence(
        &self,
        literal: &str,
        models: &Value,
        endpoints: &Value,
    ) -> Result<OpenRouterChatModelEvidence, OpenRouterChatDiscoveryError> {
        let (_, _, model) = exact_literal(literal)?;
        let facts = validate_models_document(models, literal)?;
        let candidates = validate_endpoints_document(endpoints, literal)?;
        let observed_at = self.clock.now();
        let expires_at = Timestamp::new(
            observed_at
                .seconds()
                .checked_add(3600)
                .ok_or(OpenRouterChatDiscoveryError::InvalidPolicy)?,
            observed_at.nanoseconds(),
        )?;
        OpenRouterChatModelEvidence::from_authenticated_discovery(
            model,
            self.identity(),
            crate::chat::OpenRouterChatDiscovery {
                endpoints: candidates,
                model_supported_parameters: facts.supported_parameters,
                reasoning: facts.reasoning,
                context_length: facts.context_length,
                image_input: facts.image_input,
                observed_at,
                expires_at,
            },
        )
        .map_err(OpenRouterChatDiscoveryError::from)
    }
}
