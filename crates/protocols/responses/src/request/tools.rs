//! Function tool definition wire helpers.

use serde::Serialize;
use serde_json::{Map, Value};

/// Responses-family function tool definition (shared by OpenAI / xAI / OpenRouter).
///
/// The request policy owns strictness: OpenAI Platform and Codex emit `false`,
/// xAI omits the field, and OpenRouter inserts its explicit `null` after lowering.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FunctionToolWire {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema parameters object.
    pub parameters: Value,
    /// OpenAI strict-schema flag; omit when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl FunctionToolWire {
    /// Builds a function tool with the strictness field omitted.
    #[must_use]
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function",
            name: name.into(),
            description: description.into(),
            parameters,
            strict: None,
        }
    }

    /// Builds a function tool for an explicitly strict OpenAI schema contract.
    #[must_use]
    pub fn openai_strict(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function",
            name: name.into(),
            description: description.into(),
            parameters,
            strict: Some(true),
        }
    }

    /// Lowers to a JSON object (for dialects that build bodies via [`Map`]).
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("type".to_owned(), Value::String(self.kind.to_owned()));
        map.insert("name".to_owned(), Value::String(self.name.clone()));
        map.insert(
            "description".to_owned(),
            Value::String(self.description.clone()),
        );
        map.insert("parameters".to_owned(), self.parameters.clone());
        if let Some(strict) = self.strict {
            map.insert("strict".to_owned(), Value::Bool(strict));
        }
        Value::Object(map)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_strict_omits_strict_field() {
        let tool = FunctionToolWire::function("read", "Read a file", json!({"type": "object"}));
        assert_eq!(
            serde_json::to_value(&tool).expect("serialize"),
            json!({
                "type": "function",
                "name": "read",
                "description": "Read a file",
                "parameters": {"type": "object"},
            })
        );
    }

    #[test]
    fn openai_strict_includes_strict_true() {
        let tool =
            FunctionToolWire::openai_strict("read", "Read a file", json!({"type": "object"}));
        assert_eq!(
            tool.to_value(),
            json!({
                "type": "function",
                "name": "read",
                "description": "Read a file",
                "parameters": {"type": "object"},
                "strict": true,
            })
        );
    }
}
