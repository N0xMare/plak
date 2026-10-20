//! Backend-independent local Python composition profile compilation.
//!
//! This module compiles inert presentation and catalog data only. It does not
//! select or expose an evaluator backend, attach a host, admit provider
//! evidence, dispatch nested tools, or own recovery.

use std::collections::BTreeSet;
use std::sync::Arc;

use plakous_model::{
    Digest, DigestDomain, IntegrationClass, ProjectedCustomToolFormatV2,
    ProjectedToolPresentationV2, SemanticJsonV1, ToolEffect, ToolInputContract, ToolName,
    Utf8TextContractV1,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::coding_menu::CodingPresentationExt as _;
use crate::text::{
    PLAKOUS_PY_PTC_CUSTOM_RUN_DESCRIPTION, PLAKOUS_PY_PTC_RUN_CATALOG_PREAMBLE,
    PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY, PLAKOUS_PY_PTC_WAIT_DESCRIPTION,
};
use plakous_tools::{
    CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
    PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1,
    PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_PTC_PROFILE_REVISION_V1,
    PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_SOURCE_BYTES_V1, PYTHON_PTC_WAIT_NAME_V1,
    python_ptc_run_schema, python_ptc_wait_schema, register_python_ptc_markers_into,
};
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, NestedToolEligibility, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionLoweringV2, ToolProjectionMap,
    ToolProjectionRaisingV2, ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// Exact backend-independent profile name.
pub const PYTHON_PTC_PROFILE_V1: &str = "PythonPtcV1";
const PRESENTATION_REVISION: &str = "plakous.python-ptc-presentation.v4";
const CATALOG_REVISION: &str = "plakous.python-ptc-catalog.v2";
const LIMITS_REVISION: &str = "plakous.python-ptc-limits.v2";
const RESULT_REVISION: &str = "plakous.python-ptc-result.v1";
const DESCRIPTOR_DIGEST_VERSION: u32 = 1;
const COMPONENT_DIGEST_VERSION: u32 = 1;
const MAX_CATALOG_TOOLS: usize = 64;

const CUSTOM_RUN_GRAMMAR: &str = "start: /(?s).+/";

/// Closed result grammar for local Python cell observations.
pub const PYTHON_PTC_RESULT_GRAMMAR_V1: &str = "tool_result := status_line \"\\n\" output; status_line := \"Python cell running with cell ID \" cell_id | \"Python cell terminated\" | \"Python cell completed\" | \"Python cell failed\" | \"Python interpreter reset\"; output := bounded_utf8; nested_success := utf8; nested_failure := python_exception; uncaught_exception := sanitized_utf8_failure; reset := typed_generation_change_without_source_replay";

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
        if !matches!(
            target.integration_class(),
            IntegrationClass::Standard | IntegrationClass::Custom
        ) {
            return Err(registration_error(
                "local Python composition target class is not admitted",
            ));
        }
        let nested_names = nested_names.into_iter().collect::<Vec<_>>();
        if nested_names.is_empty() || nested_names.len() > MAX_CATALOG_TOOLS {
            return Err(registration_error(
                "local Python nested catalog must contain one through 64 tools",
            ));
        }
        let nested_projection = nested_projection(&registry, &target, &nested_names)?;
        let nested_tools = ordered_nested_catalog(&nested_projection, &nested_names)?;
        validate_nested_catalog(&nested_tools)?;
        let authoring_contract = build_authoring_contract(&nested_tools);
        let outer_projection =
            outer_projection(registry, target, presentation, authoring_contract.clone())?;
        let limits_identity = component_identity(
            LIMITS_REVISION,
            &serde_json::to_vec(&PythonPtcLimitsV1::EXACT).map_err(encoding_error)?,
        )?;
        let catalog_identity = component_identity(
            CATALOG_REVISION,
            &serde_json::to_vec(&nested_tools).map_err(encoding_error)?,
        )?;
        let presentation_identity = component_identity(
            PRESENTATION_REVISION,
            &serde_json::to_vec(&presentation).map_err(encoding_error)?,
        )?;
        let result_identity =
            component_identity(RESULT_REVISION, PYTHON_PTC_RESULT_GRAMMAR_V1.as_bytes())?;
        let profile_identity = component_identity(
            PYTHON_PTC_PROFILE_REVISION_V1,
            authoring_contract.as_bytes(),
        )?;
        let identity = Digest::sha256(
            DigestDomain::ToolDefinition,
            DESCRIPTOR_DIGEST_VERSION,
            &[
                PYTHON_PTC_PROFILE_REVISION_V1.as_bytes(),
                outer_projection.digest().bytes(),
                nested_projection.digest().bytes(),
                profile_identity.bytes(),
                presentation_identity.bytes(),
                result_identity.bytes(),
                catalog_identity.bytes(),
                limits_identity.bytes(),
            ],
        )
        .map_err(digest_error)?;
        Ok(PythonPtcDescriptorV1 {
            job_config: plakous_tools::JobConfig::default(),
            outer_projection,
            nested_projection,
            nested_tools: nested_tools.into(),
            authoring_contract: authoring_contract.into(),
            presentation,
            profile_identity,
            presentation_identity,
            result_identity,
            catalog_identity,
            limits_identity,
            identity,
        })
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
        if !matches!(
            target.integration_class(),
            IntegrationClass::Standard | IntegrationClass::Custom
        ) {
            return Err(registration_error(
                "local Python composition target class is not admitted",
            ));
        }
        let nested_names = nested_names.into_iter().collect::<Vec<_>>();
        if nested_names.is_empty() || nested_names.len() > MAX_CATALOG_TOOLS {
            return Err(registration_error(
                "local Python nested catalog must contain one through 64 tools",
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
        let authoring_contract = build_authoring_contract(&nested_tools);
        let outer_projection =
            outer_projection(registry, target, presentation, authoring_contract.clone())?;
        let limits_identity = component_identity(
            LIMITS_REVISION,
            &serde_json::to_vec(&PythonPtcLimitsV1::EXACT).map_err(encoding_error)?,
        )?;
        let catalog_identity = component_identity(
            CATALOG_REVISION,
            &serde_json::to_vec(&nested_tools).map_err(encoding_error)?,
        )?;
        let presentation_identity = component_identity(
            PRESENTATION_REVISION,
            &serde_json::to_vec(&presentation).map_err(encoding_error)?,
        )?;
        let result_identity =
            component_identity(RESULT_REVISION, PYTHON_PTC_RESULT_GRAMMAR_V1.as_bytes())?;
        let profile_identity = component_identity(
            PYTHON_PTC_PROFILE_REVISION_V1,
            authoring_contract.as_bytes(),
        )?;
        let identity = Digest::sha256(
            DigestDomain::ToolDefinition,
            DESCRIPTOR_DIGEST_VERSION,
            &[
                PYTHON_PTC_PROFILE_REVISION_V1.as_bytes(),
                outer_projection.digest().bytes(),
                nested_projection.digest().bytes(),
                profile_identity.bytes(),
                presentation_identity.bytes(),
                result_identity.bytes(),
                catalog_identity.bytes(),
                limits_identity.bytes(),
            ],
        )
        .map_err(digest_error)?;
        Ok(PythonPtcDescriptorV1 {
            job_config,
            outer_projection,
            nested_projection,
            nested_tools: nested_tools.into(),
            authoring_contract: authoring_contract.into(),
            presentation,
            profile_identity,
            presentation_identity,
            result_identity,
            catalog_identity,
            limits_identity,
            identity,
        })
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
    job_config: plakous_tools::JobConfig,
    outer_projection: ToolProjectionMap,
    nested_projection: ToolProjectionMap,
    nested_tools: Arc<[PythonPtcNestedToolV1]>,
    authoring_contract: Arc<str>,
    presentation: PythonPtcPresentationV1,
    profile_identity: Digest,
    presentation_identity: Digest,
    result_identity: Digest,
    catalog_identity: Digest,
    limits_identity: Digest,
    identity: Digest,
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
    /// Profile/authoring identity.
    #[must_use]
    pub const fn profile_identity(&self) -> Digest {
        self.profile_identity
    }
    /// Outer-presentation identity.
    #[must_use]
    pub const fn presentation_identity(&self) -> Digest {
        self.presentation_identity
    }
    /// Result-grammar identity.
    #[must_use]
    pub const fn result_identity(&self) -> Digest {
        self.result_identity
    }
    /// Ordered catalog identity.
    #[must_use]
    pub const fn catalog_identity(&self) -> Digest {
        self.catalog_identity
    }
    /// Limits identity.
    #[must_use]
    pub const fn limits_identity(&self) -> Digest {
        self.limits_identity
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
    canonical_name: ToolName,
    python_name: String,
    description: String,
    input_schema: Option<PortableToolSchemaV2>,
    string_input: bool,
    projection_identity: Digest,
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

pub(crate) fn nested_projection(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    names: &[ToolName],
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let mut entries = Vec::with_capacity(names.len());
    let mut names_seen = BTreeSet::new();
    let mut identities_seen = BTreeSet::new();
    for name in names {
        if !names_seen.insert(name.clone()) {
            return Err(registration_error(
                "local Python nested catalog contains a duplicate canonical name",
            ));
        }
        let identity = registry
            .identity(name)
            .cloned()
            .ok_or_else(|| registration_error("local Python nested tool binding is missing"))?;
        if !identities_seen.insert(identity.tool_id().clone()) {
            return Err(registration_error(
                "local Python nested catalog contains a duplicate canonical identity",
            ));
        }
        if identity.nested_eligibility() != NestedToolEligibility::NestedEligible {
            return Err(registration_error(
                "local Python nested tool is not explicitly nested-eligible",
            ));
        }
        if is_composition_marker(identity.executor_id()) {
            return Err(registration_error(
                "composition and hosted outer markers cannot enter a local Python nested catalog",
            ));
        }
        let definition = registry
            .definition(name)
            .ok_or_else(|| registration_error("local Python nested definition is missing"))?;
        let ToolInputContract::Structured(input_schema) = definition.input() else {
            return Err(registration_error(
                "local Python nested tools must take one structured dictionary",
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
) -> Result<Vec<PythonPtcNestedToolV1>, ToolRuntimeError> {
    names
        .iter()
        .map(|name| {
            let entry = projection.by_exposed(name).ok_or_else(|| {
                registration_error("local Python nested projection member is absent")
            })?;
            let (input_schema, string_input) = match entry.presentation() {
                ProjectedToolPresentationV2::CustomUtf8 { .. } => (None, true),
                ProjectedToolPresentationV2::StructuredFunction { .. } => {
                    let schema = entry.input_schema().ok_or_else(|| {
                        registration_error("local Python nested projection is not structured")
                    })?;
                    (Some(schema.clone()), false)
                }
            };
            Ok(PythonPtcNestedToolV1 {
                canonical_name: name.clone(),
                python_name: normalize_python_identifier(name.as_str()),
                description: entry.description().to_owned(),
                input_schema,
                string_input,
                projection_identity: entry.identity(),
            })
        })
        .collect()
}

fn validate_nested_catalog(tools: &[PythonPtcNestedToolV1]) -> Result<(), ToolRuntimeError> {
    let mut python_names = BTreeSet::new();
    for tool in tools {
        if tool.python_name.starts_with("_plakous_host_") {
            return Err(registration_error(
                "local Python nested tool name uses the reserved host binding prefix",
            ));
        }
        if matches!(
            tool.canonical_name.as_str(),
            PYTHON_PTC_RUN_NAME_V1 | PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1 | PYTHON_PTC_WAIT_NAME_V1
        ) {
            return Err(registration_error(
                "local Python outer names cannot enter the nested catalog",
            ));
        }
        if !python_names.insert(tool.python_name.clone()) {
            return Err(registration_error(
                "local Python nested tool names collide after Python normalization",
            ));
        }
    }
    Ok(())
}

fn outer_projection(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    presentation: PythonPtcPresentationV1,
    authoring_contract: String,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let run = match presentation {
        PythonPtcPresentationV1::StructuredFunctions => {
            plakous_tools::internal::projection_exposure(
                [(
                    CanonicalProcessOperationV2::Poll,
                    registered_marker(&registry, PYTHON_PTC_RUN_NAME_V1)?,
                )],
                PYTHON_PTC_RUN_NAME_V1,
                authoring_contract,
                PortableToolSchemaV2::new(python_ptc_run_schema()).map_err(definition_error)?,
                empty_object()?,
                ToolProjectionLoweringV2::CanonicalPassthrough,
                generic_output_schema()?,
                ToolProjectionRaisingV2::ModelContentIdentityV1,
                target.clone(),
            )?
        }
        PythonPtcPresentationV1::CustomRunFunctionWait => {
            plakous_tools::internal::projection_exposure_custom(
                [(
                    CanonicalProcessOperationV2::Poll,
                    registered_marker(&registry, PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1)?,
                )],
                PYTHON_PTC_RUN_NAME_V1,
                format!("{PLAKOUS_PY_PTC_CUSTOM_RUN_DESCRIPTION}\n\n{authoring_contract}"),
                Utf8TextContractV1::new(PYTHON_PTC_SOURCE_BYTES_V1).map_err(definition_error)?,
                ProjectedCustomToolFormatV2::lark(CUSTOM_RUN_GRAMMAR).map_err(definition_error)?,
                empty_object()?,
                ToolProjectionLoweringV2::CanonicalPassthrough,
                generic_output_schema()?,
                ToolProjectionRaisingV2::ModelContentIdentityV1,
                target.clone(),
            )?
        }
    };
    let wait = plakous_tools::internal::projection_exposure(
        [(
            CanonicalProcessOperationV2::Poll,
            registered_marker(&registry, PYTHON_PTC_WAIT_NAME_V1)?,
        )],
        PYTHON_PTC_WAIT_NAME_V1,
        PLAKOUS_PY_PTC_WAIT_DESCRIPTION,
        PortableToolSchemaV2::new(python_ptc_wait_schema()).map_err(definition_error)?,
        empty_object()?,
        ToolProjectionLoweringV2::CanonicalPassthrough,
        generic_output_schema()?,
        ToolProjectionRaisingV2::ModelContentIdentityV1,
        target.clone(),
    )?;
    plakous_tools::internal::projection_map(registry, target, [run, wait])
}

fn registered_marker(
    registry: &FrozenToolRegistry,
    name: &str,
) -> Result<plakous_tools::RegisteredToolIdentity, ToolRuntimeError> {
    let name = ToolName::new(name.to_owned()).map_err(definition_error)?;
    registry
        .identity(&name)
        .filter(|identity| PythonPtcV1::is_driver_marker_executor(identity.executor_id()))
        .cloned()
        .ok_or_else(|| registration_error("exact local Python outer marker is missing"))
}

fn build_authoring_contract(tools: &[PythonPtcNestedToolV1]) -> String {
    let catalog = tools
        .iter()
        .map(|tool| {
            let signature = if tool.string_input {
                format!("async def {}(arguments: str) -> str", tool.python_name)
            } else {
                let arguments = tool.input_schema.as_ref().map_or_else(
                    || "dict".to_owned(),
                    |nested_schema| render_python_type(nested_schema.document().value()),
                );
                format!(
                    "async def {}(arguments: {arguments}) -> str",
                    tool.python_name
                )
            };
            format!(
                "- `{}`: {}\n  `{signature}`",
                tool.canonical_name, tool.description,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY}{PLAKOUS_PY_PTC_RUN_CATALOG_PREAMBLE}{catalog}")
}

/// Renders one bounded V2 schema node as a Python type expression.
///
/// The nested catalog states the argument contract as a signature instead of
/// an inlined JSON Schema document: property names, types, and which keys are
/// required survive, and every registered property description is kept as a
/// comment.
fn render_python_type(schema: &Value) -> String {
    let Some(object) = schema.as_object() else {
        return "object".to_owned();
    };
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        let literals = values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "object".to_owned()))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("Literal[{literals}]");
    }
    match object.get("type").and_then(Value::as_str) {
        Some("string") => "str".to_owned(),
        Some("integer") => "int".to_owned(),
        Some("number") => "float".to_owned(),
        Some("boolean") => "bool".to_owned(),
        Some("null") => "None".to_owned(),
        Some("array") => object.get("items").map_or_else(
            || "list".to_owned(),
            |items| format!("list[{}]", render_python_type(items)),
        ),
        Some("object") | None
            if object.contains_key("properties") || object.contains_key("additionalProperties") =>
        {
            render_python_dict_type(object)
        }
        _ => "object".to_owned(),
    }
}

fn render_python_dict_type(object: &serde_json::Map<String, Value>) -> String {
    let empty = serde_json::Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut properties = properties.iter().collect::<Vec<_>>();
    properties.sort_unstable_by_key(|(name, _)| *name);
    let fields = properties
        .into_iter()
        .map(|(name, schema)| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let comment = schema
                .as_object()
                .and_then(|property| property.get("description"))
                .and_then(Value::as_str)
                .map_or_else(String::new, |description| format!("  # {description}"));
            format!(
                "  \"{name}\"{optional}: {},{comment}",
                render_python_type(schema)
            )
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        "dict".to_owned()
    } else {
        format!("{{\n{}\n}}", fields.join("\n"))
    }
}

fn normalize_python_identifier(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len() + 1);
    for (index, character) in name.chars().enumerate() {
        let valid = character == '_' || character.is_ascii_alphanumeric();
        if index == 0 && character.is_ascii_digit() {
            normalized.push('_');
        }
        normalized.push(if valid { character } else { '_' });
    }
    if PYTHON_KEYWORDS.contains(&normalized.as_str()) {
        normalized.push('_');
    }
    normalized
}

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

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

pub(crate) fn empty_object() -> Result<SemanticJsonV1, ToolRuntimeError> {
    SemanticJsonV1::from_value(json!({})).map_err(definition_error)
}

pub(crate) fn generic_output_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(json!({"result": {"type": "string"}}), &[]))
        .map_err(definition_error)
}

pub(crate) fn component_identity(revision: &str, bytes: &[u8]) -> Result<Digest, ToolRuntimeError> {
    Digest::sha256(
        DigestDomain::ToolDefinition,
        COMPONENT_DIGEST_VERSION,
        &[revision.as_bytes(), bytes],
    )
    .map_err(digest_error)
}

fn is_composition_marker(executor: &ToolExecutorId) -> bool {
    matches!(
        executor.as_str(),
        PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1
            | PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1
            | CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1
    )
}

pub(crate) fn registration_error(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, message)
}

pub(crate) fn definition_error(error: impl std::error::Error + 'static) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidDefinition,
        "local Python profile definition is invalid",
        &error,
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "this function is used directly as a Result::map_err adapter"
)]
pub(crate) fn encoding_error(error: serde_json::Error) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidDefinition,
        "local Python profile identity could not be encoded",
        &error,
    )
}

pub(crate) fn digest_error(error: plakous_model::DigestError) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidDefinition,
        "local Python profile identity could not be built",
        &error,
    )
}
