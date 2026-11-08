//! Derived tool semantics; never part of a stored transcript encoding.

use super::presentation::{ActivityKind, McpCall};

/// Semantic operation derived from the canonical tool name and host activity kind.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolOperation {
    /// The run operation.
    Run,
    /// The wait process operation.
    WaitProcess,
    /// The read process operation.
    ReadProcess,
    /// The input process operation.
    InputProcess,
    /// The interrupt process operation.
    InterruptProcess,
    /// The stop process operation.
    StopProcess,
    /// The read operation.
    Read,
    /// The search operation.
    Search,
    /// The edit operation.
    Edit,
    /// The write operation.
    Write,
    /// The fetch operation.
    Fetch,
    /// The web search operation.
    WebSearch,
    /// The xsearch operation.
    XSearch,
    /// The generate image operation.
    GenerateImage,
    /// The mcp operation.
    Mcp,
    /// The list operation.
    List,
    /// The plan operation.
    Plan,
    /// The compose operation.
    Compose,
    /// The agent operation.
    Agent,
    /// The spawn agent operation.
    SpawnAgent,
    /// The wait agent operation.
    WaitAgent,
    /// The list agents operation.
    ListAgents,
    /// The message agent operation.
    MessageAgent,
    /// The adopt operation.
    Adopt,
    /// The close agent operation.
    CloseAgent,
    /// The interrupt agent operation.
    InterruptAgent,
    /// The recover agent operation.
    RecoverAgent,
    /// The discard workspace operation.
    DiscardWorkspace,
    /// The stop all operation.
    StopAll,
    /// The submit result operation.
    SubmitResult,
    /// The compact operation.
    Compact,
    /// The generic operation.
    Generic,
}

impl ToolOperation {
    /// Classify canonical tool facts, preferring structured/normalized input
    /// detail over legacy host-authored process subjects.
    #[must_use]
    pub fn classify(
        name: &str,
        kind: ActivityKind,
        subject: Option<&str>,
        detail: Option<&str>,
    ) -> Self {
        let operation = Self::classify_name(name, kind);
        if matches!(operation, Self::Compose | Self::Agent)
            || !matches!(kind, ActivityKind::Tool | ActivityKind::Process)
        {
            return operation;
        }
        if let Some(operation) = Self::process_detail(name, detail) {
            return operation;
        }
        if (operation == Self::Run && name != "exec_command")
            || !matches!(
                operation,
                Self::Run
                    | Self::WaitProcess
                    | Self::ReadProcess
                    | Self::InputProcess
                    | Self::InterruptProcess
                    | Self::StopProcess
            )
        {
            return operation;
        }
        Self::process_subject(subject.unwrap_or_default()).unwrap_or(operation)
    }

    fn process_detail(name: &str, detail: Option<&str>) -> Option<Self> {
        match name {
            "process_session_read" => return Some(Self::ReadProcess),
            "process_session_interrupt" => return Some(Self::InterruptProcess),
            "process_session_write"
            | "write_stdin"
            | "process_session_poll"
            | "BashOutput"
            | "get_command_or_subagent_output"
            | "exec_command" => {}
            _ => return None,
        }
        let detail = detail?;
        if matches!(name, "process_session_write" | "write_stdin")
            && matches!(detail.lines().next(), Some("Ctrl-C" | "\u{3}"))
        {
            return Some(Self::InterruptProcess);
        }
        // Detail can append a result after the argument object. Decode just
        // the first value, never infer input from result text or a substring.
        let arguments = serde_json::Deserializer::from_str(detail)
            .into_iter::<serde_json::Value>()
            .next()?
            .ok()?;
        let reading = || {
            arguments.get("block").and_then(serde_json::Value::as_bool) == Some(false)
                || arguments
                    .get("yield_time_ms")
                    .or_else(|| arguments.get("timeout_ms"))
                    .and_then(serde_json::Value::as_u64)
                    == Some(0)
        };
        match name {
            "process_session_write" | "write_stdin" => {
                let key = if name == "write_stdin" {
                    "chars"
                } else {
                    "input"
                };
                let other = if key == "chars" { "input" } else { "chars" };
                let input = arguments
                    .get(key)
                    .or_else(|| arguments.get(other))?
                    .as_str()?;
                Some(if input == "\u{3}" {
                    Self::InterruptProcess
                } else if input.is_empty() {
                    Self::WaitProcess
                } else {
                    Self::InputProcess
                })
            }
            "process_session_poll" | "BashOutput" | "get_command_or_subagent_output" => {
                let default_read = name != "BashOutput"
                    && arguments.get("yield_time_ms").is_none()
                    && arguments.get("timeout_ms").is_none();
                Some(if reading() || default_read {
                    Self::ReadProcess
                } else {
                    Self::WaitProcess
                })
            }
            "exec_command"
                if arguments.get("session_id").is_some()
                    && arguments
                        .get("cmd")
                        .or_else(|| arguments.get("command"))
                        .is_none() =>
            {
                Some(if reading() {
                    Self::ReadProcess
                } else {
                    Self::WaitProcess
                })
            }
            _ => None,
        }
    }

    /// The semantic operation carried by a host-authored process subject.
    #[must_use]
    pub fn process_subject(subject: &str) -> Option<Self> {
        match subject {
            value
                if value.starts_with("Waiting on process ")
                    || value.starts_with("Waited on process ") =>
            {
                Some(Self::WaitProcess)
            }
            value if value.starts_with("Read output of process ") => Some(Self::ReadProcess),
            value if value.starts_with("Sent input to process ") => Some(Self::InputProcess),
            value if value.starts_with("Sent interrupt to process ") => {
                Some(Self::InterruptProcess)
            }
            value if value.starts_with("Stopped process ") => Some(Self::StopProcess),
            _ => None,
        }
    }

    fn classify_name(name: &str, kind: ActivityKind) -> Self {
        // The three read/mail topology tools carry their own verb. Collapsing
        // every `ActivityKind::Agent` row to `Agent` printed `▸ ✓ Agent · 6.3s`
        // for a `wait_agent`, which says nothing about what is being waited on.
        match name {
            "spawn_agent" => return Self::SpawnAgent,
            "wait_agent" => return Self::WaitAgent,
            "list_agents" => return Self::ListAgents,
            "send_agent_message" => return Self::MessageAgent,
            // The tool is named `adopt_candidate`; without an arm here the call
            // falls through to `ActivityKind::Agent` and the transcript says
            // "Agent" for the one call that rewrites the human's checkout.
            "adopt_candidate" => return Self::Adopt,
            "close_agent" => return Self::CloseAgent,
            "interrupt_agent" => return Self::InterruptAgent,
            "recover_agent" => return Self::RecoverAgent,
            "discard_workspace" => return Self::DiscardWorkspace,
            "stop_all" => return Self::StopAll,
            "submit_result" => return Self::SubmitResult,
            _ => {}
        }
        match kind {
            ActivityKind::Compose => return Self::Compose,
            ActivityKind::Agent => return Self::Agent,
            ActivityKind::Compact => return Self::Compact,
            _ => {}
        }
        match name {
            // "Shell" is the host's normalized process-start label.
            "run_process"
            | "exec_command"
            | "run_terminal_cmd"
            | "run_terminal_command"
            | "Shell"
            | "shell"
            | "bash"
            | "Bash"
            | "process_session_start" => Self::Run,
            "write_stdin"
            | "BashOutput"
            | "get_command_or_subagent_output"
            | "process_session_poll"
            | "process_session_wait" => Self::WaitProcess,
            "process_session_write" => Self::InputProcess,
            "process_session_read" => Self::ReadProcess,
            "process_session_interrupt" => Self::InterruptProcess,
            "process_session_terminate" | "KillShell" | "TaskStop" | "kill_command_or_subagent" => {
                Self::StopProcess
            }
            "read_file" | "view_file" | "view_image" | "inspect_image" | "Read" => Self::Read,
            "search_repository" | "grep" | "Grep" | "glob" | "Glob" | "codebase_search"
            | "file_search" => Self::Search,
            "apply_patch" | "search_replace" | "edit_file" | "Edit" => Self::Edit,
            "write_file" | "create_file" | "Write" | "write" => Self::Write,
            "web_fetch" | "WebFetch" | "fetch" | "fetch_url" | "web__run" | "open_url" => {
                Self::Fetch
            }
            "web_search" | "WebSearch" => Self::WebSearch,
            "x_search" => Self::XSearch,
            "image_gen" => Self::GenerateImage,
            "list_dir" | "list_directory" | "ls" => Self::List,
            "update_plan" | "todo_write" | "merge_plan" => Self::Plan,
            name if McpCall::parse(name).is_some() => Self::Mcp,
            _ => Self::Generic,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit fixtures must decode before classification assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn derived_operation_preserves_the_stored_tool_shape_and_ignores_exposed_aliases() {
        let wire = serde_json::json!({
            "call_id": "call", "name": "read_file", "exposed_name": "write_file",
            "activity_kind": "tool", "state": "running"
        });
        let item: crate::views::transcript::ToolItem =
            serde_json::from_value(wire.clone()).expect("tool row");
        assert_eq!(item.operation(), ToolOperation::Read);
        assert_eq!(serde_json::to_value(item).expect("encode"), wire);
    }

    #[test]
    fn canonical_topology_wins_before_kind_and_kind_before_generic_aliases() {
        assert_eq!(
            ToolOperation::classify("wait_agent", ActivityKind::Agent, None, None),
            ToolOperation::WaitAgent
        );
        assert_eq!(
            ToolOperation::classify("adopt_candidate", ActivityKind::Tool, None, None),
            ToolOperation::Adopt
        );
        assert_eq!(
            ToolOperation::classify("read_file", ActivityKind::Compose, None, None),
            ToolOperation::Compose
        );
        assert_eq!(
            ToolOperation::classify("unknown", ActivityKind::Tool, None, None),
            ToolOperation::Generic
        );
        assert_eq!(
            ToolOperation::classify("mcp__docs__read", ActivityKind::Tool, None, None),
            ToolOperation::Mcp
        );
        for name in [
            "run_process",
            "exec_command",
            "Shell",
            "bash",
            "Bash",
            "process_session_start",
        ] {
            assert_eq!(
                ToolOperation::classify(name, ActivityKind::Tool, None, None),
                ToolOperation::Run
            );
        }
    }
}
