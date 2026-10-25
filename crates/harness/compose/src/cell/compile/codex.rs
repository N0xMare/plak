use plakous_model::{SemanticJsonV1, ToolDefinition, ToolEffect, ToolInputContract, ToolName};
use plakous_tool_files::codex_patch_envelope_entry;
use plakous_tool_process::ProcessToolProjectionKind;
use plakous_tools::{
    APPLY_PATCH_CAPABILITY_NAME, CanonicalProcessOperationV2, FrozenToolRegistry,
    INSPECT_IMAGE_CAPABILITY_NAME, PortableToolSchemaV2, ProviderModelProjectionIdentityV2,
    READ_FILE_CAPABILITY_NAME, SEARCH_REPOSITORY_CAPABILITY_NAME, ToolProjectionExposureV2,
    ToolProjectionLoweringV2, ToolProjectionMap, ToolProjectionRaisingV2, ToolRuntimeError,
    ToolRuntimeErrorKind, UPDATE_PLAN_CAPABILITY_NAME, object_schema, resolve_entry,
};
use serde_json::json;
pub(super) fn nested_projection(
    job_config: plakous_tools::JobConfig,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    include_extensions: bool,
    extra_nested: &[&str],
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let process = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
        job_config,
        registry.clone(),
        target.clone(),
    )?;
    let mut entries = process.entries().to_vec();
    let core = [APPLY_PATCH_CAPABILITY_NAME, UPDATE_PLAN_CAPABILITY_NAME];
    let extended = [
        READ_FILE_CAPABILITY_NAME,
        SEARCH_REPOSITORY_CAPABILITY_NAME,
        INSPECT_IMAGE_CAPABILITY_NAME,
    ];
    for name in core
        .into_iter()
        .chain(include_extensions.then_some(extended).into_iter().flatten())
    {
        if name == APPLY_PATCH_CAPABILITY_NAME {
            // The files pack owns how a Codex costume sees apply_patch; a
            // nested cell only raises it differently.
            entries.push(resolve_entry(
                &registry,
                &target,
                &codex_patch_envelope_entry()?.with_raising(ToolProjectionRaisingV2::RawUtf8V1),
            )?);
            continue;
        }
        let lowering = ToolProjectionLoweringV2::CanonicalPassthrough;
        let definition = canonical_definition(&registry, name)?;
        let ToolInputContract::Structured(schema) = definition.input() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "nested coding capability is not structured",
            ));
        };
        entries.push(capability_entry(
            &registry,
            &target,
            name,
            definition.description(),
            schema.clone(),
            lowering,
        )?);
    }
    for name in extra_nested {
        if entries
            .iter()
            .any(|entry| entry.exposed_name().as_str() == *name)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "product Code Mode extension collides with an already projected nested capability",
            ));
        }
        let definition = canonical_definition(&registry, name)?;
        let ToolInputContract::Structured(schema) = definition.input() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "nested coding capability is not structured",
            ));
        };
        entries.push(capability_entry(
            &registry,
            &target,
            name,
            definition.description(),
            schema.clone(),
            ToolProjectionLoweringV2::CanonicalPassthrough,
        )?);
    }
    plakous_tools::internal::projection_map(registry, target, entries)
}

fn capability_entry(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    canonical_name: &str,
    description: &str,
    input_schema: PortableToolSchemaV2,
    lowering: ToolProjectionLoweringV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    let definition = canonical_definition(registry, canonical_name)?;
    let identity = registry
        .identity(definition.name())
        .cloned()
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "nested coding capability identity is absent",
            )
        })?;
    let operation = if definition.effect() == ToolEffect::ReadOnly {
        CanonicalProcessOperationV2::Poll
    } else {
        CanonicalProcessOperationV2::Start
    };
    plakous_tools::internal::projection_exposure(
        [(operation, identity)],
        canonical_name,
        description,
        input_schema,
        empty_defaults()?,
        lowering,
        nested_output_schema()?,
        // Nested lineage raises the raw text (G1-2): the descriptions promise
        // strings and the cell must not have to unwrap `{"result": ...}`.
        ToolProjectionRaisingV2::RawUtf8V1,
        target.clone(),
    )
}

fn canonical_definition<'a>(
    registry: &'a FrozenToolRegistry,
    name: &str,
) -> Result<&'a ToolDefinition, ToolRuntimeError> {
    let name = ToolName::new(name.to_owned())
        .map_err(|error| definition_error("canonical Code Mode tool name is invalid", &error))?;
    registry.definition(&name).ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "required canonical Code Mode capability is not registered",
        )
    })
}

fn empty_defaults() -> Result<SemanticJsonV1, ToolRuntimeError> {
    SemanticJsonV1::from_value(json!({}))
        .map_err(|error| definition_error("Code Mode defaults are invalid", &error))
}

fn nested_output_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(json!({}), &[]))
        .map_err(|error| definition_error("Code Mode nested output schema is invalid", &error))
}

fn definition_error(
    context: &'static str,
    error: &(dyn std::error::Error + 'static),
) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(ToolRuntimeErrorKind::InvalidDefinition, context, error)
}
