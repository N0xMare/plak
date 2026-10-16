//! `plak agent` JSONL presentation codec.
//!
//! Stdin commands and stdout events are a closed projection of `UserAction` /
//! `HostEvent`. This module does not own session or tree authority.
//!
//! The v1 envelope itself — the frame, its bounds, the handshake and refusal
//! lines, and the correlation rule — lives in `plakous-host-protocol` and is
//! re-exported here, as is the `HostEvent` -> JSONL projection. What stays is
//! the host-side command enum and its parse.

use crate::surface_protocol::UserAction;
pub use plakous_host_protocol::envelope::{
    JsonlCorrelation, MAX_PROMPT_BYTES, REQUIRED_STDOUT_KINDS, SCHEMA_V, bye_line, diagnostic_line,
    encode_object, hello_line, ready_line, required_stdout_kind,
};
use serde_json::{Map, Value};

/// Closed stdin command kinds after a successful parse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentCommand {
    Prompt {
        id: String,
        text: String,
        steer: bool,
    },
    Interrupt {
        id: String,
    },
    Background {
        id: String,
    },
    Compact {
        id: String,
    },
    Recover {
        id: String,
    },
    ResumeQueue {
        id: String,
    },
    ResumeTree {
        id: String,
    },
    Tree {
        id: String,
    },
    InterruptNode {
        id: String,
        agent_id: String,
    },
    CloseNode {
        id: String,
        agent_id: String,
    },
    StopAll {
        id: String,
    },
    Clear {
        id: String,
    },
    Quit {
        id: String,
    },
    InvokeTool {
        id: String,
        name: String,
        arguments: String,
        agent_id: Option<String>,
    },
    Fork {
        id: String,
        account: Option<String>,
        compact: bool,
    },
}

impl AgentCommand {
    /// Stable JSONL command kind for diagnostics spans (content-free).
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Interrupt { .. } => "interrupt",
            Self::Background { .. } => "background",
            Self::Compact { .. } => "compact",
            Self::Recover { .. } => "recover",
            Self::ResumeQueue { .. } => "resume_queue",
            Self::ResumeTree { .. } => "resume_tree",
            Self::Tree { .. } => "tree",
            Self::InterruptNode { .. } => "interrupt_node",
            Self::CloseNode { .. } => "close_node",
            Self::StopAll { .. } => "stop_all",
            Self::Clear { .. } => "clear",
            Self::Quit { .. } => "quit",
            Self::InvokeTool { .. } => "invoke_tool",
            Self::Fork { .. } => "fork",
        }
    }

    /// Field names allowed on the inbound JSONL command diagnostics span.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "privacy allowlist for diagnostics span tests")
    )]
    pub(crate) const COMMAND_SPAN_FIELDS: &'static [&'static str] = &["kind", "session_id"];

    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Prompt { id, .. }
            | Self::Interrupt { id }
            | Self::Background { id }
            | Self::Compact { id }
            | Self::Recover { id }
            | Self::ResumeQueue { id }
            | Self::ResumeTree { id }
            | Self::Tree { id }
            | Self::InterruptNode { id, .. }
            | Self::CloseNode { id, .. }
            | Self::StopAll { id }
            | Self::Clear { id }
            | Self::Quit { id }
            | Self::InvokeTool { id, .. }
            | Self::Fork { id, .. } => id,
        }
    }

    /// Map to the existing busy/queue law. `prompt` is Prompt when idle,
    /// FollowUp when busy. `steer:true` is `Steer` only while busy; idle
    /// steer stays connected as a codec diagnostic (see `agent_stdio`).
    #[must_use]
    pub fn into_user_action(self, busy: bool) -> Option<UserAction> {
        match self {
            Self::Prompt { text, steer, .. } => {
                if steer {
                    if busy {
                        Some(UserAction::Steer(text.into()))
                    } else {
                        None
                    }
                } else if busy {
                    Some(UserAction::FollowUp(text.into()))
                } else {
                    Some(UserAction::Prompt(text.into()))
                }
            }
            Self::Interrupt { .. } => Some(UserAction::Interrupt),
            Self::Background { .. } => Some(UserAction::Background),
            Self::Compact { .. } => Some(UserAction::Compact),
            Self::Recover { .. } => Some(UserAction::Recover),
            Self::ResumeQueue { .. } => Some(UserAction::ResumeQueueDrain),
            Self::ResumeTree { .. } => Some(UserAction::ResumeTopologyTree),
            Self::Tree { .. } => Some(UserAction::OpenTopologyNavigator),
            Self::InterruptNode { agent_id, .. } => {
                Some(UserAction::InterruptTopologyNode { agent_id })
            }
            Self::CloseNode { agent_id, .. } => Some(UserAction::CloseTopologyNode { agent_id }),
            Self::StopAll { .. } => Some(UserAction::StopAllTopology),
            Self::Clear { .. } => Some(UserAction::ClearSession),
            Self::Quit { .. } => Some(UserAction::Quit),
            Self::InvokeTool {
                id,
                name,
                arguments,
                agent_id,
            } => Some(UserAction::InvokeTool {
                correlation_id: Some(id),
                name,
                arguments,
                agent_id,
            }),
            Self::Fork {
                account, compact, ..
            } => Some(UserAction::ForkSession { account, compact }),
        }
    }
}

/// Parse one stdin line. Unknown additive fields are ignored. Unknown `kind`
/// or invalid `v` stay connected via [`ParseOutcome::Diagnostic`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseOutcome {
    Command(AgentCommand),
    Diagnostic { id: Option<String>, message: String },
    Empty,
}

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_ID_BYTES: usize = 256;

pub fn parse_command_line(line: &str) -> ParseOutcome {
    if line.len() > MAX_FRAME_BYTES {
        return ParseOutcome::Diagnostic {
            id: None,
            message: "JSONL frame exceeds 1 MiB".into(),
        };
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParseOutcome::Empty;
    }
    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(object)) => object,
        Ok(_) => {
            return ParseOutcome::Diagnostic {
                id: None,
                message: "JSONL command must be an object".to_owned(),
            };
        }
        Err(error) => {
            return ParseOutcome::Diagnostic {
                id: None,
                message: format!("JSONL command is not valid JSON ({error})"),
            };
        }
    };
    if value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.len() > MAX_ID_BYTES)
    {
        return ParseOutcome::Diagnostic {
            id: None,
            message: "JSONL id exceeds 256 bytes".into(),
        };
    }
    let id = string_field(&value, "id");
    match value.get("v") {
        Some(Value::Number(number)) if number.as_u64() == Some(u64::from(SCHEMA_V)) => {}
        Some(_) => {
            return ParseOutcome::Diagnostic {
                id,
                message: "unsupported JSONL schema version".to_owned(),
            };
        }
        None => {
            return ParseOutcome::Diagnostic {
                id,
                message: "JSONL command requires v".to_owned(),
            };
        }
    }
    let Some(kind) = string_field(&value, "kind") else {
        return ParseOutcome::Diagnostic {
            id,
            message: "JSONL command requires kind".to_owned(),
        };
    };
    let id = id.unwrap_or_default();
    match kind.as_str() {
        "prompt" => parse_prompt(id, &value),
        "interrupt" => ParseOutcome::Command(AgentCommand::Interrupt { id }),
        "background" => ParseOutcome::Command(AgentCommand::Background { id }),
        "compact" => ParseOutcome::Command(AgentCommand::Compact { id }),
        "recover" => ParseOutcome::Command(AgentCommand::Recover { id }),
        "resume_queue" => ParseOutcome::Command(AgentCommand::ResumeQueue { id }),
        "resume_tree" => ParseOutcome::Command(AgentCommand::ResumeTree { id }),
        "tree" => ParseOutcome::Command(AgentCommand::Tree { id }),
        "interrupt_node" => parse_node_action(id, &value, true),
        "close_node" => parse_node_action(id, &value, false),
        "stop_all" => ParseOutcome::Command(AgentCommand::StopAll { id }),
        "clear" => ParseOutcome::Command(AgentCommand::Clear { id }),
        "quit" => ParseOutcome::Command(AgentCommand::Quit { id }),
        "invoke_tool" => parse_invoke_tool(id, &value),
        "fork" => parse_fork(id, &value),
        other => ParseOutcome::Diagnostic {
            id: Some(id),
            message: format!(
                "unknown JSONL kind `{}`",
                other.chars().take(64).collect::<String>()
            ),
        },
    }
}

#[cfg(test)]
mod background_tests {
    use super::*;

    #[test]
    fn background_is_an_additive_v1_command() {
        let parsed = parse_command_line(r#"{"v":1,"kind":"background","id":"b1"}"#);
        let ParseOutcome::Command(command) = parsed else {
            panic!("background must be a v1 command: {parsed:?}");
        };
        assert_eq!(command.kind_name(), "background");
        assert_eq!(command.id(), "b1");
    }
}

fn parse_prompt(id: String, value: &Map<String, Value>) -> ParseOutcome {
    let Some(text) = string_field(value, "text") else {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "prompt requires text".to_owned(),
        };
    };
    if text.trim().is_empty() {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "prompt text must be non-empty".to_owned(),
        };
    }
    if text.len() > MAX_PROMPT_BYTES {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "prompt exceeds the maximum supported size".to_owned(),
        };
    }
    let steer = matches!(value.get("steer"), Some(Value::Bool(true)));
    ParseOutcome::Command(AgentCommand::Prompt { id, text, steer })
}

fn parse_node_action(id: String, value: &Map<String, Value>, interrupt: bool) -> ParseOutcome {
    let Some(agent_id) = string_field(value, "agent_id").filter(|value| !value.is_empty()) else {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "node action requires agent_id".to_owned(),
        };
    };
    if interrupt {
        ParseOutcome::Command(AgentCommand::InterruptNode { id, agent_id })
    } else {
        ParseOutcome::Command(AgentCommand::CloseNode { id, agent_id })
    }
}

fn parse_invoke_tool(id: String, value: &Map<String, Value>) -> ParseOutcome {
    let Some(name) = string_field(value, "name").filter(|value| !value.is_empty()) else {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "invoke_tool requires name".to_owned(),
        };
    };
    if !plakous_hosted::agent_tree_tools::is_topology_invoke_name(&name) {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: format!("invoke_tool name must be a topology tool (`{name}` is not)"),
        };
    }
    let Some(arguments) = value.get("arguments") else {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "invoke_tool requires arguments".to_owned(),
        };
    };
    if !arguments.is_object() {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "invoke_tool arguments must be a JSON object".to_owned(),
        };
    }
    let arguments = arguments.to_string();
    let agent_id = string_field(value, "agent_id").filter(|value| !value.is_empty());
    ParseOutcome::Command(AgentCommand::InvokeTool {
        id,
        name,
        arguments,
        agent_id,
    })
}

fn parse_fork(id: String, value: &Map<String, Value>) -> ParseOutcome {
    let compact = match value.get("compact") {
        None => false,
        Some(Value::Bool(compact)) => *compact,
        Some(_) => {
            return ParseOutcome::Diagnostic {
                id: Some(id),
                message: "fork compact must be a boolean".to_owned(),
            };
        }
    };
    let account = string_field(value, "account").filter(|value| !value.is_empty());
    if compact && account.is_some() {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "fork compact keeps the current binding".to_owned(),
        };
    }
    ParseOutcome::Command(AgentCommand::Fork {
        id,
        account,
        compact,
    })
}

fn string_field(value: &Map<String, Value>, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub use plakous_host_protocol::jsonl::{host_event_kind, project_host_event};

/// Decode golden fixtures in tests without depending on field order.
#[cfg(test)]
pub fn object_from_line(line: &str) -> std::collections::BTreeMap<String, Value> {
    match serde_json::from_str::<Value>(line).expect("json") {
        Value::Object(object) => object.into_iter().collect(),
        other => panic!("expected object, got {other}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use crate::surface_protocol::HostEvent;
    use plakous_host_protocol::QueueDrainState;

    /// The session binding changes only through templates and profiles, so
    /// the retired binding kinds are unknown kinds like any other.
    #[test]
    fn removed_binding_kinds_are_unknown_commands() {
        for line in [
            r#"{"v":1,"kind":"set_model","id":"m","account":"codex","model":"gpt-5.6-sol","effort":"medium"}"#,
            r#"{"v":1,"kind":"set_effort","id":"e","effort":"high"}"#,
            r#"{"v":1,"kind":"set_account","id":"a","account":"codex"}"#,
        ] {
            let ParseOutcome::Diagnostic { message, .. } = parse_command_line(line) else {
                panic!("a retired binding kind must be a diagnostic: {line}");
            };
            assert!(message.starts_with("unknown JSONL kind `set_"), "{message}");
        }
    }

    #[test]
    fn removed_harness_mode_is_an_unknown_command() {
        for intent in ["enter", "approve", "off"] {
            let line = serde_json::json!({"v": 1, "kind": "plan", "id": "p", "intent": intent})
                .to_string();
            assert!(matches!(
                parse_command_line(&line),
                ParseOutcome::Diagnostic { .. }
            ));
        }
    }

    #[test]
    fn fork_maps_to_user_action() {
        let ParseOutcome::Command(command) =
            parse_command_line(r#"{"v":1,"kind":"fork","id":"f","account":"grok"}"#)
        else {
            panic!("fork");
        };
        assert_eq!(
            command.into_user_action(false),
            Some(UserAction::ForkSession {
                account: Some("grok".into()),
                compact: false
            })
        );
    }

    #[test]
    fn raw_frame_id_and_decoded_prompt_bounds_are_independent() {
        for field in ["id", "ignored"] {
            let frame =
                serde_json::json!({"v": 1, "kind": "tree", field: "x".repeat(MAX_FRAME_BYTES)})
                    .to_string();
            assert!(matches!(
                parse_command_line(&frame),
                ParseOutcome::Diagnostic { id: None, .. }
            ));
        }
        let id = serde_json::json!({"v": 1, "kind": "tree", "id": "é".repeat(129)}).to_string();
        assert!(matches!(
            parse_command_line(&id),
            ParseOutcome::Diagnostic { id: None, .. }
        ));
        let prompt = format!(
            "{{\"v\":1,\"kind\":\"prompt\",\"text\":\"{}\"}}",
            "\\u0061".repeat(64 * 1024)
        );
        assert!(matches!(
            parse_command_line(&prompt),
            ParseOutcome::Command(_)
        ));
        let prompt =
            serde_json::json!({"v": 1, "kind": "prompt", "text": "a".repeat(64 * 1024 + 1)})
                .to_string();
        assert!(matches!(
            parse_command_line(&prompt),
            ParseOutcome::Diagnostic { .. }
        ));
    }
    use plakous_host_protocol::topology::{
        AttemptStatus, CandidateId, NodeStatus, Tier, TreeAdmissionState, WorkspaceMode,
        WorkspaceStatus,
    };
    use plakous_host_protocol::views::presentation::ActivityKind;
    use plakous_host_protocol::views::{NodeView, TreeView};

    #[test]
    fn prompt_maps_by_busy_and_steer() {
        let parsed = parse_command_line(
            r#"{"v":1,"kind":"prompt","id":"p1","text":"hello","ignored":true}"#,
        );
        let ParseOutcome::Command(command) = parsed else {
            panic!("{parsed:?}");
        };
        assert_eq!(
            command.clone().into_user_action(false),
            Some(UserAction::Prompt("hello".into()))
        );
        assert_eq!(
            command.clone().into_user_action(true),
            Some(UserAction::FollowUp("hello".into()))
        );
        let ParseOutcome::Command(steer) =
            parse_command_line(r#"{"v":1,"kind":"prompt","id":"p2","text":"now","steer":true}"#)
        else {
            panic!("steer");
        };
        assert_eq!(
            steer.clone().into_user_action(true),
            Some(UserAction::Steer("now".into()))
        );
        assert_eq!(steer.into_user_action(false), None);
    }

    #[test]
    fn unknown_kind_is_diagnostic_and_stays_connected() {
        let outcome = parse_command_line(r#"{"v":1,"kind":"login","id":"x"}"#);
        assert!(matches!(
            outcome,
            ParseOutcome::Diagnostic { id: Some(ref id), .. } if id == "x"
        ));
    }

    #[test]
    fn empty_and_oversize_prompt_are_diagnostics() {
        assert!(matches!(
            parse_command_line(r#"{"v":1,"kind":"prompt","id":"p","text":"  "}"#),
            ParseOutcome::Diagnostic { .. }
        ));
        let huge = "a".repeat(MAX_PROMPT_BYTES + 1);
        let line = format!(r#"{{"v":1,"kind":"prompt","id":"p","text":"{huge}"}}"#);
        assert!(matches!(
            parse_command_line(&line),
            ParseOutcome::Diagnostic { message, .. } if message.contains("maximum")
        ));
    }

    #[test]
    fn closed_kinds_round_trip_to_user_actions() {
        let cases = [
            (
                r#"{"v":1,"kind":"interrupt","id":"i"}"#,
                UserAction::Interrupt,
            ),
            (r#"{"v":1,"kind":"compact","id":"c"}"#, UserAction::Compact),
            (r#"{"v":1,"kind":"recover","id":"r"}"#, UserAction::Recover),
            (
                r#"{"v":1,"kind":"resume_queue","id":"rq"}"#,
                UserAction::ResumeQueueDrain,
            ),
            (
                r#"{"v":1,"kind":"resume_tree","id":"r"}"#,
                UserAction::ResumeTopologyTree,
            ),
            (
                r#"{"v":1,"kind":"tree","id":"t"}"#,
                UserAction::OpenTopologyNavigator,
            ),
            (
                r#"{"v":1,"kind":"stop_all","id":"s"}"#,
                UserAction::StopAllTopology,
            ),
            (
                r#"{"v":1,"kind":"clear","id":"cl"}"#,
                UserAction::ClearSession,
            ),
            (r#"{"v":1,"kind":"quit","id":"q"}"#, UserAction::Quit),
            (
                r#"{"v":1,"kind":"fork","id":"f"}"#,
                UserAction::ForkSession {
                    account: None,
                    compact: false,
                },
            ),
        ];
        for (line, expected) in cases {
            let ParseOutcome::Command(command) = parse_command_line(line) else {
                panic!("{line}");
            };
            assert_eq!(command.into_user_action(false), Some(expected));
        }
        let ParseOutcome::Command(command) =
            parse_command_line(r#"{"v":1,"kind":"interrupt_node","id":"n","agent_id":"c1"}"#)
        else {
            panic!("interrupt_node");
        };
        assert_eq!(
            command.into_user_action(false),
            Some(UserAction::InterruptTopologyNode {
                agent_id: "c1".into()
            })
        );
        let ParseOutcome::Command(command) =
            parse_command_line(r#"{"v":1,"kind":"close_node","id":"n","agent_id":"c2"}"#)
        else {
            panic!("close_node");
        };
        assert_eq!(
            command.into_user_action(false),
            Some(UserAction::CloseTopologyNode {
                agent_id: "c2".into()
            })
        );
        let ParseOutcome::Command(command) = parse_command_line(
            r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"list_agents","arguments":{"include_completed":true},"agent_id":"root"}"#,
        ) else {
            panic!("invoke_tool");
        };
        assert_eq!(
            command.into_user_action(false),
            Some(UserAction::InvokeTool {
                correlation_id: Some("iv".into()),
                name: "list_agents".into(),
                arguments: r#"{"include_completed":true}"#.into(),
                agent_id: Some("root".into()),
            })
        );
    }

    #[test]
    fn invoke_tool_rejects_coding_names_and_non_object_arguments() {
        let outcome = parse_command_line(
            r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"read_file","arguments":{}}"#,
        );
        assert!(matches!(
            outcome,
            ParseOutcome::Diagnostic { message, .. } if message.contains("topology tool")
        ));
        let outcome = parse_command_line(
            r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"spawn_agent","arguments":"nope"}"#,
        );
        assert!(matches!(
            outcome,
            ParseOutcome::Diagnostic { message, .. } if message.contains("JSON object")
        ));
        let outcome =
            parse_command_line(r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"spawn_agent"}"#);
        assert!(matches!(
            outcome,
            ParseOutcome::Diagnostic { message, .. } if message.contains("requires arguments")
        ));
    }

    /// The exported name set matches the topology tool set. `merge_workspace` is
    /// renamed, not aliased, so a driver still sending it is told so rather
    /// than having the call silently accepted or silently dropped.
    #[test]
    fn jsonl_invoke_tool_accepts_adopt_candidate_and_rejects_merge_workspace() {
        let ParseOutcome::Command(command) = parse_command_line(
            r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"adopt_candidate","arguments":{"candidate_id":"cand-1"}}"#,
        ) else {
            panic!("adopt_candidate is a topology tool");
        };
        assert!(matches!(
            command,
            AgentCommand::InvokeTool { ref name, .. } if name == "adopt_candidate"
        ));
        let outcome = parse_command_line(
            r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"merge_workspace","arguments":{}}"#,
        );
        assert!(matches!(
            outcome,
            ParseOutcome::Diagnostic { message, .. } if message.contains("topology tool")
        ));
    }

    #[test]
    fn interrupt_ignores_unknown_additive_fields() {
        let outcome = parse_command_line(
            r#"{"v":1,"kind":"interrupt","id":"i","future_flag":true,"note":"ignored"}"#,
        );
        let ParseOutcome::Command(command) = outcome else {
            panic!("interrupt with additive fields should parse");
        };
        assert_eq!(command.id(), "i");
        assert_eq!(command.into_user_action(false), Some(UserAction::Interrupt));
    }

    #[test]
    fn correlation_turn_scoped_prefers_prompt_id() {
        let correlation = JsonlCorrelation {
            prompt_id: Some("prompt-1"),
            omit_unknown_turn_id: false,
            command_id: Some("tree-1"),
            pending_tree_snapshot_id: None,
        };
        let assistant = project_host_event(&HostEvent::AssistantDelta("hi".into()), correlation)
            .expect("assistant");
        assert_eq!(object_from_line(&assistant)["id"], "prompt-1");

        let tool = project_host_event(
            &HostEvent::ToolLine {
                parent_call_id: None,
                cell_id: None,
                kind: ActivityKind::Tool,
                call_id: None,
                name: "spawn_agent".into(),
                state: "done".into(),
                subject: None,
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            correlation,
        )
        .expect("tool");
        assert_eq!(object_from_line(&tool)["id"], "prompt-1");
    }

    #[test]
    fn correlation_tree_snapshot_uses_pending_tree_id_only() {
        let mut snapshot = TreeView {
            tree_id: "tree-1".into(),
            revision: 2,
            state: TreeAdmissionState::Live,
            solicited: false,
            nodes: vec![],
        };
        let unsolicited = project_host_event(
            &HostEvent::TopologyNavigatorSnapshot(snapshot.clone()),
            JsonlCorrelation {
                prompt_id: Some("prompt-1"),
                omit_unknown_turn_id: false,
                command_id: Some("tree-1"),
                pending_tree_snapshot_id: Some("tree-2"),
            },
        )
        .expect("tree");
        assert!(!object_from_line(&unsolicited).contains_key("id"));

        snapshot.solicited = true;
        let explicit = project_host_event(
            &HostEvent::TopologyNavigatorSnapshot(snapshot),
            JsonlCorrelation {
                prompt_id: Some("prompt-1"),
                omit_unknown_turn_id: false,
                command_id: Some("tree-2"),
                pending_tree_snapshot_id: Some("tree-2"),
            },
        )
        .expect("tree");
        assert_eq!(object_from_line(&explicit)["id"], "tree-2");
    }

    #[test]
    fn correlation_errors_use_command_id() {
        let correlation = JsonlCorrelation {
            prompt_id: Some("prompt-1"),
            omit_unknown_turn_id: false,
            command_id: Some("tree-1"),
            pending_tree_snapshot_id: None,
        };
        let error = project_host_event(
            &HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::error(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    "grant is missing",
                ),
            ),
            correlation,
        )
        .expect("error");
        assert_eq!(object_from_line(&error)["id"], "tree-1");
    }

    #[test]
    fn idle_invoke_tool_events_echo_command_id() {
        let correlation = JsonlCorrelation {
            prompt_id: None,
            omit_unknown_turn_id: false,
            command_id: Some("iv"),
            pending_tree_snapshot_id: None,
        };
        let tool = project_host_event(
            &HostEvent::ToolLine {
                parent_call_id: None,
                cell_id: None,
                kind: ActivityKind::Tool,
                call_id: Some("call-1".into()),
                name: "list_agents".into(),
                state: "done".into(),
                subject: None,
                detail: Some("{\"agents\":[]}".into()),
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            correlation,
        )
        .expect("tool");
        let tool = object_from_line(&tool);
        assert_eq!(tool["id"], "iv");
        assert_eq!(tool["kind"], "tool");
        assert_eq!(tool["state"], "done");
        let error = project_host_event(
            &HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::error(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    "topology tool `spawn_agent` is not granted to `root`",
                ),
            ),
            correlation,
        )
        .expect("error");
        assert_eq!(object_from_line(&error)["id"], "iv");
        let idle = project_host_event(
            &HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::info(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    "invoke_tool: available when idle (finish or interrupt the turn)",
                ),
            ),
            correlation,
        )
        .expect("status");
        let idle = object_from_line(&idle);
        assert_eq!(idle["id"], "iv");
        assert_eq!(idle["kind"], "status");
    }

    /// Regression: the v1 wire dropped every `ToolLine` carrying a
    /// `duration_ms`, and the host stamps one on every terminal line it can,
    /// so `plak agent` announced a tool and never terminalised it.
    #[test]
    fn a_terminal_tool_line_reaches_the_wire_with_its_duration() {
        let terminal = |state: &str| HostEvent::ToolLine {
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            call_id: Some("call-1".into()),
            name: "read_file".into(),
            state: state.into(),
            subject: Some("src/lib.rs".into()),
            detail: None,
            duration_ms: Some(42),
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        };
        for state in ["done", "failed", "cancelled", "rejected"] {
            let line = project_host_event(&terminal(state), JsonlCorrelation::default())
                .unwrap_or_else(|| panic!("terminal `{state}` tool line has no v1 line"));
            let object = object_from_line(&line);
            assert_eq!(object["kind"], "tool");
            assert_eq!(object["state"], state);
            assert_eq!(object["duration_ms"], 42);
            assert!(required_stdout_kind(&line));
        }
        // An unstamped line stays unstamped rather than claiming a zero.
        let unstamped = object_from_line(
            &project_host_event(
                &HostEvent::ToolLine {
                    parent_call_id: None,
                    cell_id: None,
                    kind: ActivityKind::Tool,
                    call_id: Some("call-1".into()),
                    name: "read_file".into(),
                    state: "running".into(),
                    subject: None,
                    detail: None,
                    duration_ms: None,
                    started_at_ms: None,
                    finished_at_ms: None,
                    wait: None,
                },
                JsonlCorrelation::default(),
            )
            .expect("running tool line"),
        );
        assert!(!unstamped.contains_key("duration_ms"));
    }

    #[test]
    fn tool_error_tree_are_required_stdout() {
        let tool = project_host_event(
            &HostEvent::ToolLine {
                parent_call_id: None,
                cell_id: None,
                kind: ActivityKind::Tool,
                call_id: Some("call-1".into()),
                name: "spawn_agent".into(),
                state: "done".into(),
                subject: Some("c1".into()),
                detail: None,
                // A terminal line the host could stamp. It is the case that
                // matters: a client that never receives it is left with a
                // `running` row that never terminalises.
                duration_ms: Some(1_234),
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        )
        .unwrap();
        let object = object_from_line(&tool);
        assert_eq!(object["kind"], "tool");
        assert_eq!(object["name"], "spawn_agent");
        assert_eq!(object["id"], "p1");
        assert_eq!(object["state"], "done");
        assert_eq!(object["duration_ms"], 1_234);
        assert!(required_stdout_kind(&tool));

        for name in ["exec_js", "exec_py", "wait_js", "wait_py"] {
            let line = project_host_event(
                &HostEvent::ToolLine {
                    parent_call_id: None,
                    cell_id: None,
                    kind: ActivityKind::Compose,
                    call_id: Some("cell-1".into()),
                    name: name.into(),
                    state: "running".into(),
                    subject: Some("text(\"ptc-ok-js\")".into()),
                    detail: None,
                    duration_ms: None,
                    started_at_ms: None,
                    finished_at_ms: None,
                    wait: None,
                },
                JsonlCorrelation {
                    prompt_id: Some("p1"),
                    omit_unknown_turn_id: false,
                    command_id: None,
                    pending_tree_snapshot_id: None,
                },
            )
            .unwrap();
            let object = object_from_line(&line);
            assert_eq!(object["kind"], "tool");
            assert_eq!(object["name"], name);
            assert!(required_stdout_kind(&line));
        }

        let error = project_host_event(
            &HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::error(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    "grant is missing",
                ),
            ),
            JsonlCorrelation::default(),
        )
        .unwrap();
        assert!(required_stdout_kind(&error));

        let tree = project_host_event(
            &HostEvent::TopologyNavigatorSnapshot(TreeView {
                tree_id: "tree-1".into(),
                revision: 3,
                state: TreeAdmissionState::Live,
                solicited: false,
                nodes: vec![NodeView {
                    agent_id: "tree-1".into(),
                    parent_agent_id: None,
                    depth: 0,
                    status: NodeStatus::Running,
                    attempt_status: None,
                    attempt_seq: None,
                    attempt_id: None,
                    attempts: Vec::new(),
                    tier: None,
                    description: "root".into(),
                    prompt: "root".into(),
                    workspace_path: "/tmp".into(),
                    workspace_mode: WorkspaceMode::Inherit,
                    workspace_status: None,
                    candidate_id: None,
                    blocked: None,
                    usage: None,
                    active_turn: true,
                    live_progress: None,
                    unread_mail: 0,
                    has_session: true,
                    template: None,
                    model_account: "grok".into(),
                    model_tuple: "grok-4.6".into(),
                    model_effort: None,
                    turns: Some(1),
                    started_at_unix: None,
                    ended_at_unix: None,
                    fault: None,
                    waiting_on: None,
                }],
            }),
            JsonlCorrelation::default(),
        )
        .unwrap();
        assert!(required_stdout_kind(&tree));
        assert_eq!(object_from_line(&tree)["tree_id"], "tree-1");
    }

    /// A JSONL client sees the same observations the canvas
    /// does. An absent key means the host does not know — never a default that
    /// would tell a client a node is `read_only` when the host held nothing.
    #[test]
    fn tree_rows_carry_the_tier_attempt_and_candidate_or_omit_them() {
        let mut child = NodeView {
            agent_id: "tree-1-c1".into(),
            parent_agent_id: Some("tree-1".into()),
            depth: 1,
            status: NodeStatus::Sealing,
            attempt_status: Some(AttemptStatus::Running),
            attempt_seq: Some(2),
            attempt_id: None,
            attempts: vec![1, 2],
            tier: Some(Tier::Mutate),
            description: "implement".into(),
            prompt: "do it".into(),
            workspace_path: "/tmp/c1".into(),
            workspace_mode: WorkspaceMode::Fork,
            workspace_status: Some(WorkspaceStatus::Adopting),
            candidate_id: Some(CandidateId::from_persisted("cand-0f3a".to_owned())),
            blocked: Some(plakous_host_protocol::views::BlockedObservation {
                quota: "weekly".into(),
                account: "codex".into(),
                resets_at_unix: Some(1_700_000_000),
            }),
            usage: None,
            active_turn: true,
            live_progress: None,
            unread_mail: 0,
            has_session: true,
            template: Some("implement".into()),
            model_account: "codex".into(),
            model_tuple: "gpt-5.6-luna".into(),
            model_effort: Some("low".into()),
            turns: Some(3),
            started_at_unix: None,
            ended_at_unix: None,
            fault: None,
            waiting_on: None,
        };
        let snapshot = |node: NodeView| {
            let line = project_host_event(
                &HostEvent::TopologyNavigatorSnapshot(TreeView {
                    tree_id: "tree-1".into(),
                    revision: 4,
                    state: TreeAdmissionState::Live,
                    solicited: false,
                    nodes: vec![node],
                }),
                JsonlCorrelation::default(),
            )
            .unwrap();
            object_from_line(&line)
        };
        let object = snapshot(child.clone());
        let row = &object["nodes"][0];
        assert_eq!(row["account"], "codex");
        assert_eq!(row["status"], "sealing");
        assert_eq!(row["tier"], "mutate");
        assert_eq!(row["template"], "implement");
        assert_eq!(row["attempt"], "running");
        assert_eq!(row["attempt_seq"], 2);
        assert_eq!(row["workspace_mode"], "fork");
        assert_eq!(row["workspace_status"], "adopting");
        assert_eq!(row["candidate"], "cand-0f3a");
        assert_eq!(row["blocked"]["quota"], "weekly");
        assert_eq!(row["blocked"]["account"], "codex");
        assert_eq!(row["blocked"]["resets_at"], 1_700_000_000_u64);

        child.model_account.clear();
        child.tier = None;
        child.attempt_status = None;
        child.attempt_seq = None;
        child.candidate_id = None;
        child.blocked = None;
        child.workspace_status = None;
        child.template = None;
        let object = snapshot(child);
        let row = object["nodes"][0].as_object().unwrap();
        for key in [
            "account",
            "tier",
            "attempt",
            "attempt_seq",
            "candidate",
            "blocked",
            "workspace_status",
            "template",
        ] {
            assert!(
                !row.contains_key(key),
                "`{key}` must be omitted, not defaulted"
            );
        }
        // The mode is always known: it falls back to `inherit` in the host
        // projection, so it is a value rather than an omission.
        assert_eq!(row["workspace_mode"], "fork");
    }

    #[test]
    fn hello_ready_bye_are_stable() {
        let hello = hello_line(
            None,
            "sess",
            "tree",
            Some("grok"),
            Some("grok-4.6"),
            Some("low"),
            "native-ptc",
            "live",
            Some("bundled"),
            Some("aarch64-apple-darwin"),
            Some("bundled"),
            Some("aarch64-apple-darwin"),
            None,
        );
        let object = object_from_line(&hello);
        assert_eq!(object["v"], 1);
        assert_eq!(object["kind"], "hello");
        assert_eq!(object["session_id"], "sess");
        assert_eq!(object["tree_id"], "tree");
        assert_eq!(object["composition"], "native-ptc");
        assert_eq!(object["tree_state"], "live");
        assert_eq!(object["javascript_host"], "bundled");
        assert_eq!(object["javascript_host_target"], "aarch64-apple-darwin");
        assert_eq!(object["python_host"], "bundled");
        assert_eq!(object["python_host_target"], "aarch64-apple-darwin");
        assert!(!object.contains_key("elapsed_ms"));
        assert!(required_stdout_kind(&hello));
        assert!(required_stdout_kind(&ready_line(Some("p1"), None)));
        assert!(required_stdout_kind(&bye_line(None)));
    }

    #[test]
    fn hello_ready_elapsed_ms_serializes_when_some() {
        let hello = hello_line(
            None,
            "sess",
            "tree",
            None,
            None,
            None,
            "native-ptc",
            "live",
            None,
            None,
            None,
            None,
            Some(12),
        );
        assert_eq!(object_from_line(&hello)["elapsed_ms"], 12);
        assert_eq!(
            object_from_line(&ready_line(None, Some(12)))["elapsed_ms"],
            12
        );
    }

    #[test]
    fn optional_events_project_and_are_not_required_stdout() {
        use plakous_host_protocol::QueueItem;

        let status = project_host_event(
            &HostEvent::Status {
                phase: "running".into(),
                queue_len: 2,
                detail: Some("tool".into()),
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        )
        .expect("status projects");
        let status_obj = object_from_line(&status);
        assert_eq!(status_obj["kind"], "status");
        assert_eq!(status_obj["phase"], "running");
        assert_eq!(status_obj["queue_len"], 2);
        assert_eq!(status_obj["detail"], "tool");
        assert!(!required_stdout_kind(&status));

        let note = project_host_event(
            &HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::info(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    "compaction not needed: the selected prefix already fits",
                ),
            ),
            JsonlCorrelation {
                prompt_id: None,
                omit_unknown_turn_id: false,
                command_id: Some("c1"),
                pending_tree_snapshot_id: None,
            },
        )
        .expect("system notes project as optional status");
        let note_obj = object_from_line(&note);
        assert_eq!(note_obj["kind"], "status");
        assert_eq!(note_obj["phase"], "note");
        assert_eq!(
            note_obj["detail"],
            "compaction not needed: the selected prefix already fits"
        );
        assert!(!required_stdout_kind(&note));

        let busy = project_host_event(&HostEvent::SetBusy(true), JsonlCorrelation::default())
            .expect("busy projects");
        let busy_obj = object_from_line(&busy);
        assert_eq!(busy_obj["kind"], "busy");
        assert_eq!(busy_obj["busy"], true);
        assert!(!required_stdout_kind(&busy));

        let assistant = project_host_event(
            &HostEvent::AssistantDelta("hello".into()),
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        )
        .expect("assistant projects");
        let assistant_obj = object_from_line(&assistant);
        assert_eq!(assistant_obj["kind"], "assistant");
        assert_eq!(assistant_obj["text"], "hello");
        assert!(!required_stdout_kind(&assistant));

        let queue = project_host_event(
            &HostEvent::QueueSnapshot {
                items: vec![QueueItem {
                    rejected: false,
                    id: "q1".into(),
                    revision: 4,
                    text: "follow up".into(),
                    images: Vec::new(),
                }],
                drain: QueueDrainState::RestoredPaused,
            },
            JsonlCorrelation::default(),
        )
        .expect("queue projects");
        let queue_obj = object_from_line(&queue);
        assert_eq!(queue_obj["kind"], "queue");
        assert_eq!(queue_obj["drain"], "restored_paused");
        let items = queue_obj["items"].as_array().expect("queue items");
        assert_eq!(items[0]["id"], "q1");
        assert_eq!(items[0]["revision"], 4);
        assert_eq!(items[0]["text"], "follow up");
        assert_eq!(items[0]["rejected"], false);
        assert!(!required_stdout_kind(&queue));
    }

    /// additive v1 kinds and `tool` fields; `SCHEMA_V` stays 1.
    ///
    /// A v1 client that ignores unknown kinds and fields is unaffected, and no
    /// projected line carries a runtime decision address.
    #[test]
    fn projections_are_additive_v1_lines() {
        let correlation = JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        };
        assert_eq!(SCHEMA_V, 1);

        let compose = object_from_line(
            &project_host_event(
                &HostEvent::ToolLine {
                    call_id: Some("outer".into()),
                    parent_call_id: None,
                    cell_id: Some("cell-1".into()),
                    kind: ActivityKind::Compose,
                    name: "exec_js".into(),
                    state: "running".into(),
                    subject: Some("3 tools · 1 failed".into()),
                    detail: None,
                    duration_ms: None,
                    started_at_ms: None,
                    finished_at_ms: None,
                    wait: None,
                },
                correlation,
            )
            .unwrap(),
        );
        assert_eq!(compose["kind"], "tool");
        assert_eq!(compose["activity_kind"], "compose");
        assert_eq!(compose["cell_id"], "cell-1");
        assert_eq!(compose["subject"], "3 tools · 1 failed");

        let child = object_from_line(
            &project_host_event(
                &HostEvent::ToolLine {
                    call_id: Some("child".into()),
                    parent_call_id: Some("outer".into()),
                    cell_id: Some("cell-1".into()),
                    kind: ActivityKind::Tool,
                    name: "read_file".into(),
                    state: "done".into(),
                    subject: None,
                    detail: None,
                    duration_ms: Some(7),
                    started_at_ms: None,
                    finished_at_ms: None,
                    wait: None,
                },
                correlation,
            )
            .unwrap(),
        );
        assert_eq!(child["parent_call_id"], "outer");
        assert_eq!(child["activity_kind"], "tool");
        assert_eq!(child["duration_ms"], 7);

        let reasoning = object_from_line(
            &project_host_event(
                &HostEvent::ReasoningDelta {
                    location_key: "0:0".into(),
                    text: "weighing".into(),
                },
                correlation,
            )
            .unwrap(),
        );
        assert_eq!(reasoning["kind"], "reasoning");
        assert_eq!(reasoning["text"], "weighing");

        let approval = object_from_line(
            &project_host_event(
                &HostEvent::ApprovalRequired {
                    approval_id: "approval-1".into(),
                    call_id: Some("call-1".into()),
                    tool_name: "apply_patch".into(),
                    subject: None,
                    prompt: Some("Allow this write?".into()),
                    expires_at_ms: 42,
                },
                correlation,
            )
            .unwrap(),
        );
        assert_eq!(approval["kind"], "approval");
        assert_eq!(approval["approval_id"], "approval-1");
        // The decision address is never on the wire.
        assert!(!approval.contains_key("decision"));
        assert!(!approval.contains_key("binding_digest"));

        let resolved = object_from_line(
            &project_host_event(
                &HostEvent::ApprovalResolved {
                    approval_id: "approval-1".into(),
                    resolution: "expired".into(),
                },
                correlation,
            )
            .unwrap(),
        );
        assert_eq!(resolved["kind"], "approval_resolved");
        assert_eq!(resolved["resolution"], "expired");

        let steer = object_from_line(
            &project_host_event(
                &HostEvent::SteerAck {
                    state: "queued".into(),
                    position: Some(2),
                },
                correlation,
            )
            .unwrap(),
        );
        assert_eq!(steer["kind"], "steer_ack");
        assert_eq!(steer["state"], "queued");
        assert_eq!(steer["position"], 2);
    }

    #[test]
    fn live_usage_snapshot_is_not_a_turn_event() {
        let line = project_host_event(
            &HostEvent::TurnUsage {
                input_tokens: 2100,
                output_tokens: 40,
                reasoning_tokens: 12,
                cached_input_tokens: 1800,
                cache_write_tokens: 400,
                model_operations: None,
                reporting_model_operations: None,
                peak_prepared_input_tokens: None,
                terminal: false,
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        );
        assert!(
            line.is_none(),
            "a mid-turn usage snapshot must not read as `turn`"
        );
    }

    /// The terminal `turn` record carries the growth facts a gate needs.
    ///
    /// `input_tokens` sums over operations, so it cannot say how far a turn
    /// grew against the window. The peak is one operation's own prompt and
    /// the operation count says how many requests the sum covers. Both are
    /// additive: a v1 client ignoring unknown fields is unaffected, and the
    /// mid-turn snapshot is still dropped.
    #[test]
    fn the_terminal_turn_record_carries_the_context_growth_fields() {
        let line = project_host_event(
            &HostEvent::TurnUsage {
                input_tokens: 534_462,
                output_tokens: 40,
                reasoning_tokens: 12,
                cached_input_tokens: 1800,
                cache_write_tokens: 400,
                model_operations: Some(16),
                reporting_model_operations: Some(16),
                peak_prepared_input_tokens: Some(31_500),
                terminal: true,
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        )
        .expect("a terminal usage observation projects as `turn`");
        let record: Value = serde_json::from_str(&line).expect("the projection is one JSON object");
        assert_eq!(record["kind"], "turn");
        assert_eq!(record["outcome"], "usage");
        assert_eq!(record["input_tokens"], 534_462, "the sum is unchanged");
        assert_eq!(record["model_operations"], 16);
        assert_eq!(record["peak_prepared_input_tokens"], 31_500);
    }

    /// An unknown peak is absent from the record rather than reported as zero.
    #[test]
    fn an_unknown_prepared_input_peak_is_omitted_not_zero() {
        let line = project_host_event(
            &HostEvent::TurnUsage {
                input_tokens: 2100,
                output_tokens: 40,
                reasoning_tokens: 12,
                cached_input_tokens: 1800,
                cache_write_tokens: 400,
                model_operations: None,
                reporting_model_operations: None,
                peak_prepared_input_tokens: None,
                terminal: true,
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        )
        .expect("a terminal usage observation projects as `turn`");
        let record: Value = serde_json::from_str(&line).expect("the projection is one JSON object");
        assert!(record.get("peak_prepared_input_tokens").is_none());
        assert!(record.get("model_operations").is_none());
    }

    #[test]
    fn turn_usage_projects_cached_input_as_observation() {
        let line = project_host_event(
            &HostEvent::TurnUsage {
                input_tokens: 2100,
                output_tokens: 40,
                reasoning_tokens: 12,
                cached_input_tokens: 1800,
                cache_write_tokens: 400,
                model_operations: Some(1),
                reporting_model_operations: Some(1),
                peak_prepared_input_tokens: None,
                terminal: true,
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
        )
        .expect("turn usage projects");
        let object = object_from_line(&line);
        assert_eq!(object["kind"], "turn");
        assert_eq!(object["outcome"], "usage");
        assert_eq!(object["id"], "p1");
        assert_eq!(object["input_tokens"], 2100);
        assert_eq!(object["output_tokens"], 40);
        assert_eq!(object["reasoning_tokens"], 12);
        assert_eq!(object["cached_input_tokens"], 1800);
        assert_eq!(object["cache_write_tokens"], 400);
        assert!(required_stdout_kind(&line));
    }

    #[test]
    fn navigator_unavailable_projects_as_required_error() {
        let error = project_host_event(
            &HostEvent::TopologyNavigatorUnavailable {
                reason: "no topology hub is attached".into(),
            },
            JsonlCorrelation {
                prompt_id: None,
                omit_unknown_turn_id: false,
                command_id: Some("t1"),
                pending_tree_snapshot_id: None,
            },
        )
        .expect("unavailable projects");
        let object = object_from_line(&error);
        assert_eq!(object["kind"], "error");
        assert_eq!(object["id"], "t1");
        assert_eq!(object["message"], "no topology hub is attached");
        assert!(required_stdout_kind(&error));
    }

    #[test]
    fn jsonl_command_span_fields_are_content_free_allowlist() {
        assert_eq!(
            AgentCommand::COMMAND_SPAN_FIELDS,
            &["kind", "session_id"][..]
        );
        for forbidden in [
            "text",
            "arguments",
            "prompt",
            "message",
            "detail",
            "task",
            "workspace_path",
        ] {
            assert!(
                !AgentCommand::COMMAND_SPAN_FIELDS.contains(&forbidden),
                "forbidden span field `{forbidden}` must not appear on jsonl.command"
            );
        }
    }
}
