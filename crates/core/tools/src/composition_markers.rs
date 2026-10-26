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

/// Driver-only executor for the typed foreground Python SDK.
pub const PYTHON_SDK_DRIVER_MARKER_EXECUTOR_ID_V1: &str = "plakous.python-sdk.driver-marker.v1";

/// Provider-visible function that starts one typed Python SDK cell.
pub const PYTHON_SDK_RUN_NAME_V1: &str = "run_py";

/// Registers the SDK's sole outer marker; ordinary execution fails closed.
///
/// # Errors
/// Returns the ordinary definition or registration refusal.
pub fn register_python_sdk_marker_into(
    registry: &mut ToolRegistryBuilder,
) -> Result<(), ToolRuntimeError> {
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("plakous.python-sdk.run-marker.v1")?,
        ToolExecutorId::new(PYTHON_SDK_DRIVER_MARKER_EXECUTOR_ID_V1)?,
        ToolExecutorBindingIdentity::new(1, &[b"plakous-tools/python-sdk-driver-marker-v1"])?,
        ToolDefinition::new(
            ToolName::new(PYTHON_SDK_RUN_NAME_V1).map_err(python_ptc_definition_error)?,
            "plakous.python-sdk.v1",
            "Driver-mediated typed Python SDK marker; ordinary execution is forbidden.",
            ToolInputContract::structured(object_schema(
                json!({"code":{"type":"string","minLength":1,"maxLength":262_144}}),
                &["code"],
            ))
            .map_err(python_ptc_definition_error)?,
            ToolOutputContract::utf8(16_384).map_err(python_ptc_definition_error)?,
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )
        .map_err(python_ptc_definition_error)?,
        Arc::new(DriverMediatedPythonMarker),
    ))
}

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
// Codex Code Mode (native-ptc)
// ---------------------------------------------------------------------------

/// Exact release source for this presentation contract.
pub const CODEX_NATIVE_CODE_MODE_RELEASE_V1: &str = "rust-v0.153.3";
/// Exact peeled release commit for this presentation contract.
pub const CODEX_NATIVE_CODE_MODE_COMMIT_V1: &str = "b1a547b1f73ce86205d9222ac19cff334b3b7a2e";
/// `QuickJS` cell worker revision mixed into Code Mode identity: engine,
/// cell protocol and worker adapter.
///
/// Official V8 ELF digest is not live identity. Presentation text still names
/// a V8 isolate because that is the frozen 0.153.3 model-visible contract.
pub const CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1: &str =
    "quickjs/plakous-cell-protocol.v2/plakous-js-host-bootstrap.v7";
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
            | PYTHON_SDK_DRIVER_MARKER_EXECUTOR_ID_V1
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
    /// Root-only typed Python harness SDK (`py-sdk`): `run_py`.
    PythonHarnessSdk,
}

impl CompositionRuntime {
    /// The runtime's name in diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CodexNative => "native-ptc",
            Self::ProductJavaScript => "js-ptc",
            Self::PythonPtc => "py-ptc",
            Self::PythonHarnessSdk => "py-sdk",
        }
    }

    /// The runtime's provider-visible markers: its run marker, then its
    /// optional wait marker. This is the one marker vocabulary; a product selection
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
            Self::PythonHarnessSdk => &[PYTHON_SDK_RUN_NAME_V1],
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
        self.marker_names()
            .iter()
            .enumerate()
            .map(|(index, &name)| {
                let slots: &[&str] = if self == Self::PythonPtc && index == 0 {
                    &[PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_CUSTOM_RUN_MARKER_NAME_V1]
                } else {
                    &[name]
                };
                marker(name, slots)
            })
            .collect()
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
mod wait_contract_tests;
