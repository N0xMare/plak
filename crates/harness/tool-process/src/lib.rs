//! Joined process sessions: the one exclusive-resource executor.
//!
//! The four canonical operations, the PTY backend, and the per-vocabulary
//! shell tool that drives them — `Bash`, `exec_command`, `run_terminal_command`
//! are one capability under three costumes, and the pack owns how each looks.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod canonical;
mod grok;
#[cfg(all(test, unix))]
mod jobs;
mod output;
mod pack;
mod pi;
mod projection;
mod session;
mod text;

pub use text::{
    PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION, PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
    PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT, PLAKOUS_TOOL_OUTPUT_OMITTED,
    PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION, output_omitted_marker,
};

pub use pack::ProcessToolPack;
pub use projection::{
    PROCESS_PROJECTION_FAMILY, ProcessProjectionResult, ProcessToolProjectionKind,
    ProjectedProcessTool,
};
pub use session::ProcessSessionTools;

use canonical::{
    CanonicalProcessTerminateRequest, await_process_change, classify_process_outcome,
    execute_process_await, process_backend, process_observation_labels, raised_stop_result,
    validate_canonical_poll_shape,
};
use output::{
    HeadTailPage, MAX_DRAIN_READS, append_output_file, drain_process_utf8_bounded,
    exited_session_refusal, lock_pending_process_utf8, lock_process_cursor, process_status_label,
    status_exit_code,
};
use pack::canonical_process_identities;
use projection::{
    CLAUDE_OAUTH_BASH_DESCRIPTION, CLAUDE_OAUTH_TASK_STOP_DESCRIPTION, CODEX_PROCESS_OUTPUT_TOKENS,
    canonical_process_input_schema,
};
use session::{BoundProcessSession, ObservationWait, ProcessExecutionResult, StartedProcessGuard};

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use plakous_model::{
    Digest, DigestDomain, SemanticJsonV1, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolInputContract, ToolName, ToolOutputContract, Utf8TextContractV1,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use plakous_tools::CodingPresentation;
use plakous_tools::FrozenToolRegistry;
use plakous_tools::PROCESS_OBSERVATION_BYTES;
use plakous_tools::PromptPin;
use plakous_tools::internal::{
    ProcessHintSource, ProcessOperationKind, ProcessTelemetry, process_session_active,
    process_session_joined, projection_exposure, projection_map,
};
use plakous_tools::object_schema;
use plakous_tools::{
    CanonicalProcessOperationV2, ExecutionOperationControl, JoinedProcessCommand,
    JoinedProcessInputDisposition, JoinedProcessOutputCursor, JoinedProcessSessionId,
    JoinedProcessSessions, JoinedProcessShellProfile, JoinedProcessStartRequest,
    JoinedProcessStatus, JoinedProcessTerminalMode, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, RegisteredToolId, RegisteredToolIdentity, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap,
    ToolProjectionRaisingV2, ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind,
};
use plakous_tools::{MenuBinding, MenuEntry, MenuOutput, MenuShape, ToolPack, tool_name};

use plakous_tools::{
    PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_CAPABILITY_REVISION,
    PROCESS_SESSION_EXECUTOR_ID, PROCESS_SESSION_POLL_NAME,
    PROCESS_SESSION_POLL_REGISTERED_TOOL_ID, PROCESS_SESSION_REGISTERED_TOOL_ID,
    PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
    PROCESS_SESSION_WRITE_NAME, PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
    PROCESS_TOOL_PROJECTION_REVISION,
};
use plakous_tools::{
    backend_runtime_error, deserialize_optional_semantic_u64, deserialize_semantic_u64,
    deserialize_semantic_usize, operation_control, projection_arguments,
};

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
