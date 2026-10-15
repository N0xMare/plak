//! Admitting and compiling the exact candidate for one provider route.
//!
//! The answer needs both halves the core does not have: the route catalog that
//! chooses a vocabulary, and the packs that fill it. The composition root hands
//! the first in and this crate holds the second, so the candidate compiler
//! lives here and reaches the agent through
//! `plakous_tools::ProjectionCandidateAdmission`.

use plakous_model::{CompiledProviderConfiguration, IntegrationClass, ToolName};
use plakous_tools::{
    CodingPresentation, FrozenToolRegistry, PROCESS_TOOL_PROJECTION_REVISION, PresentationRoutes,
    ProviderModelProjectionIdentityV2, ToolProjectionMap, ToolRuntimeError, ToolRuntimeErrorKind,
};

use crate::coding_menu::CodingPresentationExt as _;

/// Compiles the exact process-presentation candidate for a standard provider
/// configuration.
///
/// # Errors
///
/// Rejects custom integrations, missing family identity, neighboring models,
/// effective-model drift, or any configuration without an exact candidate.
pub fn process_presentation_candidate_v2(
    job_config: plakous_tools::JobConfig,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    routes: &PresentationRoutes,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    process_presentation_candidate_with_extensions_v2(
        job_config,
        registry,
        configuration,
        routes,
        &[],
    )
}

/// Compiles the exact process-presentation candidate plus explicit product extras.
///
/// Empty extras preserve [`process_presentation_candidate_v2`]. Registry
/// membership or `NestedEligible` alone never selects extras.
///
/// # Errors
///
/// Rejects custom integrations, missing family identity, neighboring models,
/// effective-model drift, colliding extras, or any configuration without an
/// exact candidate.
pub fn process_presentation_candidate_with_extensions_v2(
    job_config: plakous_tools::JobConfig,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    routes: &PresentationRoutes,
    extra_nested: &[ToolName],
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let effective = configuration.effective();
    let descriptor = effective.descriptor();
    if descriptor.integration_class() != IntegrationClass::Standard {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "custom integrations cannot select a standard process-presentation candidate",
        ));
    }
    let family = descriptor.model_family().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "process-presentation candidate requires an exact model family",
        )
    })?;
    let requested = descriptor.requested_model().as_str();
    let effective_model = effective.effective_model().as_str();
    let kind = CodingPresentation::admit(configuration, routes)?;
    let target = ProviderModelProjectionIdentityV2::new(
        descriptor.integration_class(),
        descriptor.protocol().as_str(),
        descriptor.route().as_str(),
        descriptor.profile().as_str(),
        family.as_str(),
        requested,
        effective_model,
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    kind.projection_map_with_extensions(job_config, registry, target, extra_nested)
}

/// Recompiles and compares the catalog candidate for one exact provider
/// configuration.
///
/// # Errors
///
/// Rejects every target-axis, registry, operation-set, schema, lowering,
/// raising, or projection-kind drift without fallback.
pub fn validate_process_presentation_candidate_v2(
    job_config: plakous_tools::JobConfig,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    routes: &PresentationRoutes,
) -> Result<(), ToolRuntimeError> {
    if !projection.is_for_registry(&registry) {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "process projection belongs to a foreign registry snapshot",
        ));
    }
    let core =
        process_presentation_candidate_v2(job_config, registry.clone(), configuration, routes)?;
    let extra = crate::coding_menu::extension_names(projection, &core);
    let expected = if extra.is_empty() {
        core
    } else {
        process_presentation_candidate_with_extensions_v2(
            job_config,
            registry,
            configuration,
            routes,
            &extra,
        )?
    };
    if projection.target() != expected.target() || projection.digest() != expected.digest() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "process projection is not the exact provider/model catalog candidate",
        ));
    }
    Ok(())
}
