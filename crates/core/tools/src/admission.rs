//! Who says a compiled projection is the exact candidate for a route.
//!
//! A V2 projection is executable authority: it names canonical capabilities and
//! the exposed names a model may call. Admitting one is *not* a fact the agent
//! can derive — the answer depends on which packs the composition root
//! enrolled and which routes it declared — so the agent takes the answer as a
//! port and refuses when it is not supplied.
//!
//! The one mechanical rule that needs no catalog stays here:
//! [`validate_custom_process_presentation_v2`] compares a custom integration's
//! projection target against its own provider configuration, field for field.

use std::fmt;

use plakous_model::{CompiledProviderConfiguration, IntegrationClass};

use crate::capabilities::PROCESS_TOOL_PROJECTION_REVISION;
use crate::{FrozenToolRegistry, ToolProjectionMapV2, ToolRuntimeError, ToolRuntimeErrorKind};

/// Admits one compiled V2 projection as the exact candidate for one provider
/// configuration.
///
/// The composition root implements this: it knows the pack list the projection
/// was compiled from and the route catalog that chose the vocabulary, and it
/// recompiles the candidate to compare.
pub trait ProjectionCandidateAdmission: Send + Sync + fmt::Debug {
    /// Reports whether this exact projection is the qualified candidate.
    ///
    /// # Errors
    ///
    /// Returns a registration error when the projection is not the exact
    /// candidate for this registry and provider configuration.
    /// `job_config` is the construction bound, restored from durable semantics
    /// on exact reopen rather than read from live product configuration.
    fn admit(
        &self,
        job_config: crate::JobConfig,
        projection: &ToolProjectionMapV2,
        registry: &FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), ToolRuntimeError>;
}

/// Validates a mechanically exact V2 projection selected by a Custom
/// integration without promoting it to a maintained provider candidate.
///
/// Every axis of the projection target must equal the compiled configuration's
/// own. No catalog and no pack list is consulted, because a custom integration
/// has no enrolled candidate to recompile.
///
/// # Errors
///
/// Rejects a foreign registry snapshot, a missing model family, and every
/// target-axis disagreement.
pub fn validate_custom_process_presentation_v2(
    projection: &ToolProjectionMapV2,
    registry: &FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
) -> Result<(), ToolRuntimeError> {
    if !projection.is_for_registry(registry) {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "custom process projection belongs to a foreign registry snapshot",
        ));
    }
    let effective = configuration.effective();
    let descriptor = effective.descriptor();
    let family = descriptor.model_family().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "custom process projection target has no exact model family",
        )
    })?;
    let target = projection.target();
    if target.integration_class() != IntegrationClass::Custom
        || descriptor.integration_class() != IntegrationClass::Custom
        || target.protocol() != descriptor.protocol().as_str()
        || target.route() != descriptor.route().as_str()
        || target.integration_profile() != descriptor.profile().as_str()
        || target.family() != family.as_str()
        || target.requested_model() != descriptor.requested_model().as_str()
        || target.effective_model() != effective.effective_model().as_str()
        || target.effective_provider_configuration() != configuration.effective_identity()
        || target.projection_revision() != PROCESS_TOOL_PROJECTION_REVISION
    {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "custom process projection disagrees with its exact Custom provider target",
        ));
    }
    Ok(())
}
