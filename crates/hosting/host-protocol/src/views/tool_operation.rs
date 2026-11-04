//! Derived tool semantics; never part of a stored transcript encoding.

use super::presentation::{ActivityKind, McpCall};

/// Semantic operation derived from the canonical tool name and host activity kind.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
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
    /// Classify a tool without consulting an exposed alias or presentation text.
    #[must_use]
    pub fn classify(name: &str, kind: ActivityKind) -> Self {
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
            | "process_session_poll" => Self::WaitProcess,
            "process_session_write" => Self::InputProcess,
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
            ToolOperation::classify("wait_agent", ActivityKind::Agent),
            ToolOperation::WaitAgent
        );
        assert_eq!(
            ToolOperation::classify("adopt_candidate", ActivityKind::Tool),
            ToolOperation::Adopt
        );
        assert_eq!(
            ToolOperation::classify("read_file", ActivityKind::Compose),
            ToolOperation::Compose
        );
        assert_eq!(
            ToolOperation::classify("unknown", ActivityKind::Tool),
            ToolOperation::Generic
        );
        assert_eq!(
            ToolOperation::classify("mcp__docs__read", ActivityKind::Tool),
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
                ToolOperation::classify(name, ActivityKind::Tool),
                ToolOperation::Run
            );
        }
    }
}
