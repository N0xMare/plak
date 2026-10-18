//! The process pack.

use super::{
    Arc, CLAUDE_OAUTH_BASH_DESCRIPTION, CLAUDE_OAUTH_TASK_STOP_DESCRIPTION,
    CODEX_PROCESS_OUTPUT_TOKENS, CanonicalProcessOperationV2, CodingPresentation,
    FrozenToolRegistry, MenuBinding, MenuEntry, MenuOutput, MenuShape,
    PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION, PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
    PLAKOUS_TOOL_OUTPUT_OMITTED, PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION,
    PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_CAPABILITY_REVISION,
    PROCESS_SESSION_EXECUTOR_ID, PROCESS_SESSION_POLL_NAME,
    PROCESS_SESSION_POLL_REGISTERED_TOOL_ID, PROCESS_SESSION_REGISTERED_TOOL_ID,
    PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
    PROCESS_SESSION_WRITE_NAME, PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID, ProcessSessionTools,
    ProcessToolProjectionKind, PromptPin, RegisteredToolIdentity, ToolName, ToolPack,
    ToolProjectionLoweringV2, ToolProjectionRaisingV2, ToolRegistration, ToolRuntimeError,
    ToolRuntimeErrorKind, Value, fmt, json, text, tool_name,
};

/// The one exclusive-resource executor: joined process sessions, their four
/// canonical operations, and the per-vocabulary shell tool that drives them.
#[derive(Clone, Default)]
pub struct ProcessToolPack {
    pub(crate) job_config: plakous_tools::JobConfig,
    pub(crate) process: Option<Arc<ProcessSessionTools>>,
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

    pub(crate) fn kind(presentation: CodingPresentation) -> ProcessToolProjectionKind {
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
                let mut entry = MenuEntry::new(
                    tool.name().as_str(),
                    bindings,
                    MenuShape::Structured {
                        description: Arc::from(tool.description()),
                        schema: tool.schema().document().value().as_ref().clone(),
                    },
                    lowering,
                );
                if let Some(tail) = text::lifetime_tail(tool.description()) {
                    entry = entry.with_lifetime_tail(tail);
                }
                let entry = entry
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
