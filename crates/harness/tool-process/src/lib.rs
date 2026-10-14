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

mod grok;
#[cfg(all(test, unix))]
mod jobs;
mod pi;
mod text;

pub use text::{
    PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION, PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
    PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT, PLAKOUS_TOOL_OUTPUT_OMITTED,
    PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION, output_omitted_marker,
};

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
    ToolOutcome, ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMapV2,
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

/// The model family every foreground-terminal presentation candidate carries.
pub const PROCESS_PROJECTION_FAMILY: &str = "responses-family";
// Host ceiling for one model-visible process *page* (not the PTY capture
// buffer). Matches Grok Build's 40_000-byte tool default and Codex CLI's
// 10_000-token exec page at 4 bytes/token. Claude Bash lowering uses
// CLAUDE_BASH_OBSERVATION_BYTES. The turn-level observation budget
// still caps how many pages enter the next model round.
/// Claude Code Bash page (`MAX_OUTPUT_LEN` = 30000 chars).
/// Claude OAuth `Bash` description: one-shot `/bin/sh` (not Claude Code's persistent shell).
/// `timeout` blocks up to the hint; the pack has no output tool, so a leaf
/// waits by giving a long command a long enough timeout.
pub(crate) const CLAUDE_OAUTH_BASH_DESCRIPTION: &str = "Executes a given command through a one-shot `/bin/sh -c` in the session cwd (`.`) and returns its output; cwd and environment do not persist. Use Read for file contents and Edit and Write, when presented, for file changes; search with grep, rg or find here. `timeout` is how long to block for the result, in ms, not a kill timer; it is clamped to 600000 ms and defaults to the construction-bound foreground budget (default 30000 ms). `run_in_background: true` returns after a 250 ms first observation. When a command moves to the background with an output file, its result and its completion notice carry a `full output: <path>` line; read that file with Read. Output over 30000 bytes keeps its beginning and end with the omitted byte count. A still-running command continues in the background with its session id after the foreground budget; completion is reported automatically. Background jobs survive the turn and stop only on explicit stop, session release, attempt end or the bound hard runtime cap.";
/// Claude OAuth `TaskStop` description: terminate a background `Bash`.
pub(crate) const CLAUDE_OAUTH_TASK_STOP_DESCRIPTION: &str = "Stop a running background task by ID. task_id is the process session_id Bash returned (N or pN). Send SIGTERM to the process group, then SIGKILL after a 2 second grace. Succeeds if the process stopped or had already exited. Processes only; agent tools, when presented, handle agents.";

// Claude Code 2.1.289's TaskStop takes a strict object with a string task id.
// Drop the deprecated shell_id alias and require the sole current identifier
// on our process-only view.
fn claude_task_stop_schema() -> Value {
    object_schema(
        json!({"task_id": {
            "type":"string",
            "description":"The ID of the background process task to stop (the session_id Bash returned). Agents are not accepted."
        }}),
        &["task_id"],
    )
}
/// Codex `exec_command` default/max `max_output_tokens` (code-mode native).
pub(crate) const CODEX_PROCESS_OUTPUT_TOKENS: u64 = 10_000;

fn canonical_process_input_schema(
    operation: CanonicalProcessOperationV2,
) -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    let document = match operation {
        CanonicalProcessOperationV2::Start => object_schema(
            json!({
                "command": {"type":"string"},
                "workdir": {"type":"string"},
                "login": {"type":"boolean"},
                "tty": {"type":"boolean"},
                "keep_stdin_open": {"type":"boolean"},
                "model_description": {"type":"string"},
                "hint_source": {"type":"string","enum":["default","model_hint","background"]},
                "yield_time_ms": {"type":"integer","minimum":0,"maximum":600_000},
                "max_output_bytes": {"type":"integer","minimum":1,"maximum":40000}
            }),
            &[
                "command",
                "workdir",
                "login",
                "tty",
                "keep_stdin_open",
                "yield_time_ms",
                "max_output_bytes",
            ],
        ),
        CanonicalProcessOperationV2::Write => object_schema(
            json!({
                "session_id": {"type":"integer","minimum":1},
                "input": {"type":"string"},
                "disposition": {"type":"string","enum":["keep_open","close"]},
                "yield_time_ms": {"type":"integer","minimum":0,"maximum":300_000},
                "max_output_bytes": {"type":"integer","minimum":1,"maximum":40000}
            }),
            &["session_id", "input", "disposition"],
        ),
        CanonicalProcessOperationV2::Poll => object_schema(
            json!({
                "session_id": {"type":"integer","minimum":1},
                "session_ids": {"type":"array","items":{"type":"integer","minimum":1},"minItems":1,"maxItems":128},
                "wake": {"type":"string","enum":["any"]},
                "cursor": {"type":"integer","minimum":0},
                "read_so_far": {"type":"boolean"},
                "wait_for_exit": {"type":"boolean"},
                "yield_time_ms": {"type":"integer","minimum":0,"maximum":3_600_000},
                "max_output_bytes": {"type":"integer","minimum":1,"maximum":40000},
                "max_output_records": {"type":"integer","minimum":1,"maximum":256}
            }),
            &["yield_time_ms", "max_output_bytes", "max_output_records"],
        ),
        CanonicalProcessOperationV2::Terminate => object_schema(
            json!({"session_id": {"type":"integer","minimum":1}}),
            &["session_id"],
        ),
    };
    PortableToolSchemaV2::new(document).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "canonical process input schema is invalid",
            &error,
        )
    })
}

/// Closed exact-model projection over the canonical joined-process capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProcessToolProjectionKind {
    /// Pi process tools: `bash` start, first-terminal `wait`, `stop`.
    PiBash,
    /// Codex unified exec sessions (`exec_command` plus `write_stdin`).
    CodexUnifiedExec,
    /// Grok process start, multi-process output/await and stop.
    GrokTerminal,
    /// Claude OAuth `Bash` joined session.
    ClaudeOAuthBash,
}

impl ProcessToolProjectionKind {
    /// Returns exact model-visible tools whose schemas and identity are bound
    /// to `PortableToolSchemaV2`.
    ///
    /// # Errors
    ///
    /// Returns an invariant error if an exact candidate projection is not valid V2.
    pub fn tools(
        self,
        job_config: plakous_tools::JobConfig,
    ) -> Result<Vec<ProjectedProcessTool>, ToolRuntimeError> {
        let shell = |description| text::bound_description(description, job_config, self, true);
        let bound = |description| text::bound_description(description, job_config, self, false);
        match self {
            // The action sentence (read / wait / stop) is on `bash` only.
            // `wait` and `stop` carry the bound budget and the lifetime contract.
            Self::PiBash => Ok(vec![
                ProjectedProcessTool::new(
                    "bash",
                    &shell(text::PI_BASH_DESCRIPTION),
                    pi::bash_schema(),
                )?,
                ProjectedProcessTool::new(
                    "wait",
                    &bound(text::PI_WAIT_DESCRIPTION),
                    pi::wait_schema(),
                )?,
                ProjectedProcessTool::new(
                    "stop",
                    &bound(text::PI_STOP_DESCRIPTION),
                    pi::stop_schema(),
                )?,
            ]),
            Self::CodexUnifiedExec => Ok(vec![
                ProjectedProcessTool::new(
                    "exec_command",
                    &shell(PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION),
                    codex_exec_command_schema(job_config),
                )?,
                ProjectedProcessTool::new(
                    "write_stdin",
                    &bound(PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION),
                    write_stdin_schema(),
                )?,
            ]),
            Self::GrokTerminal => Ok(vec![
                ProjectedProcessTool::new(
                    "run_terminal_command",
                    &shell(PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION),
                    grok::start_schema(),
                )?,
                ProjectedProcessTool::new(
                    "get_command_or_subagent_output",
                    &bound(text::GROK_GET_COMMAND_OUTPUT_DESCRIPTION),
                    grok::output_schema(),
                )?,
                ProjectedProcessTool::new(
                    "kill_command_or_subagent",
                    &bound(text::GROK_KILL_COMMAND_DESCRIPTION),
                    grok::stop_schema(),
                )?,
            ]),
            Self::ClaudeOAuthBash => Ok(vec![
                ProjectedProcessTool::new(
                    "Bash",
                    &shell(CLAUDE_OAUTH_BASH_DESCRIPTION),
                    object_schema(
                        json!({
                            "command": {"type":"string"},
                            "description": {"type":"string"},
                            "timeout": {"type":"integer","minimum":0},
                            "run_in_background": {"type":"boolean"}
                        }),
                        &["command"],
                    ),
                )?,
                ProjectedProcessTool::new(
                    "TaskStop",
                    &bound(CLAUDE_OAUTH_TASK_STOP_DESCRIPTION),
                    claude_task_stop_schema(),
                )?,
            ]),
        }
    }

    /// Compiles the exact model-facing process tools into V2 registry mappings.
    ///
    /// Every mapping binds the same canonical capability and executor while
    /// retaining its own exposed name, schema, defaults, lowering, raising,
    /// target, and effect identity.
    ///
    /// # Errors
    ///
    /// Rejects an absent/drifted canonical registration or invalid projection catalog.
    pub fn projection_map(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError> {
        let canonical = canonical_process_identities(&registry)?;
        let entries = self
            .tools(job_config)?
            .into_iter()
            .map(|tool| projection_entry(self, canonical.clone(), &target, &tool))
            .collect::<Result<Vec<_>, _>>()?;
        projection_map(registry, target, entries)
    }
}

fn write_stdin_schema() -> Value {
    object_schema(
        json!({
            "session_id": {"type":"integer","minimum":1},
            "chars": {"type":"string"},
            "yield_time_ms": {"type":"integer","minimum":0},
            "max_output_tokens": {"type":"integer","minimum":1,"maximum":10000}
        }),
        &["session_id", "chars"],
    )
}

fn codex_exec_command_schema(job_config: plakous_tools::JobConfig) -> Value {
    object_schema(
        json!({
            "cmd": {
                "type": "string",
                "description": "Shell command to execute."
            },
            "justification": {
                "type": "string",
                "description": "Accepted for compatibility with require_escalated; ignored."
            },
            "login": {
                "type": "boolean",
                "description": "True runs the shell with -l semantics; false disables them. Defaults to true."
            },
            "max_output_tokens": {
                "type": "number",
                "description": "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy."
            },
            "prefix_rule": {
                "type": "array",
                "description": "Accepted for compatibility with require_escalated; ignored.",
                "items": {"type": "string"}
            },
            "sandbox_permissions": {
                "type": "string",
                "description": "Accepted for compatibility; every command runs under the workspace policy and cannot escalate.",
                "enum": ["use_default", "require_escalated"]
            },
            "shell": {
                "type": "string",
                "enum": ["/bin/sh"],
                "description": "Shell binary to launch. The shell is /bin/sh."
            },
            "tty": {
                "type": "boolean",
                "description": "True allocates a PTY for the command; false or omitted uses plain pipes."
            },
            "workdir": {
                "type": "string",
                "description": "Working directory for the command. Workspace-relative only (never absolute); defaults to the turn cwd."
            },
            "yield_time_ms": {
                "type": "number",
                "description": format!("Foreground observation hint: defaults to {} ms; clamped to 600000 ms; zero observes for 250 ms first.", job_config.foreground_seconds() * 1000)
            },
            "session_id": {
                "type": "integer",
                "minimum": 1,
                "description": "Identifier of the running unified exec session; pass without cmd to poll output."
            }
        }),
        &[],
    )
}

fn projection_entry(
    kind: ProcessToolProjectionKind,
    canonical: Vec<(CanonicalProcessOperationV2, RegisteredToolIdentity)>,
    target: &ProviderModelProjectionIdentityV2,
    tool: &ProjectedProcessTool,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    let (lowering, defaults, operations) = match (kind, tool.name().as_str()) {
        (ProcessToolProjectionKind::PiBash, "bash") => (
            ToolProjectionLoweringV2::PiBash,
            json!({}),
            &[CanonicalProcessOperationV2::Start][..],
        ),
        (ProcessToolProjectionKind::PiBash, "wait") => (
            ToolProjectionLoweringV2::PiWait,
            json!({}),
            &[CanonicalProcessOperationV2::Poll][..],
        ),
        (ProcessToolProjectionKind::PiBash, "stop") => (
            ToolProjectionLoweringV2::PiStop,
            json!({}),
            &[CanonicalProcessOperationV2::Terminate][..],
        ),
        (ProcessToolProjectionKind::CodexUnifiedExec, "exec_command") => (
            ToolProjectionLoweringV2::CodexExecCommand,
            json!({
                "login": true,
                "tty": false,
                "yield_time_ms": 10000,
                "max_output_tokens": CODEX_PROCESS_OUTPUT_TOKENS,
            }),
            &[
                CanonicalProcessOperationV2::Start,
                CanonicalProcessOperationV2::Poll,
            ][..],
        ),
        (ProcessToolProjectionKind::CodexUnifiedExec, "write_stdin") => (
            ToolProjectionLoweringV2::CodexWriteStdin,
            json!({}),
            &[CanonicalProcessOperationV2::Write][..],
        ),
        (ProcessToolProjectionKind::GrokTerminal, "run_terminal_command") => (
            ToolProjectionLoweringV2::GrokRunTerminalCommand,
            json!({}),
            &[CanonicalProcessOperationV2::Start][..],
        ),
        (ProcessToolProjectionKind::GrokTerminal, "get_command_or_subagent_output") => (
            ToolProjectionLoweringV2::GrokGetCommandOutput,
            json!({}),
            &[CanonicalProcessOperationV2::Poll][..],
        ),
        (ProcessToolProjectionKind::GrokTerminal, "kill_command_or_subagent") => (
            ToolProjectionLoweringV2::GrokKillCommand,
            json!({}),
            &[CanonicalProcessOperationV2::Terminate][..],
        ),
        (ProcessToolProjectionKind::ClaudeOAuthBash, "Bash") => (
            ToolProjectionLoweringV2::ClaudeBash,
            json!({}),
            &[CanonicalProcessOperationV2::Start][..],
        ),
        (ProcessToolProjectionKind::ClaudeOAuthBash, "TaskStop") => (
            ToolProjectionLoweringV2::ClaudeTaskStop,
            json!({}),
            &[CanonicalProcessOperationV2::Terminate][..],
        ),
        _ => {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection catalog contains an unknown candidate mapping",
            ));
        }
    };
    projection_exposure(
        canonical
            .into_iter()
            .filter(|(operation, _)| operations.contains(operation)),
        tool.name().as_str(),
        tool.description(),
        tool.schema().clone(),
        SemanticJsonV1::from_value(defaults).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection defaults are invalid",
                &error,
            )
        })?,
        lowering,
        tool.output_schema().clone(),
        if matches!(
            lowering,
            ToolProjectionLoweringV2::GrokGetCommandOutput | ToolProjectionLoweringV2::PiWait
        ) {
            ToolProjectionRaisingV2::Identity
        } else {
            ToolProjectionRaisingV2::ProcessResult
        },
        target.clone(),
    )
}

/// One immutable, exact model-facing process-tool definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedProcessTool {
    name: ToolName,
    description: Arc<str>,
    schema: PortableToolSchemaV2,
    output_schema: PortableToolSchemaV2,
    identity: Digest,
}

impl ProjectedProcessTool {
    fn new(name: &str, description: &str, schema: Value) -> Result<Self, ToolRuntimeError> {
        let name = ToolName::new(name.to_owned()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection name is invalid",
                &error,
            )
        })?;
        let schema = PortableToolSchemaV2::new(schema).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection schema is invalid",
                &error,
            )
        })?;
        let mut output = object_schema(
            json!({
                "exit_code": {"type":"integer"},
                "session_id": {"type":"integer","minimum":1},
                "status": {"type":"string","enum":["timed_out","terminated","cancelled","output_limit","failed","outcome_unknown","cleanup_unresolved"]},
                "output": {"type":"string"}
            }),
            &["output"],
        );
        if name.as_str() == "run_terminal_command" {
            output["properties"]
                .as_object_mut()
                .ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "process output schema is not an object",
                    )
                })?
                .remove("session_id");
            output["properties"]["task_id"] = json!({"type":"string"});
        }
        // Both multi-id awaits answer with per-task rows.
        if matches!(name.as_str(), "get_command_or_subagent_output" | "wait") {
            output = object_schema(
                json!({
                    "tasks": {"type":"array","items": {"type":"object","properties": {
                        "task_id":{"type":"string"},
                        "status":{"type":"string","enum":["running","exited","terminated","cancelled","timed_out","output_limit","failed","outcome_unknown","cleanup_unresolved","already_reported","not_found"]},
                        "exit_code":{"type":"integer"},
                        "output":{"type":"string"}
                    }, "required":["task_id","status","output"],"additionalProperties":false}}
                }),
                &["tasks"],
            );
        }
        let output_schema = PortableToolSchemaV2::new(output).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection output schema is invalid",
                &error,
            )
        })?;
        let identity = Digest::sha256(
            DigestDomain::ToolDefinition,
            PROCESS_TOOL_PROJECTION_REVISION,
            &[
                b"plakous-tools/process-tool-projection-v1",
                name.as_str().as_bytes(),
                description.as_bytes(),
                schema.digest().bytes(),
                output_schema.digest().bytes(),
            ],
        )
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection identity could not be constructed",
                &error,
            )
        })?;
        Ok(Self {
            name,
            description: Arc::from(description),
            schema,
            output_schema,
            identity,
        })
    }

    /// Returns the exact model-visible name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }
    /// Returns the exact model-visible description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
    /// Returns the V2 input contract.
    #[must_use]
    pub const fn schema(&self) -> &PortableToolSchemaV2 {
        &self.schema
    }
    /// Returns the deterministic shared result shape.
    #[must_use]
    pub const fn output_schema(&self) -> &PortableToolSchemaV2 {
        &self.output_schema
    }
    /// Returns the revision-, description-, and V2-schema-bound identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Deterministic result shape shared by both exact process projections.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProcessProjectionResult {
    /// Exit code when the child joined during this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Opaque allocation-local handle while the child remains running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// Ordered stdout/stderr text observed during this call.
    pub output: String,
}

struct ProcessExecutionResult {
    projection: ProcessProjectionResult,
    /// Output bytes the session's window evicted before this page began.
    skipped: u64,
    status: &'static str,
    cursor: u64,
    sessions: JoinedProcessSessions,
    session_id: JoinedProcessSessionId,
    joined: bool,
    first_promotion: bool,
    /// The page omitted output, so the full output file is worth naming.
    truncated: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ObservationWait {
    PromotionHint,
    Hint,
    Output,
}

impl ObservationWait {
    fn preempted(self, preempt: plakous_tools::ToolPreempt) -> bool {
        preempt == plakous_tools::ToolPreempt::Steer
            || (self == Self::PromotionHint && preempt == plakous_tools::ToolPreempt::Background)
    }
}

impl ProcessExecutionResult {
    #[cfg(test)]
    fn into_outcome(self) -> Result<ToolOutcome, ToolRuntimeError> {
        self.into_outcome_with_promotion(None)
    }

    fn into_outcome_with_promotion(
        mut self,
        promotion: Option<(u64, plakous_tools::ToolPreempt)>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut output = std::mem::take(&mut self.projection.output);
        if self.first_promotion || self.truncated {
            append_output_file(&mut output, &self.sessions, self.session_id);
        }
        let mut canonical = json!({
            "status": self.status,
            "exit_code": self.projection.exit_code,
            "session_id": self.projection.session_id,
            "cursor": self.cursor,
            "output": output,
        });
        if self.first_promotion
            && let Some((hint, preempted)) = promotion
        {
            canonical["foreground_hint_ms"] = json!(hint);
            canonical["preempted"] = json!(preempted == plakous_tools::ToolPreempt::Steer);
            canonical["backgrounded_by_user"] =
                json!(preempted == plakous_tools::ToolPreempt::Background);
        }
        let outcome =
            ToolOutcome::success_text(serde_json::to_string(&canonical).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "canonical process result could not be encoded",
                    &error,
                )
            })?)?;
        let outcome = if self.first_promotion {
            if let Some((hint_ms, preempt)) = promotion {
                plakous_tools::internal::with_process_telemetry(
                    outcome,
                    ProcessTelemetry::Promoted {
                        operation: ProcessOperationKind::Start,
                        hint_source: ProcessHintSource::Default,
                        hint_ms,
                        preempt,
                    },
                )
            } else {
                outcome
            }
        } else if self.sessions.is_promoted(self.session_id) {
            plakous_tools::internal::with_process_telemetry(
                outcome,
                ProcessTelemetry::Waited {
                    operation: ProcessOperationKind::Poll,
                    yielded: false,
                },
            )
        } else {
            outcome
        };
        Ok(if self.joined {
            process_session_joined(outcome, &self.sessions, self.session_id)
        } else if self.sessions.is_promoted(self.session_id) && !self.first_promotion {
            outcome
        } else {
            process_session_active(outcome, &self.sessions, self.session_id)
        })
    }
}

/// One canonical service implementing both model-specific process views.
///
/// The numeric handle is only a bounded projection of an allocation-local
/// opaque `JoinedProcessSessionId`; it is never an OS PID or durable authority.
pub struct ProcessSessionTools {
    shell: JoinedProcessShellProfile,
    login_shell: JoinedProcessShellProfile,
}

#[derive(Clone)]
struct BoundProcessSession {
    id: JoinedProcessSessionId,
    sessions: JoinedProcessSessions,
    cursor: Arc<Mutex<JoinedProcessOutputCursor>>,
    pending_utf8: Arc<Mutex<Vec<u8>>>,
}

fn active_process_result(
    session: BoundProcessSession,
    handle: u64,
    output: String,
    cursor: JoinedProcessOutputCursor,
) -> ProcessExecutionResult {
    ProcessExecutionResult {
        skipped: 0,
        projection: ProcessProjectionResult {
            exit_code: None,
            session_id: Some(handle),
            output,
        },
        sessions: session.sessions,
        session_id: session.id,
        joined: false,
        first_promotion: false,
        truncated: false,
        status: "running",
        cursor: cursor.get(),
    }
}

struct StartedProcessGuard {
    sessions: JoinedProcessSessions,
    session_id: JoinedProcessSessionId,
    armed: bool,
}

impl StartedProcessGuard {
    const fn new(sessions: JoinedProcessSessions, session_id: JoinedProcessSessionId) -> Self {
        Self {
            sessions,
            session_id,
            armed: true,
        }
    }

    async fn disarm_or_terminate(&mut self, keep: bool) -> Result<(), ToolRuntimeError> {
        self.armed = false;
        if keep {
            return Ok(());
        }
        let sessions = self.sessions.clone();
        let id = self.session_id;
        process_backend(move || sessions.terminate(id)).await?;
        Ok(())
    }
}

impl Drop for StartedProcessGuard {
    fn drop(&mut self) {
        if self.armed {
            let sessions = self.sessions.clone();
            let id = self.session_id;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                // Cancellation can drop the future before its explicit cleanup.
                runtime.spawn_blocking(move || sessions.terminate(id));
            } else {
                let _ = sessions.terminate(id);
            }
        }
    }
}

impl ProcessSessionTools {
    /// Constructs the sole fixed shell profile enrolled by the canonical
    /// process-session registration: `/bin/sh -c`, with `/bin/sh -l -c` for
    /// explicit login-shell requests.
    ///
    /// # Errors
    ///
    /// Returns an invariant error only if the fixed absolute profiles cannot
    /// be represented by the host-process contract.
    pub fn posix_sh() -> Result<Self, ToolRuntimeError> {
        let shell = JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")])
            .map_err(backend_runtime_error)?;
        let login_shell =
            JoinedProcessShellProfile::new("/bin/sh", [String::from("-l"), String::from("-c")])
                .map_err(backend_runtime_error)?;
        Ok(Self::new(shell, login_shell))
    }

    /// Binds the canonical service to exact non-login and login shell profiles.
    #[must_use]
    pub fn new(shell: JoinedProcessShellProfile, login_shell: JoinedProcessShellProfile) -> Self {
        Self { shell, login_shell }
    }

    /// Snapshots or awaits the caller's own jobs by number, exactly as the
    /// multi-id output tool does: one shared deadline (`timeout_ms` clamped
    /// to one hour, zero for a snapshot), per-task rows with `not_found` and
    /// `already_reported` tombstones, a steer returning early, and terminal
    /// rows consuming their completion notices at the caller's commit.
    ///
    /// For a capability that covers the output tool's slot on behalf of
    /// another layer; the text is the model-visible `{ "tasks": [...] }`.
    ///
    /// # Errors
    ///
    /// Returns the process executor's own error for a call whose outcome is
    /// not known; a refusal before any effect is a failure outcome.
    pub async fn await_jobs(
        context: &ToolContext,
        numbers: &[u64],
        timeout_ms: u64,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut value = json!({
            "session_ids": numbers,
            "yield_time_ms": timeout_ms.min(3_600_000),
            "max_output_bytes": PROCESS_OBSERVATION_BYTES,
            "max_output_records": 256,
        });
        let (operation, hint_source) =
            process_observation_labels(CanonicalProcessOperationV2::Poll, &mut value);
        context.check_cancelled()?;
        execute_process_await(
            context.environment().clone(),
            value,
            operation_control(context),
            operation,
            hint_source,
        )
        .await
    }

    /// Awaits the caller's own jobs exactly as pi's `wait` does on a node
    /// without agent tools: the first terminal among `numbers`, or among every
    /// own job still listed when `numbers` is empty (`wake: "any"`), under one
    /// deadline (`timeout_ms` clamped to one hour, zero for a snapshot). The
    /// text is the model-visible `{ "tasks": [...] }`, byte-identical to the
    /// process-only presentation's.
    ///
    /// # Errors
    ///
    /// Returns the process executor's own error for a call whose outcome is
    /// not known; a refusal before any effect is a failure outcome.
    pub async fn await_first_job(
        context: &ToolContext,
        numbers: &[u64],
        timeout_ms: u64,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut value = json!({
            "wake": "any",
            "yield_time_ms": timeout_ms.min(3_600_000),
            "max_output_bytes": PROCESS_OBSERVATION_BYTES,
            "max_output_records": 256,
        });
        if !numbers.is_empty() {
            value["session_ids"] = json!(numbers);
        }
        let (operation, hint_source) =
            process_observation_labels(CanonicalProcessOperationV2::Poll, &mut value);
        context.check_cancelled()?;
        execute_process_await(
            context.environment().clone(),
            value,
            operation_control(context),
            operation,
            hint_source,
        )
        .await
    }

    /// Stops one of the caller's own jobs by number, exactly as the stop
    /// tool does, and returns its model-visible stop result:
    /// `{ exit_code?, status?, output }`, `status` only when the process did
    /// not end by its own exit.
    ///
    /// # Errors
    ///
    /// Returns the process executor's own error for a stop whose outcome is
    /// not known; a refusal before any effect is a failure outcome.
    pub async fn stop_job(
        context: &ToolContext,
        number: u64,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut value = json!({"session_id": number});
        let (operation, hint_source) =
            process_observation_labels(CanonicalProcessOperationV2::Terminate, &mut value);
        context.check_cancelled()?;
        let environment = context.environment().clone();
        let request = CanonicalProcessTerminateRequest { session_id: number };
        let result = match Self::terminate_canonical(&environment, &request).await {
            Ok(result) => result,
            Err(error) if error.proven_no_effect() => {
                return ToolOutcome::failure_text(error.message().as_str());
            }
            Err(error) => return Err(error),
        };
        let sessions = result.sessions.clone();
        let session_id = result.session_id;
        let canonical = classify_process_outcome(
            result.into_outcome_with_promotion(None)?,
            operation,
            hint_source,
            false,
        );
        let stopped = ToolOutcome::success_text(raised_stop_result(&canonical.joined_text())?)?;
        let stopped = match plakous_tools::internal::process_telemetry(&canonical) {
            Some(telemetry) => plakous_tools::internal::with_process_telemetry(stopped, telemetry),
            None => stopped,
        };
        Ok(process_session_joined(stopped, &sessions, session_id))
    }

    /// Creates the sole canonical capability registration backed by this shared service.
    ///
    /// # Errors
    ///
    /// Returns an invalid-definition error if the locked registration cannot be constructed.
    #[allow(
        clippy::too_many_lines,
        reason = "the four exact registrations and one shared binding are reviewed together"
    )]
    pub fn registrations(self: Arc<Self>) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        if self.shell.executable() != Path::new("/bin/sh")
            || self.shell.arguments() != ["-c"]
            || self.login_shell.executable() != Path::new("/bin/sh")
            || self.login_shell.arguments() != ["-l", "-c"]
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "canonical process-session executor requires the fixed /bin/sh profiles",
            ));
        }
        let shell = self
            .shell
            .identity()
            .binding_identity()
            .digest()
            .identity_bytes();
        let login_shell = self
            .login_shell
            .identity()
            .binding_identity()
            .digest()
            .identity_bytes();
        let executor_binding_identity = ToolExecutorBindingIdentity::new(
            1,
            &[
                b"process-session-jobs",
                b"non-login-shell",
                &shell,
                b"login-shell",
                &login_shell,
            ],
        )?;
        let executor: Arc<dyn ToolExecutor> = self;
        [
            (
                CanonicalProcessOperationV2::Start,
                PROCESS_SESSION_CAPABILITY_NAME,
                PROCESS_SESSION_REGISTERED_TOOL_ID,
                "Start one canonical joined process session.",
                ToolEffect::Mutating,
            ),
            (
                CanonicalProcessOperationV2::Write,
                PROCESS_SESSION_WRITE_NAME,
                PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
                "Write bounded input to one canonical joined process session.",
                ToolEffect::Mutating,
            ),
            (
                CanonicalProcessOperationV2::Poll,
                PROCESS_SESSION_POLL_NAME,
                PROCESS_SESSION_POLL_REGISTERED_TOOL_ID,
                "Read bounded output from one exact process-session cursor.",
                ToolEffect::ReadOnly,
            ),
            (
                CanonicalProcessOperationV2::Terminate,
                PROCESS_SESSION_TERMINATE_NAME,
                PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
                "Terminate and join one canonical process session.",
                ToolEffect::Mutating,
            ),
        ]
        .into_iter()
        .map(|(operation, name, tool_id, description, effect)| {
            let definition = ToolDefinition::new(
                ToolName::new(name.to_owned()).map_err(|error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "canonical process operation name is invalid",
                        &error,
                    )
                })?,
                PROCESS_SESSION_CAPABILITY_REVISION,
                description,
                ToolInputContract::Structured(canonical_process_input_schema(operation)?),
                ToolOutputContract::Utf8(Utf8TextContractV1::new(1_048_576).map_err(|error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "canonical process output contract is invalid",
                        &error,
                    )
                })?),
                ToolConcurrency::Serial,
                effect,
            )
            .map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "canonical process operation definition is invalid",
                    &error,
                )
            })?;
            Ok(ToolRegistration::new(
                RegisteredToolId::new(tool_id)?,
                ToolExecutorId::new(PROCESS_SESSION_EXECUTOR_ID)?,
                executor_binding_identity,
                definition,
                Arc::clone(&executor),
            )
            .nested_eligible())
        })
        .collect()
    }

    #[cfg(test)]
    async fn exec_command_with_update(
        &self,
        environment: &plakous_tools::ExecutionEnvironment,
        raw: &str,
        control: ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let tool = projection_tool(ProcessToolProjectionKind::CodexUnifiedExec, "exec_command")?;
        tool.schema
            .parse_instance(raw)
            .map_err(projection_arguments)?;
        let args: CodexExecArgs = serde_json::from_str(raw).map_err(projection_arguments)?;
        let profile = self.codex_shell(args.shell.as_deref(), args.login.unwrap_or(true))?;
        let terminal_mode = if args.tty.unwrap_or(false) {
            JoinedProcessTerminalMode::UnixPty
        } else {
            JoinedProcessTerminalMode::Pipes
        };
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(profile, args.cmd).map_err(backend_runtime_error)?,
            args.workdir.unwrap_or_else(|| PathBuf::from(".")),
            Vec::new(),
            args.tty.unwrap_or(false),
            terminal_mode,
        )
        .map_err(backend_runtime_error)?;
        let sessions = environment
            .joined_process_sessions()
            .map_err(backend_runtime_error)?;
        let observation_control = control.clone();
        let start_sessions = sessions.clone();
        let max_bytes = args
            .max_output_tokens
            .and_then(non_negative_f64_to_u64)
            .and_then(|tokens| tokens.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(PROCESS_OBSERVATION_BYTES)
            .clamp(1, PROCESS_OBSERVATION_BYTES);
        let id = process_backend(move || {
            plakous_tools::internal::start_process_with_output_limit(
                &start_sessions,
                request,
                control,
                max_bytes,
            )
        })
        .await?;
        let handle = u64::from(JoinedProcessSessions::job_number(id));
        let mut guard = StartedProcessGuard::new(sessions.clone(), id);
        let result = async {
            let yield_time_ms = args
                .yield_time_ms
                .and_then(non_negative_f64_to_u64)
                .unwrap_or(10_000);
            let max_output_tokens = args.max_output_tokens.and_then(non_negative_f64_to_u64);
            let mut result = Self::observe_with_update(
                environment,
                handle,
                yield_time_ms,
                max_output_tokens,
                JoinedProcessOutputCursor::new(0),
                true,
                &observation_control,
            )
            .await;
            if result
                .as_ref()
                .is_ok_and(|result| result.projection.session_id.is_some())
            {
                plakous_tools::internal::promote(&sessions, id).map_err(backend_runtime_error)?;
                if let Ok(result) = &mut result {
                    result.first_promotion = true;
                }
            }
            result
        }
        .await;
        guard.disarm_or_terminate(result.is_ok()).await?;
        result
    }

    #[cfg(test)]
    async fn write_stdin_with_update(
        environment: &plakous_tools::ExecutionEnvironment,
        raw: &str,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let tool = projection_tool(ProcessToolProjectionKind::CodexUnifiedExec, "write_stdin")?;
        tool.schema
            .parse_instance(raw)
            .map_err(projection_arguments)?;
        let args: CodexWriteArgs = serde_json::from_str(raw).map_err(projection_arguments)?;
        let session = Self::session(environment, args.session_id)?;
        if let Some(chars) = args.chars.as_deref().filter(|chars| !chars.is_empty()) {
            let backend = session.sessions.clone();
            let id = session.id;
            let chars = chars.to_owned();
            process_backend(move || {
                backend.write_stdin(
                    id,
                    chars.as_bytes(),
                    JoinedProcessInputDisposition::KeepOpen,
                )
            })
            .await?;
        }
        let cursor = *lock_process_cursor(&session.cursor);
        Self::observe_with_update(
            environment,
            args.session_id,
            args.yield_time_ms.unwrap_or(5_000),
            args.max_output_tokens,
            cursor,
            !args.chars.as_deref().unwrap_or("").is_empty(),
            control,
        )
        .await
    }

    async fn start_canonical(
        &self,
        environment: &plakous_tools::ExecutionEnvironment,
        request: CanonicalProcessStartRequest,
        control: ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        request.validate()?;
        let yield_time_ms = request.yield_time_ms.clamp(250, 600_000);
        let max_output_bytes = request.max_output_bytes;
        let profile = if request.login {
            &self.login_shell
        } else {
            &self.shell
        };
        let sessions = environment
            .joined_process_sessions()
            .map_err(backend_runtime_error)?;
        let observation_control = control.clone();
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(profile.clone(), request.command)
                .map_err(backend_runtime_error)?,
            request.workdir,
            Vec::new(),
            request.keep_stdin_open,
            if request.tty {
                JoinedProcessTerminalMode::UnixPty
            } else {
                JoinedProcessTerminalMode::Pipes
            },
        )
        .map_err(backend_runtime_error)?;
        let start_sessions = sessions.clone();
        let id = process_backend(move || {
            plakous_tools::internal::start_process_with_output_limit(
                &start_sessions,
                request,
                control
                    .with_preempt(tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1),
                max_output_bytes,
            )
        })
        .await?;
        let handle = u64::from(JoinedProcessSessions::job_number(id));
        let mut guard = StartedProcessGuard::new(sessions.clone(), id);
        let result = async {
            let observed_at = std::time::Instant::now();
            let mut result = Self::observe_summarized(
                environment,
                handle,
                yield_time_ms,
                max_output_bytes,
                JoinedProcessOutputCursor::new(0),
                ObservationWait::PromotionHint,
                &observation_control,
            )
            .await;
            if result
                .as_ref()
                .is_ok_and(|result| result.projection.session_id.is_some())
            {
                let promotion = promote_job(&sessions, id).await?;
                if promotion.as_ref().is_err_and(|error| {
                    error.kind() == plakous_tools::ExecutionBackendErrorKind::ResourceLimit
                }) && *observation_control.preempt().borrow() != plakous_tools::ToolPreempt::None
                {
                    // A full notice ledger cannot turn observation preemption into cancellation.
                    let remaining = std::time::Duration::from_millis(yield_time_ms)
                        .saturating_sub(observed_at.elapsed());
                    let foreground = observation_control.clone().with_preempt(
                        tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1,
                    );
                    result = Self::continue_foreground_until_promotable(
                        environment,
                        &sessions,
                        id,
                        u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                        max_output_bytes,
                        &foreground,
                    )
                    .await;
                } else {
                    promotion.map_err(backend_runtime_error)?;
                    if let Ok(result) = &mut result {
                        result.first_promotion = true;
                    }
                }
            }
            result
        }
        .await;
        guard.disarm_or_terminate(result.is_ok()).await?;
        result
    }

    async fn continue_foreground_until_promotable(
        environment: &plakous_tools::ExecutionEnvironment,
        sessions: &JoinedProcessSessions,
        id: JoinedProcessSessionId,
        mut window: u64,
        max_output_bytes: usize,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let handle = u64::from(JoinedProcessSessions::job_number(id));
        loop {
            let mut result = Self::observe_summarized(
                environment,
                handle,
                window,
                max_output_bytes,
                JoinedProcessOutputCursor::new(0),
                ObservationWait::Hint,
                control,
            )
            .await?;
            if result.projection.session_id.is_none() {
                return Ok(result);
            }
            match promote_job(sessions, id).await? {
                Ok(()) => {
                    result.first_promotion = true;
                    return Ok(result);
                }
                Err(error)
                    if error.kind() == plakous_tools::ExecutionBackendErrorKind::ResourceLimit =>
                {
                    window = 250;
                }
                Err(error) => return Err(backend_runtime_error(error)),
            }
        }
    }

    async fn write_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessWriteRequest,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        request.validate()?;
        let session = Self::session(environment, request.session_id)?;
        if request.input == "\u{3}" {
            return Self::terminate_canonical(
                environment,
                &CanonicalProcessTerminateRequest {
                    session_id: request.session_id,
                },
            )
            .await;
        }
        if let Some(status) = session.sessions.joined_terminal_status(session.id)
            && !request.input.is_empty()
        {
            return Err(exited_session_refusal(request.session_id, status));
        }
        // Empty input with `keep_open` only polls; `close` still closes.
        if !request.input.is_empty()
            || matches!(request.disposition, CanonicalProcessInputDisposition::Close)
        {
            let sessions = session.sessions.clone();
            let id = session.id;
            let input = request.input.clone();
            let disposition = request.disposition.into();
            process_backend(move || sessions.write_stdin(id, input.as_bytes(), disposition))
                .await?;
        }
        // Reference `write_stdin` answers with the output that follows the
        // write, so the model does not need a second call to see it.
        if let Some(yield_time_ms) = request.yield_time_ms {
            let cursor = *lock_process_cursor(&session.cursor);
            return Self::observe_summarized(
                environment,
                request.session_id,
                yield_time_ms,
                request
                    .max_output_bytes
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .unwrap_or(PROCESS_OBSERVATION_BYTES),
                cursor,
                if request.input.is_empty() {
                    ObservationWait::Output
                } else {
                    ObservationWait::Hint
                },
                control,
            )
            .await;
        }
        Ok(ProcessExecutionResult {
            skipped: 0,
            projection: ProcessProjectionResult {
                exit_code: None,
                session_id: Some(request.session_id),
                output: String::new(),
            },
            status: "running",
            cursor: lock_process_cursor(&session.cursor).get(),
            sessions: session.sessions,
            session_id: session.id,
            joined: false,
            first_promotion: false,
            truncated: false,
        })
    }

    async fn poll_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessPollRequest,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        request.validate()?;
        if let Some(reported) = Self::reported_canonical(environment, request.session_id) {
            return Ok(reported);
        }
        let session = Self::session(environment, request.session_id)?;
        if request.read_so_far {
            let outcome = Self::await_canonical(
                environment,
                &CanonicalProcessAwaitRequest {
                    session_ids: vec![request.session_id],
                    wake: None,
                    yield_time_ms: if request.wait_for_exit {
                        request.yield_time_ms
                    } else {
                        0
                    },
                    max_output_bytes: request.max_output_bytes,
                    max_output_records: request.max_output_records,
                },
                control,
            )
            .await?;
            let value: Value =
                serde_json::from_str(&outcome.joined_text()).map_err(projection_arguments)?;
            let task = &value["tasks"][0];
            let terminal = task["status"] != "running";
            let status = session.sessions.joined_terminal_status(session.id);
            return Ok(ProcessExecutionResult {
                projection: ProcessProjectionResult {
                    exit_code: status.and_then(status_exit_code),
                    session_id: if terminal {
                        None
                    } else {
                        Some(request.session_id)
                    },
                    output: task["output"].as_str().unwrap_or_default().to_owned(),
                },
                skipped: 0,
                status: status.map_or("running", process_status_label),
                cursor: lock_process_cursor(&session.cursor).get(),
                sessions: session.sessions,
                session_id: session.id,
                joined: terminal,
                first_promotion: false,
                truncated: false,
            });
        }
        let stored = lock_process_cursor(&session.cursor).get();
        if let Some(cursor) = request.cursor
            && cursor != stored
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "process poll cursor is stale or foreign",
            ));
        }
        Self::observe_summarized(
            environment,
            request.session_id,
            request.yield_time_ms,
            request.max_output_bytes,
            JoinedProcessOutputCursor::new(stored),
            ObservationWait::Output,
            control,
        )
        .await
    }

    async fn terminate_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessTerminateRequest,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        if let Some(reported) = Self::reported_canonical(environment, request.session_id) {
            return Ok(reported);
        }
        let session = Self::session(environment, request.session_id)?;
        let cursor = lock_process_cursor(&session.cursor).get();
        let sessions = session.sessions.clone();
        let id = session.id;
        let status =
            process_backend(move || sessions.stop(id, plakous_tools::JobStopReason::Model)).await?;
        Ok(ProcessExecutionResult {
            skipped: 0,
            projection: ProcessProjectionResult {
                exit_code: status_exit_code(status),
                session_id: None,
                output: String::new(),
            },
            status: process_status_label(status),
            cursor,
            sessions: session.sessions,
            session_id: session.id,
            joined: true,
            first_promotion: false,
            truncated: false,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one multi-session wait keeps terminal reporting and notice attribution together"
    )]
    async fn await_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessAwaitRequest,
        control: &ExecutionOperationControl,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        request.validate()?;
        let mut handles = Vec::new();
        for handle in &request.session_ids {
            if !handles.contains(handle) {
                handles.push(*handle);
            }
        }
        if handles.is_empty() {
            // A bare first-terminal await watches every own job still listed.
            handles = environment
                .joined_process_sessions()
                .map_err(backend_runtime_error)?
                .jobs()
                .into_iter()
                .map(|job| u64::from(job.number))
                .collect();
        }
        let sessions = handles
            .into_iter()
            .map(|handle| (handle, Self::session(environment, handle)))
            .collect::<Vec<_>>();
        let mut changes = environment
            .joined_process_sessions()
            .map_err(backend_runtime_error)?
            .job_terminal_changes();
        let mut preempt = control.preempt().clone();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(request.yield_time_ms);
        loop {
            changes.borrow_and_update();
            control.check_cancelled().map_err(backend_runtime_error)?;
            let terminal =
                |(_, session): &(u64, Result<BoundProcessSession, ToolRuntimeError>)| match session
                {
                    Ok(session) => session
                        .sessions
                        .joined_terminal_status(session.id)
                        .is_some(),
                    Err(_) => true,
                };
            let settled = match request.wake {
                // An already-reported or unknown id is no fresh terminal: only
                // a listed job that is or turns terminal wakes the await.
                // It settles at once when every listed id is already reported
                // or unknown, or nothing is listed (a bare wait with no jobs:
                // `all` on an empty session list).
                Some(AwaitWake::Any) => {
                    sessions.iter().all(|(_, session)| session.is_err())
                        || sessions.iter().any(|(_, session)| {
                            session.as_ref().is_ok_and(|session| {
                                session
                                    .sessions
                                    .joined_terminal_status(session.id)
                                    .is_some()
                            })
                        })
                }
                None => sessions.iter().all(terminal),
            };
            if settled
                || std::time::Instant::now() >= deadline
                || *control.preempt().borrow() == plakous_tools::ToolPreempt::Steer
            {
                break;
            }
            await_process_change(&mut changes, &mut preempt, deadline, control).await;
        }
        let page_budget = (request.max_output_bytes / sessions.len().max(1)).max(1);
        let any_promoted = sessions.iter().any(|(_, session)| {
            session
                .as_ref()
                .is_ok_and(|session| session.sessions.is_promoted(session.id))
        });
        let mut tasks = Vec::new();
        let mut terminals = Vec::new();
        for (handle, session) in sessions {
            let Ok(session) = session else {
                let reported = Self::reported_canonical(environment, handle);
                let mut task = json!({"task_id":format!("p{handle}"), "status":if reported.is_some() {"already_reported"} else {"not_found"}, "output":""});
                if let Some(code) = reported.and_then(|result| result.projection.exit_code) {
                    task["exit_code"] = json!(code);
                }
                tasks.push(task);
                continue;
            };
            let backend = session.sessions.clone();
            let id = session.id;
            let max_records = request.max_output_records;
            let poll = process_backend(move || {
                plakous_tools::internal::process_session_tail(
                    &backend,
                    id,
                    page_budget,
                    max_records,
                )
            })
            .await?;
            let status = poll.status();
            let mut bytes = Vec::new();
            for record in poll.records() {
                bytes.extend_from_slice(record.bytes());
            }
            let mut page = HeadTailPage::new(page_budget);
            page.skip(poll.skipped_bytes());
            page.push(&String::from_utf8_lossy(&bytes));
            if status.is_terminal() {
                if session.sessions.active_session_ids().contains(&session.id) {
                    let backend = session.sessions.clone();
                    process_backend(move || backend.join(id)).await?;
                }
                if session.sessions.is_promoted(session.id) {
                    terminals.push(u32::try_from(handle).map_err(projection_arguments)?);
                }
            }
            let truncated = page.omitted != 0;
            let mut output = page.finish();
            if truncated {
                append_output_file(&mut output, &session.sessions, session.id);
            }
            let mut task = json!({"task_id":format!("p{handle}"), "status":process_status_label(status), "output":output});
            if let Some(code) = status_exit_code(status) {
                task["exit_code"] = json!(code);
            }
            tasks.push(task);
        }
        let yielded = tasks.iter().any(|task| task["status"] == "running")
            && *control.preempt().borrow() == plakous_tools::ToolPreempt::Steer;
        let outcome = ToolOutcome::success_text(
            serde_json::to_string(&json!({"tasks":tasks})).map_err(projection_arguments)?,
        )?;
        let outcome = if any_promoted {
            plakous_tools::internal::with_process_telemetry(
                outcome,
                ProcessTelemetry::Waited {
                    operation: ProcessOperationKind::Await,
                    yielded,
                },
            )
        } else {
            outcome
        };
        Ok(plakous_tools::internal::process_job_terminals(
            outcome, terminals,
        ))
    }

    fn reported_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
    ) -> Option<ProcessExecutionResult> {
        let sessions = environment.existing_joined_process_sessions()?;
        let number = u32::try_from(handle).ok()?;
        let (id, status) = plakous_tools::internal::reported_process_session(&sessions, number)?;
        Some(ProcessExecutionResult {
            skipped: 0,
            projection: ProcessProjectionResult {
                exit_code: status_exit_code(status),
                session_id: None,
                output: String::new(),
            },
            status: process_status_label(status),
            cursor: 0,
            sessions,
            session_id: id,
            joined: true,
            first_promotion: false,
            truncated: false,
        })
    }

    #[cfg(test)]
    fn codex_shell(
        &self,
        requested: Option<&str>,
        login: bool,
    ) -> Result<JoinedProcessShellProfile, ToolRuntimeError> {
        let profile = if login {
            &self.login_shell
        } else {
            &self.shell
        };
        if requested.is_some_and(|requested| requested != profile.executable().to_string_lossy()) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "requested shell is not the enrolled shell profile",
            ));
        }
        Ok(profile.clone())
    }

    fn session(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
    ) -> Result<BoundProcessSession, ToolRuntimeError> {
        let unknown = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "process session handle is unknown or stale: not in this session",
            )
        };
        let sessions = environment
            .existing_joined_process_sessions()
            .ok_or_else(unknown)?;
        let number = u32::try_from(handle).map_err(|_| unknown())?;
        let id = sessions.session_for_number(number).ok_or_else(unknown)?;
        let (cursor, pending_utf8) =
            plakous_tools::internal::process_session_projection_state(&sessions, id)
                .map_err(backend_runtime_error)?;
        Ok(BoundProcessSession {
            id,
            sessions,
            cursor,
            pending_utf8,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the closed process observation axes remain explicit at the single call boundary"
    )]
    #[cfg(test)]
    async fn observe_with_update(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
        yield_time_ms: u64,
        max_output_tokens: Option<u64>,
        cursor: JoinedProcessOutputCursor,
        may_join: bool,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let max_bytes = max_output_tokens
            .and_then(|tokens| tokens.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(PROCESS_OBSERVATION_BYTES)
            .clamp(1, PROCESS_OBSERVATION_BYTES);
        Self::observe_bounded_with_update(
            environment,
            handle,
            yield_time_ms,
            max_bytes,
            256,
            cursor,
            may_join,
            ObservationWait::Hint,
            control,
        )
        .await
    }

    /// One model-facing observation of a running session: everything it
    /// produced during the yield, shown as head and tail when it exceeds
    /// `max_bytes`, with the cursor advanced to the end. A chatty process is
    /// then summarised per call instead of paged 40 KB at a time, which a
    /// model polling a long test run could never catch up with.
    #[allow(
        clippy::too_many_arguments,
        reason = "the observation binds independent output and wait bounds"
    )]
    async fn observe_summarized(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
        yield_time_ms: u64,
        max_bytes: usize,
        cursor: JoinedProcessOutputCursor,
        wait: ObservationWait,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(yield_time_ms))
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "yield timeout overflow",
                )
            })?;
        let mut page = HeadTailPage::new(max_bytes);
        let mut cursor = cursor;
        let mut drained_after_deadline = 0_usize;
        let session = Self::session(environment, handle)?;
        let mut changes =
            plakous_tools::internal::process_session_changes(&session.sessions, session.id)
                .map_err(backend_runtime_error)?;
        let mut preempt = control.preempt().clone();
        loop {
            changes.borrow_and_update();
            let remaining = u64::try_from(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX);
            let result = Self::observe_bounded_with_update(
                environment,
                handle,
                remaining,
                PROCESS_OBSERVATION_BYTES,
                256,
                cursor,
                true,
                wait,
                control,
            )
            .await?;
            page.skip(result.skipped);
            let empty = result.projection.output.is_empty();
            // A full chunk means more output is already waiting: drain the
            // backlog (bounded by the session window) before returning.
            let full = result.projection.output.len() + 4 >= PROCESS_OBSERVATION_BYTES;
            page.push(&result.projection.output);
            if std::time::Instant::now() >= deadline {
                drained_after_deadline += 1;
            }
            if result.joined
                || (wait == ObservationWait::Output && !empty && !full)
                || wait.preempted(*control.preempt().borrow())
                || (std::time::Instant::now() >= deadline
                    && (!full || drained_after_deadline > MAX_DRAIN_READS))
            {
                let truncated = page.omitted != 0;
                return Ok(ProcessExecutionResult {
                    projection: ProcessProjectionResult {
                        output: page.finish(),
                        ..result.projection
                    },
                    skipped: 0,
                    truncated,
                    ..result
                });
            }
            cursor = JoinedProcessOutputCursor::new(result.cursor);
            if empty {
                await_process_change(&mut changes, &mut preempt, deadline, control).await;
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the closed poll contract carries exact independent bounds"
    )]
    async fn observe_bounded_with_update(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
        yield_time_ms: u64,
        max_bytes: usize,
        max_records: usize,
        cursor: JoinedProcessOutputCursor,
        may_join: bool,
        wait: ObservationWait,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let session = Self::session(environment, handle)?;
        let mut changes =
            plakous_tools::internal::process_session_changes(&session.sessions, session.id)
                .map_err(backend_runtime_error)?;
        let mut preempt = control.preempt().clone();
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(yield_time_ms))
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "yield timeout overflow",
                )
            })?;
        let mut output = String::new();
        let mut skipped = 0_u64;
        let mut cursor = cursor;
        loop {
            changes.borrow_and_update();
            let remaining = max_bytes.saturating_sub(output.len());
            debug_assert_ne!(remaining, 0);
            let backend = session.sessions.clone();
            let id = session.id;
            let poll =
                process_backend(move || backend.poll_from(id, cursor, remaining, max_records))
                    .await?;
            let backend_terminal = poll.status().is_terminal() && !poll.more_output();
            skipped = skipped.saturating_add(poll.skipped_bytes());
            let (terminal, buffered_without_text_progress) = {
                let mut pending_utf8 = lock_pending_process_utf8(&session.pending_utf8);
                let pending_before = pending_utf8.len();
                let output_before = output.len();
                for record in poll.records() {
                    pending_utf8.extend_from_slice(record.bytes());
                }
                output.push_str(&drain_process_utf8_bounded(
                    &mut pending_utf8,
                    backend_terminal,
                    remaining,
                ));
                let terminal = backend_terminal && pending_utf8.is_empty();
                let text_progress = output.len() != output_before;
                let buffered_without_text_progress = !text_progress
                    && !pending_utf8.is_empty()
                    && (pending_utf8.len() != pending_before || backend_terminal);
                (terminal, buffered_without_text_progress)
            };
            cursor = poll.next_cursor();
            *lock_process_cursor(&session.cursor) = cursor;
            if may_join && terminal {
                let backend = session.sessions.clone();
                let status = process_backend(move || backend.join(id)).await?;
                return Ok(ProcessExecutionResult {
                    skipped,
                    projection: ProcessProjectionResult {
                        exit_code: status_exit_code(status),
                        session_id: None,
                        output,
                    },
                    sessions: session.sessions,
                    session_id: session.id,
                    joined: true,
                    first_promotion: false,
                    truncated: false,
                    status: process_status_label(status),
                    cursor: cursor.get(),
                });
            }
            control.check_cancelled().map_err(backend_runtime_error)?;
            if wait.preempted(*control.preempt().borrow())
                || (wait == ObservationWait::Output && !output.is_empty())
                || output.len() == max_bytes
                || buffered_without_text_progress
                || !may_join
                || (!poll.status().is_terminal() && std::time::Instant::now() >= deadline)
            {
                let mut active = active_process_result(session, handle, output, cursor);
                active.skipped = skipped;
                return Ok(active);
            }
            if poll.more_output() {
                continue;
            }
            await_process_change(&mut changes, &mut preempt, deadline, control).await;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
// This is the closed wire DTO for four independent canonical process
// capabilities, not one state machine encoded as flags.
#[allow(
    clippy::struct_excessive_bools,
    reason = "closed wire DTO carries independent process capabilities"
)]
struct CanonicalProcessStartRequest {
    command: String,
    workdir: PathBuf,
    login: bool,
    tty: bool,
    keep_stdin_open: bool,
    #[serde(default, rename = "model_description")]
    _model_description: Option<String>,
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    yield_time_ms: u64,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    max_output_bytes: usize,
}

impl CanonicalProcessStartRequest {
    fn validate(&self) -> Result<(), ToolRuntimeError> {
        if self.command.is_empty()
            || self.command.len() > 1_048_576
            || !(1..=PROCESS_OBSERVATION_BYTES).contains(&self.max_output_bytes)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process start bounds are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CanonicalProcessInputDisposition {
    KeepOpen,
    Close,
}

impl From<CanonicalProcessInputDisposition> for JoinedProcessInputDisposition {
    fn from(value: CanonicalProcessInputDisposition) -> Self {
        match value {
            CanonicalProcessInputDisposition::KeepOpen => Self::KeepOpen,
            CanonicalProcessInputDisposition::Close => Self::Close,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalProcessWriteRequest {
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    session_id: u64,
    input: String,
    disposition: CanonicalProcessInputDisposition,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    yield_time_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    max_output_bytes: Option<u64>,
}

impl CanonicalProcessWriteRequest {
    fn validate(&self) -> Result<(), ToolRuntimeError> {
        if self.session_id == 0
            || self.input.len() > 262_144
            || self.yield_time_ms.is_some_and(|yield_time_ms| {
                yield_time_ms
                    > if self.input.is_empty() {
                        300_000
                    } else {
                        30_000
                    }
            })
            || self
                .max_output_bytes
                .is_some_and(|bytes| !(1..=PROCESS_OBSERVATION_BYTES as u64).contains(&bytes))
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process write bounds are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalProcessPollRequest {
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    session_id: u64,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    cursor: Option<u64>,
    #[serde(default)]
    read_so_far: bool,
    #[serde(default)]
    wait_for_exit: bool,
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    yield_time_ms: u64,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    max_output_bytes: usize,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    max_output_records: usize,
}

impl CanonicalProcessPollRequest {
    fn validate(&self) -> Result<(), ToolRuntimeError> {
        if self.session_id == 0
            || self.yield_time_ms
                > if self.wait_for_exit {
                    3_600_000
                } else {
                    300_000
                }
            || !(1..=PROCESS_OBSERVATION_BYTES).contains(&self.max_output_bytes)
            || !(1..=256).contains(&self.max_output_records)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process poll bounds are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalProcessTerminateRequest {
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    session_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalProcessAwaitRequest {
    #[serde(default, deserialize_with = "deserialize_session_ids")]
    session_ids: Vec<u64>,
    /// `any`: return at the first terminal; without `session_ids`, over every
    /// own job still listed. Absent: await every named process.
    #[serde(default)]
    wake: Option<AwaitWake>,
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    yield_time_ms: u64,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    max_output_bytes: usize,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    max_output_records: usize,
}

/// When a multi-process await returns.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum AwaitWake {
    /// At the first terminal among the awaited processes.
    Any,
}

fn validate_canonical_poll_shape(value: &Value) -> Result<(), ToolRuntimeError> {
    if value.get("wake").is_some() && value.get("session_id").is_none() {
        return Ok(());
    }
    if value.get("session_id").is_some() == value.get("session_ids").is_some() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "canonical process poll requires exactly one of session_id or session_ids",
        ));
    }
    Ok(())
}

fn deserialize_session_ids<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<u64>, D::Error> {
    Vec::<Value>::deserialize(deserializer)?
        .into_iter()
        .map(|value| deserialize_semantic_u64(value).map_err(serde::de::Error::custom))
        .collect()
}

impl CanonicalProcessAwaitRequest {
    fn validate(&self) -> Result<(), ToolRuntimeError> {
        if (self.session_ids.is_empty() && self.wake.is_none())
            || self.session_ids.len() > 128
            || self.session_ids.contains(&0)
            || self.yield_time_ms > 3_600_000
            || !(1..=PROCESS_OBSERVATION_BYTES).contains(&self.max_output_bytes)
            || !(1..=256).contains(&self.max_output_records)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process await bounds are invalid",
            ));
        }
        Ok(())
    }
}

fn process_observation_labels(
    canonical: CanonicalProcessOperationV2,
    value: &mut Value,
) -> (ProcessOperationKind, ProcessHintSource) {
    let hint_source = match value
        .as_object_mut()
        .and_then(|value| value.remove("hint_source"))
        .as_ref()
        .and_then(Value::as_str)
    {
        Some("model_hint") => ProcessHintSource::ModelHint,
        Some("background") => ProcessHintSource::Background,
        _ => ProcessHintSource::Default,
    };
    let operation = match canonical {
        CanonicalProcessOperationV2::Start => ProcessOperationKind::Start,
        CanonicalProcessOperationV2::Write => ProcessOperationKind::WriteWait,
        CanonicalProcessOperationV2::Poll
            if (value.get("session_ids").is_some() || value.get("wake").is_some())
                && value
                    .get("yield_time_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    != 0 =>
        {
            ProcessOperationKind::Await
        }
        _ => ProcessOperationKind::Poll,
    };
    (operation, hint_source)
}

fn classify_process_outcome(
    outcome: ToolOutcome,
    operation: ProcessOperationKind,
    hint_source: ProcessHintSource,
    yielded: bool,
) -> ToolOutcome {
    match plakous_tools::internal::process_telemetry(&outcome) {
        Some(ProcessTelemetry::Promoted {
            hint_ms, preempt, ..
        }) => plakous_tools::internal::with_process_telemetry(
            outcome,
            ProcessTelemetry::Promoted {
                operation,
                hint_source: if preempt == plakous_tools::ToolPreempt::Background {
                    ProcessHintSource::Background
                } else {
                    hint_source
                },
                hint_ms,
                preempt,
            },
        ),
        Some(ProcessTelemetry::Waited {
            yielded: previous, ..
        }) => plakous_tools::internal::with_process_telemetry(
            outcome,
            ProcessTelemetry::Waited {
                operation,
                yielded: previous || yielded,
            },
        ),
        None => outcome,
    }
}

/// Reserves the job's output file on the blocking pool (the reservation walks
/// the jobs root), then promotes inline. Publication is never followed by an
/// await, so cancellation, which drops a suspended start and terminates its
/// process, can never reach a job whose ownership has moved to the job table.
/// The outer error is only a lost worker; the inner one is the promotion's.
async fn promote_job(
    sessions: &JoinedProcessSessions,
    id: JoinedProcessSessionId,
) -> Result<Result<(), plakous_tools::ExecutionBackendError>, ToolRuntimeError> {
    let reserving = sessions.clone();
    let reservation =
        process_backend(move || Ok(plakous_tools::internal::reserve_job_output(&reserving, id)))
            .await?;
    Ok(plakous_tools::internal::promote_reserved(
        sessions,
        id,
        reservation,
    ))
}

async fn process_backend<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, plakous_tools::ExecutionBackendError> + Send + 'static,
) -> Result<T, ToolRuntimeError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "process backend worker did not complete",
                &error,
            )
        })?
        .map_err(backend_runtime_error)
}

async fn await_process_change(
    changes: &mut tokio::sync::watch::Receiver<u64>,
    preempt: &mut tokio::sync::watch::Receiver<plakous_tools::ToolPreempt>,
    deadline: std::time::Instant,
    control: &ExecutionOperationControl,
) {
    // Some callers deliberately have no preempt sender. A closed watch must
    // not turn a long observation into a ready-future busy loop.
    let preempt_open = preempt.has_changed().is_ok();
    tokio::select! {
        _ = changes.changed() => {}
        _ = preempt.changed(), if preempt_open => {}
        () = control.cancellation().cancelled() => {}
        () = tokio::time::sleep_until(deadline.into()) => {}
    }
}

/// The stop tool's raised result for one canonical terminate result: the
/// same bytes the projection's process raising writes for a stop.
fn raised_stop_result(canonical: &str) -> Result<String, ToolRuntimeError> {
    #[derive(Serialize)]
    struct Stopped<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<&'a str>,
        output: &'a str,
    }
    let value: Value = serde_json::from_str(canonical).map_err(projection_arguments)?;
    let status = value["status"].as_str().unwrap_or_default();
    serde_json::to_string(&Stopped {
        exit_code: value["exit_code"].as_i64(),
        session_id: value["session_id"].as_u64(),
        status: matches!(
            status,
            "timed_out"
                | "terminated"
                | "cancelled"
                | "output_limit"
                | "failed"
                | "outcome_unknown"
                | "cleanup_unresolved"
        )
        .then_some(status),
        output: value["output"].as_str().unwrap_or_default(),
    })
    .map_err(projection_arguments)
}

async fn execute_process_await(
    environment: Arc<plakous_tools::ExecutionEnvironment>,
    value: Value,
    control: ExecutionOperationControl,
    operation: ProcessOperationKind,
    hint_source: ProcessHintSource,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let request = serde_json::from_value(value).map_err(projection_arguments)?;
    let result = ProcessSessionTools::await_canonical(&environment, &request, &control).await;
    match result {
        Err(error) if error.proven_no_effect() => {
            ToolOutcome::failure_text(error.message().as_str())
        }
        result => {
            result.map(|outcome| classify_process_outcome(outcome, operation, hint_source, false))
        }
    }
}

impl ToolExecutor for ProcessSessionTools {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let identity = invocation.identity();
            let canonical = match (identity.name().as_str(), identity.tool_id().as_str()) {
                (PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Start)
                }
                (PROCESS_SESSION_WRITE_NAME, PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Write)
                }
                (PROCESS_SESSION_POLL_NAME, PROCESS_SESSION_POLL_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Poll)
                }
                (PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Terminate)
                }
                _ => None,
            }
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "canonical process registration identity is not enrolled",
                )
            })?;
            let mut value = invocation.arguments().value().as_ref().clone();
            let (operation, hint_source) = process_observation_labels(canonical, &mut value);
            if canonical == CanonicalProcessOperationV2::Poll {
                validate_canonical_poll_shape(&value)?;
            }
            context.check_cancelled()?;
            let environment = context.environment().clone();
            let control = operation_control(&context);
            let promotion_hint = (canonical == CanonicalProcessOperationV2::Start).then(|| {
                deserialize_semantic_u64(value["yield_time_ms"].clone())
                    .unwrap_or(30_000)
                    .clamp(250, 600_000)
            });
            let preempt = control.preempt().clone();
            if canonical == CanonicalProcessOperationV2::Poll
                && (value.get("session_ids").is_some() || value.get("wake").is_some())
            {
                return execute_process_await(environment, value, control, operation, hint_source)
                    .await;
            }
            let result = match canonical {
                CanonicalProcessOperationV2::Start => {
                    self.start_canonical(
                        &environment,
                        serde_json::from_value(value).map_err(projection_arguments)?,
                        control,
                    )
                    .await
                }
                CanonicalProcessOperationV2::Write => {
                    Self::write_canonical(
                        &environment,
                        &serde_json::from_value(value).map_err(projection_arguments)?,
                        &control,
                    )
                    .await
                }
                CanonicalProcessOperationV2::Poll => {
                    Self::poll_canonical(
                        &environment,
                        &serde_json::from_value(value).map_err(projection_arguments)?,
                        &control,
                    )
                    .await
                }
                CanonicalProcessOperationV2::Terminate => {
                    Self::terminate_canonical(
                        &environment,
                        &serde_json::from_value(value).map_err(projection_arguments)?,
                    )
                    .await
                }
            };
            match result {
                Ok(process_result) => {
                    let yielded = !process_result.joined
                        && *preempt.borrow() == plakous_tools::ToolPreempt::Steer;
                    process_result
                        .into_outcome_with_promotion(
                            promotion_hint.map(|hint| (hint, *preempt.borrow())),
                        )
                        .map(|outcome| {
                            classify_process_outcome(outcome, operation, hint_source, yielded)
                        })
                }
                Err(error) if error.proven_no_effect() => {
                    // One rule, not a list of conditions. Whatever the process
                    // executor refuses on its way in — a malformed argument, an
                    // unknown or exited `session_id`, a `workdir` outside the
                    // workspace, a session limit, a full operation queue, a mode
                    // it does not support — never started a process and never
                    // delivered a byte, so the outcome is known. A started
                    // dispatch that returned `Err` would be normalized to
                    // `OutcomeUnknown` and force recovery, and the model's own
                    // program could not catch it. Refusals it can act on must
                    // reach it as ordinary tool failures instead.
                    //
                    // The disposition travels with the error from the authority
                    // that raised it, so a condition nobody has hit yet is
                    // classified by where it was raised rather than by a list
                    // that the next condition will outgrow.
                    ToolOutcome::failure_text(error.message().as_str())
                }
                Err(error) => Err(error),
            }
        })
    }
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code, reason = "Codex wire compat fields are decoded and ignored")]
struct CodexExecArgs {
    cmd: String,
    workdir: Option<PathBuf>,
    shell: Option<String>,
    login: Option<bool>,
    tty: Option<bool>,
    yield_time_ms: Option<f64>,
    max_output_tokens: Option<f64>,
    session_id: Option<u64>,
    sandbox_permissions: Option<String>,
    justification: Option<String>,
    prefix_rule: Option<Vec<String>>,
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexWriteArgs {
    session_id: u64,
    chars: Option<String>,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
}

#[cfg(test)]
fn non_negative_f64_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "u64::MAX fits exactly in f64 mantissa"
    )]
    let max = u64::MAX as f64;
    if value > max {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "whole non-negative values within u64 range"
    )]
    Some(value as u64)
}

#[cfg(test)]
fn projection_tool(
    projection: ProcessToolProjectionKind,
    name: &str,
) -> Result<ProjectedProcessTool, ToolRuntimeError> {
    projection
        .tools(plakous_tools::JobConfig::default())?
        .into_iter()
        .find(|tool| tool.name().as_str() == name)
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection tool is absent",
            )
        })
}

/// Refuses an operation on a session that has already exited and been joined.
///
/// A joined terminal status is proof the group is gone and reaped, so this
/// write reached nothing and mutated nothing. That makes the outcome known,
/// and the model is told exactly what it found rather than being handed an
/// unreconcilable journal gap. A session that is terminating but not yet
/// joined has no such proof and stays on the unknown-outcome path.
fn exited_session_refusal(session_id: u64, status: JoinedProcessStatus) -> ToolRuntimeError {
    let disposition = match status {
        JoinedProcessStatus::Exited(Some(code)) => format!("status {code}"),
        JoinedProcessStatus::Exited(None) => String::from("no status"),
        JoinedProcessStatus::OutcomeUnknown => String::from("an unknown outcome"),
        JoinedProcessStatus::Terminated => String::from("an explicit termination"),
        JoinedProcessStatus::Cancelled => String::from("a cancellation"),
        JoinedProcessStatus::TimedOut => String::from("a timeout"),
        JoinedProcessStatus::OutputLimit => String::from("an output limit"),
        // `Failed`, and nothing else: `Running` and `CleanupUnresolved` are
        // not joined terminal statuses and never reach here.
        _ => String::from("a failure"),
    };
    ToolRuntimeError::new(
        // The same no-effect family as a stale or unknown `session_id`: the
        // model supplied an identity that can no longer be written to, and it
        // can correct that itself.
        ToolRuntimeErrorKind::InvalidArguments,
        format!(
            "process session {session_id} already exited with {disposition}; nothing was written"
        ),
    )
}

fn status_exit_code(status: JoinedProcessStatus) -> Option<i32> {
    match status {
        JoinedProcessStatus::Exited(code) => code,
        _ => None,
    }
}

const fn process_status_label(status: JoinedProcessStatus) -> &'static str {
    match status {
        JoinedProcessStatus::Running => "running",
        JoinedProcessStatus::Exited(_) => "exited",
        JoinedProcessStatus::Terminated => "terminated",
        JoinedProcessStatus::Cancelled => "cancelled",
        JoinedProcessStatus::TimedOut => "timed_out",
        JoinedProcessStatus::OutputLimit => "output_limit",
        JoinedProcessStatus::Failed => "failed",
        JoinedProcessStatus::CleanupUnresolved => "cleanup_unresolved",
        // `JoinedProcessStatus` is `#[non_exhaustive]` outside its crate; a
        // status this pack has not enrolled has no exact outcome evidence.
        _ => "outcome_unknown",
    }
}

fn lock_process_cursor(
    cursor: &Mutex<JoinedProcessOutputCursor>,
) -> std::sync::MutexGuard<'_, JoinedProcessOutputCursor> {
    cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_pending_process_utf8(pending: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn drain_process_utf8_bounded(pending: &mut Vec<u8>, terminal: bool, max_bytes: usize) -> String {
    let mut output = String::new();
    while output.len() < max_bytes {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                let available = max_bytes.saturating_sub(output.len()).min(text.len());
                let retained = utf8_string_prefix_len(text, available);
                output.push_str(&text[..retained]);
                pending.drain(..retained);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid != 0 {
                    let available = max_bytes.saturating_sub(output.len()).min(valid);
                    let text = std::str::from_utf8(&pending[..valid]).unwrap_or_default();
                    let retained = utf8_string_prefix_len(text, available);
                    output.push_str(&text[..retained]);
                    pending.drain(..retained);
                    if retained != valid {
                        break;
                    }
                }
                match error.error_len() {
                    Some(invalid)
                        if max_bytes.saturating_sub(output.len())
                            >= char::REPLACEMENT_CHARACTER.len_utf8() =>
                    {
                        output.push(char::REPLACEMENT_CHARACTER);
                        pending.drain(..invalid);
                    }
                    None if terminal
                        && max_bytes.saturating_sub(output.len())
                            >= char::REPLACEMENT_CHARACTER.len_utf8() =>
                    {
                        output.push(char::REPLACEMENT_CHARACTER);
                        pending.clear();
                        break;
                    }
                    _ => break,
                }
            }
        }
    }
    output
}

/// Reads a summarised observation may make after its yield to drain a backlog:
/// more than one whole session window takes at one chunk each.
const MAX_DRAIN_READS: usize = 64;

/// One foreground page: the first 30% and the last 70% of its byte budget,
/// with the omitted byte count between them. A failure is usually at the end
/// of a long log, and the command that produced it at the start.
struct HeadTailPage {
    head: String,
    tail: String,
    head_budget: usize,
    tail_budget: usize,
    omitted: u64,
}

impl HeadTailPage {
    fn new(budget: usize) -> Self {
        let head_budget = budget * 3 / 10;
        Self {
            head: String::new(),
            tail: String::new(),
            head_budget,
            tail_budget: budget - head_budget,
            omitted: 0,
        }
    }

    /// Adds bytes the window evicted before they could be shown.
    fn skip(&mut self, bytes: u64) {
        self.omitted = self.omitted.saturating_add(bytes);
    }

    fn push(&mut self, mut text: &str) {
        let room = self.head_budget.saturating_sub(self.head.len());
        if room != 0 {
            let taken = utf8_string_prefix_len(text, room);
            self.head.push_str(&text[..taken]);
            text = &text[taken..];
        }
        self.tail.push_str(text);
        if self.tail.len() > self.tail_budget {
            let mut cut = self.tail.len() - self.tail_budget;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.omitted += cut as u64;
            self.tail.drain(..cut);
        }
    }

    fn finish(mut self) -> String {
        if self.omitted == 0 {
            self.head.push_str(&self.tail);
            return self.head;
        }
        // Cut on line boundaries when one is near, so neither side shows a
        // half line.
        if let Some(line_end) = self
            .head
            .rfind('\n')
            .filter(|end| self.head.len() - end <= 256)
        {
            self.omitted += (self.head.len() - line_end - 1) as u64;
            self.head.truncate(line_end + 1);
        }
        if let Some(line_start) = self.tail.find('\n').filter(|start| *start < 256) {
            self.omitted += (line_start + 1) as u64;
            self.tail.drain(..=line_start);
        }
        let marker = output_omitted_marker(self.omitted);
        let marker = marker.trim_start_matches('\n');
        if !self.head.ends_with('\n') && !self.head.is_empty() {
            self.head.push('\n');
        }
        self.head.push_str(marker);
        self.head.push_str(&self.tail);
        self.head
    }
}

/// Names the job's output file, when it has one, as the last line of `output`
/// (after the incomplete-file line when spooling to it failed).
fn append_output_file(
    output: &mut String,
    sessions: &JoinedProcessSessions,
    id: JoinedProcessSessionId,
) {
    if let Some(file) = sessions.job_output_file(JoinedProcessSessions::job_number(id)) {
        push_output_file(output, &file);
    }
}

fn push_output_file(output: &mut String, file: &plakous_tools::JobOutputFile) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if let Some(written) = file.write_failed_after {
        output.push_str(&plakous_tools::job_output_incomplete_line(written));
        output.push('\n');
    }
    output.push_str(text::PROCESS_OUTPUT_FILE);
    output.push_str(&file.path);
}

fn utf8_string_prefix_len(text: &str, maximum: usize) -> usize {
    let mut retained = maximum.min(text.len());
    while !text.is_char_boundary(retained) {
        retained = retained.saturating_sub(1);
    }
    retained
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::error::Error;
    use std::str::FromStr;
    use std::sync::Arc;

    use super::*;
    use plakous_model::{
        AttemptId, CallId, IntegrationClass, RunId, ToolCorrelation, ToolEffect, ToolName, TurnId,
    };
    use plakous_tools::internal::{
        ProcessSessionUpdate, authorization_binding, authorization_grant, bind_effect_dispatch,
        executor, process_session_update, tool_context,
    };
    use plakous_tools::{
        EffectAuthorizationGrantSource, ExecutionEnvironment, FrozenToolRegistry, ProgressSink,
        ToolExecutor, ToolInvocationRequest, ToolOutcomeKind, ToolPolicyRevision,
        ToolRegistryBuilder, ToolRegistryLimits, register_packs,
    };

    #[test]
    fn a_failed_spool_is_reported_before_the_full_output_line() {
        let mut output = String::from("tail");
        push_output_file(
            &mut output,
            &plakous_tools::JobOutputFile {
                path: String::from("/state/jobs/s/b-p3-0011223344556677.log"),
                write_failed_after: Some(4096),
            },
        );
        assert_eq!(
            output,
            "tail\n[output file incomplete: write failed after 4096 bytes]\n\
             full output: /state/jobs/s/b-p3-0011223344556677.log"
        );
        let mut output = String::new();
        push_output_file(
            &mut output,
            &plakous_tools::JobOutputFile {
                path: String::from("/p.log"),
                write_failed_after: None,
            },
        );
        assert_eq!(output, "full output: /p.log");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unified_exec_non_tty_has_null_stdin_and_pty_accepts_input()
    -> Result<(), Box<dyn Error>> {
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000603",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let tools = ProcessSessionTools::posix_sh()?;
        for command in ["cat", "test -c /dev/stdin"] {
            let result = tools
                .exec_command_with_update(
                    &environment,
                    &json!({"cmd": command, "login": false, "yield_time_ms": 1000}).to_string(),
                    control.clone(),
                )
                .await?;
            assert!(result.joined, "non-tty {command} must finish on EOF");
            assert_eq!(result.projection.exit_code, Some(0));
        }
        let result = tools.exec_command_with_update(
            &environment,
            r#"{"cmd":"read line; printf 'received:%s' \"$line\"","tty":true,"login":false,"yield_time_ms":0}"#,
            control.clone(),
        ).await?;
        let handle = result.projection.session_id.ok_or("missing PTY session")?;
        let result = ProcessSessionTools::write_stdin_with_update(
            &environment,
            &json!({"session_id": handle, "chars": "hello\n", "yield_time_ms": 1000}).to_string(),
            &control,
        )
        .await?;
        assert!(result.projection.output.contains("received:hello"));
        assert!(result.joined);
        Ok(())
    }

    #[test]
    fn process_await_wait_does_not_reserve_a_blocking_worker() -> Result<(), Box<dyn Error>> {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000599",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()?;
        let started = runtime.block_on(ProcessSessionTools::posix_sh()?.start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: "read line".into(),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 64,
            },
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        ))?;
        let handle = started.projection.session_id.ok_or("promoted job handle")?;
        let (preempt, receiver) = tokio::sync::watch::channel(plakous_tools::ToolPreempt::None);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let result = runtime.block_on(async {
            let (entered, receiver_entered) = tokio::sync::oneshot::channel();
            let waiting_environment = Arc::clone(&environment);
            let waiting = tokio::spawn(async move {
                let _ = entered.send(());
                execute_process_await(
                    waiting_environment,
                    json!({"session_ids":[handle],"yield_time_ms":5000,"max_output_bytes":64,"max_output_records":256}),
                    ExecutionOperationControl::new(plakous_model::CancellationToken::new()).with_preempt(receiver),
                    ProcessOperationKind::Await,
                    ProcessHintSource::Default,
                ).await
            });
            receiver_entered.await?;
            let (worker_entered, worker_receiver) = tokio::sync::oneshot::channel();
            let worker = tokio::task::spawn_blocking(move || {
                let _ = worker_entered.send(());
                let _ = release_rx.recv();
            });
            let available = tokio::time::timeout(std::time::Duration::from_secs(2), worker_receiver).await;
            // Once notification waiting ends, the tail read legitimately needs
            // the blocking pool. Release it before awaiting the final result.
            preempt.send(plakous_tools::ToolPreempt::Steer)?;
            release_tx.send(())?;
            worker.await?;
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), waiting).await??;
            assert!(available.is_ok(), "process await reserved the only blocking worker while waiting");
            Ok::<_, Box<dyn Error>>(result)
        });
        environment.joined_process_sessions()?.stop_jobs(
            plakous_tools::JobStopReason::SessionExit,
            std::time::Duration::from_millis(200),
        )?;
        let outcome = result??;
        let value: Value = serde_json::from_str(&outcome.joined_text())?;
        assert_eq!(value["tasks"][0]["status"], "running");
        assert_eq!(value["tasks"][0]["task_id"], format!("p{handle}"));
        Ok(())
    }
    #[test]
    fn process_executor_revision_rotates_the_exact_tuple_and_rejects_v2()
    -> Result<(), Box<dyn Error>> {
        const PREDECESSOR_CAPABILITY_REVISION: &str = "2";
        const PREDECESSOR_START_ID: &str = "plakous.process-session-start.v2";
        const PREDECESSOR_WRITE_ID: &str = "plakous.process-session-write.v2";
        const PREDECESSOR_POLL_ID: &str = "plakous.process-session-poll.v2";
        const PREDECESSOR_TERMINATE_ID: &str = "plakous.process-session-terminate.v2";
        const PREDECESSOR_EXECUTOR_ID: &str = "plakous.process-session-executor.v2";

        assert_ne!(
            PROCESS_SESSION_CAPABILITY_REVISION,
            PREDECESSOR_CAPABILITY_REVISION
        );
        assert_ne!(PROCESS_SESSION_REGISTERED_TOOL_ID, PREDECESSOR_START_ID);
        assert_ne!(
            PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
            PREDECESSOR_WRITE_ID
        );
        assert_ne!(PROCESS_SESSION_POLL_REGISTERED_TOOL_ID, PREDECESSOR_POLL_ID);
        assert_ne!(
            PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
            PREDECESSOR_TERMINATE_ID
        );
        assert_ne!(PROCESS_SESSION_EXECUTOR_ID, PREDECESSOR_EXECUTOR_ID);

        let process = Arc::new(ProcessSessionTools::posix_sh()?);
        let definition = ToolDefinition::new(
            ToolName::new(PROCESS_SESSION_CAPABILITY_NAME.to_owned())?,
            PREDECESSOR_CAPABILITY_REVISION,
            "Start one canonical joined process session.",
            ToolInputContract::Structured(canonical_process_input_schema(
                CanonicalProcessOperationV2::Start,
            )?),
            ToolOutputContract::Utf8(Utf8TextContractV1::new(1_048_576)?),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?;
        let executor: Arc<dyn ToolExecutor> = process;
        let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
        builder.register(ToolRegistration::new(
            RegisteredToolId::new(PREDECESSOR_START_ID)?,
            ToolExecutorId::new(PREDECESSOR_EXECUTOR_ID)?,
            ToolExecutorBindingIdentity::new(1, &[b"predecessor-process-session-tools"])?,
            definition,
            executor,
        ))?;
        let effective_configuration =
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"predecessor-process-configuration"],
            )?)?)?;
        let target = ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            PROCESS_PROJECTION_FAMILY,
            "test-model",
            "test-model",
            effective_configuration,
            PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        let error = ProcessToolProjectionKind::CodexUnifiedExec
            .projection_map(
                plakous_tools::JobConfig::default(),
                builder.freeze(),
                target,
            )
            .err()
            .ok_or("predecessor process tuple unexpectedly enrolled")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn await_chatty_terminal_deduplicates_and_reports_consumed_jobs()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
            JoinedProcessInputDisposition,
        };
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000599",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let started = ProcessSessionTools::posix_sh()?
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: "cat".into(),
                    workdir: ".".into(),
                    login: false,
                    tty: false,
                    keep_stdin_open: true,
                    _model_description: None,
                    yield_time_ms: 0,
                    max_output_bytes: 64,
                },
                control.clone(),
            )
            .await?;
        let handle = started.projection.session_id.ok_or("missing session")?;
        let sessions = environment.joined_process_sessions()?;
        let id = sessions
            .session_for_number(u32::try_from(handle)?)
            .ok_or("missing id")?;
        // Acknowledge each byte before the next write so each is a distinct record,
        // independent of pipe buffering or scheduler timing.
        let mut cursor = JoinedProcessOutputCursor::new(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        for _ in 0..16_385 {
            sessions.write_stdin(id, b"x", JoinedProcessInputDisposition::KeepOpen)?;
            loop {
                let poll = plakous_tools::internal::process_session_tail(&sessions, id, 1, 1)?;
                if poll.next_cursor() != cursor {
                    cursor = poll.next_cursor();
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "cat did not acknowledge output"
                );
                std::thread::yield_now();
            }
        }
        sessions.write_stdin(id, b"", JoinedProcessInputDisposition::Close)?;
        while sessions.joined_terminal_status(id).is_none() {
            assert!(std::time::Instant::now() < deadline, "cat did not exit");
            std::thread::yield_now();
        }
        let request = CanonicalProcessAwaitRequest {
            session_ids: vec![handle, handle],
            wake: None,
            yield_time_ms: 0,
            max_output_bytes: 64,
            max_output_records: 256,
        };
        let outcome =
            ProcessSessionTools::await_canonical(&environment, &request, &control).await?;
        let value: Value = serde_json::from_str(&outcome.joined_text())?;
        assert_eq!(value["tasks"][0]["status"], "exited");
        assert_eq!(value["tasks"].as_array().ok_or("tasks")?.len(), 1);
        assert_eq!(value["tasks"][0]["exit_code"], 0);
        plakous_tools::internal::consume_jobs(&sessions, &[u32::try_from(handle)?]);
        let outcome =
            ProcessSessionTools::await_canonical(&environment, &request, &control).await?;
        let value: Value = serde_json::from_str(&outcome.joined_text())?;
        assert_eq!(value["tasks"][0]["status"], "already_reported");
        let stopped = ProcessSessionTools::terminate_canonical(
            &environment,
            &CanonicalProcessTerminateRequest { session_id: handle },
        )
        .await?;
        assert_eq!(stopped.status, "exited");
        Ok(())
    }

    #[test]
    fn await_schemas_require_exactly_one_nonempty_id_form() -> Result<(), Box<dyn Error>> {
        for ids in [
            json!({}),
            json!({"session_ids":[]}),
            json!({"session_id":1,"session_ids":[1]}),
        ] {
            let mut args = ids;
            if args.get("session_ids") != Some(&json!([])) {
                let error = validate_canonical_poll_shape(&args).expect_err("exclusive id form");
                assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
            }
            args["yield_time_ms"] = json!(0);
            args["max_output_bytes"] = json!(64);
            args["max_output_records"] = json!(256);
            let single = serde_json::from_value::<CanonicalProcessPollRequest>(args.clone())
                .is_ok_and(|request| request.validate().is_ok());
            let multiple = serde_json::from_value::<CanonicalProcessAwaitRequest>(args.clone())
                .is_ok_and(|request| request.validate().is_ok());
            assert!(!single && !multiple, "{args}");
        }
        let grok =
            ProcessToolProjectionKind::GrokTerminal.tools(plakous_tools::JobConfig::default())?;
        for args in [
            json!({}),
            json!({"task_ids":[]}),
            json!({"task_ids":vec!["p1";129]}),
        ] {
            assert!(
                grok[1].schema().parse_instance(&args.to_string()).is_err(),
                "{args}"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one lifecycle test keeps start, close, poll, terminate, and stale-handle assertions causal"
    )]
    async fn canonical_process_operations_close_poll_terminate_and_reject_stale_authority()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000564",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let started = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from("printf ready; read ignored; printf eof"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: true,

                    _model_description: None,
                    yield_time_ms: 1_000,
                    max_output_bytes: 4_096,
                },
                control.clone(),
            )
            .await?;
        let handle = started
            .projection
            .session_id
            .ok_or("expected live handle")?;
        assert_eq!(started.projection.output, "ready");
        assert!(
            ProcessSessionTools::poll_canonical(
                &environment,
                &CanonicalProcessPollRequest {
                    session_id: handle,
                    cursor: Some(0),
                    read_so_far: false,
                    wait_for_exit: false,
                    yield_time_ms: 0,
                    max_output_bytes: 64,
                    max_output_records: 4,
                },
                &control,
            )
            .await
            .is_err()
        );
        ProcessSessionTools::write_canonical(
            &environment,
            &CanonicalProcessWriteRequest {
                session_id: handle,
                input: String::new(),
                disposition: CanonicalProcessInputDisposition::Close,
                yield_time_ms: None,
                max_output_bytes: None,
            },
            &control,
        )
        .await?;
        let session = ProcessSessionTools::session(&environment, handle)?;
        let mut changes = session.sessions.job_terminal_changes();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while session
                .sessions
                .joined_terminal_status(session.id)
                .is_none()
            {
                changes.changed().await?;
            }
            Ok::<_, tokio::sync::watch::error::RecvError>(())
        })
        .await??;
        let completed = ProcessSessionTools::poll_canonical(
            &environment,
            &CanonicalProcessPollRequest {
                session_id: handle,
                cursor: Some(started.cursor),
                read_so_far: false,
                wait_for_exit: false,
                yield_time_ms: 5_000,
                max_output_bytes: 4_096,
                max_output_records: 256,
            },
            &control,
        )
        .await?;
        assert_eq!(completed.projection.output, "eof");
        assert!(completed.joined);
        let retained = ProcessSessionTools::terminate_canonical(
            &environment,
            &CanonicalProcessTerminateRequest { session_id: handle },
        )
        .await?;
        assert_eq!(
            retained.projection.exit_code,
            completed.projection.exit_code
        );
        plakous_tools::internal::consume_jobs(
            &environment.joined_process_sessions()?,
            &[u32::try_from(handle)?],
        );
        assert_eq!(
            ProcessSessionTools::terminate_canonical(
                &environment,
                &CanonicalProcessTerminateRequest { session_id: handle },
            )
            .await?
            .projection
            .exit_code,
            completed.projection.exit_code
        );

        let running = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from("sleep 30"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 0,
                    max_output_bytes: 64,
                },
                control.clone(),
            )
            .await?;
        let running_handle = running
            .projection
            .session_id
            .ok_or("expected live handle")?;
        assert!(
            ProcessSessionTools::terminate_canonical(
                &environment,
                &CanonicalProcessTerminateRequest {
                    session_id: running_handle
                },
            )
            .await?
            .joined
        );
        assert!(
            ProcessSessionTools::write_canonical(
                &environment,
                &CanonicalProcessWriteRequest {
                    session_id: running_handle,
                    input: String::from("x"),
                    disposition: CanonicalProcessInputDisposition::KeepOpen,
                    yield_time_ms: None,
                    max_output_bytes: None,
                },
                &control,
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_process_tool_does_not_repeat_the_runtime_cleanup_attempt()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000565",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let tools = ProcessSessionTools::posix_sh()?;
        let started = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from("sleep 30"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 0,
                    max_output_bytes: 64,
                },
                ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
            )
            .await?;
        assert!(started.projection.session_id.is_some());
        assert!(!sessions.is_drained());

        drop(tools);
        assert!(
            !sessions.is_drained(),
            "executor Drop must not start a second cleanup attempt"
        );

        sessions.join_all()?;
        assert!(sessions.is_drained());
        Ok(())
    }

    #[test]
    fn canonical_process_registration_rejects_shell_identity_drift() -> Result<(), ToolRuntimeError>
    {
        let drifted = ["/usr/bin/sh", "/bin/bash"]
            .into_iter()
            .find(|path| Path::new(path).is_file() && *path != "/bin/sh")
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "non-canonical shell executable is unavailable",
                )
            })?;
        let shell = JoinedProcessShellProfile::new(drifted, [String::from("-c")])
            .map_err(backend_runtime_error)?;
        let login =
            JoinedProcessShellProfile::new(drifted, [String::from("-l"), String::from("-c")])
                .map_err(backend_runtime_error)?;
        let error = Arc::new(ProcessSessionTools::new(shell, login))
            .registrations()
            .err()
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "drifted shell unexpectedly registered",
                )
            })?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
        Ok(())
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "full exact provider schema goldens are intentionally reviewed in one causal table"
    )]
    fn exact_process_projection_schemas_are_v2_bound_goldens() -> Result<(), ToolRuntimeError> {
        let codex = ProcessToolProjectionKind::CodexUnifiedExec
            .tools(plakous_tools::JobConfig::default())?;
        assert_eq!(codex.len(), 2);
        assert_eq!(codex[0].name().as_str(), "exec_command");
        assert_eq!(codex[1].name().as_str(), "write_stdin");
        assert_eq!(
            codex[0].description(),
            text::bound_description(
                PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION,
                plakous_tools::JobConfig::default(),
                ProcessToolProjectionKind::CodexUnifiedExec,
                true,
            )
        );
        assert!(
            codex[0]
                .description()
                .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
        );
        assert_eq!(
            codex[1].description(),
            text::bound_description(
                PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION,
                plakous_tools::JobConfig::default(),
                ProcessToolProjectionKind::CodexUnifiedExec,
                false,
            )
        );
        assert!(
            codex[1]
                .description()
                .contains("Background jobs survive the turn")
        );
        assert_eq!(
            codex[0].schema().document().value().as_ref(),
            &codex_exec_command_schema(plakous_tools::JobConfig::default())
        );
        assert_eq!(
            codex[1].schema().document().value().as_ref(),
            &write_stdin_schema()
        );
        let exact_output = object_schema(
            json!({
                "exit_code": {"type":"integer"},
                "session_id": {"type":"integer","minimum":1},
                "status": {"type":"string","enum":["timed_out","terminated","cancelled","output_limit","failed","outcome_unknown","cleanup_unresolved"]},
                "output": {"type":"string"}
            }),
            &["output"],
        );
        assert_eq!(
            codex[0].output_schema().document().value().as_ref(),
            &exact_output
        );
        assert_eq!(
            codex[1].output_schema().document().value().as_ref(),
            &exact_output
        );
        assert_eq!(codex[0].schema().document().value()["required"], json!([]));
        assert_eq!(
            codex[0].schema().document().value()["properties"]["yield_time_ms"]["description"],
            format!(
                "Foreground observation hint: defaults to {} ms; clamped to 600000 ms; zero observes for 250 ms first.",
                plakous_tools::JobConfig::default().foreground_seconds() * 1000
            )
        );
        assert_eq!(
            codex[0].schema().document().value()["properties"]["yield_time_ms"]["type"],
            "number"
        );
        assert!(
            codex[0].schema().document().value()["properties"]
                .get("sandbox_permissions")
                .is_some()
        );
        assert_eq!(
            codex[1].schema().document().value()["required"],
            json!(["session_id", "chars"])
        );
        codex[1]
            .schema()
            .parse_instance(r#"{"session_id":1,"chars":"hello\n"}"#)
            .map_err(projection_arguments)?;
        assert!(
            serde_json::from_str::<CodexWriteArgs>(r#"{"session_id":1,"chars":"hello\n"}"#).is_ok()
        );
        assert_eq!(codex[0].output_schema(), codex[1].output_schema());
        assert_eq!(
            codex[0].output_schema().document().value()["required"],
            json!(["output"])
        );
        assert!(
            codex[0]
                .schema()
                .parse_instance(r#"{"cmd":"true"}"#)
                .is_ok()
        );
        assert!(
            codex[0]
                .schema()
                .parse_instance(r#"{"session_id":1}"#)
                .is_ok()
        );
        assert!(
            codex[0]
                .schema()
                .parse_instance(r#"{"cmd":"","session_id":1}"#)
                .is_ok()
        );
        assert!(codex[0].schema().parse_instance(r"{}").is_ok());
        assert!(
            codex[0]
                .schema()
                .parse_instance(
                    r#"{"cmd":"true","sandbox_permissions":"require_escalated","justification":"ok","prefix_rule":["git","pull"]}"#
                )
                .is_ok()
        );
        for invalid in [
            r#"{"cmd":"true","shell":"/bin/bash"}"#,
            r#"{"cmd":"true","unknown":1}"#,
        ] {
            assert!(
                codex[0].schema().parse_instance(invalid).is_err(),
                "{invalid}"
            );
        }

        let grok =
            ProcessToolProjectionKind::GrokTerminal.tools(plakous_tools::JobConfig::default())?;
        assert_eq!(
            grok.iter()
                .map(|tool| tool.name().as_str())
                .collect::<Vec<_>>(),
            [
                "run_terminal_command",
                "get_command_or_subagent_output",
                "kill_command_or_subagent"
            ]
        );
        assert_eq!(grok[0].name().as_str(), "run_terminal_command");
        assert_eq!(
            grok[0].description(),
            text::bound_description(
                PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
                plakous_tools::JobConfig::default(),
                ProcessToolProjectionKind::GrokTerminal,
                true,
            )
        );
        assert!(
            grok[0]
                .description()
                .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
        );
        assert!(
            grok[0].schema().document().value()["properties"]
                .get("background")
                .is_none()
        );
        assert!(
            grok[0].output_schema().document().value()["properties"]
                .get("task_id")
                .is_some()
        );
        assert!(
            grok[0].output_schema().document().value()["properties"]
                .get("session_id")
                .is_none()
        );
        assert_eq!(
            grok[0].schema().document().value()["required"],
            json!(["command", "description"])
        );
        assert!(
            grok[0]
                .schema()
                .parse_instance(r#"{"command":"true","description":"verify","block_until_ms":0}"#)
                .is_ok()
        );
        assert!(
            grok[1]
                .schema()
                .parse_instance(r#"{"task_ids":["p1","2"],"timeout_ms":3600000}"#)
                .is_ok()
        );
        assert!(
            grok[2]
                .schema()
                .parse_instance(r#"{"task_id":"p1"}"#)
                .is_ok()
        );
        assert!(
            grok[0]
                .schema()
                .parse_instance(r#"{"command":"true","description":"verify"}"#)
                .is_ok()
        );
        for invalid in [
            r#"{"command":"true"}"#,
            r#"{"command":"true","description":"verify","timeout":1.5}"#,
            r#"{"command":"true","description":"verify","task_id":1}"#,
            r#"{"command":"true","description":"verify","background":false}"#,
            r#"{"command":"true","description":"verify","background":true}"#,
        ] {
            assert!(
                grok[0].schema().parse_instance(invalid).is_err(),
                "{invalid}"
            );
        }

        let claude =
            ProcessToolProjectionKind::ClaudeOAuthBash.tools(plakous_tools::JobConfig::default())?;
        assert_eq!(claude[0].name().as_str(), "Bash");
        assert_eq!(
            claude[0].description(),
            text::bound_description(
                CLAUDE_OAUTH_BASH_DESCRIPTION,
                plakous_tools::JobConfig::default(),
                ProcessToolProjectionKind::ClaudeOAuthBash,
                true,
            )
        );
        assert!(
            claude[0]
                .description()
                .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
        );
        assert!(
            claude[0]
                .schema()
                .parse_instance(r#"{"command":"true"}"#)
                .is_ok()
        );
        assert!(
            claude[0]
                .schema()
                .parse_instance(r#"{"command":"true","run_in_background":true}"#)
                .is_ok()
        );
        let names = claude
            .iter()
            .map(|tool| tool.name().as_str())
            .collect::<Vec<_>>();
        // No output tool; background output is
        // reported on completion and a leaf waits through a longer timeout.
        assert_eq!(names, ["Bash", "TaskStop"]);
        let bash = claude[0].description();
        assert!(bash.contains("`timeout` is how long to block for the result"));
        assert!(bash.contains("Wait by giving a long command a timeout long enough to finish"));
        assert!(bash.contains("search with grep, rg or find"));
        assert!(!bash.contains("BashOutput"));
        assert!(!bash.contains("observation hint"));
        assert!(
            claude[1]
                .schema()
                .parse_instance(r#"{"task_id":"3"}"#)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn background_preempts_starts_not_existing_job_observations() {
        assert!(ObservationWait::PromotionHint.preempted(plakous_tools::ToolPreempt::Background));
        assert!(!ObservationWait::Hint.preempted(plakous_tools::ToolPreempt::Background));
        assert!(!ObservationWait::Output.preempted(plakous_tools::ToolPreempt::Background));
        assert!(ObservationWait::Hint.preempted(plakous_tools::ToolPreempt::Steer));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_hint_drains_more_than_one_record_batch_without_waiting()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000601",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal("/bin/sh", ["-c", "cat"].map(String::from))?,
            ".",
            [],
            true,
            JoinedProcessTerminalMode::Pipes,
        )?;
        let setup = sessions.clone();
        let start_control = control.clone();
        let id = tokio::task::spawn_blocking(
            move || -> Result<_, plakous_tools::ExecutionBackendError> {
                let id = setup.start(request, start_control)?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut cursor = JoinedProcessOutputCursor::new(0);
                // An acknowledgement per write guarantees 400 separate unread records.
                for _ in 0..400 {
                    setup.write_stdin(id, b"line\n", JoinedProcessInputDisposition::KeepOpen)?;
                    loop {
                        let poll = plakous_tools::internal::process_session_tail(&setup, id, 5, 1)?;
                        if poll.next_cursor() != cursor {
                            cursor = poll.next_cursor();
                            break;
                        }
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                }
                setup.write_stdin(id, b"", JoinedProcessInputDisposition::Close)?;
                while setup.joined_terminal_status(id).is_none() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
                assert!(
                    setup
                        .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
                        .more_output()
                );
                Ok(id)
            },
        )
        .await??;
        let before = std::time::Instant::now();
        let result = ProcessSessionTools::observe_bounded_with_update(
            &environment,
            u64::from(JoinedProcessSessions::job_number(id)),
            3000,
            4096,
            256,
            JoinedProcessOutputCursor::new(0),
            true,
            ObservationWait::Hint,
            &control,
        )
        .await?;
        assert!(
            before.elapsed() < std::time::Duration::from_secs(1),
            "pending terminal records waited for the hint deadline"
        );
        assert_eq!(result.projection.output, "line\n".repeat(400));
        assert!(result.joined);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn contended_observation_does_not_block_the_async_worker() -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000602",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal("/bin/sh", ["-c", "trap 'echo stopping > stopping; while :; do :; done' TERM; echo ready > ready; while :; do :; done"].map(String::from))?,
            ".", [], false, JoinedProcessTerminalMode::Pipes,
        )?;
        let setup = sessions.clone();
        let start_control = control.clone();
        let id = process_backend(move || setup.start(request, start_control)).await?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !workspace.path().join("ready").exists() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        plakous_tools::internal::promote(&sessions, id)?;
        let stopper = sessions.clone();
        let stop = tokio::task::spawn_blocking(move || {
            stopper.stop_with_grace(
                id,
                plakous_tools::JobStopReason::Model,
                std::time::Duration::from_millis(600),
            )
        });
        while !workspace.path().join("stopping").exists() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let (entered, receiver) = tokio::sync::oneshot::channel();
        let observation = tokio::spawn(async move {
            let _ = entered.send(());
            ProcessSessionTools::observe_bounded_with_update(
                &environment,
                u64::from(JoinedProcessSessions::job_number(id)),
                3000,
                4096,
                256,
                JoinedProcessOutputCursor::new(0),
                true,
                ObservationWait::Hint,
                &control,
            )
            .await
        });
        receiver.await?;
        let before = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let elapsed = before.elapsed();
        stop.await??;
        let observed = observation.await?;
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "a contended process poll blocked the only async worker: {elapsed:?}"
        );
        observed?;
        Ok(())
    }

    #[test]
    fn explicit_start_hints_are_not_rejected_before_clamping() -> Result<(), ToolRuntimeError> {
        for (kind, arguments) in [
            (
                ProcessToolProjectionKind::CodexUnifiedExec,
                r#"{"cmd":"true","yield_time_ms":900000}"#,
            ),
            (
                ProcessToolProjectionKind::PiBash,
                r#"{"command":"true","timeout":900}"#,
            ),
            (
                ProcessToolProjectionKind::ClaudeOAuthBash,
                r#"{"command":"true","timeout":900000}"#,
            ),
            (
                ProcessToolProjectionKind::GrokTerminal,
                r#"{"command":"true","description":"verify","block_until_ms":900000}"#,
            ),
        ] {
            assert!(
                kind.tools(plakous_tools::JobConfig::default())?[0]
                    .schema()
                    .parse_instance(arguments)
                    .is_ok(),
                "{kind:?} must accept hints above the observation ceiling"
            );
        }
        Ok(())
    }

    #[test]
    fn process_projection_result_omits_inapplicable_session_fields() -> Result<(), serde_json::Error>
    {
        assert_eq!(
            serde_json::to_value(ProcessProjectionResult {
                exit_code: Some(0),
                session_id: None,
                output: "done".to_owned(),
            })?,
            json!({"exit_code":0,"output":"done"})
        );
        assert_eq!(
            serde_json::to_value(ProcessProjectionResult {
                exit_code: None,
                session_id: Some(7),
                output: String::new(),
            })?,
            json!({"session_id":7,"output":""})
        );
        Ok(())
    }

    #[test]
    fn process_utf8_decoder_retains_split_scalars_and_replaces_only_invalid_bytes() {
        let mut pending = vec![b'a', 0xe2, 0x82];
        assert_eq!(drain_process_utf8_bounded(&mut pending, false, 8), "a");
        assert_eq!(pending, [0xe2, 0x82]);
        pending.extend_from_slice(&[0xac, 0xff, b'b']);
        assert_eq!(drain_process_utf8_bounded(&mut pending, false, 8), "€�b");
        assert_eq!(pending, [] as [u8; 0]);

        pending.extend_from_slice(&[0xf0, 0x9f]);
        assert_eq!(drain_process_utf8_bounded(&mut pending, true, 8), "�");
        assert_eq!(pending, [] as [u8; 0]);
    }

    #[test]
    fn process_utf8_decoder_makes_exact_progress_for_one_to_four_byte_scalars() {
        for (scalar, width) in [("a", 1_usize), ("¢", 2), ("€", 3), ("😀", 4)] {
            for bound in 1..width {
                let mut pending = scalar.as_bytes().to_vec();
                assert_eq!(drain_process_utf8_bounded(&mut pending, true, bound), "");
                assert_eq!(pending, scalar.as_bytes());
            }
            let mut pending = scalar.as_bytes().to_vec();
            assert_eq!(
                drain_process_utf8_bounded(&mut pending, true, width),
                scalar
            );
            assert_eq!(pending, [] as [u8; 0]);
        }
    }

    #[test]
    fn process_utf8_decoder_retains_invalid_terminal_tails_until_replacement_fits() {
        for invalid in [vec![0xff], vec![0xf0, 0x9f], vec![0xe2, 0x82]] {
            for bound in 1..char::REPLACEMENT_CHARACTER.len_utf8() {
                let mut pending = invalid.clone();
                assert_eq!(drain_process_utf8_bounded(&mut pending, true, bound), "");
                assert_eq!(pending, invalid);
            }
            let mut pending = invalid;
            assert_eq!(
                drain_process_utf8_bounded(
                    &mut pending,
                    true,
                    char::REPLACEMENT_CHARACTER.len_utf8(),
                ),
                "�"
            );
            assert_eq!(pending, [] as [u8; 0]);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_utf8_larger_than_observation_bound_returns_without_deadline_spin()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000568",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let started = std::time::Instant::now();
        // A one-byte page cannot hold a three-byte character: the exited
        // process is joined at once and the page counts the bytes it could not
        // show instead of waiting out the yield.
        let first = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from("printf '\\342\\202\\254'"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 5_000,
                    max_output_bytes: 1,
                },
                control,
            )
            .await?;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(first.joined);
        assert_eq!(
            first.projection.output,
            output_omitted_marker(3).trim_start()
        );
        assert!(environment.joined_process_sessions()?.is_drained());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_publishes_its_job_only_in_the_poll_that_completes_it()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };
        use std::future::Future as _;

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000572",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let mut start = Box::pin(tools.start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("exec sleep 30"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 250,
                max_output_bytes: 4_096,
            },
            control,
        ));
        // Cancellation drops a suspended start, and its guard then terminates
        // the process. A job published while the start is suspended would be
        // terminated after its ownership moved to the job table. The start is
        // re-polled only when woken, so the check sees any publication that
        // happened off this task before the start could observe it.
        let completed = std::future::poll_fn(|cx| {
            if sessions.has_jobs() {
                return std::task::Poll::Ready(None);
            }
            start.as_mut().poll(cx).map(Some)
        })
        .await;
        drop(start);
        let running = sessions.running_job_count();
        let started = completed.ok_or("a job was published while its start was suspended")??;
        let handle = started.projection.session_id.ok_or("expected a job")?;
        assert!(started.first_promotion);
        assert_eq!(running, 1);
        ProcessSessionTools::terminate_canonical(
            &environment,
            &CanonicalProcessTerminateRequest { session_id: handle },
        )
        .await?;
        Ok(())
    }

    /// Drives a start by hand into its reservation, which the test holds
    /// there through the observer's room check (run on the blocking pool
    /// under the reservation), then either drops the start (a cancelled
    /// start) or releases the reservation and polls the start to completion.
    /// Neither path depends on when the blocking pool runs.
    #[cfg(unix)]
    #[allow(
        clippy::too_many_lines,
        reason = "one hand-driven start keeps its setup, reservation and drop or re-poll assertions causal"
    )]
    fn start_to_its_reservation(repoll: bool) -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
            JobOutputRoot,
        };
        use std::future::Future as _;
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
        use std::task::{Context, Poll, Wake, Waker};
        use std::time::{Duration, Instant, SystemTime};

        #[derive(Default)]
        struct Progress {
            woken: bool,
            reserving: bool,
            released: bool,
        }
        #[derive(Default)]
        struct Signals(Mutex<Progress>, Condvar);
        impl Signals {
            fn progress(&self) -> MutexGuard<'_, Progress> {
                self.0.lock().unwrap_or_else(PoisonError::into_inner)
            }
            fn update(&self, change: impl FnOnce(&mut Progress)) {
                change(&mut self.progress());
                self.1.notify_all();
            }
        }
        impl Wake for Signals {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.update(|progress| progress.woken = true);
            }
        }
        const SESSION: &str = "00000000-0000-0000-0000-0000000000d0";
        const BRANCH: &str = "00000000-0000-0000-0000-0000000000d1";

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let _entered = runtime.enter();
        let workspace = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let root = JobOutputRoot::open(&state.path().join("jobs"))?;
        let session_dir = state.path().join("jobs").join(SESSION);
        std::fs::create_dir(&session_dir)?;
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))?;
        let reported = session_dir.join(format!("{BRANCH}-p77-0123456789abcdef.log"));
        std::fs::write(&reported, b"reported long ago")?;
        std::fs::File::open(&reported)?
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 3600))?;
        std::fs::set_permissions(&reported, std::fs::Permissions::from_mode(0o400))?;

        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000d02",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        sessions.bind_job_outputs(
            root,
            plakous_model::SessionId::from_str(SESSION)?,
            plakous_model::BranchId::from_str(BRANCH)?,
        );
        let signals = Arc::new(Signals::default());
        let hold = Arc::clone(&signals);
        // The first room check is the reservation's, before its sweep; it
        // waits until the test releases it. Later checks (the inline
        // promotion's) pass at once.
        sessions.observe_jobs(
            |_| true,
            move || {
                let mut progress = hold.progress();
                if !progress.reserving {
                    progress.reserving = true;
                    hold.1.notify_all();
                    while !progress.released {
                        progress = hold
                            .1
                            .wait(progress)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                }
                true
            },
        );
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let mut start = Box::pin(tools.start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("echo $$ > pid; exec sleep 30"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 250,
                max_output_bytes: 4_096,
            },
            control,
        ));
        let waker = Waker::from(Arc::clone(&signals));
        let mut cx = Context::from_waker(&waker);
        let deadline = Instant::now() + Duration::from_secs(20);
        let wait_for = |done: &dyn Fn(&Progress) -> bool| -> Result<(), Box<dyn Error>> {
            let mut progress = signals.progress();
            while !done(&progress) {
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or("the start stalled")?;
                progress = signals
                    .1
                    .wait_timeout(progress, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            Ok(())
        };
        // Until the reservation holds, the start cannot complete: a promotable
        // start always reserves first.
        loop {
            signals.progress().woken = false;
            if let Poll::Ready(result) = start.as_mut().poll(&mut cx) {
                return Err(format!(
                    "start completed without a reservation: ok={}",
                    result.is_ok()
                )
                .into());
            }
            wait_for(&|progress| progress.woken || progress.reserving)?;
            if signals.progress().reserving {
                break;
            }
        }
        let pid = std::fs::read_to_string(workspace.path().join("pid"))?
            .trim()
            .to_owned();
        if repoll {
            signals.update(|progress| progress.released = true);
            // Publication happens only in the poll that completes the start.
            let started = loop {
                assert!(
                    !sessions.has_jobs(),
                    "a job was published while its start was suspended"
                );
                signals.progress().woken = false;
                if let Poll::Ready(result) = start.as_mut().poll(&mut cx) {
                    break result?;
                }
                wait_for(&|progress| progress.woken)?;
            };
            assert!(started.first_promotion);
            assert_eq!(sessions.running_job_count(), 1);
            assert!(!reported.exists(), "the reservation did not sweep");
            let number = u32::try_from(started.projection.session_id.ok_or("expected a job")?)?;
            assert!(
                sessions.job_output_file(number).is_some(),
                "the published job has no file although its reservation fit"
            );
            drop(start);
            runtime.block_on(ProcessSessionTools::terminate_canonical(
                &environment,
                &CanonicalProcessTerminateRequest {
                    session_id: u64::from(number),
                },
            ))?;
            return Ok(());
        }
        // Cancellation drops the start while its reservation is held; the
        // reservation then finishes with no one to publish its job.
        drop(start);
        signals.update(|progress| progress.released = true);
        while std::process::Command::new("kill")
            .args(["-0", pid.as_str()])
            .stderr(std::process::Stdio::null())
            .status()?
            .success()
        {
            assert!(
                Instant::now() < deadline,
                "a start dropped in its reservation left its process {pid} running"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        while reported.exists() {
            assert!(
                Instant::now() < deadline,
                "the released reservation never swept"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!sessions.has_jobs() && sessions.jobs().is_empty());
        let leftovers = std::fs::read_dir(&session_dir)?.count();
        assert_eq!(leftovers, 0, "a dropped reservation left a job file behind");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_start_dropped_after_its_reservation_publishes_nothing_and_terminates()
    -> Result<(), Box<dyn Error>> {
        start_to_its_reservation(false)
    }

    #[cfg(unix)]
    #[test]
    fn a_start_repolled_after_its_reservation_publishes_inline_with_its_file()
    -> Result<(), Box<dyn Error>> {
        start_to_its_reservation(true)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_polled_session_is_summarised_to_its_newest_output() -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000571",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        // About 1.3 MB of output: more than one page, more than the session
        // window. One call shows the start and the end and counts the rest.
        let result = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from(
                        "seq 1 200000; echo 'FAILED: tail_only_marker'; exit 101",
                    ),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 20_000,
                    max_output_bytes: 4_000,
                },
                control,
            )
            .await?;
        let output = &result.projection.output;
        assert!(result.joined);
        assert_eq!(result.projection.exit_code, Some(101));
        assert!(output.len() < 4_100, "{}", output.len());
        assert!(output.contains(" bytes of output omitted ...]"));
        assert!(
            output.ends_with("200000\nFAILED: tail_only_marker\n"),
            "{output}"
        );
        assert!(environment.joined_process_sessions()?.is_drained());
        Ok(())
    }

    /// A finished session keeps its handle and stops counting as running.
    ///
    /// These are the two meanings of "active" that the retained set carries,
    /// and they sit next to each other here. Reading one as the other is what
    /// exhausted the concurrency limit on a sequential program, and later what
    /// fenced a turn at cleanup, so the distinction is pinned rather than left
    /// to the reader.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_finished_session_keeps_its_handle_and_frees_its_slot() -> Result<(), Box<dyn Error>>
    {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000571",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let tools = ProcessSessionTools::posix_sh()?;
        let started = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from("sleep 0.3; printf done"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 1,
                    max_output_bytes: 4096,
                },
                ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
            )
            .await?;
        assert!(started.projection.session_id.is_some());

        let sessions = environment.joined_process_sessions()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sessions
            .session_for_number(u32::try_from(
                started.projection.session_id.ok_or("handle")?,
            )?)
            .and_then(|id| sessions.joined_terminal_status(id))
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never reached a joined terminal status"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Retained: the handle is how the final output is still readable.
        let retained = sessions.jobs();
        assert_eq!(
            retained.len(),
            1,
            "a finished session keeps its handle so its output stays pollable"
        );
        // Not running: the slot it held is free again.
        assert_eq!(
            retained
                .iter()
                .filter(|job| job.status == plakous_tools::JobStatus::Running)
                .count(),
            0,
            "a child that has exited holds no concurrency"
        );

        sessions.join_all()?;
        plakous_tools::internal::consume_jobs(
            &sessions,
            &retained.iter().map(|job| job.number).collect::<Vec<_>>(),
        );
        assert_eq!(sessions.jobs(), [] as [plakous_tools::JobView; 0]);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_backend_retirement_prunes_stale_projection_handles()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000569",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let tools = ProcessSessionTools::posix_sh()?;
        let started = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from("sleep 30"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 1,
                    max_output_bytes: 1,
                },
                ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
            )
            .await?;
        assert!(started.projection.session_id.is_some());
        let sessions = environment.joined_process_sessions()?;
        sessions.join_all()?;
        plakous_tools::internal::consume_jobs(&sessions, &[1]);
        assert_eq!(sessions.jobs(), [] as [plakous_tools::JobView; 0]);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_output_does_not_turn_a_short_command_into_a_live_session()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000567",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let tools = ProcessSessionTools::posix_sh()?;
        let result = tools.exec_command_with_update(
            &environment,
            r#"{"cmd":"printf first; sleep 0.05; printf second","login":false,"yield_time_ms":5000}"#,
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        ).await?;
        assert_eq!(result.projection.output, "firstsecond");
        assert_eq!(result.projection.exit_code, Some(0));
        assert_eq!(result.projection.session_id, None);
        assert!(result.joined);
        assert!(sessions.is_drained());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exited_output_over_one_observation_remains_correlatable_until_joined()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        const OUTPUT_BYTES: usize = PROCESS_OBSERVATION_BYTES + 1_914;
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000568",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let first = tools.exec_command_with_update(
            &environment,
            &format!(
                r#"{{"cmd":"head -c {OUTPUT_BYTES} /dev/zero | tr '\\0' x","login":false,"yield_time_ms":5000}}"#
            ),
            control.clone(),
        ).await?;
        assert_eq!(first.projection.output.len(), PROCESS_OBSERVATION_BYTES);
        assert_eq!(first.projection.exit_code, None);
        assert_eq!(first.status, "running");
        assert!(!first.joined);
        let handle = first
            .projection
            .session_id
            .ok_or("bounded observation lost its continuation handle")?;
        let first_outcome = first.into_outcome()?;
        assert!(matches!(
            process_session_update(&first_outcome),
            Some(ProcessSessionUpdate::Promoted { .. })
        ));

        let mut final_observation = ProcessSessionTools::write_stdin_with_update(
            &environment,
            &format!(r#"{{"session_id":{handle},"chars":""}}"#),
            &control,
        )
        .await?;
        let mut final_bytes = final_observation.projection.output.len();
        for _ in 0..8 {
            if final_observation.joined {
                break;
            }
            final_observation = ProcessSessionTools::observe_with_update(
                &environment,
                handle,
                5_000,
                None,
                JoinedProcessOutputCursor::new(final_observation.cursor),
                true,
                &control,
            )
            .await?;
            final_bytes = final_bytes.saturating_add(final_observation.projection.output.len());
        }
        assert_eq!(final_bytes, OUTPUT_BYTES - PROCESS_OBSERVATION_BYTES);
        assert_eq!(final_observation.projection.session_id, None);
        assert_eq!(final_observation.status, "exited");
        assert!(final_observation.joined);
        let final_outcome = final_observation.into_outcome()?;
        assert!(matches!(
            process_session_update(&final_outcome),
            Some(ProcessSessionUpdate::JobTerminal { .. })
        ));
        assert!(sessions.is_drained());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn join_before_return_keeps_the_head_and_the_tail_of_a_long_log()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };
        const PAD: &str = "........................................";

        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000569",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let sessions = environment.joined_process_sessions()?;
        let tools = ProcessSessionTools::posix_sh()?;
        // About 200 KB of log: the command at the top, the failure at the end.
        let result = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: format!(
                        "echo 'running build'; i=0; while [ $i -lt 4000 ]; do \
                     echo \"compiling unit $i {PAD}\"; i=$((i+1)); done; \
                     echo 'error: test tail_only_failure failed'; exit 3"
                    ),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,

                    _model_description: None,
                    yield_time_ms: 20_000,
                    max_output_bytes: PROCESS_OBSERVATION_BYTES,
                },
                ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
            )
            .await?;
        let output = &result.projection.output;
        assert!(output.starts_with("running build\n"), "{output:.200}");
        assert!(output.ends_with("error: test tail_only_failure failed\n"));
        let marker_start = output.find("[... ").ok_or("omitted marker")?;
        let omitted: u64 = output[marker_start + 5..]
            .split(' ')
            .next()
            .ok_or("omitted count")?
            .parse()?;
        let marker = output_omitted_marker(omitted);
        let kept = output.len() - marker.trim_start_matches('\n').len();
        assert!(kept <= PROCESS_OBSERVATION_BYTES, "{kept}");
        // Every produced byte is either shown or counted as omitted.
        let produced = "running build\n".len()
            + (0..4_000)
                .map(|i| format!("compiling unit {i} {PAD}\n").len())
                .sum::<usize>()
            + "error: test tail_only_failure failed\n".len();
        assert_eq!(kept as u64 + omitted, produced as u64);
        assert_eq!(result.projection.exit_code, Some(3));
        assert_eq!(result.projection.session_id, None);
        assert!(result.joined);
        assert!(sessions.is_drained());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn codex_observations_accumulate_one_bound_and_resume_exactly()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        const TOTAL_BYTES: usize = 2 * PROCESS_OBSERVATION_BYTES + 17;
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000566",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?;
        let tools = ProcessSessionTools::posix_sh()?;
        let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
        let first = tools.exec_command_with_update(
            &environment,
            &format!(
                r#"{{"cmd":"stty -echo; head -c {TOTAL_BYTES} /dev/zero | tr '\\0' x; read ignored","login":false,"tty":true}}"#
            ),
            control.clone(),
        ).await?;
        assert_eq!(first.projection.output.len(), PROCESS_OBSERVATION_BYTES);
        assert_eq!(first.cursor, PROCESS_OBSERVATION_BYTES as u64);
        let handle = first
            .projection
            .session_id
            .ok_or("expected retained output")?;

        // Empty writes observe only currently buffered bytes. Wait for the producer
        // through a non-consuming backend cursor before asserting exact page sizes.
        let session = ProcessSessionTools::session(&environment, handle)?;
        let mut buffered_cursor = JoinedProcessOutputCursor::new(first.cursor);
        let buffering_started = std::time::Instant::now();
        while buffered_cursor.get() < TOTAL_BYTES as u64 {
            buffered_cursor = session
                .sessions
                .poll_from(session.id, buffered_cursor, TOTAL_BYTES, 256)?
                .next_cursor();
            assert!(
                buffering_started.elapsed() < std::time::Duration::from_secs(5),
                "producer output did not reach the retained buffer: {} / {TOTAL_BYTES}",
                buffered_cursor.get(),
            );
            std::thread::yield_now();
        }
        assert_eq!(buffered_cursor.get(), TOTAL_BYTES as u64);
        assert_eq!(lock_process_cursor(&session.cursor).get(), first.cursor);

        let second = ProcessSessionTools::write_stdin_with_update(
            &environment,
            &format!(r#"{{"session_id":{handle},"chars":""}}"#),
            &control,
        )
        .await?;
        assert_eq!(second.projection.output.len(), PROCESS_OBSERVATION_BYTES);
        assert_eq!(second.cursor, (2 * PROCESS_OBSERVATION_BYTES) as u64);
        assert_eq!(second.projection.session_id, Some(handle));

        let final_observation = ProcessSessionTools::write_stdin_with_update(
            &environment,
            &format!(r#"{{"session_id":{handle},"chars":""}}"#),
            &control,
        )
        .await?;
        assert_eq!(final_observation.projection.output, "x".repeat(17));
        assert_eq!(final_observation.cursor, TOTAL_BYTES as u64);
        assert!(!final_observation.joined);
        assert_eq!(final_observation.projection.session_id, Some(handle));
        let mut joined = ProcessSessionTools::write_stdin_with_update(
            &environment,
            &format!(r#"{{"session_id":{handle},"chars":"\n"}}"#),
            &control,
        )
        .await?;
        for _ in 0..8 {
            if joined.joined {
                break;
            }
            joined = ProcessSessionTools::observe_with_update(
                &environment,
                handle,
                0,
                None,
                JoinedProcessOutputCursor::new(joined.cursor),
                true,
                &control,
            )
            .await?;
            assert_eq!(joined.projection.output, "");
            assert_eq!(joined.cursor, TOTAL_BYTES as u64);
        }
        assert!(joined.joined);
        assert_eq!(joined.cursor, TOTAL_BYTES as u64);
        assert_eq!(joined.projection.output, "");
        assert_eq!(joined.projection.session_id, None);
        Ok(())
    }

    async fn execute_process_session_tool(
        registry: &FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        tool: &str,
        arguments: &str,
    ) -> Result<ToolOutcome, Box<dyn Error>> {
        let ids = (
            RunId::from_str("00000000-0000-0000-0000-000000000580")?,
            TurnId::from_str("00000000-0000-0000-0000-000000000581")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000582")?,
            CallId::from_str("00000000-0000-0000-0000-000000000583")?,
        );
        let invocation = registry.prepare(ToolInvocationRequest::new(
            ids.0,
            ids.1,
            ids.2,
            ids.3,
            ToolName::new(tool)?,
            ToolCorrelation::default(),
            arguments,
        ))?;
        execute_process_invocation(registry, environment, invocation).await
    }

    async fn execute_process_invocation(
        registry: &FrozenToolRegistry,
        environment: Arc<ExecutionEnvironment>,
        mut invocation: ToolInvocation,
    ) -> Result<ToolOutcome, Box<dyn Error>> {
        use plakous_model::{Clock, FixedClock, Timestamp};

        let ids = (
            invocation.run_id(),
            invocation.turn_id(),
            invocation.attempt_id(),
            invocation.call_id(),
        );
        if registry
            .definition(invocation.identity().name())
            .ok_or("process tool definition missing")?
            .effect()
            == ToolEffect::Mutating
        {
            let intent =
                Digest::sha256(DigestDomain::Snapshot, 1, &[b"process-session-test-intent"])?;
            let claim =
                Digest::sha256(DigestDomain::Snapshot, 1, &[b"process-session-test-claim"])?;
            invocation = bind_effect_dispatch(invocation, intent, claim);
        }
        let revision = ToolPolicyRevision::new("process-session-executor-test", 1)?;
        let binding =
            authorization_binding(&invocation, environment.descriptor().clone(), revision)?;
        let grant = authorization_grant(binding, EffectAuthorizationGrantSource::Policy);
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(80, 0)?));
        let (progress, _receiver) = ProgressSink::channel(8, ids.3, Arc::clone(&clock))?;
        let context = tool_context(
            ids.0,
            ids.1,
            ids.2,
            plakous_model::CancellationToken::new(),
            None,
            clock,
            environment,
            grant,
            progress,
        );
        let executor = executor(registry, &invocation)?;
        Ok(executor.execute(invocation, context).await?)
    }

    fn unified_exec_projection(
        registry: FrozenToolRegistry,
        kind: ProcessToolProjectionKind,
    ) -> Result<ToolProjectionMapV2, Box<dyn Error>> {
        let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"stdin-projection-test"],
        )?)?)?;
        Ok(kind.projection_map(
            plakous_tools::JobConfig::default(),
            registry,
            ProviderModelProjectionIdentityV2::new(
                IntegrationClass::Standard,
                "test.responses",
                "test.route",
                "test.profile.r1",
                PROCESS_PROJECTION_FAMILY,
                "test-model",
                "test-model",
                configuration,
                PROCESS_TOOL_PROJECTION_REVISION,
            )?,
        )?)
    }

    async fn execute_projected_process_tool(
        registry: &FrozenToolRegistry,
        map: &ToolProjectionMapV2,
        environment: Arc<ExecutionEnvironment>,
        tool: &str,
        arguments: Value,
    ) -> Result<ToolOutcome, Box<dyn Error>> {
        let prepared = map.prepare_in_environment(
            ToolInvocationRequest::new(
                RunId::from_str("00000000-0000-0000-0000-000000000580")?,
                TurnId::from_str("00000000-0000-0000-0000-000000000581")?,
                AttemptId::from_str("00000000-0000-0000-0000-000000000582")?,
                CallId::from_str("00000000-0000-0000-0000-000000000583")?,
                ToolName::new(tool)?,
                ToolCorrelation::default(),
                arguments.to_string(),
            ),
            &environment,
        )?;
        let (invocation, receipt) = prepared.into_parts();
        let outcome = execute_process_invocation(registry, environment, invocation).await?;
        Ok(map.raise(
            &receipt.ok_or("missing process projection receipt")?,
            outcome,
        )?)
    }

    /// On a node without agent tools Grok's output and kill tools refuse a
    /// task id that is not a process id before any effect, without claiming
    /// to know what it names.
    #[test]
    fn grok_process_tools_refuse_non_process_task_ids_as_policy() -> Result<(), Box<dyn Error>> {
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000621",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?;
        let map = unified_exec_projection(
            process_session_registry()?,
            ProcessToolProjectionKind::GrokTerminal,
        )?;
        for (tool, arguments) in [
            (
                "get_command_or_subagent_output",
                json!({"task_ids": ["p1", "c3"]}),
            ),
            ("kill_command_or_subagent", json!({"task_id": "c3"})),
        ] {
            let Err(error) = map.prepare_in_environment(
                ToolInvocationRequest::new(
                    RunId::from_str("00000000-0000-0000-0000-000000000622")?,
                    TurnId::from_str("00000000-0000-0000-0000-000000000623")?,
                    AttemptId::from_str("00000000-0000-0000-0000-000000000624")?,
                    CallId::from_str("00000000-0000-0000-0000-000000000625")?,
                    ToolName::new(tool)?,
                    ToolCorrelation::default(),
                    arguments.to_string(),
                ),
                &environment,
            ) else {
                return Err(format!("{tool} admitted an agent id").into());
            };
            assert_eq!(error.kind(), ToolRuntimeErrorKind::Policy, "{tool}");
            assert!(error.proven_no_effect(), "{tool}");
            assert_eq!(
                error.message().as_str(),
                "task id `c3` is not a process id; this agent has no agent tools"
            );
        }
        Ok(())
    }

    /// A stop issued by another layer through [`ProcessSessionTools::stop_job`]
    /// shows the model the same bytes the projected stop tool raises.
    #[test]
    fn stop_job_raises_the_projected_stop_bytes() -> Result<(), Box<dyn Error>> {
        let workspace = tempfile::tempdir()?;
        let environment = ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000611",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?;
        let registry = process_session_registry()?;
        let map = unified_exec_projection(registry, ProcessToolProjectionKind::GrokTerminal)?;
        let prepared = map.prepare_in_environment(
            ToolInvocationRequest::new(
                RunId::from_str("00000000-0000-0000-0000-000000000612")?,
                TurnId::from_str("00000000-0000-0000-0000-000000000613")?,
                AttemptId::from_str("00000000-0000-0000-0000-000000000614")?,
                CallId::from_str("00000000-0000-0000-0000-000000000615")?,
                ToolName::new("kill_command_or_subagent")?,
                ToolCorrelation::default(),
                json!({"task_id": "p1"}).to_string(),
            ),
            &environment,
        )?;
        let (_, receipt) = prepared.into_parts();
        let receipt = receipt.ok_or("missing stop receipt")?;
        for canonical in [
            json!({"status":"terminated","exit_code":null,"session_id":null,"cursor":4,"output":""}),
            json!({"status":"exited","exit_code":0,"session_id":null,"cursor":0,"output":""}),
            json!({"status":"exited","exit_code":3,"session_id":null,"cursor":9,"output":""}),
            json!({"status":"outcome_unknown","exit_code":null,"session_id":null,"cursor":0,"output":""}),
        ] {
            let projected =
                map.raise(&receipt, ToolOutcome::success_text(canonical.to_string())?)?;
            assert_eq!(
                raised_stop_result(&canonical.to_string())?,
                projected.joined_text(),
                "{canonical}"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn production_unified_exec_lowering_gives_non_tty_null_stdin()
    -> Result<(), Box<dyn Error>> {
        for kind in [ProcessToolProjectionKind::CodexUnifiedExec] {
            let workspace = tempfile::tempdir()?;
            let environment = Arc::new(ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                    "00000000-0000-0000-0000-000000000604",
                )?),
                workspace.path(),
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )?);
            let registry = process_session_registry()?;
            let map = unified_exec_projection(registry.clone(), kind)?;
            for tty in [None, Some(false)] {
                let mut arguments = json!({"cmd":"cat", "login":false, "yield_time_ms":1000});
                if let Some(tty) = tty {
                    arguments["tty"] = json!(tty);
                }
                let outcome = execute_projected_process_tool(
                    &registry,
                    &map,
                    environment.clone(),
                    "exec_command",
                    arguments,
                )
                .await?;
                assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
                let result: Value = serde_json::from_str(&outcome.joined_text())?;
                assert_eq!(
                    result["exit_code"], 0,
                    "non-tty cat must finish on EOF: {kind:?}"
                );
                assert!(result["session_id"].is_null());
                assert_eq!(result["output"], "");
            }
        }
        Ok(())
    }

    /// A process page cut at its output bound, of bytes the presentation
    /// escapes, raises on every process presentation inside the per-result
    /// model-visible bound.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_maximum_escaped_process_page_raises_on_every_process_presentation()
    -> Result<(), Box<dyn Error>> {
        let command = "yes '\"\\' | head -c 200000";
        for (kind, tool, arguments) in [
            (
                ProcessToolProjectionKind::PiBash,
                "bash",
                json!({"command": command}),
            ),
            (
                ProcessToolProjectionKind::CodexUnifiedExec,
                "exec_command",
                json!({"cmd": command, "login": false, "yield_time_ms": 10_000}),
            ),
            (
                ProcessToolProjectionKind::GrokTerminal,
                "run_terminal_command",
                json!({"command": command, "description": "print escaped bytes"}),
            ),
            (
                ProcessToolProjectionKind::ClaudeOAuthBash,
                "Bash",
                json!({"command": command}),
            ),
        ] {
            let workspace = tempfile::tempdir()?;
            let environment = Arc::new(ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                    "00000000-0000-0000-0000-000000000606",
                )?),
                workspace.path(),
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )?);
            let registry = process_session_registry()?;
            let map = unified_exec_projection(registry.clone(), kind)?;
            let outcome =
                execute_projected_process_tool(&registry, &map, environment, tool, arguments)
                    .await
                    .map_err(|error| format!("{kind:?}: {error:?}"))?;
            assert_eq!(outcome.kind(), ToolOutcomeKind::Success, "{kind:?}");
            let raised = outcome.joined_text();
            assert!(
                raised.len() <= SemanticJsonV1::maximum_string_bytes() - 4_096,
                "{kind:?}: {} bytes",
                raised.len()
            );
            let value: Value = serde_json::from_str(&raised)?;
            assert!(value.is_object(), "{kind:?}");
            assert!(raised.contains("\\\"\\\\"), "{kind:?}: output kept");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn production_unified_exec_closed_stdin_refusal_explains_tty_retry()
    -> Result<(), Box<dyn Error>> {
        for kind in [ProcessToolProjectionKind::CodexUnifiedExec] {
            let workspace = tempfile::tempdir()?;
            let environment = Arc::new(ExecutionEnvironment::host(
                plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                    "00000000-0000-0000-0000-000000000605",
                )?),
                workspace.path(),
                ".",
                plakous_tools::ExecutionEnvironmentConfig::default(),
            )?);
            let registry = process_session_registry()?;
            let map = unified_exec_projection(registry.clone(), kind)?;
            let started = execute_projected_process_tool(
                &registry,
                &map,
                environment.clone(),
                "exec_command",
                json!({"cmd":"exec sleep 60", "login":false, "tty":false, "yield_time_ms":0}),
            )
            .await?;
            assert_eq!(started.kind(), ToolOutcomeKind::Success);
            let handle = serde_json::from_str::<Value>(&started.joined_text())?["session_id"]
                .as_u64()
                .ok_or("missing live process handle")?;
            let canonical = execute_process_session_tool(
                &registry,
                environment.clone(),
                PROCESS_SESSION_WRITE_NAME,
                &json!({"session_id":handle, "input":"hello", "disposition":"keep_open"})
                    .to_string(),
            )
            .await?;
            assert_eq!(canonical.kind(), ToolOutcomeKind::Failure);
            assert_eq!(canonical.joined_text(), "joined process stdin is closed");
            for input in ["hello", "\u{3}not-an-interrupt"] {
                let refused = execute_projected_process_tool(
                    &registry,
                    &map,
                    environment.clone(),
                    "write_stdin",
                    json!({"session_id":handle, "chars":input}),
                )
                .await?;
                assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
                assert_eq!(
                    refused.joined_text(),
                    "stdin is closed for this session; rerun exec_command with tty=true to keep stdin open"
                );
            }
            let stopped = execute_projected_process_tool(
                &registry,
                &map,
                environment.clone(),
                "write_stdin",
                json!({"session_id":handle, "chars":"\u{3}"}),
            )
            .await?;
            assert_eq!(stopped.kind(), ToolOutcomeKind::Success);
            let polled = execute_projected_process_tool(
                &registry,
                &map,
                environment,
                "write_stdin",
                json!({"session_id":handle, "chars":"", "yield_time_ms":0}),
            )
            .await?;
            assert_eq!(polled.kind(), ToolOutcomeKind::Success);
        }
        Ok(())
    }

    /// Writing to a session that has already exited and been joined is a
    /// definite, model-visible tool failure.
    ///
    /// A live Codex turn sent Ctrl-C to a session whose child had exited. The
    /// generic conflict that came back could not be reconciled against the
    /// journal, so the turn ended in `OutcomeUnknown` and the agent went into
    /// recovery-required. The write reached nothing, so the outcome is
    /// knowable and the turn must be able to continue.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the exited-session lifecycle is one test: start, write, exit, refusal, continue"
    )]
    async fn write_to_an_exited_session_is_a_definite_failure_not_an_unknown_outcome()
    -> Result<(), Box<dyn Error>> {
        use plakous_model::ToolResultPart;
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000590",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let started = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            &serde_json::json!({
                "command": "read line; exit 7",
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": true,
                "yield_time_ms": 0,
                "max_output_bytes": 4096
            })
            .to_string(),
        )
        .await?;
        assert_eq!(started.kind(), ToolOutcomeKind::Success);
        let [ToolResultPart::Text(text)] = started.parts() else {
            return Err("process start did not return one text part".into());
        };
        let handle = serde_json::from_str::<Value>(text.text())?["session_id"]
            .as_u64()
            .ok_or("process start retained no session")?;

        let write = |input: &str| {
            serde_json::json!({
                "session_id": handle,
                "input": input,
                "disposition": "keep_open"
            })
            .to_string()
        };

        // A write to a still-running session is unchanged: it succeeds, and
        // here it is what makes the child exit.
        let delivered = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_WRITE_NAME,
            &write("\n"),
        )
        .await?;
        assert_eq!(delivered.kind(), ToolOutcomeKind::Success);

        let sessions = environment.joined_process_sessions()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let terminal = sessions
                .session_for_number(u32::try_from(handle)?)
                .and_then(|id| sessions.joined_terminal_status(id));
            if terminal == Some(JoinedProcessStatus::Exited(Some(7))) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child did not reach a joined terminal status"
            );
            tokio::task::yield_now().await;
        }

        // The session is terminal and joined but not retired, which is exactly
        // the live state. The write is refused definitely, and the executor
        // returns an outcome instead of an error the driver would normalize to
        // `OutcomeUnknown`.
        let refused = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_WRITE_NAME,
            &write("x"),
        )
        .await?;
        assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
        let [ToolResultPart::Text(refusal)] = refused.parts() else {
            return Err("refused write did not return one text part".into());
        };
        assert_eq!(
            refusal.text(),
            format!("process session {handle} already exited with status 7; nothing was written")
        );

        // The turn continues: the session is still pollable, and the poll
        // reports the exit the write was refused against.
        let polled = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_POLL_NAME,
            &serde_json::json!({
                "session_id": handle,
                "yield_time_ms": 0,
                "max_output_bytes": 4096,
                "max_output_records": 16
            })
            .to_string(),
        )
        .await?;
        assert_eq!(polled.kind(), ToolOutcomeKind::Success);
        let [ToolResultPart::Text(polled_text)] = polled.parts() else {
            return Err("poll did not return one text part".into());
        };
        assert_eq!(
            serde_json::from_str::<Value>(polled_text.text())?["exit_code"],
            serde_json::json!(7)
        );

        // The promoted snapshot stays readable, but writes still fail definitely.
        let stale = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_WRITE_NAME,
            &write("x"),
        )
        .await?;
        assert_eq!(stale.kind(), ToolOutcomeKind::Failure);
        assert!(sessions.is_drained());
        Ok(())
    }

    /// A refusal the process executor raises on its way in reaches the model as
    /// an ordinary tool failure, whatever the condition was.
    ///
    /// A live Code Mode cell ran one `cat` at a time and had its 67th start
    /// refused by the session limit. The refusal arrived as an error rather
    /// than an outcome, so the turn was escalated to `OutcomeUnknown` and the
    /// model's own JavaScript could not catch it. Nothing was spawned, so
    /// there is nothing to reconcile.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refusal_raised_before_any_spawn_reaches_the_model_as_a_tool_failure()
    -> Result<(), Box<dyn Error>> {
        use plakous_model::ToolResultPart;
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
            JoinedProcessCommand, MAX_JOINED_PROCESS_SESSIONS,
        };

        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000591",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let sessions = environment.joined_process_sessions()?;
        let shell = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|candidate| Path::new(candidate).is_file())
            .ok_or("test shell is unavailable")?;
        for _ in 0..MAX_JOINED_PROCESS_SESSIONS {
            sessions.start(
                JoinedProcessStartRequest::new(
                    JoinedProcessCommand::literal(
                        shell,
                        [String::from("-c"), String::from("sleep 30")],
                    )?,
                    ".",
                    [],
                    false,
                    JoinedProcessTerminalMode::Pipes,
                )?,
                ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
            )?;
        }

        let registry = process_session_registry()?;
        let refused = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            &serde_json::json!({
                "command": "true",
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": false,
                "yield_time_ms": 0,
                "max_output_bytes": 4096
            })
            .to_string(),
        )
        .await?;
        assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
        let [ToolResultPart::Text(text)] = refused.parts() else {
            return Err("the refusal did not return one text part".into());
        };
        assert!(
            text.text()
                .starts_with("running process limit (64) was reached:")
        );
        assert!(text.text().contains("p64"));
        sessions.join_all()?;
        Ok(())
    }

    /// A sequential program runs as many commands as it likes through the real
    /// executor, however many of them leave a page unread.
    ///
    /// This is the live Code Mode shape end to end: each command fills its
    /// output page, so the tool layer returns a retained `session_id` without
    /// joining, and the finished session keeps its handle. The backend stopped
    /// counting those against the running limit; the handle table has to agree,
    /// or the 65th command is spawned and killed instead of run.
    #[cfg(unix)]
    #[tokio::test]
    async fn sequential_page_filled_starts_never_exhaust_the_executor_handle_table()
    -> Result<(), Box<dyn Error>> {
        use plakous_model::ToolResultPart;
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
            MAX_JOINED_PROCESS_SESSIONS,
        };

        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000592",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        // A stdin handshake keeps each start running through its first observation.
        let arguments = serde_json::json!({
            "command": "printf '%0.sx' $(seq 1 256); read line",
            "workdir": ".",
            "login": false,
            "tty": false,
            "keep_stdin_open": true,
            "yield_time_ms": 20,
            "max_output_bytes": 16
        })
        .to_string();

        let mut retained = 0_usize;
        for index in 0..(MAX_JOINED_PROCESS_SESSIONS + 8) {
            let outcome = execute_process_session_tool(
                &registry,
                Arc::clone(&environment),
                PROCESS_SESSION_CAPABILITY_NAME,
                &arguments,
            )
            .await?;
            assert_eq!(
                outcome.kind(),
                ToolOutcomeKind::Success,
                "start {index} was refused"
            );
            let [ToolResultPart::Text(text)] = outcome.parts() else {
                return Err("process start did not return one text part".into());
            };
            let handle = serde_json::from_str::<Value>(text.text())?["session_id"]
                .as_u64()
                .ok_or("start did not promote its running child")?;
            retained = retained.saturating_add(1);
            let sessions = environment.joined_process_sessions()?;
            let id = sessions
                .session_for_number(u32::try_from(handle)?)
                .ok_or("job handle")?;
            sessions.write_stdin(id, b"finish\n", JoinedProcessInputDisposition::KeepOpen)?;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while sessions.joined_terminal_status(id).is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        }
        assert!(
            retained > MAX_JOINED_PROCESS_SESSIONS,
            "the shape under test needs more retained sessions than the limit, got {retained}"
        );
        environment.joined_process_sessions()?.join_all()?;
        Ok(())
    }

    /// This pack alone: the four canonical process operations, which is all
    /// these executor tests dispatch.
    fn process_session_registry() -> Result<FrozenToolRegistry, Box<dyn Error>> {
        let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
        register_packs(
            &[Arc::new(ProcessToolPack::new(
                Arc::new(ProcessSessionTools::posix_sh()?),
                plakous_tools::JobConfig::default(),
            )) as Arc<dyn ToolPack>],
            &mut builder,
        )?;
        Ok(builder.freeze())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_session_missing_cwd_is_correctable_before_pipes_or_pty_spawn()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        for tty in [false, true] {
            let workspace = tempfile::tempdir()?;
            let environment = Arc::new(ExecutionEnvironment::host(
                ExecutionEnvironmentScope::Run(RunId::from_str(
                    "00000000-0000-0000-0000-000000000589",
                )?),
                workspace.path(),
                ".",
                ExecutionEnvironmentConfig::default(),
            )?);
            let sessions = environment.joined_process_sessions()?;
            let registry = process_session_registry()?;
            let arguments = |workdir| {
                serde_json::json!({
                    "command": "printf executed > process-marker",
                    "workdir": workdir,
                    "login": false,
                    "tty": tty,
                    "keep_stdin_open": false,
                    "yield_time_ms": 1000,
                    "max_output_bytes": 4096,
                })
                .to_string()
            };
            let refused = execute_process_session_tool(
                &registry,
                Arc::clone(&environment),
                PROCESS_SESSION_CAPABILITY_NAME,
                &arguments("missing/workdir"),
            )
            .await?;
            assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
            assert!(sessions.is_drained());
            assert!(!workspace.path().join("process-marker").exists());
            let corrected = execute_process_session_tool(
                &registry,
                Arc::clone(&environment),
                PROCESS_SESSION_CAPABILITY_NAME,
                &arguments("."),
            )
            .await?;
            assert_eq!(corrected.kind(), ToolOutcomeKind::Success);
            assert!(sessions.is_drained());
            assert_eq!(
                std::fs::read(workspace.path().join("process-marker"))?,
                b"executed"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn process_session_executor_rejects_unknown_write_handle_as_model_failure()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000584",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let outcome = execute_process_session_tool(
            &registry,
            environment,
            PROCESS_SESSION_WRITE_NAME,
            r#"{"session_id":424242,"input":"x","disposition":"keep_open"}"#,
        )
        .await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert!(
            outcome.joined_text().contains("unknown or stale"),
            "unexpected failure text: {}",
            outcome.joined_text()
        );
        Ok(())
    }

    #[tokio::test]
    async fn process_session_executor_rejects_malformed_start_arguments_as_model_failure()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000585",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let outcome = execute_process_session_tool(
            &registry,
            environment,
            PROCESS_SESSION_CAPABILITY_NAME,
            r#"{"command":"","workdir":".","login":false,"tty":false,"keep_stdin_open":false,"yield_time_ms":1000,"max_output_bytes":4096}"#,
        )
        .await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert!(
            outcome
                .joined_text()
                .contains("canonical process start bounds are invalid"),
            "unexpected failure text: {}",
            outcome.joined_text()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_hint_promotes_instead_of_killing() -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000586",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let outcome = execute_process_session_tool(&registry, Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            r#"{"command":"sleep 60","workdir":".","login":false,"tty":false,"keep_stdin_open":false,"yield_time_ms":250,"max_output_bytes":4096}"#,
        ).await?;
        let result: Value = serde_json::from_str(&outcome.joined_text())?;
        environment.joined_process_sessions()?.join_all()?;
        assert_eq!(result["status"], "running");
        assert_eq!(result["session_id"], 1);
        assert_eq!(
            plakous_tools::internal::process_telemetry(&outcome),
            Some(ProcessTelemetry::Promoted {
                operation: ProcessOperationKind::Start,
                hint_source: ProcessHintSource::Default,
                hint_ms: 250,
                preempt: plakous_tools::ToolPreempt::None
            })
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_session_executor_rejects_stale_poll_cursor_as_model_failure()
    -> Result<(), Box<dyn Error>> {
        use plakous_tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        };

        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000586",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let started = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            r#"{"command":"printf ready; read ignored; printf eof","workdir":".","login":false,"tty":false,"keep_stdin_open":true,"yield_time_ms":1000,"max_output_bytes":4096}"#,
        )
        .await?;
        assert_eq!(started.kind(), ToolOutcomeKind::Success);
        let started_json: Value = serde_json::from_str(&started.joined_text())?;
        let handle = started_json["session_id"]
            .as_u64()
            .ok_or("expected live process handle")?;
        let outcome = execute_process_session_tool(
            &registry,
            environment,
            PROCESS_SESSION_POLL_NAME,
            &format!(
                r#"{{"session_id":{handle},"cursor":0,"yield_time_ms":0,"max_output_bytes":64,"max_output_records":4}}"#
            ),
        )
        .await?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert!(
            outcome.joined_text().contains("stale or foreign"),
            "unexpected failure text: {}",
            outcome.joined_text()
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The process pack.
// ---------------------------------------------------------------------------

/// The one exclusive-resource executor: joined process sessions, their four
/// canonical operations, and the per-vocabulary shell tool that drives them.
#[derive(Clone, Default)]
pub struct ProcessToolPack {
    job_config: plakous_tools::JobConfig,
    process: Option<Arc<ProcessSessionTools>>,
}

impl fmt::Debug for ProcessToolPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessToolPack")
            .field("job_config", &self.job_config)
            .field("registers", &self.process.is_some())
            .finish()
    }
}

impl ProcessToolPack {
    /// The pack over one process service. The service must satisfy the fixed
    /// registration shell profile, so durable executor identity cannot change
    /// with application shell selection.
    #[must_use]
    pub fn new(process: Arc<ProcessSessionTools>, job_config: plakous_tools::JobConfig) -> Self {
        Self {
            job_config,
            process: Some(process),
        }
    }

    /// The pack as a menu over already registered process operations.
    #[must_use]
    pub fn menu_only(job_config: plakous_tools::JobConfig) -> Self {
        Self {
            job_config,
            process: None,
        }
    }

    fn kind(presentation: CodingPresentation) -> ProcessToolProjectionKind {
        match presentation {
            CodingPresentation::Pi => ProcessToolProjectionKind::PiBash,
            CodingPresentation::Codex => ProcessToolProjectionKind::CodexUnifiedExec,
            CodingPresentation::Grok => ProcessToolProjectionKind::GrokTerminal,
            CodingPresentation::Claude => ProcessToolProjectionKind::ClaudeOAuthBash,
        }
    }
}

impl ToolPack for ProcessToolPack {
    fn id(&self) -> &'static str {
        "plakous.process.v3"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        match self.process.as_ref() {
            Some(process) => Arc::clone(process).registrations(),
            None => Ok(Vec::new()),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one reviewed menu table per presentation"
    )]
    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let kind = Self::kind(presentation);
        let start = tool_name(
            PROCESS_SESSION_CAPABILITY_NAME,
            "canonical process operation name is invalid",
        )?;
        let write = tool_name(
            PROCESS_SESSION_WRITE_NAME,
            "canonical process operation name is invalid",
        )?;
        let poll = tool_name(
            PROCESS_SESSION_POLL_NAME,
            "canonical process operation name is invalid",
        )?;
        let terminate = tool_name(
            PROCESS_SESSION_TERMINATE_NAME,
            "canonical process operation name is invalid",
        )?;
        kind.tools(self.job_config)?
            .into_iter()
            .map(|tool| {
                let (lowering, defaults, bindings) = match (kind, tool.name().as_str()) {
                    (ProcessToolProjectionKind::PiBash, "bash") => (
                        ToolProjectionLoweringV2::PiBash,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Start,
                            start.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::PiBash, "wait") => (
                        ToolProjectionLoweringV2::PiWait,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Poll,
                            poll.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::PiBash, "stop") => (
                        ToolProjectionLoweringV2::PiStop,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Terminate,
                            terminate.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::CodexUnifiedExec, "exec_command") => (
                        ToolProjectionLoweringV2::CodexExecCommand,
                        exec_command_defaults(),
                        vec![
                            MenuBinding::exact(CanonicalProcessOperationV2::Start, start.clone()),
                            MenuBinding::exact(CanonicalProcessOperationV2::Poll, poll.clone()),
                        ],
                    ),
                    (ProcessToolProjectionKind::CodexUnifiedExec, "write_stdin") => (
                        ToolProjectionLoweringV2::CodexWriteStdin,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Write,
                            write.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::GrokTerminal, "run_terminal_command") => (
                        ToolProjectionLoweringV2::GrokRunTerminalCommand,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Start,
                            start.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::GrokTerminal, "get_command_or_subagent_output") => {
                        (
                            ToolProjectionLoweringV2::GrokGetCommandOutput,
                            json!({}),
                            vec![MenuBinding::exact(
                                CanonicalProcessOperationV2::Poll,
                                poll.clone(),
                            )],
                        )
                    }
                    (ProcessToolProjectionKind::GrokTerminal, "kill_command_or_subagent") => (
                        ToolProjectionLoweringV2::GrokKillCommand,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Terminate,
                            terminate.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::ClaudeOAuthBash, "Bash") => (
                        ToolProjectionLoweringV2::ClaudeBash,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Start,
                            start.clone(),
                        )],
                    ),
                    (ProcessToolProjectionKind::ClaudeOAuthBash, "TaskStop") => (
                        ToolProjectionLoweringV2::ClaudeTaskStop,
                        json!({}),
                        vec![MenuBinding::exact(
                            CanonicalProcessOperationV2::Terminate,
                            terminate.clone(),
                        )],
                    ),
                    _ => {
                        return Err(ToolRuntimeError::new(
                            ToolRuntimeErrorKind::InvalidDefinition,
                            "process projection catalog contains an unknown candidate mapping",
                        ));
                    }
                };
                let entry = MenuEntry::new(
                    tool.name().as_str(),
                    bindings,
                    MenuShape::Structured {
                        description: Arc::from(tool.description()),
                        schema: tool.schema().document().value().as_ref().clone(),
                    },
                    lowering,
                )
                .with_defaults(defaults)
                .with_output(MenuOutput::Explicit(tool.output_schema().clone()))
                .with_raising(
                    if matches!(
                        lowering,
                        ToolProjectionLoweringV2::GrokGetCommandOutput
                            | ToolProjectionLoweringV2::PiWait
                    ) {
                        ToolProjectionRaisingV2::Identity
                    } else {
                        ToolProjectionRaisingV2::ProcessResult
                    },
                );
                Ok(match lowering {
                    // Pi's jobs await yields `wait` to a runtime marker of
                    // that spelling (spec 01 §8.2); on a node granted agent
                    // tools the tree's await claims it.
                    ToolProjectionLoweringV2::PiWait => {
                        entry.with_alternate("wait_jobs").claimable()
                    }
                    // On a node granted agent tools the tree presents one
                    // capability under these names that also covers children.
                    ToolProjectionLoweringV2::GrokGetCommandOutput
                    | ToolProjectionLoweringV2::GrokKillCommand
                    | ToolProjectionLoweringV2::PiStop => entry.claimable(),
                    _ => entry,
                })
            })
            .collect()
    }

    fn verify(
        &self,
        _presentation: CodingPresentation,
        registry: &FrozenToolRegistry,
    ) -> Result<(), ToolRuntimeError> {
        canonical_process_identities(registry).map(|_| ())
    }

    fn pins(&self) -> &'static [PromptPin] {
        PROCESS_PROMPT_PINS
    }
}

fn exec_command_defaults() -> Value {
    json!({
        "login": true,
        "tty": false,
        "yield_time_ms": 10000,
        "max_output_tokens": CODEX_PROCESS_OUTPUT_TOKENS,
    })
}

/// Every model-visible string the process pack owns.
static PROCESS_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "process.write_stdin",
        PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION,
        "c4007b923f93e4782f5944c35ba83d6ba4ea94cc0c4008eafa47960cad046ea0",
    ),
    PromptPin::new(
        "process.codex.exec_command",
        PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION,
        "5efcc3e22456824bf970f837574f5f028886f80cef47829ca7d4c43ada7e2d6a",
    ),
    PromptPin::new(
        "process.grok.run_terminal_command",
        PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
        "25478da99f5bb142b2881e10b0d884f11b9e3ceb0c8d43487f7cef5b7d20eb61",
    ),
    PromptPin::new(
        "process.grok.get_command_or_subagent_output",
        text::GROK_GET_COMMAND_OUTPUT_DESCRIPTION,
        "b9bdda67a81e323f5802bdd71ca92625096397875b736ddd9dfccabf3d6ee797",
    ),
    PromptPin::new(
        "process.grok.kill_command_or_subagent",
        text::GROK_KILL_COMMAND_DESCRIPTION,
        "b3ae525ec62fbee3ef81d392ba731fe8f7ea02c07df215b97f5d5ec8d4bf04ee",
    ),
    PromptPin::new(
        "process.claude.Bash",
        CLAUDE_OAUTH_BASH_DESCRIPTION,
        "584a3d1517ba6a2de662b6bda30f67c2ddf5013ed0853ba51c1d34f0a22734cc",
    ),
    PromptPin::new(
        "process.claude.TaskStop",
        CLAUDE_OAUTH_TASK_STOP_DESCRIPTION,
        "aa564af83cfcf59258fd9c31c880c5ab0465001936ecfb76e4ec6824f71bae7c",
    ),
    PromptPin::new(
        "process.pi.bash",
        text::PI_BASH_DESCRIPTION,
        "72dc678d18d5ff31954098fdf97c52e9d6c21391b0ca288b572f40fd4bcab412",
    ),
    PromptPin::new(
        "process.pi.wait",
        text::PI_WAIT_DESCRIPTION,
        "bae759aa508afaeaff7ff509190bf5178349696d8390632981764e7cb5341aa4",
    ),
    PromptPin::new(
        "process.pi.stop",
        text::PI_STOP_DESCRIPTION,
        "cebdffd07c92e631ff8ff99e707b08d551f5d97b75fe857cc108d4357f99d3ea",
    ),
    PromptPin::new(
        "process.output_omitted",
        PLAKOUS_TOOL_OUTPUT_OMITTED,
        "4502f1aecbd8c0fc4e61965eb6fcd92c8a5ec1dfaab93fde06bc5cb9236dec41",
    ),
];

/// The four canonical joined-process operations, checked against their exact
/// durable identities.
///
/// # Errors
///
/// Rejects an absent operation and every tool-id, executor, or revision drift.
pub(crate) fn canonical_process_identities(
    registry: &FrozenToolRegistry,
) -> Result<Vec<(CanonicalProcessOperationV2, RegisteredToolIdentity)>, ToolRuntimeError> {
    let specifications = [
        (
            CanonicalProcessOperationV2::Start,
            PROCESS_SESSION_CAPABILITY_NAME,
            PROCESS_SESSION_REGISTERED_TOOL_ID,
        ),
        (
            CanonicalProcessOperationV2::Write,
            PROCESS_SESSION_WRITE_NAME,
            PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
        ),
        (
            CanonicalProcessOperationV2::Poll,
            PROCESS_SESSION_POLL_NAME,
            PROCESS_SESSION_POLL_REGISTERED_TOOL_ID,
        ),
        (
            CanonicalProcessOperationV2::Terminate,
            PROCESS_SESSION_TERMINATE_NAME,
            PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
        ),
    ];
    specifications
        .into_iter()
        .map(|(operation, name, tool_id)| {
            let name = ToolName::new(name.to_owned()).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "canonical process operation name is invalid",
                    &error,
                )
            })?;
            let identity = registry.identity(&name).cloned().ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "canonical process operation is not registered",
                )
            })?;
            if identity.tool_id().as_str() != tool_id
                || identity.executor_id().as_str() != PROCESS_SESSION_EXECUTOR_ID
                || identity.semantic_version() != PROCESS_SESSION_CAPABILITY_REVISION
            {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "canonical process operation identity is not enrolled",
                ));
            }
            Ok((operation, identity))
        })
        .collect::<Result<Vec<_>, _>>()
}
