//! Decode tagged blocks without serde's intermediate tagged-enum buffer losing raw JSON.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::MessageBlock;

type Fields = BTreeMap<String, Box<RawValue>>;

fn required(fields: &mut Fields, key: &str) -> Result<Box<RawValue>, String> {
    fields
        .remove(key)
        .ok_or_else(|| format!("missing Messages block field {key}"))
}

fn field<T: DeserializeOwned>(fields: &mut Fields, key: &str) -> Result<T, String> {
    serde_json::from_str(required(fields, key)?.get())
        .map_err(|_| format!("invalid Messages block field {key}"))
}

fn optional<T: DeserializeOwned>(fields: &mut Fields, key: &str) -> Result<Option<T>, String> {
    fields
        .remove(key)
        .map(|raw| {
            serde_json::from_str(raw.get())
                .map_err(|_| format!("invalid Messages block field {key}"))
        })
        .transpose()
        .map(Option::flatten)
}

impl<'de> Deserialize<'de> for MessageBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let mut fields: Fields =
            serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
        decode(&mut fields).map_err(serde::de::Error::custom)
    }
}

fn decode(fields: &mut Fields) -> Result<MessageBlock, String> {
    let kind: String = field(fields, "type")?;
    Ok(match kind.as_str() {
        "native" => MessageBlock::Native {
            block: required(fields, "block")?,
        },
        "text" => MessageBlock::Text {
            text: field(fields, "text")?,
            cache_control: optional(fields, "cache_control")?,
        },
        "thinking" => MessageBlock::Thinking {
            thinking: field(fields, "thinking")?,
            signature: required(fields, "signature")?,
        },
        "redacted_thinking" => MessageBlock::RedactedThinking {
            data: required(fields, "data")?,
        },
        "tool_use" => MessageBlock::ToolUse {
            id: field(fields, "id")?,
            name: field(fields, "name")?,
            input: field(fields, "input")?,
            caller: optional(fields, "caller")?,
        },
        "server_tool_use" => MessageBlock::ServerToolUse {
            id: field(fields, "id")?,
            name: field(fields, "name")?,
            input: field(fields, "input")?,
        },
        "code_execution_tool_result" => MessageBlock::CodeExecutionToolResult {
            tool_use_id: field(fields, "tool_use_id")?,
            content: field(fields, "content")?,
        },
        "tool_result" => MessageBlock::ToolResult {
            tool_use_id: field(fields, "tool_use_id")?,
            content: field(fields, "content")?,
            is_error: optional(fields, "is_error")?.unwrap_or(false),
        },
        "image" => MessageBlock::Image {
            source: field(fields, "source")?,
            cache_control: optional(fields, "cache_control")?,
        },
        _ => return Err("unknown Messages artifact block type".into()),
    })
}
