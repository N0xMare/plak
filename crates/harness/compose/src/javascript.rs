//! Product-owned local JavaScript composition over the rquickjs sidecar
//! (`plakous-js-host`) using the same Code Mode presentation as Codex 0.153.3.
//!
//! This module compiles inert presentation data only. Engine attachment,
//! extension evidence, provider admission, nested dispatch, and recovery remain
//! owned by the facade and the private runtime driver.

use std::collections::BTreeSet;
use std::sync::Arc;

use plakous_model::{
    Digest, DigestDomain, IntegrationClass, ProjectedToolPresentationV2, SemanticJsonV1,
    ToolEffect, ToolInputContract, ToolName,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::codex_code_mode::{normalize_javascript_identifier, render_schema_type};
use crate::coding_menu::CodingPresentationExt as _;
use crate::text::{
    PLAKOUS_JS_PTC_RUN_CATALOG_PREAMBLE, PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX,
    PLAKOUS_JS_PTC_WAIT_DESCRIPTION,
};
use plakous_tools::{
    CODEX_NATIVE_CODE_MODE_COMMIT_V1, CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
    CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1, CODEX_NATIVE_CODE_MODE_RELEASE_V1,
    PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PRODUCT_JAVASCRIPT_PROFILE_REVISION_V1,
    PRODUCT_JAVASCRIPT_RUN_NAME_V1, PRODUCT_JAVASCRIPT_WAIT_NAME_V1,
    PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, product_javascript_run_schema,
    product_javascript_wait_schema, register_product_javascript_markers_into,
};
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, NestedToolEligibility, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionLoweringV2, ToolProjectionMap,
    ToolProjectionRaisingV2, ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind,
};

const BOUNDED_CATALOG_BRIDGE_REVISION: &str =
    "plakous.product-javascript-bounded-catalog-bridge.v1";
const DESCRIPTOR_DIGEST_VERSION: u32 = 2;
const MAX_NESTED_TOOLS: usize = 64;
/// Exact semantic revision of the product-owned host-result projection.
pub const PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_REVISION_V1: &str =
    "plakous.product-javascript-result-projection.v1";
/// Closed product-owned result grammar implemented over the Codex host response.
///
/// This is not a provider-native claim. It freezes response states, explicit
/// error presence, ordered text/media items, and their bounded projection.
pub const PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1: &str = "tool_result := status_text, projected_items; status_text := (\"Script running with cell ID \" cell_id | \"Script terminated\" | \"Script completed\" | \"Script failed\") \"\\nWall time \" decimal_tenths \" seconds\\nOutput:\\n\"; projected_items := ordered projected_item* after max_output_tokens truncation; projected_item := text(utf8) | image(data_url, detail?) | audio(data_url); host_error := appended text(\"Script error:\\n\" utf8) before truncation";

/// Pure compiler for a product-owned JavaScript profile using Code Mode presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductJavaScriptCompositionV1;

impl ProductJavaScriptCompositionV1 {
    /// Registers the two direct-only outer markers required by this profile.
    ///
    /// Ordinary registry dispatch of either marker fails closed. Only the
    /// private composition coordinator may intercept their executor binding.
    ///
    /// # Errors
    ///
    /// Returns the registry's normal invalid-definition, collision, or capacity error.
    pub fn register_markers_into(
        registry: &mut ToolRegistryBuilder,
    ) -> Result<(), ToolRuntimeError> {
        register_product_javascript_markers_into(registry)
    }

    /// Compiles one exact product profile over an ordered nested catalog.
    ///
    /// `nested_names` are canonical registered names and retain caller order in
    /// the engine catalog. The outer projection is always two ordinary
    /// structured functions and is identity-bound to the exact Standard or
    /// Custom target supplied by the admitted integration.
    ///
    /// # Errors
    ///
    /// Rejects non-Standard/Custom targets, missing outer markers or nested
    /// bindings, direct-only or non-structured nested tools, recursive marker
    /// inclusion, duplicate members, and JavaScript-normalized name collisions.
    pub fn compile(
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        nested_names: impl IntoIterator<Item = ToolName>,
    ) -> Result<ProductJavaScriptCompositionDescriptorV1, ToolRuntimeError> {
        if !matches!(
            target.integration_class(),
            IntegrationClass::Standard | IntegrationClass::Custom
        ) {
            return Err(registration_error(
                "product JavaScript composition target class is not admitted",
            ));
        }
        let nested_names = nested_names.into_iter().collect::<Vec<_>>();
        if nested_names.is_empty() || nested_names.len() > MAX_NESTED_TOOLS {
            return Err(registration_error(
                "product JavaScript nested catalog must contain 1..=64 tools",
            ));
        }
        let nested_projection = nested_projection(&registry, &target, &nested_names)?;
        let nested_tools = ordered_nested_catalog(&nested_projection, &nested_names)?;
        validate_nested_catalog(&nested_tools)?;
        let run_description = build_run_description(&nested_tools, &nested_projection);
        let outer_projection = outer_projection(registry, target, run_description.clone())?;
        let identity = descriptor_identity(
            &outer_projection,
            &nested_projection,
            &nested_tools,
            &run_description,
        )?;
        Ok(ProductJavaScriptCompositionDescriptorV1 {
            job_config: plakous_tools::JobConfig::default(),
            outer_projection,
            nested_projection,
            nested_tools: nested_tools.into(),
            run_description: run_description.into(),
            identity,
        })
    }

    /// Compiles one product profile whose nested catalog is the vertical Direct pack.
    ///
    /// `nested_names` are Direct exposed names (plus extensions) in caller order.
    ///
    /// # Errors
    ///
    /// Same fail-closed cases as [`Self::compile`], plus Direct projection misses.
    pub fn compile_for_kind(
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        kind: plakous_tools::CodingPresentation,
        nested_names: impl IntoIterator<Item = ToolName>,
    ) -> Result<ProductJavaScriptCompositionDescriptorV1, ToolRuntimeError> {
        if !matches!(
            target.integration_class(),
            IntegrationClass::Standard | IntegrationClass::Custom
        ) {
            return Err(registration_error(
                "product JavaScript composition target class is not admitted",
            ));
        }
        let nested_names = nested_names.into_iter().collect::<Vec<_>>();
        if nested_names.is_empty() || nested_names.len() > MAX_NESTED_TOOLS {
            return Err(registration_error(
                "product JavaScript nested catalog must contain 1..=64 tools",
            ));
        }
        let nested_projection = kind.compile_nested_projection(
            job_config,
            registry.clone(),
            target.clone(),
            &nested_names,
        )?;
        let nested_tools = ordered_nested_catalog(&nested_projection, &nested_names)?;
        validate_nested_catalog(&nested_tools)?;
        let run_description = build_run_description(&nested_tools, &nested_projection);
        let outer_projection = outer_projection(registry, target, run_description.clone())?;
        let identity = descriptor_identity(
            &outer_projection,
            &nested_projection,
            &nested_tools,
            &run_description,
        )?;
        Ok(ProductJavaScriptCompositionDescriptorV1 {
            job_config,
            outer_projection,
            nested_projection,
            nested_tools: nested_tools.into(),
            run_description: run_description.into(),
            identity,
        })
    }
}

/// Reconstruction-complete tools-owned product JavaScript profile descriptor.
#[derive(Clone)]
pub struct ProductJavaScriptCompositionDescriptorV1 {
    job_config: plakous_tools::JobConfig,
    outer_projection: ToolProjectionMap,
    nested_projection: ToolProjectionMap,
    nested_tools: Arc<[ProductJavaScriptNestedToolV1]>,
    run_description: Arc<str>,
    identity: Digest,
}

impl ProductJavaScriptCompositionDescriptorV1 {
    /// Construction-bound process budgets used by the nested catalog.
    #[must_use]
    pub const fn job_config(&self) -> plakous_tools::JobConfig {
        self.job_config
    }

    /// Provider-visible ordinary `exec_js` and `wait_js` functions.
    #[must_use]
    pub const fn outer_projection(&self) -> &ToolProjectionMap {
        &self.outer_projection
    }

    /// Exact executor-bound local nested projection.
    #[must_use]
    pub const fn nested_projection(&self) -> &ToolProjectionMap {
        &self.nested_projection
    }

    /// Ordered catalog supplied to the rquickjs sidecar under Code Mode presentation.
    #[must_use]
    pub fn nested_tools(&self) -> &[ProductJavaScriptNestedToolV1] {
        &self.nested_tools
    }

    /// Deterministic provider-visible description including the ordered nested signatures.
    #[must_use]
    pub fn run_description(&self) -> &str {
        &self.run_description
    }

    /// Exact product-owned host-result grammar revision bound by this descriptor.
    #[must_use]
    pub const fn result_grammar_revision(&self) -> &'static str {
        PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_REVISION_V1
    }

    /// Closed result grammar bound by this descriptor.
    #[must_use]
    pub const fn result_grammar(&self) -> &'static str {
        PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1
    }

    /// Complete identity binding target, outer presentation, engine, and nested catalog.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Whether an enrolled executor is this profile's driver-only marker.
    #[must_use]
    pub fn is_driver_marker_executor(executor: &ToolExecutorId) -> bool {
        executor.as_str() == PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1
    }
}

/// One ordered function entry in the local JavaScript host catalog.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProductJavaScriptNestedToolV1 {
    exposed_name: ToolName,
    description: String,
    input_schema: Option<PortableToolSchemaV2>,
    string_input: bool,
    projection_identity: Digest,
}

impl ProductJavaScriptNestedToolV1 {
    /// JavaScript-global and provider-neutral catalog name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Exact registered description presented inside the engine catalog.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Exact structured-function input schema, absent for freeform nested tools.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        self.input_schema.as_ref()
    }

    /// Whether nested JavaScript uses `(input: string)` instead of structured args.
    #[must_use]
    pub const fn string_input(&self) -> bool {
        self.string_input
    }

    /// JSON Schema for neutral local-composition attachment (string for freeform tools).
    #[must_use]
    pub fn composition_input_schema_value(&self) -> Value {
        if self.string_input {
            json!({"type": "string"})
        } else {
            self.input_schema.as_ref().map_or_else(
                || json!({"type": "object"}),
                |schema| schema.document().value().as_ref().clone(),
            )
        }
    }

    /// Identity of the canonical lowering and executor binding.
    #[must_use]
    pub const fn projection_identity(&self) -> Digest {
        self.projection_identity
    }
}

fn nested_projection(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    names: &[ToolName],
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let mut entries = Vec::with_capacity(names.len());
    let mut canonical = BTreeSet::new();
    for name in names {
        if !canonical.insert(name.clone()) {
            return Err(registration_error(
                "product JavaScript nested catalog contains a duplicate canonical tool",
            ));
        }
        let identity = registry.identity(name).cloned().ok_or_else(|| {
            registration_error("product JavaScript nested tool binding is missing")
        })?;
        if identity.nested_eligibility() != NestedToolEligibility::NestedEligible {
            return Err(registration_error(
                "product JavaScript nested tool is not explicitly nested-eligible",
            ));
        }
        if matches!(
            identity.executor_id().as_str(),
            PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1
                | CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1
                | PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1
        ) {
            return Err(registration_error(
                "composition outer markers cannot enter a nested catalog",
            ));
        }
        let definition = registry
            .definition(name)
            .ok_or_else(|| registration_error("product JavaScript nested definition is missing"))?;
        let ToolInputContract::Structured(input_schema) = definition.input() else {
            return Err(registration_error(
                "product JavaScript nested tools must be ordinary structured functions",
            ));
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
            input_schema.clone(),
            empty_object()?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            generic_output_schema()?,
            ToolProjectionRaisingV2::RawUtf8V1,
            target.clone(),
        )?);
    }
    plakous_tools::internal::projection_map(registry.clone(), target.clone(), entries)
}

fn ordered_nested_catalog(
    projection: &ToolProjectionMap,
    names: &[ToolName],
) -> Result<Vec<ProductJavaScriptNestedToolV1>, ToolRuntimeError> {
    names
        .iter()
        .map(|name| {
            let entry = projection.by_exposed(name).ok_or_else(|| {
                registration_error("product JavaScript nested projection member is absent")
            })?;
            let (input_schema, string_input) = match entry.presentation() {
                ProjectedToolPresentationV2::CustomUtf8 { .. } => (None, true),
                ProjectedToolPresentationV2::StructuredFunction { .. } => {
                    let schema = entry.input_schema().ok_or_else(|| {
                        registration_error("product JavaScript nested projection is not structured")
                    })?;
                    (Some(schema.clone()), false)
                }
            };
            Ok(ProductJavaScriptNestedToolV1 {
                exposed_name: name.clone(),
                description: entry.description().to_owned(),
                input_schema,
                string_input,
                projection_identity: entry.identity(),
            })
        })
        .collect()
}

fn validate_nested_catalog(
    tools: &[ProductJavaScriptNestedToolV1],
) -> Result<(), ToolRuntimeError> {
    let mut normalized = BTreeSet::new();
    for tool in tools {
        let name = tool.exposed_name.as_str();
        // Only this runtime's own markers hold their names in its nested
        // namespace (spec 01 §8.2); pi's `wait` is an ordinary nested tool here.
        if matches!(
            name,
            PRODUCT_JAVASCRIPT_RUN_NAME_V1 | PRODUCT_JAVASCRIPT_WAIT_NAME_V1
        ) {
            return Err(registration_error(
                "composition outer names cannot enter a nested catalog",
            ));
        }
        if !normalized.insert(normalize_javascript_identifier(name)) {
            return Err(registration_error(
                "product JavaScript nested tool names collide after host normalization",
            ));
        }
    }
    Ok(())
}

fn outer_projection(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    run_description: String,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let run = registered_identity(&registry, PRODUCT_JAVASCRIPT_RUN_NAME_V1)?;
    let wait = registered_identity(&registry, PRODUCT_JAVASCRIPT_WAIT_NAME_V1)?;
    let entries = [
        plakous_tools::internal::projection_exposure(
            [(CanonicalProcessOperationV2::Poll, run)],
            PRODUCT_JAVASCRIPT_RUN_NAME_V1,
            run_description,
            PortableToolSchemaV2::new(product_javascript_run_schema()).map_err(schema_error)?,
            empty_object()?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            generic_output_schema()?,
            ToolProjectionRaisingV2::ModelContentIdentityV1,
            target.clone(),
        )?,
        plakous_tools::internal::projection_exposure(
            [(CanonicalProcessOperationV2::Poll, wait)],
            PRODUCT_JAVASCRIPT_WAIT_NAME_V1,
            PLAKOUS_JS_PTC_WAIT_DESCRIPTION,
            PortableToolSchemaV2::new(product_javascript_wait_schema()).map_err(schema_error)?,
            empty_object()?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            generic_output_schema()?,
            ToolProjectionRaisingV2::ModelContentIdentityV1,
            target.clone(),
        )?,
    ];
    plakous_tools::internal::projection_map(registry, target, entries)
}

fn registered_identity(
    registry: &FrozenToolRegistry,
    name: &str,
) -> Result<plakous_tools::RegisteredToolIdentity, ToolRuntimeError> {
    let name = ToolName::new(name.to_owned())
        .map_err(|_| registration_error("product JavaScript marker name is invalid"))?;
    registry
        .identity(&name)
        .filter(|identity| {
            identity.executor_id().as_str() == PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1
        })
        .cloned()
        .ok_or_else(|| registration_error("exact product JavaScript outer marker is missing"))
}

fn descriptor_identity(
    outer: &ToolProjectionMap,
    nested: &ToolProjectionMap,
    tools: &[ProductJavaScriptNestedToolV1],
    run_description: &str,
) -> Result<Digest, ToolRuntimeError> {
    let tools = serde_json::to_vec(tools).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "product JavaScript nested catalog could not be encoded",
            &error,
        )
    })?;
    Digest::sha256(
        DigestDomain::ToolDefinition,
        DESCRIPTOR_DIGEST_VERSION,
        &[
            PRODUCT_JAVASCRIPT_PROFILE_REVISION_V1.as_bytes(),
            BOUNDED_CATALOG_BRIDGE_REVISION.as_bytes(),
            CODEX_NATIVE_CODE_MODE_RELEASE_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_COMMIT_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1.as_bytes(),
            PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_REVISION_V1.as_bytes(),
            PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1.as_bytes(),
            run_description.as_bytes(),
            PLAKOUS_JS_PTC_WAIT_DESCRIPTION.as_bytes(),
            outer.digest().bytes(),
            nested.digest().bytes(),
            &tools,
        ],
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "product JavaScript descriptor identity could not be built",
            &error,
        )
    })
}

fn build_run_description(
    tools: &[ProductJavaScriptNestedToolV1],
    projection: &ToolProjectionMap,
) -> String {
    let catalog = tools
        .iter()
        .map(|tool| {
            let name = tool.exposed_name.as_str();
            let function = normalize_javascript_identifier(name);
            let result_type = if projection.by_exposed(&tool.exposed_name).is_some_and(
                plakous_tools::ToolProjectionExposureV2::local_programmatic_returns_object,
            ) {
                crate::codex_code_mode::PROCESS_TOOL_RESULT_TYPE
            } else {
                "Promise<string>"
            };
            let signature = if tool.string_input {
                format!("tools.{function}(input: string): {result_type}")
            } else {
                let input = tool.input_schema.as_ref().map_or_else(
                    || "unknown".to_owned(),
                    |schema| render_schema_type(schema.document().value()),
                );
                format!("tools.{function}(args: {input}): {result_type}")
            };
            format!("- `{name}`: {}\n  `{signature}`", tool.description)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX}{PLAKOUS_JS_PTC_RUN_CATALOG_PREAMBLE}{catalog}")
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "callers construct owned JSON objects embedded in the returned schema"
)]
fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn empty_object() -> Result<SemanticJsonV1, ToolRuntimeError> {
    SemanticJsonV1::from_value(json!({})).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "product JavaScript projection defaults are invalid",
            &error,
        )
    })
}

fn generic_output_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(json!({"result": {"type": "string"}}), &[]))
        .map_err(schema_error)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "this function is used directly as a Result::map_err adapter"
)]
fn schema_error(error: plakous_model::SemanticJsonError) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidDefinition,
        "product JavaScript projection schema is invalid",
        &error,
    )
}

fn registration_error(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, message)
}
