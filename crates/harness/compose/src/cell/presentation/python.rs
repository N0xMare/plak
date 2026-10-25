//! Backend-independent local Python composition profile compilation.
//!
//! This module compiles inert presentation and catalog data only. It does not
//! select or expose an evaluator backend, attach a host, admit provider
//! evidence, dispatch nested tools, or own recovery.

use super::super::compile::{CompiledCell, NestedProjection, compile_cell};
use super::super::host_api::HostApi;
use super::CellPresentation;
use plakous_model::{Digest, ToolName};
use plakous_tools::{
    FrozenToolRegistry, PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_PTC_SOURCE_BYTES_V1,
    PortableToolSchemaV2, ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionMap,
    ToolRegistryBuilder, ToolRuntimeError, register_python_ptc_markers_into,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Exact backend-independent profile name.
pub const PYTHON_PTC_PROFILE_V1: &str = "PythonPtcV1";
/// Closed result grammar for local Python cell observations.
pub const PYTHON_PTC_RESULT_GRAMMAR_V1: &str = "tool_result := status_line \"\\n\" output; status_line := \"Python cell running with cell ID \" cell_id | \"Python cell terminated\" | \"Python cell completed\" | \"Python cell failed\" | \"Python interpreter reset\"; output := bounded_utf8; nested_success := utf8 | process_object; nested_failure := python_exception; uncaught_exception := sanitized_utf8_failure; reset := typed_generation_change_without_source_replay";

/// Exact provider-visible outer representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonPtcPresentationV1 {
    /// Ordinary structured `exec_py` function.
    StructuredFunctions,
    /// Raw UTF-8 custom `exec_py` function.
    CustomRunFunctionWait,
}

/// Exact V1 resource and composition maxima, independent of evaluator backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PythonPtcLimitsV1 {
    /// Maximum UTF-8 Python source bytes per cell.
    pub source_bytes: u64,
    /// Maximum wall time of one parent request to the interpreter worker; not
    /// a per-cell or interpreter-lifetime budget.
    pub worker_request_ms: u64,
    /// In-engine allocator ceiling applied inside the interpreter worker.
    pub engine_memory_bytes: u64,
    /// Maximum stdout bytes per cell.
    pub stdout_bytes: u64,
    /// Maximum stderr bytes per cell.
    pub stderr_bytes: u64,
    /// Maximum final result bytes per cell.
    pub final_result_bytes: u64,
    /// Maximum nested calls per cell.
    pub nested_calls: u32,
    /// Maximum concurrently outstanding nested calls.
    pub concurrent_nested_calls: u32,
    /// Maximum argument bytes per nested call.
    pub nested_argument_bytes: u64,
    /// Maximum result bytes per nested call.
    pub nested_result_bytes: u64,
    /// Maximum aggregate nested-result bytes per cell.
    pub aggregate_nested_result_bytes: u64,
    /// Maximum nesting depth.
    pub nesting_depth: u32,
}

impl PythonPtcLimitsV1 {
    /// Exact single-frame V1 profile limits within the protocol maxima.
    pub const EXACT: Self = Self {
        source_bytes: PYTHON_PTC_SOURCE_BYTES_V1 as u64,
        worker_request_ms: 90_000,
        engine_memory_bytes: 268_435_456,
        stdout_bytes: 1_048_576,
        stderr_bytes: 1_048_576,
        final_result_bytes: 1_048_576,
        nested_calls: 100,
        concurrent_nested_calls: 16,
        nested_argument_bytes: 1_048_576,
        nested_result_bytes: 1_048_576,
        aggregate_nested_result_bytes: 16_777_216,
        nesting_depth: 1,
    };
}

/// Pure compiler for the backend-independent `PythonPtcV1` contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PythonPtcV1;

impl PythonPtcV1 {
    /// Registers direct-only structured-run, custom-run, and wait markers.
    ///
    /// Ordinary registry dispatch fails closed. The private composition
    /// coordinator is the only permitted consumer of their executor binding.
    ///
    /// # Errors
    ///
    /// Returns the registry's normal invalid-definition, collision, or
    /// capacity error.
    pub fn register_markers_into(
        registry: &mut ToolRegistryBuilder,
    ) -> Result<(), ToolRuntimeError> {
        register_python_ptc_markers_into(registry)
    }

    /// Compiles one exact ordered catalog and outer presentation.
    ///
    /// # Errors
    ///
    /// Rejects an empty or oversized catalog, missing/direct-only/freeform
    /// members, composition recursion, duplicate identities, normalized Python
    /// name collisions, target drift, and malformed projection data.
    pub fn compile(
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        presentation: PythonPtcPresentationV1,
        nested_names: impl IntoIterator<Item = ToolName>,
    ) -> Result<PythonPtcDescriptorV1, ToolRuntimeError> {
        let names = nested_names.into_iter().collect::<Vec<_>>();
        PythonPtcDescriptorV1::from_cell(
            plakous_tools::JobConfig::default(),
            presentation,
            compile_cell(
                &CellPresentation::python(presentation),
                plakous_tools::JobConfig::default(),
                registry,
                target,
                &names,
                NestedProjection::Registered,
            )?,
        )
    }

    /// Compiles one Python profile whose nested catalog is the vertical Direct pack.
    ///
    /// # Errors
    ///
    /// Same fail-closed cases as [`Self::compile`], plus Direct projection misses.
    pub fn compile_for_kind(
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        presentation: PythonPtcPresentationV1,
        kind: plakous_tools::CodingPresentation,
        nested_names: impl IntoIterator<Item = ToolName>,
    ) -> Result<PythonPtcDescriptorV1, ToolRuntimeError> {
        let names = nested_names.into_iter().collect::<Vec<_>>();
        PythonPtcDescriptorV1::from_cell(
            job_config,
            presentation,
            compile_cell(
                &CellPresentation::python(presentation),
                job_config,
                registry,
                target,
                &names,
                NestedProjection::Coding(kind),
            )?,
        )
    }

    /// Whether an executor is this profile's driver-only marker.
    #[must_use]
    pub fn is_driver_marker_executor(executor: &ToolExecutorId) -> bool {
        executor.as_str() == PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1
    }
}

/// Reconstruction-complete backend-independent Python profile descriptor.
#[derive(Clone)]
pub struct PythonPtcDescriptorV1 {
    api: Arc<HostApi>,
    pub(in crate::cell) job_config: plakous_tools::JobConfig,
    pub(in crate::cell) outer_projection: ToolProjectionMap,
    pub(in crate::cell) nested_projection: ToolProjectionMap,
    pub(in crate::cell) nested_tools: Arc<[PythonPtcNestedToolV1]>,
    pub(in crate::cell) authoring_contract: Arc<str>,
    pub(in crate::cell) presentation: PythonPtcPresentationV1,
    pub(in crate::cell) identity: Digest,
}

impl PythonPtcDescriptorV1 {
    /// Construction-bound process budgets used by the nested catalog.
    #[must_use]
    pub const fn job_config(&self) -> plakous_tools::JobConfig {
        self.job_config
    }

    /// Exact provider-visible outer projection.
    #[must_use]
    pub const fn outer_projection(&self) -> &ToolProjectionMap {
        &self.outer_projection
    }
    /// Exact executor-bound local nested projection.
    #[must_use]
    pub const fn nested_projection(&self) -> &ToolProjectionMap {
        &self.nested_projection
    }
    /// Exact ordered injected-function catalog.
    #[must_use]
    pub fn nested_tools(&self) -> &[PythonPtcNestedToolV1] {
        &self.nested_tools
    }
    /// Concise model-visible Python authoring contract.
    #[must_use]
    pub fn authoring_contract(&self) -> &str {
        &self.authoring_contract
    }
    /// Selected provider-visible representation.
    #[must_use]
    pub const fn presentation(&self) -> PythonPtcPresentationV1 {
        self.presentation
    }
    /// Exact V1 limits.
    #[must_use]
    pub const fn limits(&self) -> PythonPtcLimitsV1 {
        PythonPtcLimitsV1::EXACT
    }
    /// Complete descriptor identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
    /// Exact result grammar.
    #[must_use]
    pub const fn result_grammar(&self) -> &'static str {
        PYTHON_PTC_RESULT_GRAMMAR_V1
    }
}

/// One exact ordered nested injected function.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PythonPtcNestedToolV1 {
    pub(in crate::cell) canonical_name: ToolName,
    pub(in crate::cell) python_name: String,
    pub(in crate::cell) description: String,
    pub(in crate::cell) input_schema: Option<PortableToolSchemaV2>,
    pub(in crate::cell) string_input: bool,
    pub(in crate::cell) projection_identity: Digest,
}

impl PythonPtcNestedToolV1 {
    /// Canonical registered name.
    #[must_use]
    pub const fn canonical_name(&self) -> &ToolName {
        &self.canonical_name
    }
    /// Injected Python identifier.
    #[must_use]
    pub fn python_name(&self) -> &str {
        &self.python_name
    }
    /// Exact registered description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
    /// Exact dictionary input schema, absent for freeform nested tools.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        self.input_schema.as_ref()
    }
    /// Whether nested Python uses a string argument instead of a dictionary.
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
    /// Exact canonical projection/executor binding identity.
    #[must_use]
    pub const fn projection_identity(&self) -> Digest {
        self.projection_identity
    }
}

impl PythonPtcDescriptorV1 {
    /// Host API that generates both the authoring contract and the worker bootstrap.
    #[must_use]
    pub fn host_api(&self) -> &HostApi {
        &self.api
    }

    fn from_cell(
        job_config: plakous_tools::JobConfig,
        presentation: PythonPtcPresentationV1,
        cell: CompiledCell,
    ) -> Result<Self, ToolRuntimeError> {
        let nested_tools = cell.api.python_tools()?;
        let authoring_contract = cell.execute_text;
        Ok(Self {
            job_config,
            api: Arc::new(cell.api),
            outer_projection: cell.outer_projection,
            nested_projection: cell.nested_projection,
            nested_tools: nested_tools.into(),
            authoring_contract: authoring_contract.into(),
            presentation,
            identity: cell.identity,
        })
    }
}
