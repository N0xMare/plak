//! Product-owned local JavaScript composition over the rquickjs sidecar
//! (`plakous-js-host`) using the same Code Mode presentation as Codex 0.153.3.
//!
//! This module compiles inert presentation data only. Engine attachment,
//! extension evidence, provider admission, nested dispatch, and recovery remain
//! owned by the facade and the private runtime driver.

use super::super::compile::{CellDescriptor, NestedProjection, compile_cell};
use super::CellPresentation;
use plakous_model::{Digest, ToolName};
use plakous_tools::{
    FrozenToolRegistry, PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionMap, ToolRegistryBuilder,
    ToolRuntimeError, register_product_javascript_markers_into,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

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
        let names = nested_names.into_iter().collect::<Vec<_>>();
        ProductJavaScriptCompositionDescriptorV1::from_cell(
            plakous_tools::JobConfig::default(),
            compile_cell(
                &CellPresentation::javascript(),
                plakous_tools::JobConfig::default(),
                registry,
                target,
                &names,
                NestedProjection::Registered,
            )?,
        )
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
        let names = nested_names.into_iter().collect::<Vec<_>>();
        ProductJavaScriptCompositionDescriptorV1::from_cell(
            job_config,
            compile_cell(
                &CellPresentation::javascript(),
                job_config,
                registry,
                target,
                &names,
                NestedProjection::Coding(kind),
            )?,
        )
    }
}

/// Reconstruction-complete tools-owned product JavaScript profile descriptor.
#[derive(Clone)]
pub struct ProductJavaScriptCompositionDescriptorV1 {
    pub(in crate::cell) job_config: plakous_tools::JobConfig,
    pub(in crate::cell) outer_projection: ToolProjectionMap,
    pub(in crate::cell) nested_projection: ToolProjectionMap,
    pub(in crate::cell) nested_tools: Arc<[ProductJavaScriptNestedToolV1]>,
    pub(in crate::cell) run_description: Arc<str>,
    pub(in crate::cell) identity: Digest,
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
    pub(in crate::cell) exposed_name: ToolName,
    pub(in crate::cell) description: String,
    pub(in crate::cell) input_schema: Option<PortableToolSchemaV2>,
    pub(in crate::cell) string_input: bool,
    pub(in crate::cell) projection_identity: Digest,
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

impl ProductJavaScriptCompositionDescriptorV1 {
    fn from_cell(
        job_config: plakous_tools::JobConfig,
        cell: CellDescriptor,
    ) -> Result<Self, ToolRuntimeError> {
        Ok(Self {
            job_config,
            nested_tools: cell.api.javascript_tools()?.into(),
            outer_projection: cell.outer_projection,
            nested_projection: cell.nested_projection,
            run_description: cell.execute_text.into(),
            identity: cell.identity,
        })
    }
}
