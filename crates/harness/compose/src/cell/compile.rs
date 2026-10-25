use std::collections::BTreeSet;

use plakous_model::{
    IntegrationClass, ProjectedCustomToolFormatV2, SemanticJsonV1, ToolEffect, ToolInputContract,
    ToolName, Utf8TextContractV1,
};
use plakous_tools::{
    CanonicalProcessOperationV2, CodingPresentation, FrozenToolRegistry, JobConfig,
    NestedToolEligibility, PortableToolSchemaV2, ProviderModelProjectionIdentityV2,
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap, ToolProjectionRaisingV2,
    ToolRuntimeError, ToolRuntimeErrorKind, is_composition_marker_executor, object_schema,
};

use super::host_api::HostApi;
use super::identity::descriptor_identity;
use super::presentation::{CatalogStyle, CellPresentation, OuterInput, OuterTool};
use super::render_py::render_pyi;
use super::render_ts::{normalize_javascript_identifier, render_ts};
use crate::coding_menu::CodingPresentationExt as _;

mod codex;

#[derive(Clone, Copy)]
pub(super) enum NestedProjection<'a> {
    Registered,
    Coding(CodingPresentation),
    Codex {
        include_extensions: bool,
        extra_nested: &'a [&'a str],
    },
}

pub(super) struct CellDescriptor {
    pub outer_projection: ToolProjectionMap,
    pub nested_projection: ToolProjectionMap,
    pub api: HostApi,
    pub execute_text: String,
    pub identity: plakous_model::Digest,
}

pub(super) fn compile_cell(
    presentation: &CellPresentation,
    job_config: JobConfig,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    names: &[ToolName],
    strategy: NestedProjection<'_>,
) -> Result<CellDescriptor, ToolRuntimeError> {
    let is_codex = matches!(
        presentation.catalog,
        CatalogStyle::CodexExact | CatalogStyle::CodexMinimal
    );
    if !is_codex {
        if !matches!(
            target.integration_class(),
            IntegrationClass::Standard | IntegrationClass::Custom
        ) {
            return Err(registration_error(presentation.messages.target_refused));
        }
        if names.is_empty() || names.len() > 64 {
            return Err(registration_error(
                presentation.messages.catalog_bound_refused,
            ));
        }
    }
    let nested_projection = match strategy {
        NestedProjection::Registered => {
            registered_projection(&registry, &target, names, presentation.catalog)?
        }
        NestedProjection::Coding(kind) => {
            kind.compile_nested_projection(job_config, registry.clone(), target.clone(), names)?
        }
        NestedProjection::Codex {
            include_extensions,
            extra_nested,
        } => {
            let projection = codex::nested_projection(
                job_config,
                registry.clone(),
                target.clone(),
                include_extensions,
                extra_nested,
            )?;
            plakous_tools::internal::projection_map(
                registry.clone(),
                target.clone(),
                projection
                    .entries()
                    .iter()
                    .filter(|entry| names.contains(entry.exposed_name()))
                    .cloned(),
            )?
        }
    };
    validate_nested_eligibility(&nested_projection, names)?;
    let api = HostApi::from_projection(&nested_projection, names, presentation.catalog)?;
    validate_catalog(&api, presentation)?;
    let execute_text = if presentation.catalog == CatalogStyle::Python {
        render_pyi(&api)
    } else {
        render_ts(&api, presentation.catalog)
    };
    let outer_projection = outer_projection(presentation, registry, target, &execute_text)?;
    let identity = descriptor_identity(
        presentation,
        &outer_projection,
        &nested_projection,
        &api,
        outer_projection.entries()[0].description(),
    )?;
    Ok(CellDescriptor {
        outer_projection,
        nested_projection,
        api,
        execute_text,
        identity,
    })
}

fn validate_nested_eligibility(
    projection: &ToolProjectionMap,
    names: &[ToolName],
) -> Result<(), ToolRuntimeError> {
    for name in names {
        let entry = projection
            .by_exposed(name)
            .ok_or_else(|| registration_error("required nested capability is not projected"))?;
        if entry
            .canonical_operations()
            .values()
            .any(|identity| is_composition_marker_executor(identity.executor_id()))
        {
            return Err(registration_error(
                "composition outer markers cannot enter a nested catalog",
            ));
        }
        if entry
            .canonical_operations()
            .values()
            .any(|identity| identity.nested_eligibility() != NestedToolEligibility::NestedEligible)
        {
            return Err(registration_error(
                "required nested capability is not explicitly nested-eligible",
            ));
        }
    }
    Ok(())
}

fn validate_catalog(
    api: &HostApi,
    presentation: &CellPresentation,
) -> Result<(), ToolRuntimeError> {
    let mut normalized = BTreeSet::new();
    for tool in api
        .namespaces
        .iter()
        .flat_map(|namespace| &namespace.functions)
    {
        let python = presentation.catalog == CatalogStyle::Python;
        let name = tool.exposed_name.as_str();
        if python && tool.python_name.starts_with("_plakous_host_") {
            return Err(registration_error(
                "local Python nested tool name uses the reserved host binding prefix",
            ));
        }
        if matches!(
            presentation.catalog,
            CatalogStyle::Python | CatalogStyle::ProductJavaScript
        ) && (name == presentation.execute.name
            || presentation
                .wait
                .as_ref()
                .is_some_and(|wait| name == wait.name)
            || (python && name == plakous_tools::PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1))
        {
            return Err(registration_error(if python {
                "local Python outer names cannot enter the nested catalog"
            } else {
                "composition outer names cannot enter a nested catalog"
            }));
        }
        let identifier = if python {
            tool.python_name.clone()
        } else {
            normalize_javascript_identifier(name)
        };
        if !normalized.insert(identifier) {
            return Err(registration_error(presentation.messages.collision_refused));
        }
    }
    Ok(())
}

fn registered_projection(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    names: &[ToolName],
    style: CatalogStyle,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let python = style == CatalogStyle::Python;
    let mut entries = Vec::with_capacity(names.len());
    let mut seen = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            return Err(registration_error(if python {
                "local Python nested catalog contains a duplicate canonical name"
            } else {
                "product JavaScript nested catalog contains a duplicate canonical tool"
            }));
        }
        let identity = registry.identity(name).cloned().ok_or_else(|| {
            registration_error(if python {
                "local Python nested tool binding is missing"
            } else {
                "product JavaScript nested tool binding is missing"
            })
        })?;
        if python && !identities.insert(identity.tool_id().clone()) {
            return Err(registration_error(
                "local Python nested catalog contains a duplicate canonical identity",
            ));
        }
        if identity.nested_eligibility() != NestedToolEligibility::NestedEligible {
            return Err(registration_error(if python {
                "local Python nested tool is not explicitly nested-eligible"
            } else {
                "product JavaScript nested tool is not explicitly nested-eligible"
            }));
        }
        if is_composition_marker_executor(identity.executor_id()) {
            return Err(registration_error(if python {
                "composition and hosted outer markers cannot enter a local Python nested catalog"
            } else {
                "composition outer markers cannot enter a nested catalog"
            }));
        }
        let definition = registry
            .definition(name)
            .ok_or_else(|| registration_error("nested definition is missing"))?;
        let ToolInputContract::Structured(schema) = definition.input() else {
            return Err(registration_error(if python {
                "local Python nested tools must take one structured dictionary"
            } else {
                "product JavaScript nested tools must be ordinary structured functions"
            }));
        };
        let operation = if definition.effect() == ToolEffect::ReadOnly {
            CanonicalProcessOperationV2::Poll
        } else {
            CanonicalProcessOperationV2::Start
        };
        entries.push(plakous_tools::internal::projection_exposure(
            [(operation, identity)],
            name.as_str(),
            definition.description(),
            schema.clone(),
            empty_object()?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            output_schema()?,
            ToolProjectionRaisingV2::RawUtf8V1,
            target.clone(),
        )?);
    }
    plakous_tools::internal::projection_map(registry.clone(), target.clone(), entries)
}

fn outer_projection(
    presentation: &CellPresentation,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    execute_text: &str,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let execute_text = if let Some(prefix) = presentation.messages.custom_execute_prefix
        && matches!(presentation.execute.input, OuterInput::Custom { .. })
    {
        format!("{prefix}\n\n{execute_text}")
    } else {
        execute_text.to_owned()
    };
    let mut entries = vec![outer_entry(
        &registry,
        &target,
        presentation,
        &presentation.execute,
        execute_text,
    )?];
    if let Some(wait) = &presentation.wait {
        entries.push(outer_entry(
            &registry,
            &target,
            presentation,
            wait,
            presentation.messages.wait.to_owned(),
        )?);
    }
    plakous_tools::internal::projection_map(registry, target, entries)
}

fn outer_entry(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    presentation: &CellPresentation,
    tool: &OuterTool,
    description: String,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    let name = ToolName::new(tool.marker).map_err(definition_error)?;
    let identity = registry
        .identity(&name)
        .filter(|identity| identity.executor_id().as_str() == presentation.marker_executor)
        .cloned()
        .ok_or_else(|| registration_error("exact local cell outer marker is missing"))?;
    let operations = [(CanonicalProcessOperationV2::Poll, identity)];
    match &tool.input {
        OuterInput::Structured(schema) => plakous_tools::internal::projection_exposure(
            operations,
            tool.name,
            description,
            PortableToolSchemaV2::new(schema.clone()).map_err(definition_error)?,
            empty_object()?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            output_schema()?,
            ToolProjectionRaisingV2::ModelContentIdentityV1,
            target.clone(),
        ),
        OuterInput::Custom { grammar, bytes } => {
            plakous_tools::internal::projection_exposure_custom(
                operations,
                tool.name,
                description,
                Utf8TextContractV1::new(*bytes).map_err(definition_error)?,
                ProjectedCustomToolFormatV2::lark(*grammar).map_err(definition_error)?,
                empty_object()?,
                ToolProjectionLoweringV2::CanonicalPassthrough,
                output_schema()?,
                ToolProjectionRaisingV2::ModelContentIdentityV1,
                target.clone(),
            )
        }
    }
}

fn empty_object() -> Result<SemanticJsonV1, ToolRuntimeError> {
    SemanticJsonV1::from_value(serde_json::json!({})).map_err(definition_error)
}

fn output_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(
        serde_json::json!({"result":{"type":"string"}}),
        &[],
    ))
    .map_err(definition_error)
}

pub(super) fn registration_error(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, message)
}

pub(super) fn definition_error(error: impl std::error::Error + 'static) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidDefinition,
        "local cell definition is invalid",
        &error,
    )
}
