use plakous_tools::{
    CODE_MODE_EXEC_MAX_BYTES_V1, CODEX_NATIVE_CODE_MODE_COMMIT_V1,
    CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1, CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1,
    CODEX_NATIVE_CODE_MODE_RELEASE_V1, PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1,
    PRODUCT_JAVASCRIPT_PROFILE_REVISION_V1, PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1,
    PYTHON_PTC_PROFILE_REVISION_V1, PYTHON_PTC_SOURCE_BYTES_V1, codex_code_mode_wait_schema,
    product_javascript_run_schema, product_javascript_wait_schema, python_ptc_run_schema,
    python_ptc_wait_schema,
};
use serde::Serialize;
use serde_json::Value;

use crate::text::{
    PLAKOUS_JS_PTC_WAIT_DESCRIPTION, PLAKOUS_PY_PTC_CUSTOM_RUN_DESCRIPTION,
    PLAKOUS_PY_PTC_WAIT_DESCRIPTION,
};

mod codex;
mod javascript;
mod python;

pub use codex::*;
pub use javascript::*;
pub use python::*;

/// The catalog's model-visible authoring language and wording.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) enum CatalogStyle {
    /// Frozen seven-tool Codex reference.
    CodexExact,
    /// Codex's minimum-optimal catalog, with explicit extensions.
    CodexMinimal,
    /// Product-owned JavaScript cells.
    ProductJavaScript,
    /// Product-owned Python coroutine cells.
    Python,
    /// Typed Python SDK with fresh globals and named session state.
    PythonSdk,
}

/// Where a running cell yields its observation window.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) enum YieldMode {
    /// The JavaScript worker parks until resumed.
    EngineParked,
    /// The host returns while the Python interpreter keeps running.
    HostWindow,
    /// A foreground cell runs to a terminal and never yields.
    Never,
}

/// Interpreter state ownership across root turns.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) enum CellLifecycle {
    /// The interpreter joins the root turn.
    RootTurnJoined,
    /// One interpreter belongs to an agent incarnation.
    AgentIncarnationPersistent,
}

/// Resource and observation bounds bound by a cell descriptor.
#[derive(Clone, Debug, Serialize)]
pub(crate) enum CellLimits {
    /// Pinned JavaScript source, catalog and observation bounds.
    JavaScript {
        /// Maximum source bytes.
        source_bytes: u32,
        /// Maximum nested tools.
        catalog_tools: usize,
        /// Default observation window.
        yield_ms: u64,
        /// Default output budget.
        output_tokens: u64,
    },
    /// Exact Python PTC engine and output bounds.
    Python(PythonPtcLimitsV1),
    /// SDK foreground, encoded-value and observation bounds.
    PythonSdk {
        source_bytes: u32,
        foreground_ms: u64,
        result_bytes: usize,
        cell_bytes: usize,
        consumes_values: bool,
    },
}

pub(super) enum OuterInput {
    Structured(Value),
    Custom { grammar: &'static str, bytes: u32 },
}

/// One authority-free outer marker and its presented input contract.
pub(crate) struct OuterTool {
    pub(super) name: &'static str,
    pub(super) marker: &'static str,
    pub(super) input_field: Option<&'static str>,
    pub(super) input: OuterInput,
}

/// Exact presentation wording and compile-time refusal messages.
pub(crate) struct CellMessages {
    pub(super) wait: &'static str,
    pub(super) custom_execute_prefix: Option<&'static str>,
    pub(super) target_refused: &'static str,
    pub(super) catalog_bound_refused: &'static str,
    pub(super) collision_refused: &'static str,
}

const JAVASCRIPT_MESSAGES: CellMessages = CellMessages {
    wait: PLAKOUS_JS_PTC_WAIT_DESCRIPTION,
    custom_execute_prefix: None,
    target_refused: "product JavaScript composition target class is not admitted",
    catalog_bound_refused: "product JavaScript nested catalog must contain 1..=64 tools",
    collision_refused: "product JavaScript nested tool names collide after host normalization",
};
const PYTHON_MESSAGES: CellMessages = CellMessages {
    wait: PLAKOUS_PY_PTC_WAIT_DESCRIPTION,
    custom_execute_prefix: Some(PLAKOUS_PY_PTC_CUSTOM_RUN_DESCRIPTION),
    target_refused: "local Python composition target class is not admitted",
    catalog_bound_refused: "local Python nested catalog must contain one through 64 tools",
    collision_refused: "local Python nested tool names collide after Python normalization",
};
const CODEX_MESSAGES: CellMessages = CellMessages {
    wait: WAIT_DESCRIPTION,
    custom_execute_prefix: None,
    target_refused: "Code Mode composition target class is not admitted",
    catalog_bound_refused: "Code Mode nested catalog membership is not exact",
    collision_refused: "Code Mode nested JavaScript tool-name collision",
};

/// Inert facts that distinguish one cell flavor from another.
pub(crate) struct CellPresentation {
    pub(super) profile: &'static str,
    pub(super) execute: OuterTool,
    pub(super) wait: Option<OuterTool>,
    pub(super) yield_mode: YieldMode,
    pub(super) lifecycle: CellLifecycle,
    pub(super) result: &'static str,
    pub(super) limits: CellLimits,
    pub(super) catalog: CatalogStyle,
    pub(super) messages: &'static CellMessages,
    pub(super) revisions: &'static [&'static str],
    pub(super) marker_executor: &'static str,
}

impl CellPresentation {
    pub(super) fn javascript() -> Self {
        Self {
            profile: PRODUCT_JAVASCRIPT_PROFILE_REVISION_V1,
            execute: OuterTool {
                name: "exec_js",
                marker: "exec_js",
                input_field: Some("source"),
                input: OuterInput::Structured(product_javascript_run_schema()),
            },
            wait: Some(OuterTool {
                name: "wait_js",
                marker: "wait_js",
                input_field: None,
                input: OuterInput::Structured(product_javascript_wait_schema()),
            }),
            yield_mode: YieldMode::EngineParked,
            lifecycle: CellLifecycle::RootTurnJoined,
            result: PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1,
            limits: Self::javascript_limits(64),
            catalog: CatalogStyle::ProductJavaScript,
            messages: &JAVASCRIPT_MESSAGES,
            revisions: &[
                "plakous.product-javascript-bounded-catalog-bridge.v1",
                CODEX_NATIVE_CODE_MODE_RELEASE_V1,
                CODEX_NATIVE_CODE_MODE_COMMIT_V1,
                CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1,
                PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_REVISION_V1,
            ],
            marker_executor: PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1,
        }
    }

    pub(super) fn python(presentation: PythonPtcPresentationV1) -> Self {
        let (marker, input_field, input) = match presentation {
            PythonPtcPresentationV1::StructuredFunctions => (
                "exec_py",
                Some("code"),
                OuterInput::Structured(python_ptc_run_schema()),
            ),
            PythonPtcPresentationV1::CustomRunFunctionWait => (
                "plakous_python_custom_run_marker",
                None,
                OuterInput::Custom {
                    grammar: "start: /(?s).+/",
                    bytes: PYTHON_PTC_SOURCE_BYTES_V1,
                },
            ),
        };
        Self {
            profile: PYTHON_PTC_PROFILE_REVISION_V1,
            execute: OuterTool {
                name: "exec_py",
                marker,
                input_field,
                input,
            },
            wait: Some(OuterTool {
                name: "wait_py",
                marker: "wait_py",
                input_field: None,
                input: OuterInput::Structured(python_ptc_wait_schema()),
            }),
            yield_mode: YieldMode::HostWindow,
            lifecycle: CellLifecycle::AgentIncarnationPersistent,
            result: PYTHON_PTC_RESULT_GRAMMAR_V1,
            limits: CellLimits::Python(PythonPtcLimitsV1::EXACT),
            catalog: CatalogStyle::Python,
            messages: &PYTHON_MESSAGES,
            revisions: &[
                "plakous.python-ptc-presentation.v4",
                "plakous.python-ptc-catalog.v2",
                "plakous.python-ptc-limits.v2",
                "plakous.python-ptc-result.v1",
            ],
            marker_executor: PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1,
        }
    }

    pub(super) fn codex(exact: bool, profile: &'static str) -> Self {
        Self {
            profile,
            execute: OuterTool {
                name: "exec",
                marker: "exec",
                input_field: None,
                input: OuterInput::Custom {
                    grammar: CODEX_NATIVE_CODE_MODE_EXEC_GRAMMAR_V1,
                    bytes: CODE_MODE_EXEC_MAX_BYTES_V1,
                },
            },
            wait: Some(OuterTool {
                name: "wait",
                marker: "wait",
                input_field: None,
                input: OuterInput::Structured(codex_code_mode_wait_schema()),
            }),
            yield_mode: YieldMode::EngineParked,
            lifecycle: CellLifecycle::RootTurnJoined,
            result: PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1,
            limits: Self::javascript_limits(if exact { 7 } else { 64 }),
            catalog: if exact {
                CatalogStyle::CodexExact
            } else {
                CatalogStyle::CodexMinimal
            },
            messages: &CODEX_MESSAGES,
            revisions: &[
                CODEX_NATIVE_CODE_MODE_RELEASE_V1,
                CODEX_NATIVE_CODE_MODE_COMMIT_V1,
                CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1,
            ],
            marker_executor: CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
        }
    }

    fn javascript_limits(catalog_tools: usize) -> CellLimits {
        CellLimits::JavaScript {
            source_bytes: CODE_MODE_EXEC_MAX_BYTES_V1,
            catalog_tools,
            yield_ms: CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1,
            output_tokens: CODEX_NATIVE_CODE_MODE_DEFAULT_OUTPUT_TOKENS_V1,
        }
    }
}
