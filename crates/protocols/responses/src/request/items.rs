//! Input-item Value builders shared by Responses-family dialects.
//!
//! OpenAI builds bodies as [`serde_json::Value`]. xAI / OpenRouter use typed
//! structs today but may adopt these helpers when migrating body construction.

use serde_json::{Map, Value};

/// Builds an `input_text` content part.
#[must_use]
pub fn input_text_part(text: impl Into<String>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String("input_text".to_owned()));
    map.insert("text".to_owned(), Value::String(text.into()));
    Value::Object(map)
}

/// Builds an `output_text` content part.
#[must_use]
pub fn output_text_part(text: impl Into<String>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String("output_text".to_owned()));
    map.insert("text".to_owned(), Value::String(text.into()));
    Value::Object(map)
}

/// Builds a `refusal` content part (assistant messages).
#[must_use]
pub fn refusal_part(refusal: impl Into<String>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String("refusal".to_owned()));
    map.insert("refusal".to_owned(), Value::String(refusal.into()));
    Value::Object(map)
}

/// Builds a `message` input item with role and content parts.
#[must_use]
pub fn message_item(role: impl Into<String>, content: Vec<Value>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String("message".to_owned()));
    map.insert("role".to_owned(), Value::String(role.into()));
    map.insert("content".to_owned(), Value::Array(content));
    Value::Object(map)
}

/// Builds a `function_call` input item.
#[must_use]
pub fn function_call_item(
    call_id: impl Into<String>,
    name: impl Into<String>,
    arguments: impl Into<String>,
) -> Value {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String("function_call".to_owned()));
    map.insert("call_id".to_owned(), Value::String(call_id.into()));
    map.insert("name".to_owned(), Value::String(name.into()));
    map.insert("arguments".to_owned(), Value::String(arguments.into()));
    Value::Object(map)
}

/// Builds a `function_call_output` input item.
///
/// `output` may be a string (xAI / OpenRouter / simple OpenAI) or a content
/// array (multi-part OpenAI tool results).
#[must_use]
pub fn function_call_output_item(call_id: impl Into<String>, output: Value) -> Value {
    let mut map = Map::new();
    map.insert(
        "type".to_owned(),
        Value::String("function_call_output".to_owned()),
    );
    map.insert("call_id".to_owned(), Value::String(call_id.into()));
    map.insert("output".to_owned(), output);
    Value::Object(map)
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
    fn text_parts_and_message() {
        assert_eq!(
            input_text_part("hi"),
            json!({"type": "input_text", "text": "hi"})
        );
        assert_eq!(
            output_text_part("bye"),
            json!({"type": "output_text", "text": "bye"})
        );
        assert_eq!(
            message_item("user", vec![input_text_part("hi")]),
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            })
        );
    }

    #[test]
    fn function_call_shapes() {
        assert_eq!(
            function_call_item("c1", "read", "{}"),
            json!({
                "type": "function_call",
                "call_id": "c1",
                "name": "read",
                "arguments": "{}"
            })
        );
        assert_eq!(
            function_call_output_item("c1", Value::String("ok".to_owned())),
            json!({
                "type": "function_call_output",
                "call_id": "c1",
                "output": "ok"
            })
        );
    }
}
