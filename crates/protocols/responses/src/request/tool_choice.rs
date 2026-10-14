//! Function-tool `tool_choice` wire helpers.
//!
//! OpenAI custom-tool choice (`{"type":"custom","name":...}`) stays dialect-local.
//! SuperGrok omission of `tool_choice` when tools are empty is also dialect policy.

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

/// Responses-family function-only `tool_choice` wire value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FunctionToolChoiceWire {
    /// Model may call tools or answer.
    Auto,
    /// Disable tool calls.
    None,
    /// Require at least one tool call.
    Required,
    /// Force one named function tool.
    Function(String),
}

impl FunctionToolChoiceWire {
    /// Lowers to the Responses JSON wire shape.
    ///
    /// String modes serialize as plain strings; function mode as
    /// `{"type":"function","name":...}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Auto => Value::String("auto".to_owned()),
            Self::None => Value::String("none".to_owned()),
            Self::Required => Value::String("required".to_owned()),
            Self::Function(name) => json!({"type": "function", "name": name}),
        }
    }
}

impl Serialize for FunctionToolChoiceWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Required => serializer.serialize_str("required"),
            Self::Function(name) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "function")?;
                map.serialize_entry("name", name)?;
                map.end()
            }
        }
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

    #[test]
    fn string_modes_and_function_object() {
        assert_eq!(FunctionToolChoiceWire::Auto.to_value(), json!("auto"));
        assert_eq!(FunctionToolChoiceWire::None.to_value(), json!("none"));
        assert_eq!(
            FunctionToolChoiceWire::Required.to_value(),
            json!("required")
        );
        assert_eq!(
            FunctionToolChoiceWire::Function("read_file".to_owned()).to_value(),
            json!({"type": "function", "name": "read_file"})
        );
    }

    #[test]
    fn serialize_matches_to_value() {
        for choice in [
            FunctionToolChoiceWire::Auto,
            FunctionToolChoiceWire::None,
            FunctionToolChoiceWire::Required,
            FunctionToolChoiceWire::Function("tool".to_owned()),
        ] {
            let via_serde = serde_json::to_value(&choice).expect("serialize");
            assert_eq!(via_serde, choice.to_value());
        }
    }
}
