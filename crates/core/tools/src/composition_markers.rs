//! The durable identity of composition driver markers.
//!
//! A composition's outer tools are *markers*: they are registered so a session
//! descriptor records them, and ordinary dispatch of one fails closed, because
//! only the private composition coordinator may intercept their executor
//! binding. That makes their registrations durable format —
//! `durable.rs::canonical_registry_authority` refuses a reopened descriptor
//! whose marker rows are not exactly these — so they stay in the core with the
//! rest of the durable identity vocabulary while the compilers and renderers
//! that *use* them live above it.
//!
//! Nothing here compiles a presentation. The names, ids, revisions, schemas
//! and definitions are the whole contents.

use std::sync::Arc;

use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    Utf8TextContractV1,
};
use serde_json::{Value, json};

use crate::schema::object_schema;
use crate::{
    NamespacePresentation, PresentationProvider, RegisteredToolId, RegisteredToolIdentity,
    ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture,
    ToolInvocation, ToolRegistration, ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind,
};

const MAX_CELL_WAIT_TIME_MS: u32 = 600_000;

// ---------------------------------------------------------------------------
// Product JavaScript (js-ptc)
// ---------------------------------------------------------------------------

/// Provider-visible ordinary function that starts one product-owned JavaScript evaluation.
pub const PRODUCT_JAVASCRIPT_RUN_NAME_V1: &str = "exec_js";
/// Provider-visible ordinary function that waits on one yielded evaluation.
pub const PRODUCT_JAVASCRIPT_WAIT_NAME_V1: &str = "wait_js";
/// Driver-only executor identity shared by the product-owned outer markers.
pub const PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1: &str =
    "plakous.product-javascript-composition.driver-marker.v1";
/// Semantic revision every product JavaScript marker definition carries.
pub const PRODUCT_JAVASCRIPT_PROFILE_REVISION_V1: &str =
    "plakous.product-javascript-composition.v3";

/// The model-visible `exec_js` argument schema.
#[must_use]
pub fn product_javascript_run_schema() -> Value {
    object_schema(
        json!({
            "source": {
                "type": "string",
                "minLength": 1
            },
            "yield_time_ms": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10_000
            },
            "max_output_tokens": {
                "type": "integer",
                "minimum": 1,
                "maximum": 10_000
            }
        }),
        &["source"],
    )
}

/// The model-visible `wait_js` argument schema.
#[must_use]
pub fn product_javascript_wait_schema() -> Value {
    object_schema(
        json!({
            "cell_id": {"type": "string", "minLength": 1},
            "yield_time_ms": {"type": "integer", "minimum": 0, "maximum": MAX_CELL_WAIT_TIME_MS},
            "max_tokens": {"type": "integer", "minimum": 1, "maximum": 10_000},
            "terminate": {"type": "boolean"}
        }),
        &["cell_id"],
    )
}

/// Registers both product JavaScript outer markers.
///
/// # Errors
///
/// Returns the registry's normal invalid-definition, collision, or capacity error.
pub fn register_product_javascript_markers_into(
    registry: &mut ToolRegistryBuilder,
) -> Result<(), ToolRuntimeError> {
    let executor: Arc<dyn ToolExecutor> = Arc::new(DriverMediatedProductJavaScriptMarker);
    let executor_id = ToolExecutorId::new(PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1)?;
    let binding = ToolExecutorBindingIdentity::new(
        1,
        &[
            b"plakous-tools/product-javascript-composition-driver-marker-v1",
            CODEX_NATIVE_CODE_MODE_RELEASE_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_COMMIT_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1.as_bytes(),
        ],
    )?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.product-javascript.run-marker.v1")?,
        executor_id.clone(),
        binding,
        product_javascript_marker_definition(
            PRODUCT_JAVASCRIPT_RUN_NAME_V1,
            "Driver-mediated product JavaScript run marker; ordinary execution is forbidden.",
            product_javascript_run_schema(),
        )?,
        Arc::clone(&executor),
    ))?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.product-javascript.wait-marker.v1")?,
        executor_id,
        binding,
        product_javascript_marker_definition(
            PRODUCT_JAVASCRIPT_WAIT_NAME_V1,
            "Driver-mediated product JavaScript wait marker; ordinary execution is forbidden.",
            product_javascript_wait_schema(),
        )?,
        executor,
    ))
}

#[derive(Debug)]
struct DriverMediatedProductJavaScriptMarker;

impl ToolExecutor for DriverMediatedProductJavaScriptMarker {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "product JavaScript outer marker requires private-driver-mediated handling",
            ))
        })
    }
}

fn product_javascript_marker_definition(
    name: &str,
    description: &str,
    schema: Value,
) -> Result<ToolDefinition, ToolRuntimeError> {
    ToolDefinition::new(
        ToolName::new(name.to_owned()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "product JavaScript marker name is invalid",
                &error,
            )
        })?,
        PRODUCT_JAVASCRIPT_PROFILE_REVISION_V1,
        description,
        ToolInputContract::structured(schema).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "product JavaScript marker input contract is invalid",
                &error,
            )
        })?,
        ToolOutputContract::model_content(128, 1_048_576).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "product JavaScript marker output contract is invalid",
                &error,
            )
        })?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "product JavaScript marker definition is invalid",
            &error,
        )
    })
}

// ---------------------------------------------------------------------------
// Python PTC (py-ptc)
// ---------------------------------------------------------------------------

/// Provider-visible entry point for starting one Python cell.
pub const PYTHON_PTC_RUN_NAME_V1: &str = "exec_py";
/// Provider-visible observation of one yielded Python cell.
pub const PYTHON_PTC_WAIT_NAME_V1: &str = "wait_py";
/// Driver-only executor shared by every outer marker for this profile.
pub const PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1: &str = "plakous.python-ptc.driver-marker.v1";
/// Semantic revision every Python PTC marker definition carries.
pub const PYTHON_PTC_PROFILE_REVISION_V1: &str = "plakous.python-ptc-profile.v3";
/// Maximum UTF-8 bytes one Python cell source may carry.
pub const PYTHON_PTC_SOURCE_BYTES_V1: u32 = 262_144;
/// Maximum bytes one Python cell observation may return.
pub const PYTHON_PTC_MAX_VISIBLE_OUTPUT_BYTES_V1: u64 = 1_048_576;

/// Canonical name of the raw-UTF-8 Python cell marker.
pub const PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1: &str = "plakous_python_custom_run_marker";

/// The model-visible structured `exec_py` argument schema.
#[must_use]
pub fn python_ptc_run_schema() -> Value {
    object_schema(
        json!({
            "code": {"type": "string", "minLength": 1},
            "yield_time_ms": {"type": "integer", "minimum": 0, "maximum": 10_000},
            "max_output_bytes": {
                "type": "integer",
                "minimum": 1,
                "maximum": PYTHON_PTC_MAX_VISIBLE_OUTPUT_BYTES_V1
            }
        }),
        &["code"],
    )
}

/// The model-visible `wait_py` argument schema (output caps are UTF-8 bytes).
#[must_use]
pub fn python_ptc_wait_schema() -> Value {
    object_schema(
        json!({
            "cell_id": {"type": "string", "minLength": 1},
            "yield_time_ms": {"type": "integer", "minimum": 0, "maximum": MAX_CELL_WAIT_TIME_MS},
            "max_output_bytes": {"type": "integer", "minimum": 1, "maximum": PYTHON_PTC_MAX_VISIBLE_OUTPUT_BYTES_V1},
            "terminate": {"type": "boolean"}
        }),
        &["cell_id"],
    )
}

/// Registers the structured-run, custom-run and wait Python PTC markers.
///
/// # Errors
///
/// Returns the registry's normal invalid-definition, collision, or capacity error.
pub fn register_python_ptc_markers_into(
    registry: &mut ToolRegistryBuilder,
) -> Result<(), ToolRuntimeError> {
    let executor: Arc<dyn ToolExecutor> = Arc::new(DriverMediatedPythonMarker);
    let executor_id = ToolExecutorId::new(PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1)?;
    let binding = ToolExecutorBindingIdentity::new(
        1,
        &[
            b"plakous-tools/python-ptc-driver-marker-v1",
            PYTHON_PTC_PROFILE_REVISION_V1.as_bytes(),
        ],
    )?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.python-ptc.run-marker.v1")?,
        executor_id.clone(),
        binding,
        python_ptc_marker_definition(
            PYTHON_PTC_RUN_NAME_V1,
            "Driver-mediated structured local Python run marker; ordinary execution is forbidden.",
            ToolInputContract::structured(python_ptc_run_schema())
                .map_err(python_ptc_definition_error)?,
        )?,
        Arc::clone(&executor),
    ))?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.python-ptc.custom-run-marker.v1")?,
        executor_id.clone(),
        binding,
        python_ptc_marker_definition(
            PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1,
            "Driver-mediated raw UTF-8 local Python run marker; ordinary execution is forbidden.",
            ToolInputContract::utf8(PYTHON_PTC_SOURCE_BYTES_V1)
                .map_err(python_ptc_definition_error)?,
        )?,
        Arc::clone(&executor),
    ))?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.python-ptc.wait-marker.v1")?,
        executor_id,
        binding,
        python_ptc_marker_definition(
            PYTHON_PTC_WAIT_NAME_V1,
            "Driver-mediated local Python wait marker; ordinary execution is forbidden.",
            ToolInputContract::structured(python_ptc_wait_schema())
                .map_err(python_ptc_definition_error)?,
        )?,
        executor,
    ))
}

#[derive(Debug)]
struct DriverMediatedPythonMarker;

impl ToolExecutor for DriverMediatedPythonMarker {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "local Python outer marker requires private-driver-mediated handling",
            ))
        })
    }
}

fn python_ptc_marker_definition(
    name: &str,
    description: &str,
    input: ToolInputContract,
) -> Result<ToolDefinition, ToolRuntimeError> {
    ToolDefinition::new(
        ToolName::new(name.to_owned()).map_err(python_ptc_definition_error)?,
        PYTHON_PTC_PROFILE_REVISION_V1,
        description,
        input,
        ToolOutputContract::utf8(1_048_576).map_err(python_ptc_definition_error)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )
    .map_err(python_ptc_definition_error)
}

fn python_ptc_definition_error(error: impl std::error::Error + 'static) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidDefinition,
        "local Python composition definition is invalid",
        &error,
    )
}

// ---------------------------------------------------------------------------
// Neutral Python SDK (py-harness-sdk)
// ---------------------------------------------------------------------------

/// Provider-visible sole tool of the neutral Python SDK composition.
pub const PYTHON_HARNESS_SDK_RUN_NAME_V1: &str = "run_py";
/// Driver-only executor of the `run_py` marker.
pub const PYTHON_HARNESS_SDK_DRIVER_MARKER_EXECUTOR_ID_V1: &str =
    "plakous.python-harness-sdk.driver-marker.v1";
/// Semantic revision the `run_py` marker definition carries.
pub const PYTHON_HARNESS_SDK_PROFILE_REVISION_V1: &str = "plakous.python-harness-sdk-profile.v1";

/// One SDK-only optional capability of `PythonHarnessSdkV1`: its canonical
/// name and the registered tool id that marks the SDK registration, so a
/// per-presentation registration of the same name never enters the SDK.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PythonHarnessSdkOptionalToolV1 {
    /// Canonical, nested-call name.
    pub name: &'static str,
    /// Registered tool id of the SDK registration.
    pub tool_id: &'static str,
}

/// `web.search`.
pub const PYTHON_HARNESS_SDK_WEB_SEARCH_V1: PythonHarnessSdkOptionalToolV1 =
    PythonHarnessSdkOptionalToolV1 {
        name: "web_search",
        tool_id: "plakous.web.web-search.sdk.v1",
    };
/// `web.fetch`.
pub const PYTHON_HARNESS_SDK_WEB_FETCH_V1: PythonHarnessSdkOptionalToolV1 =
    PythonHarnessSdkOptionalToolV1 {
        name: "web_fetch",
        tool_id: "plakous.web.web-fetch.sdk.v1",
    };
/// `web.fetch_url`.
pub const PYTHON_HARNESS_SDK_FETCH_URL_V1: PythonHarnessSdkOptionalToolV1 =
    PythonHarnessSdkOptionalToolV1 {
        name: "fetch_url",
        tool_id: "plakous.web.fetch-url.sdk.v1",
    };
/// `x.search`.
pub const PYTHON_HARNESS_SDK_X_SEARCH_V1: PythonHarnessSdkOptionalToolV1 =
    PythonHarnessSdkOptionalToolV1 {
        name: "x_search",
        tool_id: "plakous.web.x-search.sdk.v1",
    };
/// `image.generate`.
pub const PYTHON_HARNESS_SDK_IMAGE_GEN_V1: PythonHarnessSdkOptionalToolV1 =
    PythonHarnessSdkOptionalToolV1 {
        name: "image_gen",
        tool_id: "plakous.web.image-gen.sdk.v1",
    };

/// The model-visible `run_py` argument schema: exactly `{"code": string}`.
#[must_use]
pub fn python_harness_sdk_run_schema() -> Value {
    object_schema(json!({"code": {"type": "string"}}), &["code"])
}

/// The identity-bearing limits of the neutral Python SDK composition
/// (`PythonHarnessSdkLimitsV1`). They are not configuration: a later layer
/// that changes a value revises the descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PythonHarnessSdkLimitsV1 {
    /// Maximum UTF-8 source bytes of one cell, refused before any checkout.
    pub source_bytes: u64,
    /// Engine allocator ceiling inside the worker.
    pub engine_memory_bytes: u64,
    /// Pure compute per cell, excluding host suspensions.
    pub compute_ms: u64,
    /// Foreground wall budget of one cell from admission, nested calls included.
    pub foreground_budget_ms: u64,
    /// Background-cell slots per agent; zero stops a cell at the budget.
    pub background_cell_slots: u32,
    /// Nested calls per cell; the next one stops the cell.
    pub nested_calls: u32,
    /// Concurrent nested calls; later calls wait in order.
    pub concurrent_nested_calls: u32,
    /// Helper requests per agent turn, reserved at the nested-call choke point.
    pub helper_requests_per_turn: u32,
    /// Host suspensions per cell.
    pub suspensions: u32,
    /// Interpreter recursion frames.
    pub recursion_frames: u32,
    /// Model-visible output bytes of one cell observation.
    pub output_bytes: u64,
    /// Images attached to one cell observation.
    pub images: u32,
    /// Bytes of one attached image.
    pub image_bytes: u64,
    /// Named-state names per agent.
    pub state_names: u32,
    /// Canonical JSON bytes of one named-state value.
    pub state_value_bytes: u64,
    /// Canonical JSON bytes of all named-state values together.
    pub state_total_bytes: u64,
    /// Encoded bytes of one nested argument.
    pub nested_argument_bytes: u64,
    /// Encoded bytes of one nested result.
    pub nested_result_bytes: u64,
    /// Encoded bytes of all nested results of one cell.
    pub aggregate_nested_result_bytes: u64,
}

impl PythonHarnessSdkLimitsV1 {
    /// The Gate A values.
    pub const EXACT: Self = Self {
        source_bytes: 262_144,
        engine_memory_bytes: 268_435_456,
        compute_ms: 90_000,
        foreground_budget_ms: 300_000,
        background_cell_slots: 0,
        nested_calls: 100,
        concurrent_nested_calls: 16,
        helper_requests_per_turn: 32,
        suspensions: 1_000,
        recursion_frames: 1_000,
        output_bytes: 16_384,
        images: 2,
        image_bytes: 196_608,
        state_names: 64,
        state_value_bytes: 1_048_576,
        state_total_bytes: 16_777_216,
        nested_argument_bytes: 1_048_576,
        nested_result_bytes: 1_048_576,
        aggregate_nested_result_bytes: 16_777_216,
    };
}

/// Registers the `run_py` marker.
///
/// # Errors
///
/// Returns the registry's normal invalid-definition, collision, or capacity error.
pub fn register_python_harness_sdk_markers_into(
    registry: &mut ToolRegistryBuilder,
) -> Result<(), ToolRuntimeError> {
    let definition = ToolDefinition::new(
        ToolName::new(PYTHON_HARNESS_SDK_RUN_NAME_V1.to_owned())
            .map_err(python_ptc_definition_error)?,
        PYTHON_HARNESS_SDK_PROFILE_REVISION_V1,
        "Driver-mediated neutral Python SDK run marker; ordinary execution is forbidden.",
        ToolInputContract::structured(python_harness_sdk_run_schema())
            .map_err(python_ptc_definition_error)?,
        ToolOutputContract::model_content(128, 1_048_576).map_err(python_ptc_definition_error)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )
    .map_err(python_ptc_definition_error)?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.python-harness-sdk.run-marker.v1")?,
        ToolExecutorId::new(PYTHON_HARNESS_SDK_DRIVER_MARKER_EXECUTOR_ID_V1)?,
        ToolExecutorBindingIdentity::new(
            1,
            &[
                b"plakous-tools/python-harness-sdk-driver-marker-v1",
                PYTHON_HARNESS_SDK_PROFILE_REVISION_V1.as_bytes(),
            ],
        )?,
        definition,
        Arc::new(DriverMediatedPythonMarker),
    ))
}

// ---------------------------------------------------------------------------
// Codex Code Mode (native-ptc)
// ---------------------------------------------------------------------------

/// Exact release source for this presentation contract.
pub const CODEX_NATIVE_CODE_MODE_RELEASE_V1: &str = "rust-v0.153.3";
/// Exact peeled release commit for this presentation contract.
pub const CODEX_NATIVE_CODE_MODE_COMMIT_V1: &str = "b1a547b1f73ce86205d9222ac19cff334b3b7a2e";
/// rquickjs sidecar engine revision mixed into Code Mode identity.
///
/// Official V8 ELF digest is not live identity. Presentation text still names
/// a V8 isolate because that is the frozen 0.153.3 model-visible contract.
pub const CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1: &str =
    "rquickjs-host/plakous-js-eval-bootstrap.v5";
/// Canonical driver marker name and provider-visible custom-tool name.
pub const CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1: &str = "exec";
/// Canonical driver marker name and provider-visible function-tool name.
pub const CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1: &str = "wait";
/// Stable executor identity proving that outer calls require driver mediation.
pub const CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1: &str =
    "plakous.codex-code-mode.driver-marker.v1";
/// Semantic revision every Code Mode marker definition carries.
pub const CODE_MODE_MARKER_VERSION_V1: &str = "codex-native-local-code-mode-v1";
/// Maximum UTF-8 bytes one Code Mode `exec` cell source may carry.
pub const CODE_MODE_EXEC_MAX_BYTES_V1: u32 = 1_048_576;

/// The model-visible Code Mode `wait` argument schema.
#[must_use]
pub fn codex_code_mode_wait_schema() -> Value {
    object_schema(
        json!({
            "cell_id": {
                "type": "string",
                "description": "Identifier of the running exec cell."
            },
            "yield_time_ms": {
                "type": "number",
                "maximum": MAX_CELL_WAIT_TIME_MS,
                "description": "Wait before yielding more output. Defaults to 10000 ms; clamped to at most 600000 ms."
            },
            "max_tokens": {
                "type": "number",
                "description": "Output token budget for this wait call. Defaults to 10000 tokens."
            },
            "terminate": {
                "type": "boolean",
                "description": "True stops the running exec cell; false or omitted waits for output."
            }
        }),
        &["cell_id"],
    )
}

/// The Code Mode `exec` marker definition.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in bound cannot be frozen.
pub fn codex_code_mode_exec_marker_definition() -> Result<ToolDefinition, ToolRuntimeError> {
    codex_code_mode_marker_definition(
        CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
        "Driver-mediated Codex Code Mode exec marker; ordinary execution is forbidden.",
        ToolInputContract::Utf8(
            Utf8TextContractV1::new(CODE_MODE_EXEC_MAX_BYTES_V1).map_err(|error| {
                code_mode_definition_error("Code Mode exec UTF-8 contract is invalid", &error)
            })?,
        ),
    )
}

/// The Code Mode `wait` marker definition.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in bound cannot be frozen.
pub fn codex_code_mode_wait_marker_definition() -> Result<ToolDefinition, ToolRuntimeError> {
    codex_code_mode_marker_definition(
        CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
        "Driver-mediated Codex Code Mode wait marker; ordinary execution is forbidden.",
        ToolInputContract::structured(codex_code_mode_wait_schema()).map_err(|error| {
            code_mode_definition_error("Code Mode wait input contract is invalid", &error)
        })?,
    )
}

/// Registers both Codex Code Mode outer markers.
///
/// # Errors
///
/// Returns the registry's normal invalid-definition, collision, or capacity error.
pub fn register_codex_code_mode_markers_into(
    registry: &mut ToolRegistryBuilder,
) -> Result<(), ToolRuntimeError> {
    let executor: Arc<dyn ToolExecutor> = Arc::new(DriverMediatedCodeModeMarker);
    let executor_id = ToolExecutorId::new(CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1)?;
    let binding = ToolExecutorBindingIdentity::new(
        1,
        &[
            b"plakous-tools/codex-native-code-mode-driver-marker-v1",
            CODEX_NATIVE_CODE_MODE_RELEASE_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_COMMIT_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1.as_bytes(),
        ],
    )?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.codex-code-mode.exec-marker.v1")?,
        executor_id.clone(),
        binding,
        codex_code_mode_exec_marker_definition()?,
        Arc::clone(&executor),
    ))?;
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.codex-code-mode.wait-marker.v1")?,
        executor_id,
        binding,
        codex_code_mode_wait_marker_definition()?,
        executor,
    ))
}

#[derive(Debug)]
struct DriverMediatedCodeModeMarker;

impl ToolExecutor for DriverMediatedCodeModeMarker {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "Codex Code Mode outer marker requires private-driver-mediated handling",
            ))
        })
    }
}

fn codex_code_mode_marker_definition(
    name: &str,
    description: &str,
    input: ToolInputContract,
) -> Result<ToolDefinition, ToolRuntimeError> {
    ToolDefinition::new(
        ToolName::new(name.to_owned()).map_err(|error| {
            code_mode_definition_error("Code Mode marker name is invalid", &error)
        })?,
        CODE_MODE_MARKER_VERSION_V1,
        description,
        input,
        ToolOutputContract::model_content(128, 1_048_576).map_err(|error| {
            code_mode_definition_error("Code Mode marker output contract is invalid", &error)
        })?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )
    .map_err(|error| code_mode_definition_error("Code Mode marker definition is invalid", &error))
}

fn code_mode_definition_error(
    message: &'static str,
    error: &(impl std::error::Error + 'static),
) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(ToolRuntimeErrorKind::InvalidDefinition, message, error)
}

/// The two `wait` vocabularies whose `cell_id` a model may echo as a numeral.
const WAIT_MARKER_NAMES_V1: [&str; 3] = [
    PYTHON_PTC_WAIT_NAME_V1,
    PRODUCT_JAVASCRIPT_WAIT_NAME_V1,
    CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
];

/// The words a model writes when it means "no particular cell".
///
/// A model that has lost track of whether a cell is running writes one of these
/// rather than inventing an identifier, which is the sane thing to do. Each
/// means the same thing, and with at most one cell active per agent that thing
/// is unambiguous: wait on whatever is running.
///
/// Every entry is a non-empty word, because the presented contract requires a
/// non-empty string and this superset never widens it. An absent, null or empty
/// `cell_id` is a schema violation like any other and is refused as one; it is
/// not a spelling of anything.
const WAIT_NO_SPECIFIC_CELL_V1: [&str; 3] = ["none", "null", "all"];

/// Accepts a `wait` `cell_id` written as a JSON number, or as one of the
/// spellings a model uses for "no particular cell", and normalises it to the
/// string the contract declares.
/// Whole, non-negative wait hints also clamp to the common 600-second ceiling
/// before schema validation. Other hint shapes are left for validation.
///
/// **This is an accepted superset, not a schema change.** The presented schema
/// and description are identity-bearing and stay byte-identical: a model is
/// still told `cell_id` is a string, and the normalised arguments are what the
/// schema then validates and what dispatch receives. Nothing downstream sees a
/// number.
///
/// It exists because the product hands the model a bare numeral — a cell
/// announces itself as `Script running with cell ID 2` — and a model that
/// echoes that identifier as the number it looks like had its call rejected
/// before dispatch, with no result it could learn from. Accepting the numeral
/// costs nothing and is not ambiguous: `2` and `"2"` name the same cell.
///
/// One of `WAIT_NO_SPECIFIC_CELL_V1` becomes `active_cell` when a cell is
/// running. With no cell running the arguments are left alone: each of those
/// words is a valid non-empty string, so the contract admits the call and the
/// driver answers it before dispatch, because a wait with nothing to wait for
/// is a question with an answer and not a mistake.
///
/// Anything else is left exactly as it arrived, so the schema still decides it.
/// That includes an absent, null or empty `cell_id`, which the contract refuses
/// for the field it is missing whether or not a cell is running.
///
/// Only a call that [`is_cell_wait_marker`] admits may be normalised; the
/// callers decide by the called tool's canonical identity, never by its
/// spelling, so a non-marker tool exposed as `wait` is left alone.
#[must_use]
pub fn normalized_wait_arguments(arguments: &str, active_cell: Option<&str>) -> Option<String> {
    let mut value: Value = serde_json::from_str(arguments).ok()?;
    let object = value.as_object_mut()?;
    let replacement = match object.get("cell_id") {
        // A cell identifier is a whole non-negative count, and JSON carries it
        // as a number with no fraction and no exponent noise.
        Some(cell_id) if cell_id.is_number() => {
            cell_id.as_u64().map(|id| Value::String(id.to_string()))
        }
        Some(Value::String(spelling))
            if WAIT_NO_SPECIFIC_CELL_V1
                .contains(&spelling.trim().to_ascii_lowercase().as_str()) =>
        {
            active_cell.map(|id| Value::String(id.to_owned()))
        }
        None | Some(_) => None,
    };
    let mut changed = false;
    if let Some(replacement) = replacement {
        object.insert("cell_id".to_owned(), replacement);
        changed = true;
    }
    if let Some(hint) = object.get("yield_time_ms").and_then(Value::as_f64)
        && hint.fract() == 0.0
        && hint > f64::from(MAX_CELL_WAIT_TIME_MS)
        && hint <= 9_007_199_254_740_991.0
    {
        object.insert("yield_time_ms".to_owned(), json!(MAX_CELL_WAIT_TIME_MS));
        changed = true;
    }
    if !changed {
        return None;
    }
    serde_json::to_string(&value).ok()
}

/// Reports whether one canonical identity is a composition `wait` marker: a
/// marker executor registered under one of the cell-wait names.
#[must_use]
pub fn is_cell_wait_marker(identity: &RegisteredToolIdentity) -> bool {
    is_composition_marker_executor(identity.executor_id())
        && WAIT_MARKER_NAMES_V1.contains(&identity.name().as_str())
}

/// Reports whether one executor id is a composition driver marker.
///
/// A marker executor never dispatches ordinarily and never enters a nested
/// catalog; both refusals read this one table.
#[must_use]
pub fn is_composition_marker_executor(executor: &ToolExecutorId) -> bool {
    matches!(
        executor.as_str(),
        PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1
            | PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1
            | PYTHON_HARNESS_SDK_DRIVER_MARKER_EXECUTOR_ID_V1
            | CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1
    )
}

/// A program runtime: the composition whose outer markers own the
/// provider-visible namespace while pack and orchestration tools are nested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositionRuntime {
    /// Codex-native Code Mode (`native-ptc`): `exec` and `wait`.
    CodexNative,
    /// Product JavaScript (`js-ptc`): `exec_js` and `wait_js`.
    ProductJavaScript,
    /// Product Python (`py-ptc`): `exec_py` and `wait_py`.
    PythonPtc,
}

impl CompositionRuntime {
    /// The runtime's name in diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CodexNative => "native-ptc",
            Self::ProductJavaScript => "js-ptc",
            Self::PythonPtc => "py-ptc",
        }
    }

    /// The runtime's two provider-visible markers: its run marker, then its
    /// wait marker. This is the one marker vocabulary; a product selection
    /// names its outer tools through it.
    #[must_use]
    pub const fn marker_names(self) -> &'static [&'static str] {
        match self {
            Self::CodexNative => &[
                CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
                CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
            ],
            Self::ProductJavaScript => &[
                PRODUCT_JAVASCRIPT_RUN_NAME_V1,
                PRODUCT_JAVASCRIPT_WAIT_NAME_V1,
            ],
            Self::PythonPtc => &[PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_WAIT_NAME_V1],
        }
    }

    /// The runtime's outer markers as presentations: each covers its own
    /// marker slot and declares no alternate, so it holds its name in both
    /// namespaces.
    ///
    /// # Errors
    ///
    /// Returns an invalid-definition error if a marker name cannot be a tool name.
    pub fn marker_presentations(self) -> Result<Vec<NamespacePresentation>, ToolRuntimeError> {
        let &[run, wait] = self.marker_names() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidDefinition,
                "a composition runtime has exactly two outer markers",
            ));
        };
        let run_slots: &[&str] = match self {
            Self::PythonPtc => &[PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1],
            Self::CodexNative | Self::ProductJavaScript => &[run],
        };
        let marker = |exposed: &str, slots: &[&str]| -> Result<_, ToolRuntimeError> {
            Ok(NamespacePresentation::new(
                PresentationProvider::Runtime(self.name()),
                marker_name(exposed)?,
                slots
                    .iter()
                    .map(|slot| marker_name(slot))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        };
        Ok(vec![marker(run, run_slots)?, marker(wait, &[wait])?])
    }
}

fn marker_name(name: &str) -> Result<ToolName, ToolRuntimeError> {
    ToolName::new(name.to_owned()).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "composition marker name is invalid",
            &error,
        )
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod wait_contract_tests {
    use super::{codex_code_mode_wait_schema, product_javascript_wait_schema};
    use plakous_model::PortableToolSchemaV2;
    use serde_json::{Value, json};

    fn accepts(schema: &Value, arguments: &Value) -> bool {
        PortableToolSchemaV2::new(schema.clone())
            .expect("wait schema is valid")
            .parse_instance(&arguments.to_string())
            .is_ok()
    }

    #[test]
    fn cell_wait_contracts_share_a_600_second_ceiling() {
        for schema in [
            codex_code_mode_wait_schema(),
            product_javascript_wait_schema(),
        ] {
            assert_eq!(
                schema["properties"]["yield_time_ms"]["maximum"],
                json!(600_000)
            );
            assert!(accepts(
                &schema,
                &json!({"cell_id": "2", "yield_time_ms": 600_000})
            ));
            assert!(!accepts(
                &schema,
                &json!({"cell_id": "2", "yield_time_ms": 600_001})
            ));
        }
    }

    #[test]
    fn cell_wait_hints_clamp_before_schema_validation() {
        for cell in [json!("2"), json!(2), json!("none")] {
            let arguments = json!({"cell_id": cell, "yield_time_ms": 900_000, "max_tokens": 500, "terminate": false});
            let normalized = super::normalized_wait_arguments(&arguments.to_string(), Some("2"))
                .expect("a large hint is normalized");
            assert_eq!(
                serde_json::from_str::<Value>(&normalized).expect("normalized JSON"),
                json!({"cell_id": "2", "yield_time_ms": 600_000, "max_tokens": 500, "terminate": false})
            );
        }
        for hint in [0, 10_000, 600_000] {
            let arguments = json!({"cell_id": "2", "yield_time_ms": hint}).to_string();
            assert!(super::normalized_wait_arguments(&arguments, None).is_none());
        }
        for hint in [json!(-1), json!(600_000.5), json!("900000"), Value::Null] {
            let arguments = json!({"cell_id": "2", "yield_time_ms": hint}).to_string();
            assert!(super::normalized_wait_arguments(&arguments, None).is_none());
        }
    }

    /// Both `wait` vocabularies require `cell_id` as a **string**, while the
    /// cell identifiers the product hands the model are bare numerals: a cell
    /// announces itself as "Script running with cell ID 2".
    ///
    /// This pins the presented contract, which is identity-bearing and does
    /// not change. `normalized_wait_arguments` is what lets a model echo the
    /// numeral it was shown, by normalising before this contract validates.
    #[test]
    fn a_wait_cell_id_is_accepted_as_a_string_and_refused_as_the_number_it_looks_like() {
        for schema in [
            codex_code_mode_wait_schema(),
            product_javascript_wait_schema(),
        ] {
            assert!(accepts(&schema, &json!({"cell_id": "2"})));
            assert!(!accepts(&schema, &json!({"cell_id": 2})));
            // The other two shapes a rejected `wait` could have had.
            assert!(!accepts(&schema, &json!({})));
            assert!(!accepts(&schema, &json!({"cell_id": "2", "cell": "2"})));
        }
        // `terminate` and the two budgets are optional in both vocabularies.
        assert!(accepts(
            &codex_code_mode_wait_schema(),
            &json!({"cell_id": "2", "yield_time_ms": 1000, "max_tokens": 500, "terminate": false})
        ));
    }

    /// Normalisation turns the numeral a model was shown into the string the
    /// contract declares, and leaves everything else exactly as it arrived so
    /// the contract still decides.
    #[test]
    fn normalisation_accepts_a_numeric_cell_id_and_touches_nothing_else() {
        use super::normalized_wait_arguments;

        assert_eq!(
            normalized_wait_arguments(r#"{"cell_id":2,"terminate":true}"#, None).as_deref(),
            Some(r#"{"cell_id":"2","terminate":true}"#)
        );
        assert_eq!(
            normalized_wait_arguments(r#"{"cell_id":0}"#, None).as_deref(),
            Some(r#"{"cell_id":"0"}"#)
        );
        // Already a string, or a shape the contract must refuse: nothing
        // to normalise, so the original bytes are validated.
        for untouched in [
            r#"{"cell_id":"2"}"#,
            r#"{"cell_id":true}"#,
            r#"{"cell_id":-1}"#,
            r#"{"cell_id":1.5}"#,
            r#"{"cell_id":["2"]}"#,
            // Absent, null and empty are schema violations, not spellings.
            r"{}",
            r#"{"cell_id":null}"#,
            r#"{"cell_id":""}"#,
            "not json",
        ] {
            assert_eq!(
                normalized_wait_arguments(untouched, Some("7")),
                None,
                "{untouched}"
            );
        }
    }

    /// A call sent
    /// `{"cell_id": "none", "yield_time_ms": 1}` and the call was rejected
    /// `InvalidArguments`, twice, which converted the whole case to a
    /// `product_failure`. A model that has lost track of whether a cell is
    /// running writes one of these rather than inventing an identifier.
    ///
    /// With at most one cell active per agent, "no particular cell" names the
    /// running one and nothing else.
    ///
    /// Every word here is a valid non-empty string, so this superset never
    /// widens the presented contract: it reinterprets values the contract
    /// already admits. An absent, null or empty `cell_id` is a schema violation
    /// and stays one.
    #[test]
    fn every_spelling_of_no_particular_cell_names_the_running_one() {
        use super::normalized_wait_arguments;

        for spelling in [
            r#"{"cell_id":"none"}"#,
            r#"{"cell_id":"NONE"}"#,
            r#"{"cell_id":" none "}"#,
            r#"{"cell_id":"null"}"#,
            r#"{"cell_id":"all"}"#,
        ] {
            assert_eq!(
                normalized_wait_arguments(spelling, Some("3")).as_deref(),
                Some(r#"{"cell_id":"3"}"#),
                "{spelling}"
            );
        }
        // The rest of the call survives normalisation untouched.
        assert_eq!(
            normalized_wait_arguments(r#"{"cell_id":"none","yield_time_ms":1}"#, Some("3"))
                .as_deref(),
            Some(r#"{"cell_id":"3","yield_time_ms":1}"#)
        );
        // With nothing running there is nothing to name, so the arguments
        // are left alone. Each word is still a valid string, so the contract
        // admits the call and the driver answers it before dispatch.
        for spelling in [
            r#"{"cell_id":"none"}"#,
            r#"{"cell_id":"null"}"#,
            r#"{"cell_id":"all"}"#,
        ] {
            assert_eq!(
                normalized_wait_arguments(spelling, None),
                None,
                "{spelling}"
            );
        }
        // Absent, null and empty are refused by the contract for the field
        // they are missing, whether or not a cell is running.
        for violation in [r"{}", r#"{"cell_id":null}"#, r#"{"cell_id":""}"#] {
            for active in [Some("3"), None] {
                assert_eq!(
                    normalized_wait_arguments(violation, active),
                    None,
                    "{violation}"
                );
            }
        }
        // A named cell is never reinterpreted as "no particular cell".
        assert_eq!(
            normalized_wait_arguments(r#"{"cell_id":"nonesuch"}"#, Some("3")),
            None
        );
    }

    /// Normalising a call twice is the same as normalising it once. The tool
    /// exposure applies this before validation with no active cell in hand, and
    /// the driver applies it with one, so both run over the same call.
    #[test]
    fn normalisation_is_idempotent_across_both_application_points() {
        use super::normalized_wait_arguments;

        let once =
            normalized_wait_arguments(r#"{"cell_id":"none"}"#, Some("5")).expect("first pass");
        assert_eq!(once, r#"{"cell_id":"5"}"#);
        assert_eq!(normalized_wait_arguments(&once, Some("5")), None);
        assert_eq!(normalized_wait_arguments(&once, None), None);
    }
}
