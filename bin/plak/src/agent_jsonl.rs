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
        images: Vec<plakous_host_protocol::DraftImage>,
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
    ResolveApproval {
        id: String,
        approval_id: String,
        choice: plakous_host_protocol::ApprovalChoice,
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
            Self::ResolveApproval { .. } => "resolve_approval",
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
            | Self::ResolveApproval { id, .. }
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
            Self::Prompt {
                text,
                images,
                steer,
                ..
            } => {
                let draft = plakous_host_protocol::Draft::new(text, images);
                if steer {
                    if busy {
                        Some(UserAction::Steer(draft))
                    } else {
                        None
                    }
                } else if busy {
                    Some(UserAction::FollowUp(draft))
                } else {
                    Some(UserAction::Prompt(draft))
                }
            }
            Self::Interrupt { .. } => Some(UserAction::Interrupt),
            Self::Background { .. } => Some(UserAction::Background),
            Self::Compact { .. } => Some(UserAction::Compact),
            Self::Recover { .. } => Some(UserAction::Recover),
            Self::ResolveApproval {
                approval_id,
                choice,
                ..
            } => Some(UserAction::ResolveApproval {
                approval_id,
                choice,
            }),
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
        "resolve_approval" => parse_resolve_approval(id, &value),
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

fn parse_resolve_approval(id: String, value: &Map<String, Value>) -> ParseOutcome {
    let Some(approval_id) = string_field(value, "approval_id").filter(|id| !id.is_empty()) else {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: "resolve_approval requires a non-empty approval_id from an approval event"
                .into(),
        };
    };
    let choice = match value.get("choice").and_then(Value::as_str) {
        Some("allow") => plakous_host_protocol::ApprovalChoice::Allow,
        Some("deny") => plakous_host_protocol::ApprovalChoice::Deny,
        _ => {
            return ParseOutcome::Diagnostic {
                id: Some(id),
                message: "resolve_approval choice must be allow or deny".into(),
            };
        }
    };
    ParseOutcome::Command(AgentCommand::ResolveApproval {
        id,
        approval_id,
        choice,
    })
}

#[cfg(test)]
mod background_tests {
    use super::*;

    #[test]
    fn unavailable_images_are_a_correlated_diagnostic_not_an_action_or_host_failure() {
        let prompt = serde_json::json!({"text":"not admitted", "images":[{"path":"missing.png"}]});
        let refusal = plakous_hosted::capabilities::CapabilityUnavailable {
            id: "images",
            kind: plakous_hosted::capabilities::CapabilityKind::Surface,
            feature: "images",
        };
        let ParseOutcome::Diagnostic { id, message } = parse_prompt_with_image_capability(
            "image-1".to_owned(),
            prompt.as_object().unwrap(),
            Err(refusal),
        ) else {
            panic!("capability refusal must leave the connection and active turn alone")
        };
        assert_eq!(id.as_deref(), Some("image-1"));
        assert!(message.contains("feature `images`"));
        assert!(matches!(
            parse_command_line(r#"{"v":1,"kind":"prompt","id":"text-2","text":"still connected"}"#),
            ParseOutcome::Command(AgentCommand::Prompt { .. })
        ));
    }

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
    parse_prompt_with_image_capability(id, value, crate::build_features::require_feature("images"))
}

fn parse_prompt_with_image_capability(
    id: String,
    value: &Map<String, Value>,
    images_available: Result<(), plakous_hosted::capabilities::CapabilityUnavailable>,
) -> ParseOutcome {
    if value
        .get("images")
        .and_then(Value::as_array)
        .is_some_and(|images| !images.is_empty())
        && let Err(error) = images_available
    {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: crate::CliFailure::from(error).message().to_owned(),
        };
    }
    let images = match prompt_images(value.get("images")) {
        Ok(images) => images,
        Err(message) => {
            return ParseOutcome::Diagnostic {
                id: Some(id),
                message,
            };
        }
    };
    let text = match value.get("text") {
        Some(Value::String(text)) => text.clone(),
        None if !images.is_empty() => String::new(),
        _ => {
            return ParseOutcome::Diagnostic {
                id: Some(id),
                message: "prompt requires text or images".to_owned(),
            };
        }
    };
    if text.trim().is_empty() && images.is_empty() {
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
    if let Err(error) = plakous_hosted::attachments::check_message_budget(&text, &images) {
        return ParseOutcome::Diagnostic {
            id: Some(id),
            message: format!("prompt refused: {error}"),
        };
    }
    let steer = matches!(value.get("steer"), Some(Value::Bool(true)));
    ParseOutcome::Command(AgentCommand::Prompt {
        id,
        text,
        images,
        steer,
    })
}

/// The image files a prompt's optional `images` array of paths names, read
/// and sniffed as `plak run --image` reads them; relative paths resolve
/// against the agent's working directory.
fn prompt_images(images: Option<&Value>) -> Result<Vec<plakous_host_protocol::DraftImage>, String> {
    let Some(images) = images else {
        return Ok(Vec::new());
    };
    let Some(paths) = images.as_array() else {
        return Err("prompt images must be an array of file paths".to_owned());
    };
    if paths.len() > plakous_hosted::attachments::MAX_IMAGES {
        return Err(format!(
            "a prompt carries at most {} images",
            plakous_hosted::attachments::MAX_IMAGES
        ));
    }
    paths
        .iter()
        .map(|path| {
            let path = path
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| "prompt images must be an array of file paths".to_owned())?;
            plakous_hosted::attachments::read_image(std::path::Path::new(path))
                .map_err(|error| error.to_string())
        })
        .collect()
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
mod tests;
