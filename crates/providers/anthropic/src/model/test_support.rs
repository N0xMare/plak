//! Explicit-clock construction for offline binding evidence.

use super::{AnthropicLanguageModel, configuration_error};
use crate::{AnthropicError, AnthropicKeySource};
use plakous_http::{Client, Limits};
use plakous_model::{Clock, ConfiguredLanguageModel, ProviderBehaviorConfiguration};
use std::{num::NonZeroU64, sync::Arc};

impl AnthropicLanguageModel {
    /// Builds synthetic binding evidence with an explicit clock, without dispatch.
    /// Production constructors always use the system clock.
    ///
    /// # Errors
    /// Returns the same configuration and route validation failures.
    #[doc(hidden)]
    pub fn configured_with_fixture_clock(
        source: Arc<AnthropicKeySource>,
        account: &str,
        model: &crate::AnthropicApiModel,
        admitted_facts: &[u8],
        behavior: ProviderBehaviorConfiguration,
        maximum_projected_input: NonZeroU64,
        clock: Arc<dyn Clock>,
    ) -> Result<ConfiguredLanguageModel, AnthropicError> {
        Self::build(
            source,
            account,
            model,
            admitted_facts,
            behavior,
            maximum_projected_input,
            Client::new_http1(Limits::for_streaming_inference())
                .map_err(configuration_error)?
                .pooled(),
            None,
            clock,
        )
        .map(|(configured, _)| configured)
    }
}
