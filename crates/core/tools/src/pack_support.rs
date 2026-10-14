//! The helpers every coding pack writes with.
//!
//! Argument decoding, the workspace generation stamp, the cancellation bridge,
//! and the two error conversions are the same in all four packs, and none of
//! them is a tool. They live in the core so the packs above it share one
//! spelling of each, and so a fifth pack gets them for free.

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;

use crate::{
    ExecutionBackendError, ExecutionBackendErrorKind, ExecutionOperationControl, ToolContext,
    ToolInvocation, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// The `workspace_generation=` prefix a stamped workspace puts on a result.
#[must_use]
pub fn generation_stamp_prefix(context: &ToolContext) -> String {
    match context.workspace().generation_stamp() {
        Some(generation) => format!("workspace_generation={generation} "),
        None => String::new(),
    }
}

/// One invalid-arguments error for a projection-lowered call.
#[must_use]
pub fn projection_arguments(error: impl std::error::Error + 'static) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::InvalidArguments,
        "process projection arguments are invalid",
        &error,
    )
}

/// Decodes already schema-validated arguments into a pack's own type.
///
/// # Errors
///
/// Returns an invalid-arguments error when the validated JSON does not decode.
pub fn decode_arguments<T: for<'de> Deserialize<'de>>(
    invocation: &ToolInvocation,
) -> Result<T, ToolRuntimeError> {
    serde_json::from_value(invocation.arguments().value().as_ref().clone()).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidArguments,
            "validated standard-tool arguments could not be decoded",
            &error,
        )
    })
}

/// The cancellation bridge one invocation hands to a backend operation.
#[must_use]
pub fn operation_control(context: &ToolContext) -> ExecutionOperationControl {
    ExecutionOperationControl::new(context.cancellation().clone())
        .with_preempt(context.preempt().clone())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the helper is used directly as a Result::map_err conversion boundary"
)]
/// Maps an execution-backend failure onto its tool-runtime kind.
#[must_use]
pub fn backend_runtime_error(error: ExecutionBackendError) -> ToolRuntimeError {
    let kind = match error.kind() {
        ExecutionBackendErrorKind::InvalidRequest => ToolRuntimeErrorKind::InvalidArguments,
        ExecutionBackendErrorKind::WorkspacePolicy
        | ExecutionBackendErrorKind::NotFound
        | ExecutionBackendErrorKind::IsADirectory
        | ExecutionBackendErrorKind::NotARegularFile
        | ExecutionBackendErrorKind::OutsideWorkspace
        | ExecutionBackendErrorKind::SymlinkEscape => ToolRuntimeErrorKind::WorkspacePolicy,
        ExecutionBackendErrorKind::ResourceLimit => ToolRuntimeErrorKind::ResourceLimit,
        ExecutionBackendErrorKind::Cancelled => ToolRuntimeErrorKind::Cancelled,
        ExecutionBackendErrorKind::TimedOut => ToolRuntimeErrorKind::ExecutionTimeout,
        ExecutionBackendErrorKind::CleanupUnresolved => ToolRuntimeErrorKind::CleanupTimeout,
        ExecutionBackendErrorKind::RecoveryRequired => ToolRuntimeErrorKind::EffectSafety,
        _ => ToolRuntimeErrorKind::Execution,
    };
    // The backend knows whether it got as far as acting; the tool category it
    // maps onto does not always carry that. A refused write and a failed write
    // both arrive as `Execution`, and only the backend can tell them apart.
    ToolRuntimeError::new(kind, error.message().as_str())
        .with_proven_no_effect(error.proven_no_effect())
}

/// One execution error for content a tool could not construct.
#[must_use]
pub fn content_runtime_error(error: impl std::error::Error + 'static) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::Execution,
        "standard tool content could not be constructed",
        &error,
    )
}

/// Accepts an exact non-negative integer written as a number.
///
/// # Errors
///
/// Returns a deserialization error for a non-integer or out-of-range value.
pub fn deserialize_semantic_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    semantic_u64(&Value::deserialize(deserializer)?)
}

/// Optional [`deserialize_semantic_u64`].
///
/// # Errors
///
/// Returns a deserialization error for a non-integer or out-of-range value.
pub fn deserialize_optional_semantic_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(value) => semantic_u64(&value).map(Some),
    }
}

/// Reads an exact non-negative integer out of one JSON value.
///
/// # Errors
///
/// Returns a deserialization error for a non-integer or out-of-range value.
pub fn semantic_u64<E: de::Error>(value: &Value) -> Result<u64, E> {
    if let Some(value) = value.as_u64() {
        return Ok(value);
    }
    let value = value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0 && value.fract() == 0.0)
        .ok_or_else(|| de::Error::custom("expected an exact non-negative integer"))?;
    value
        .to_string()
        .parse::<u64>()
        .map_err(|_| de::Error::custom("integer exceeds u64"))
}

/// [`deserialize_semantic_u64`] narrowed to `usize`.
///
/// # Errors
///
/// Returns a deserialization error for a non-integer or out-of-range value.
pub fn deserialize_semantic_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    usize::try_from(deserialize_semantic_u64(deserializer)?)
        .map_err(|_| de::Error::custom("integer exceeds usize"))
}

/// Neutral read result shared by file execution and presentation adapters.
/// `content` preserves the selected UTF-8 bytes, including line endings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFileReadResult {
    /// Exact selected file content, without presentation metadata.
    pub content: String,
    /// One-indexed first selected line.
    pub offset: u64,
    /// Number of selected lines.
    pub lines: u64,
    /// Whether the selected content reaches the file's end.
    pub eof: bool,
    /// Next line offset, absent at EOF.
    pub next_offset: Option<u64>,
    /// Invalid UTF-8 in the selected bytes was replaced with U+FFFD.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lossy: bool,
    /// Selected lines clipped at the line-length bound.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub clipped_lines: u64,
    /// Optional observational workspace generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_generation: Option<u64>,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if passes a reference"
)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}
