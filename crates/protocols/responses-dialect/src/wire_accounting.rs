//! Closed structural occurrence accounting for one serialized Responses body.
//!
//! This module owns no estimator or model-specific coefficients. It recognizes
//! only the bounded function-only, stateless grammar whose repeatable shapes
//! can be certified by a dialect owner. Broader or malformed wire shapes are
//! deliberately not classified.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

const OCCURRENCE_KINDS: usize = 11;
const MAX_TOTAL_OCCURRENCES: u32 = 1_048_576;
const MAX_SCHEMA_DEPTH: usize = 64;

/// One repeatable structural shape in the closed Responses wire grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesWireOccurrence {
    /// A message input item, whether freshly lowered or exactly replayed.
    Message,
    /// An `input_text` message or function-result content part.
    InputText,
    /// An `output_text` message content part.
    OutputText,
    /// A replayed refusal content part.
    Refusal,
    /// A function-call input item, whether freshly lowered or exactly replayed.
    FunctionCall,
    /// A function-call-result input item.
    FunctionCallOutput,
    /// A function tool definition.
    FunctionTool,
    /// One JSON value in a function tool's parameter-schema tree.
    SchemaNode,
    /// One replayable encrypted reasoning input item.
    ReasoningReplay,
    /// One text summary part inside replayable reasoning.
    ReasoningSummary,
    /// One structurally bounded, otherwise opaque replay item.
    OpaqueReplay,
}

impl ResponsesWireOccurrence {
    /// Every occurrence kind in stable table order.
    pub const ALL: [Self; OCCURRENCE_KINDS] = [
        Self::Message,
        Self::InputText,
        Self::OutputText,
        Self::Refusal,
        Self::FunctionCall,
        Self::FunctionCallOutput,
        Self::FunctionTool,
        Self::SchemaNode,
        Self::ReasoningReplay,
        Self::ReasoningSummary,
        Self::OpaqueReplay,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Message => 0,
            Self::InputText => 1,
            Self::OutputText => 2,
            Self::Refusal => 3,
            Self::FunctionCall => 4,
            Self::FunctionCallOutput => 5,
            Self::FunctionTool => 6,
            Self::SchemaNode => 7,
            Self::ReasoningReplay => 8,
            Self::ReasoningSummary => 9,
            Self::OpaqueReplay => 10,
        }
    }
}

/// Checked occurrence vector derived from one exact serialized Responses body.
///
/// Values can only be created by the shared final-body inspection. The total
/// and every per-kind counter are bounded by `1_048_576` and use checked
/// arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesWireOccurrenceCounts {
    counts: [u32; OCCURRENCE_KINDS],
    total: u32,
}

impl ResponsesWireOccurrenceCounts {
    const fn empty() -> Self {
        Self {
            counts: [0; OCCURRENCE_KINDS],
            total: 0,
        }
    }

    /// Returns the checked count for one closed wire shape.
    #[must_use]
    pub const fn count(self, occurrence: ResponsesWireOccurrence) -> u32 {
        self.counts[occurrence.index()]
    }

    /// Returns the checked aggregate count across every closed wire shape.
    #[must_use]
    pub const fn total(self) -> u32 {
        self.total
    }

    fn increment(
        &mut self,
        occurrence: ResponsesWireOccurrence,
    ) -> Result<(), WireAccountingError> {
        let count = self
            .counts
            .get_mut(occurrence.index())
            .ok_or(WireAccountingError::ResourceLimit)?;
        *count = count
            .checked_add(1)
            .filter(|count| *count <= MAX_TOTAL_OCCURRENCES)
            .ok_or(WireAccountingError::ResourceLimit)?;
        self.total = self
            .total
            .checked_add(1)
            .filter(|total| *total <= MAX_TOTAL_OCCURRENCES)
            .ok_or(WireAccountingError::ResourceLimit)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WireAccountingError {
    Malformed,
    Unsupported,
    ResourceLimit,
}

pub(crate) fn inspect_wire_occurrences(
    body: &Value,
) -> Result<ResponsesWireOccurrenceCounts, WireAccountingError> {
    let object = body.as_object().ok_or(WireAccountingError::Malformed)?;
    validate_members(
        object,
        &["model", "input", "stream"],
        &[
            "instructions",
            "tools",
            "tool_choice",
            "max_output_tokens",
            "reasoning",
            "store",
            "include",
        ],
    )?;
    required_nonempty_string(object, "model")?;
    if object.get("stream") != Some(&Value::Bool(true)) {
        return Err(WireAccountingError::Malformed);
    }
    if object.get("store") != Some(&Value::Bool(false))
        || object.get("include") != Some(&serde_json::json!(["reasoning.encrypted_content"]))
    {
        return Err(WireAccountingError::Unsupported);
    }
    if let Some(instructions) = object.get("instructions")
        && instructions.as_str().is_none_or(str::is_empty)
    {
        return Err(WireAccountingError::Malformed);
    }
    if let Some(limit) = object.get("max_output_tokens") {
        let valid = limit
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .is_some_and(|value| value != 0);
        if !valid {
            return Err(WireAccountingError::Malformed);
        }
    }

    validate_reasoning_configuration(object)?;
    let mut counts = ResponsesWireOccurrenceCounts::empty();
    let mut tool_names = BTreeSet::new();
    if let Some(tools) = object.get("tools") {
        let tools = tools.as_array().ok_or(WireAccountingError::Malformed)?;
        if tools.is_empty() {
            return Err(WireAccountingError::Malformed);
        }
        for tool in tools {
            inspect_function_tool(tool, &mut counts, &mut tool_names)?;
        }
    }
    match (object.get("tools"), object.get("tool_choice")) {
        (Some(_), Some(choice)) => validate_tool_choice(choice, &tool_names)?,
        (None, None) => {}
        _ => return Err(WireAccountingError::Unsupported),
    }

    let input = object
        .get("input")
        .and_then(Value::as_array)
        .ok_or(WireAccountingError::Malformed)?;
    for item in input {
        inspect_input_item(item, &mut counts)?;
    }
    Ok(counts)
}

fn inspect_input_item(
    item: &Value,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    let object = item.as_object().ok_or(WireAccountingError::Malformed)?;
    let kind = required_nonempty_string(object, "type")?;
    match kind {
        "message" => inspect_message(object, counts),
        "function_call" => inspect_function_call(object, counts),
        "function_call_output" => inspect_function_call_output(object, counts),
        "reasoning" => inspect_reasoning_replay(object, counts),
        "custom_tool_call" | "custom_tool_call_output" => Err(WireAccountingError::Unsupported),
        _ => inspect_opaque_replay(object, counts),
    }
}

fn inspect_message(
    object: &Map<String, Value>,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    validate_members(object, &["type", "role", "content"], &["id", "status"])?;
    let role = required_nonempty_string(object, "role")?;
    if !matches!(role, "developer" | "system" | "user" | "assistant") {
        return Err(WireAccountingError::Unsupported);
    }
    let replayed = validate_optional_replay_identity(object)?;
    if replayed && role != "assistant" {
        return Err(WireAccountingError::Unsupported);
    }
    counts.increment(ResponsesWireOccurrence::Message)?;
    let parts = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or(WireAccountingError::Malformed)?;
    for part in parts {
        inspect_message_part(part, counts)?;
    }
    Ok(())
}

fn inspect_message_part(
    part: &Value,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    let object = part.as_object().ok_or(WireAccountingError::Malformed)?;
    match required_nonempty_string(object, "type")? {
        "input_text" => {
            validate_members(object, &["type", "text"], &[])?;
            required_string(object, "text")?;
            counts.increment(ResponsesWireOccurrence::InputText)
        }
        "output_text" => {
            validate_members(object, &["type", "text"], &["annotations", "logprobs"])?;
            required_string(object, "text")?;
            if let Some(annotations) = object.get("annotations")
                && !annotations.as_array().is_some_and(Vec::is_empty)
            {
                return Err(WireAccountingError::Unsupported);
            }
            if let Some(logprobs) = object.get("logprobs")
                && !logprobs.is_null()
                && !logprobs.is_array()
            {
                return Err(WireAccountingError::Malformed);
            }
            counts.increment(ResponsesWireOccurrence::OutputText)
        }
        "refusal" => {
            validate_members(object, &["type", "refusal"], &[])?;
            required_string(object, "refusal")?;
            counts.increment(ResponsesWireOccurrence::Refusal)
        }
        _ => Err(WireAccountingError::Unsupported),
    }
}

fn inspect_function_call(
    object: &Map<String, Value>,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    validate_members(
        object,
        &["type", "call_id", "name", "arguments"],
        &["id", "status"],
    )?;
    required_nonempty_string(object, "call_id")?;
    required_nonempty_string(object, "name")?;
    required_string(object, "arguments")?;
    validate_optional_replay_identity(object)?;
    counts.increment(ResponsesWireOccurrence::FunctionCall)
}

fn inspect_function_call_output(
    object: &Map<String, Value>,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    validate_members(object, &["type", "call_id", "output"], &[])?;
    required_nonempty_string(object, "call_id")?;
    match object.get("output") {
        Some(Value::String(_)) => {}
        Some(Value::Array(parts)) => {
            for part in parts {
                let part = part.as_object().ok_or(WireAccountingError::Malformed)?;
                if required_nonempty_string(part, "type")? != "input_text" {
                    return Err(WireAccountingError::Unsupported);
                }
                validate_members(part, &["type", "text"], &[])?;
                required_string(part, "text")?;
                counts.increment(ResponsesWireOccurrence::InputText)?;
            }
        }
        _ => return Err(WireAccountingError::Malformed),
    }
    counts.increment(ResponsesWireOccurrence::FunctionCallOutput)
}

fn inspect_reasoning_replay(
    object: &Map<String, Value>,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    // Grok Build input omits output-only `status`. `content` may carry
    // `reasoning_text` parts that the official client patches in.
    validate_members(
        object,
        &["type", "id", "summary", "encrypted_content"],
        &["content"],
    )?;
    required_nonempty_string(object, "id")?;
    required_nonempty_string(object, "encrypted_content")?;
    counts.increment(ResponsesWireOccurrence::ReasoningReplay)?;
    let summary = object
        .get("summary")
        .and_then(Value::as_array)
        .ok_or(WireAccountingError::Malformed)?;
    for part in summary {
        let part = part.as_object().ok_or(WireAccountingError::Malformed)?;
        validate_members(part, &["type", "text"], &[])?;
        if required_nonempty_string(part, "type")? != "summary_text" {
            return Err(WireAccountingError::Unsupported);
        }
        required_string(part, "text")?;
        counts.increment(ResponsesWireOccurrence::ReasoningSummary)?;
    }
    Ok(())
}

fn inspect_opaque_replay(
    object: &Map<String, Value>,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    validate_required_replay_identity(object)?;
    counts.increment(ResponsesWireOccurrence::OpaqueReplay)
}

fn inspect_function_tool(
    tool: &Value,
    counts: &mut ResponsesWireOccurrenceCounts,
    names: &mut BTreeSet<String>,
) -> Result<(), WireAccountingError> {
    let object = tool.as_object().ok_or(WireAccountingError::Malformed)?;
    validate_members(object, &["type", "name", "description", "parameters"], &[])?;
    if required_nonempty_string(object, "type")? != "function" {
        return Err(WireAccountingError::Unsupported);
    }
    let name = required_nonempty_string(object, "name")?;
    required_string(object, "description")?;
    if !names.insert(name.to_owned()) {
        return Err(WireAccountingError::Malformed);
    }
    let schema = object
        .get("parameters")
        .ok_or(WireAccountingError::Malformed)?;
    if !schema.is_object() {
        return Err(WireAccountingError::Malformed);
    }
    counts.increment(ResponsesWireOccurrence::FunctionTool)?;
    inspect_schema_value(schema, 0, counts)
}

fn inspect_schema_value(
    value: &Value,
    depth: usize,
    counts: &mut ResponsesWireOccurrenceCounts,
) -> Result<(), WireAccountingError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(WireAccountingError::ResourceLimit);
    }
    counts.increment(ResponsesWireOccurrence::SchemaNode)?;
    match value {
        Value::Array(values) => {
            for value in values {
                inspect_schema_value(value, depth + 1, counts)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                inspect_schema_value(value, depth + 1, counts)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

fn validate_reasoning_configuration(body: &Map<String, Value>) -> Result<(), WireAccountingError> {
    match body.get("reasoning") {
        None => Ok(()),
        Some(reasoning) => {
            let reasoning = reasoning
                .as_object()
                .ok_or(WireAccountingError::Malformed)?;
            validate_members(reasoning, &["summary"], &["effort"])?;
            if reasoning.get("summary").and_then(Value::as_str) != Some("concise") {
                return Err(WireAccountingError::Unsupported);
            }
            if let Some(effort) = reasoning.get("effort").and_then(Value::as_str)
                && !matches!(effort, "low" | "medium" | "high" | "xhigh")
            {
                return Err(WireAccountingError::Unsupported);
            }
            if reasoning
                .get("effort")
                .is_some_and(|value| !value.is_string())
            {
                return Err(WireAccountingError::Malformed);
            }
            Ok(())
        }
    }
}

fn validate_tool_choice(
    choice: &Value,
    tool_names: &BTreeSet<String>,
) -> Result<(), WireAccountingError> {
    if choice
        .as_str()
        .is_some_and(|choice| matches!(choice, "auto" | "none" | "required"))
    {
        return Ok(());
    }
    let choice = choice.as_object().ok_or(WireAccountingError::Malformed)?;
    validate_members(choice, &["type", "name"], &[])?;
    if required_nonempty_string(choice, "type")? != "function" {
        return Err(WireAccountingError::Unsupported);
    }
    let name = required_nonempty_string(choice, "name")?;
    if !tool_names.contains(name) {
        return Err(WireAccountingError::Malformed);
    }
    Ok(())
}

fn validate_optional_replay_identity(
    object: &Map<String, Value>,
) -> Result<bool, WireAccountingError> {
    match (object.get("id"), object.get("status")) {
        (None, None) => Ok(false),
        (Some(_), Some(_)) => {
            validate_required_replay_identity(object)?;
            Ok(true)
        }
        _ => Err(WireAccountingError::Malformed),
    }
}

fn validate_required_replay_identity(
    object: &Map<String, Value>,
) -> Result<(), WireAccountingError> {
    required_nonempty_string(object, "id")?;
    if object.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(WireAccountingError::Malformed);
    }
    Ok(())
}

fn validate_members(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<(), WireAccountingError> {
    if required.iter().any(|name| !object.contains_key(*name)) {
        return Err(WireAccountingError::Malformed);
    }
    if object
        .keys()
        .any(|name| !required.contains(&name.as_str()) && !optional.contains(&name.as_str()))
    {
        return Err(WireAccountingError::Unsupported);
    }
    Ok(())
}

fn required_nonempty_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, WireAccountingError> {
    let value = required_string(object, field)?;
    if value.is_empty() {
        return Err(WireAccountingError::Malformed);
    }
    Ok(value)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, WireAccountingError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(WireAccountingError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn closed_wire_shapes_are_counted_exactly() -> Result<(), WireAccountingError> {
        let body = json!({
            "model": "model-a",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
                {"id":"message-1","type":"message","status":"completed","role":"assistant","content":[
                    {"type":"output_text","text":"answer","annotations":[],"logprobs":[]},
                    {"type":"refusal","refusal":"bounded refusal"}
                ]},
                {"type":"function_call","call_id":"call-1","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"call-1","output":[
                    {"type":"input_text","text":"part one"},
                    {"type":"input_text","text":"part two"}
                ]},
                {"id":"reasoning-1","type":"reasoning","summary":[
                    {"type":"summary_text","text":"checked"}
                ],"encrypted_content":"opaque-reasoning"},
                {"id":"future-1","type":"future_output","status":"completed","payload":{"opaque":true}}
            ],
            "stream": true,
            "tools": [{
                "type":"function",
                "name":"read",
                "description":"Read a value",
                "parameters":{
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "type":"object",
                    "properties":{"path":{"type":"string"}},
                    "required":["path"],
                    "additionalProperties":false
                }
            }],
            "tool_choice":"auto",
            "max_output_tokens":64,
            "reasoning":{"effort":"xhigh","summary":"concise"},
            "store":false,
            "include":["reasoning.encrypted_content"]
        });
        let counts = inspect_wire_occurrences(&body)?;
        let expected = [
            (ResponsesWireOccurrence::Message, 2),
            (ResponsesWireOccurrence::InputText, 3),
            (ResponsesWireOccurrence::OutputText, 1),
            (ResponsesWireOccurrence::Refusal, 1),
            (ResponsesWireOccurrence::FunctionCall, 1),
            (ResponsesWireOccurrence::FunctionCallOutput, 1),
            (ResponsesWireOccurrence::FunctionTool, 1),
            (ResponsesWireOccurrence::SchemaNode, 9),
            (ResponsesWireOccurrence::ReasoningReplay, 1),
            (ResponsesWireOccurrence::ReasoningSummary, 1),
            (ResponsesWireOccurrence::OpaqueReplay, 1),
        ];
        for (occurrence, expected) in expected {
            assert_eq!(counts.count(occurrence), expected, "{occurrence:?}");
        }
        assert_eq!(counts.total(), 22);
        Ok(())
    }

    #[test]
    fn replayed_output_text_accepts_only_closed_logprob_container_shapes()
    -> Result<(), WireAccountingError> {
        let body = |logprobs| {
            json!({
                "model":"model-a",
                "input":[{
                    "id":"message-1",
                    "type":"message",
                    "status":"completed",
                    "role":"assistant",
                    "content":[{
                        "type":"output_text",
                        "text":"answer",
                        "annotations":[],
                        "logprobs":logprobs
                    }]
                }],
                "stream":true,
                "store":false,
                "include":["reasoning.encrypted_content"]
            })
        };
        for value in [
            Value::Null,
            json!([]),
            json!([{"token":"a","logprob":-0.1}]),
        ] {
            let counts = inspect_wire_occurrences(&body(value))?;
            assert_eq!(counts.count(ResponsesWireOccurrence::OutputText), 1);
        }
        assert_eq!(
            inspect_wire_occurrences(&body(json!("not-an-array"))),
            Err(WireAccountingError::Malformed)
        );
        Ok(())
    }

    #[test]
    fn unknown_and_malformed_shapes_are_rejected() {
        let base = json!({
            "model":"model-a",
            "input":[],
            "stream":true
        });
        let mut cases = Vec::new();
        let mut unknown_top = base.clone();
        unknown_top["unknown"] = json!(true);
        cases.push(unknown_top);
        let mut image = base.clone();
        image["input"] = json!([{"type":"message","role":"user","content":[
            {"type":"input_image","image_url":"https://example.test/image"}
        ]}]);
        cases.push(image);
        let mut custom = base.clone();
        custom["input"] = json!([{
            "id":"custom-1","type":"custom_tool_call","status":"completed",
            "call_id":"call-1","name":"custom","input":"x"
        }]);
        cases.push(custom);
        let mut reasoning = base.clone();
        reasoning["input"] = json!([{
            "id":"reasoning-1","type":"reasoning","summary":[]
        }]);
        cases.push(reasoning);
        let mut reasoning_status = base.clone();
        reasoning_status["input"] = json!([{
            "id":"reasoning-1","type":"reasoning","status":"completed","summary":[],
            "encrypted_content":"opaque"
        }]);
        cases.push(reasoning_status);
        let mut opaque = base.clone();
        opaque["input"] = json!([{"type":"future_output","payload":{}}]);
        cases.push(opaque);
        let mut invalid_schema = base;
        invalid_schema["tools"] = json!([{
            "type":"function","name":"read","description":"read","parameters":"object"
        }]);
        invalid_schema["tool_choice"] = json!("auto");
        cases.push(invalid_schema);

        for case in cases {
            assert!(inspect_wire_occurrences(&case).is_err(), "{case}");
        }
    }

    #[test]
    fn every_counter_and_the_aggregate_are_checked_and_bounded() {
        let mut aggregate = ResponsesWireOccurrenceCounts::empty();
        aggregate.total = MAX_TOTAL_OCCURRENCES;
        assert_eq!(
            aggregate.increment(ResponsesWireOccurrence::Message),
            Err(WireAccountingError::ResourceLimit)
        );

        let mut individual = ResponsesWireOccurrenceCounts::empty();
        individual.counts[ResponsesWireOccurrence::Message.index()] = MAX_TOTAL_OCCURRENCES;
        assert_eq!(
            individual.increment(ResponsesWireOccurrence::Message),
            Err(WireAccountingError::ResourceLimit)
        );
    }
}
