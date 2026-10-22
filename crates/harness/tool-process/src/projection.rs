//! Provider projections of the process tools: schemas, descriptions and the projected shell tool.

use super::{
    Arc, CanonicalProcessOperationV2, Digest, DigestDomain, FrozenToolRegistry,
    PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION, PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
    PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION, PROCESS_TOOL_PROJECTION_REVISION, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, RegisteredToolIdentity, SemanticJsonV1, Serialize, ToolName,
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap, ToolProjectionRaisingV2,
    ToolRuntimeError, ToolRuntimeErrorKind, Value, canonical_process_identities, grok, json,
    object_schema, pi, projection_exposure, projection_map, text,
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
pub(crate) const CLAUDE_OAUTH_BASH_DESCRIPTION: &str = "Executes a given command through a one-shot `/bin/sh -c` in the session cwd (`.`) and returns its output; cwd and environment do not persist. Use Read for file contents and Edit and Write, when presented, for file changes; search with grep, rg or find here. `timeout` is how long to block for the result, in ms, not a kill timer; it is clamped to 600000 ms and defaults to the construction-bound foreground budget (default 30000 ms). `run_in_background: true` returns after a 250 ms first observation. When a command moves to the background with an output file, its result and its completion notice carry a `full output: <path>` line; read that file with Read. Output over 30000 bytes keeps its beginning and end with the omitted byte count. `&` children survive shell exit as a job; redirect a service's output to a file. A still-running command continues in the background with its session id after the foreground budget; completion is reported automatically. Background jobs survive the turn and stop only on explicit stop, session release, attempt end or the bound hard runtime cap.";

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

pub(crate) fn canonical_process_input_schema(
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
    ) -> Result<ToolProjectionMap, ToolRuntimeError> {
        let canonical = canonical_process_identities(&registry)?;
        let entries = self
            .tools(job_config)?
            .into_iter()
            .map(|tool| projection_entry(self, canonical.clone(), &target, &tool))
            .collect::<Result<Vec<_>, _>>()?;
        projection_map(registry, target, entries)
    }
}

pub(crate) fn write_stdin_schema() -> Value {
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

pub(crate) fn codex_exec_command_schema(job_config: plakous_tools::JobConfig) -> Value {
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
    pub(crate) name: ToolName,
    pub(crate) description: Arc<str>,
    pub(crate) schema: PortableToolSchemaV2,
    pub(crate) output_schema: PortableToolSchemaV2,
    pub(crate) identity: Digest,
}

impl ProjectedProcessTool {
    pub(crate) fn new(
        name: &str,
        description: &str,
        schema: Value,
    ) -> Result<Self, ToolRuntimeError> {
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
                "status": {"type":"string","enum":["leftover","timed_out","terminated","cancelled","output_limit","failed","outcome_unknown","cleanup_unresolved"]},
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
                        "status":{"type":"string","enum":["running","leftover","exited","terminated","cancelled","timed_out","output_limit","failed","outcome_unknown","cleanup_unresolved","already_reported","not_found"]},
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
