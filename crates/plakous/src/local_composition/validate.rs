//! Configuration validation for the product JavaScript and Python integrations.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

#[allow(
    clippy::too_many_arguments,
    reason = "process budgets are an extra exact construction identity, not a new concern"
)]
pub(super) fn validate_product_configuration(
    job_config: plakous_tools::JobConfig,
    descriptor: &LocalCompositionDescriptor,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    nested_names: &[ToolName],
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: &ProductExtensionEvidence,
) -> Result<(), String> {
    if !projection.is_for_registry(&registry)
        || !descriptor.outer_projection.is_for_registry(&registry)
        || !descriptor.nested_projection.is_for_registry(&registry)
    {
        return Err("product JavaScript projection belongs to a foreign registry".into());
    }
    let effective = configuration.effective();
    let model = effective.descriptor();
    let family = model
        .model_family()
        .ok_or_else(|| "product JavaScript target has no exact model family".to_owned())?;
    let expected_target = ProviderModelProjectionIdentityV2::new(
        model.integration_class(),
        model.protocol().as_str(),
        model.route().as_str(),
        model.profile().as_str(),
        family.as_str(),
        model.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|error| error.to_string())?;
    if !matches!(
        expected_target.integration_class(),
        IntegrationClass::Standard | IntegrationClass::Custom
    ) || descriptor.outer_projection.target() != &expected_target
        || descriptor.nested_projection.target() != &expected_target
        || projection.target() != &expected_target
    {
        return Err(
            "product JavaScript descriptor is foreign to the exact provider configuration".into(),
        );
    }
    if evidence.configuration_identities()
        != (
            configuration.selected_identity(),
            configuration.effective_identity(),
        )
        || evidence.target() != expected_target.integration_class()
    {
        return Err(
            "product extension evidence is foreign to the exact provider configuration".into(),
        );
    }
    let configured = &effective.behavior().tools;
    if configured.len() != 2
        || configured.iter().any(|tool| {
            tool.execution() != ToolExecutionDisposition::Local
                || !matches!(
                    tool.definition().name().as_str(),
                    PRODUCT_JAVASCRIPT_RUN_NAME_V1 | PRODUCT_JAVASCRIPT_WAIT_NAME_V1
                )
                || registry.definition(tool.definition().name()) != Some(tool.definition())
        })
        || !configured
            .iter()
            .any(|tool| tool.definition().name().as_str() == PRODUCT_JAVASCRIPT_RUN_NAME_V1)
        || !configured
            .iter()
            .any(|tool| tool.definition().name().as_str() == PRODUCT_JAVASCRIPT_WAIT_NAME_V1)
    {
        return Err(
            "provider configuration does not own the exact product JavaScript markers".into(),
        );
    }
    let exact = ProductJavaScriptCompositionV1::compile_for_kind(
        job_config,
        registry,
        expected_target,
        presentation_kind,
        nested_names.iter().cloned(),
    )
    .map_err(|error| error.to_string())?;
    let neutral = neutral_product_descriptor(&exact)?;
    let (_, _, catalog, presentation, _, _) = evidence.extension_facts();
    if neutral.identity != descriptor.identity
        || exact.outer_projection().digest() != projection.digest()
        || catalog != exact.nested_projection().digest()
        || presentation != exact.outer_projection().digest()
    {
        return Err("product JavaScript descriptor, receipt, or outer projection drifted".into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "nested Direct pack is an extra exact identity, not a new concern"
)]
#[cfg(feature = "composition-python")]
pub(super) fn validate_python_configuration(
    job_config: plakous_tools::JobConfig,
    descriptor: &LocalCompositionDescriptor,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    nested_names: &[ToolName],
    presentation: plakous_compose::PythonPtcPresentationV1,
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: &ProductExtensionEvidence,
) -> Result<(), String> {
    if !projection.is_for_registry(&registry)
        || !descriptor.outer_projection.is_for_registry(&registry)
        || !descriptor.nested_projection.is_for_registry(&registry)
    {
        return Err("Python projection belongs to a foreign registry".into());
    }
    let effective = configuration.effective();
    let model = effective.descriptor();
    let family = model
        .model_family()
        .ok_or_else(|| "Python target has no exact model family".to_owned())?;
    let target = ProviderModelProjectionIdentityV2::new(
        model.integration_class(),
        model.protocol().as_str(),
        model.route().as_str(),
        model.profile().as_str(),
        family.as_str(),
        model.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|error| error.to_string())?;
    if !matches!(
        target.integration_class(),
        IntegrationClass::Standard | IntegrationClass::Custom
    ) || descriptor.outer_projection.target() != &target
        || descriptor.nested_projection.target() != &target
        || projection.target() != &target
        || evidence.configuration_identities()
            != (
                configuration.selected_identity(),
                configuration.effective_identity(),
            )
        || evidence.target() != target.integration_class()
    {
        return Err(
            "Python descriptor or evidence is foreign to the exact provider configuration".into(),
        );
    }
    let exact = PythonPtcV1::compile_for_kind(
        job_config,
        registry,
        target,
        presentation,
        presentation_kind,
        nested_names.iter().cloned(),
    )
    .map_err(|error| error.to_string())?;
    let neutral = neutral_python_descriptor(&exact)?;
    let (_, _, catalog, presentation_fact, limits, _) = evidence.extension_facts();
    if neutral.identity != descriptor.identity
        || exact.outer_projection().digest() != projection.digest()
        || catalog != exact.catalog_identity()
        || presentation_fact != exact.presentation_identity()
        || limits != python_extension_limits()
    {
        return Err("Python descriptor, receipt, or outer projection drifted".into());
    }
    Ok(())
}
