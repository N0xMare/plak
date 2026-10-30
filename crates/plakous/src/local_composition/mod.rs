//! Capability-poor attachment for driver-owned local composition.
//!
//! The facade deliberately exposes no session, bridge, responder, executor, or
//! operation trait. Those remain private driver authority:
//!
//! ```compile_fail
//! use plakous::agent::LocalCompositionSession;
//! ```
//!
//! Exact DTO fields cannot be forged by a consumer:
//!
//! ```compile_fail
//! use plakous::agent::LocalCompositionProfile;
//! fn forge() -> LocalCompositionProfile {
//!     LocalCompositionProfile { descriptor: todo!(), profile_identity: todo!(), engine_contract_identity: todo!() }
//! }
//! ```

use std::{
    fmt,
    fs::File,
    future::Future,
    io::Read,
    os::unix::ffi::OsStringExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use plakous_agent::internal::{
    AgentBuilder, AgentError, AgentErrorKind, LocalCompositionAttachmentIdentity,
    LocalCompositionConstruction, LocalCompositionContentItem, LocalCompositionDescriptor,
    LocalCompositionExecuteInput, LocalCompositionFuture, LocalCompositionImageDetail,
    LocalCompositionIntegration, LocalCompositionLifecycle, LocalCompositionResultProjection,
    LocalCompositionRuntimeContract, LocalCompositionSessionBundle, LocalCompositionShutdownResult,
    LocalCompositionToolDefinition, LocalCompositionWaitInput,
};
use plakous_compose::codex_exec as native;
use plakous_compose::{
    CodexMinimalLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeDescriptorV1,
    ProductJavaScriptCompositionDescriptorV1, ProductJavaScriptCompositionV1,
};
use plakous_compose::{PythonPtcDescriptorV1, PythonPtcLimitsV1, PythonPtcV1};
use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestDomain, EffectiveProviderConfigurationId,
    IntegrationClass, SelectedProviderConfigurationId, ToolExecutionDisposition, ToolName,
};
use plakous_tools::{
    FrozenToolRegistry, PROCESS_TOOL_PROJECTION_REVISION,
    PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PRODUCT_JAVASCRIPT_RUN_NAME_V1,
    PRODUCT_JAVASCRIPT_WAIT_NAME_V1, ProviderModelProjectionIdentityV2, ToolExecutorId,
    ToolProjectionMap,
};
use plakous_tools::{
    PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_WAIT_NAME_V1,
};

const PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS: u64 = 10_000;
const PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS: u64 = 10_000;
const PYTHON_DEFAULT_YIELD_TIME_MS: u64 = 10_000;
/// Maximum UTF-8 source bytes in one cell.
const PYTHON_SOURCE_MAX_BYTES: usize = 256 * 1024;

mod artifact;
mod attach;
mod evidence;
mod integration;
mod javascript;
mod python;
#[cfg(feature = "composition-python")]
#[path = "selection/python.rs"]
mod python_capability;
#[cfg(not(feature = "composition-python"))]
#[path = "selection/python_unavailable.rs"]
mod python_capability;
#[cfg(all(test, unix))]
mod python_tests;
mod sdk;
mod session;
mod validate;

pub use artifact::*;
pub use attach::*;
pub use evidence::*;
use integration::{CellIntegration, JavaScriptPresentation, ProductJavaScript};
pub(crate) use javascript::*;
use python::neutral_python_descriptor;
use validate::validate_product_configuration;
use validate::validate_python_configuration;

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::new(AgentErrorKind::InvalidInput, message.into())
}

fn identity_error(error: plakous_model::DigestError) -> AgentError {
    AgentError::new(AgentErrorKind::Identity, error.to_string())
}

fn digest_environment(label: &[u8], facts: &[u8]) -> Result<Digest, AgentError> {
    Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[label, facts]).map_err(identity_error)
}

#[cfg(test)]
mod shutdown_tests;
