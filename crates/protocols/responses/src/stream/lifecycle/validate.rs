//! Item, content and terminal validators the lifecycle applies to done events.

use serde_json::Value;

use super::{
    ContentLifecycle, ItemLifecycle, LifecycleDialectPolicy, ReasoningLifecycleMode,
    ResponsesLifecycleError, invalid_lifecycle,
};

/// Strict terminal items must be identical, except an OpenAI Platform reasoning
/// item may carry a refreshed `encrypted_content`. The caller keeps `terminal`
/// unchanged: that native is the replay authority.
pub(super) fn strict_terminal_item_matches(
    done: &Value,
    terminal: &Value,
    policy: LifecycleDialectPolicy,
) -> bool {
    if terminal == done {
        return true;
    }
    if !policy.reasoning_terminal_ciphertext_refresh
        || item_kind(done) != Some("reasoning")
        || item_kind(terminal) != Some("reasoning")
    {
        return false;
    }
    without_encrypted_content(done) == without_encrypted_content(terminal)
}

fn without_encrypted_content(value: &Value) -> Value {
    let mut cloned = value.clone();
    if let Some(object) = cloned.as_object_mut() {
        object.remove("encrypted_content");
    }
    cloned
}

pub(super) fn item_kind(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

pub(super) fn item_status(item: &Value) -> Option<&str> {
    item.get("status").and_then(Value::as_str)
}

pub(super) fn output_item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

pub(super) fn require_added_status(
    status: Option<&str>,
    completed_label: bool,
) -> Result<(), ResponsesLifecycleError> {
    if matches!(status, None | Some("in_progress"))
        || (completed_label && status == Some("completed"))
    {
        Ok(())
    } else {
        Err(invalid_lifecycle(
            "added output item status is not in progress",
        ))
    }
}

fn require_done_status(status: Option<&str>) -> Result<(), ResponsesLifecycleError> {
    // Platform/Codex subscription streams may omit item `status` on
    // `output_item.done` (same soft posture as create). When present, accept
    // the documented terminal item labels plus the occasional `done` alias.
    if matches!(status, None | Some("completed" | "incomplete" | "done")) {
        Ok(())
    } else {
        Err(invalid_lifecycle(
            "completed output item has invalid status",
        ))
    }
}

pub(super) fn completed_content_part(
    content: &ContentLifecycle,
    policy: LifecycleDialectPolicy,
) -> Result<Value, ResponsesLifecycleError> {
    let mut expected = content.added.clone();
    let text_field = match expected.get("type").and_then(Value::as_str) {
        Some("output_text") => Some("text"),
        Some("refusal") if policy.content_text_done => Some("refusal"),
        Some("reasoning_text") if policy.reasoning_content_text => Some("text"),
        _ => None,
    };
    if let Some(text_field) = text_field {
        expected
            .as_object_mut()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "part" })?
            .insert(text_field.to_owned(), Value::String(content.text.clone()));
    }
    Ok(expected)
}

pub(super) fn validate_function_event_target(
    state: &ItemLifecycle,
    item_id: Option<&str>,
) -> Result<(), ResponsesLifecycleError> {
    if !matches!(
        item_kind(&state.added),
        Some("function_call" | "custom_tool_call")
    ) {
        return Err(invalid_lifecycle(
            "arguments event targets a non-function item",
        ));
    }
    if item_id != output_item_id(&state.added) {
        return Err(invalid_lifecycle(
            "arguments event item ID does not match its function item",
        ));
    }
    Ok(())
}

pub(super) fn validate_reasoning_content_target(
    state: &ItemLifecycle,
    item_id: Option<&str>,
) -> Result<(), ResponsesLifecycleError> {
    if item_kind(&state.added) != Some("reasoning") {
        return Err(invalid_lifecycle(
            "reasoning text event targets a non-reasoning item",
        ));
    }
    if item_id.is_some_and(|item_id| Some(item_id) != output_item_id(&state.added)) {
        return Err(invalid_lifecycle(
            "reasoning text event item ID does not match its reasoning item",
        ));
    }
    Ok(())
}

pub(super) fn validate_done_item(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    if output_item_id(&state.added) != output_item_id(done) {
        return Err(invalid_lifecycle("output item ID changed at completion"));
    }
    let added_kind = item_kind(&state.added);
    let done_kind = item_kind(done);
    match (added_kind, done_kind) {
        (Some("message"), Some("message")) => validate_message_done(state, done, policy),
        (Some("reasoning"), Some("reasoning")) => validate_reasoning_done(state, done, policy),
        (Some("function_call"), Some("function_call"))
        | (Some("custom_tool_call"), Some("custom_tool_call")) => {
            validate_function_done(state, done, policy)
        }
        (Some("compaction"), Some("compaction")) => validate_compaction_done(done),
        (Some(added), Some(done_k)) if added == done_k => {
            // Opaque: same non-typed kind.
            validate_opaque_done(&state.added, done, policy)
        }
        _ => Err(invalid_lifecycle("output item type changed at completion")),
    }
}

fn validate_message_done(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    let object = done
        .as_object()
        .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
    require_done_status(item_status(done))?;
    let role = object.get("role").and_then(Value::as_str).unwrap_or("");
    let added_role = state
        .added
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("");
    let native_content = done
        .get("content")
        .and_then(Value::as_array)
        .ok_or(ResponsesLifecycleError::InvalidField { field: "content" })?;
    if role != added_role || role != "assistant" || native_content.len() != state.content.len() {
        return Err(invalid_lifecycle("message shape changed at completion"));
    }
    for (index, part) in native_content.iter().enumerate() {
        let index = u32::try_from(index)
            .map_err(|_| invalid_lifecycle("message content index overflowed"))?;
        if policy.content_text_done {
            let content = state
                .content
                .get(&index)
                .ok_or_else(|| invalid_lifecycle("item completed before its content part"))?;
            let accumulated = completed_content_part(content, policy)?;
            let expected = content.done.as_ref().unwrap_or(&accumulated);
            if part != expected {
                return Err(invalid_lifecycle(
                    "completed item changed a completed content part",
                ));
            }
        } else {
            let expected = state
                .content
                .get(&index)
                .and_then(|content| content.done.as_ref())
                .ok_or_else(|| invalid_lifecycle("item completed before its content part"))?;
            if part != expected {
                return Err(invalid_lifecycle(
                    "completed item changed a completed content part",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_incomplete_text_snapshot(
    state: &ItemLifecycle,
    terminal: &Value,
) -> Result<(), ResponsesLifecycleError> {
    let object = terminal
        .as_object()
        .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
    if item_kind(&state.added) != Some("message")
        || item_kind(terminal) != Some("message")
        || output_item_id(&state.added) != output_item_id(terminal)
        || item_status(terminal) != Some("incomplete")
        || state.added.get("role").and_then(Value::as_str) != Some("assistant")
        || object.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return Err(invalid_lifecycle(
            "incomplete text snapshot changed message identity or status",
        ));
    }
    let Some(parts) = object.get("content").and_then(Value::as_array) else {
        return Err(ResponsesLifecycleError::InvalidField { field: "content" });
    };
    let Some(content) = state.content.get(&0) else {
        return Err(invalid_lifecycle(
            "incomplete text snapshot lacks streamed content",
        ));
    };
    if parts.len() != 1
        || state.content.len() != 1
        || content.added.get("type").and_then(Value::as_str) != Some("output_text")
        || content.done.is_some()
        || content.text_done.is_some()
        || content.text.is_empty()
        || parts[0] != completed_content_part(content, LifecycleDialectPolicy::grok())?
    {
        return Err(invalid_lifecycle(
            "incomplete text snapshot differs from streamed text",
        ));
    }
    let mut expected = state.added.clone();
    let expected_object = expected
        .as_object_mut()
        .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
    expected_object.insert("status".to_owned(), Value::String("incomplete".to_owned()));
    expected_object.insert("content".to_owned(), Value::Array(parts.clone()));
    if &expected != terminal {
        return Err(invalid_lifecycle(
            "incomplete text snapshot changed added message fields",
        ));
    }
    Ok(())
}

fn validate_reasoning_done(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    require_done_status(item_status(done))?;
    let native_summary = done
        .get("summary")
        .and_then(Value::as_array)
        .ok_or(ResponsesLifecycleError::InvalidField { field: "summary" })?;
    if policy.reasoning_content_text && !state.content.is_empty() {
        if !native_summary.is_empty() || !state.reasoning_summaries.is_empty() {
            return Err(invalid_lifecycle(
                "reasoning content text must not be relabeled as summary",
            ));
        }
        let native_content = done
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ResponsesLifecycleError::InvalidField { field: "content" })?;
        if native_content.len() != state.content.len() {
            return Err(invalid_lifecycle("reasoning content count changed"));
        }
        for (index, part) in native_content.iter().enumerate() {
            let index = u32::try_from(index)
                .map_err(|_| invalid_lifecycle("reasoning content index overflowed"))?;
            let content = state.content.get(&index).ok_or_else(|| {
                invalid_lifecycle("reasoning item completed before its content part")
            })?;
            let expected = content.done.as_ref().ok_or_else(|| {
                invalid_lifecycle("reasoning item completed before its content part")
            })?;
            if content.text_done.as_deref() != Some(content.text.as_str()) || part != expected {
                return Err(invalid_lifecycle(
                    "reasoning completion differs from reasoning content text",
                ));
            }
        }
        return Ok(());
    }
    match policy.reasoning {
        ReasoningLifecycleMode::DeltaOnly => {
            if native_summary.len() != state.reasoning_summaries.len() {
                return Err(invalid_lifecycle("reasoning summary count changed"));
            }
            for (index, part) in native_summary.iter().enumerate() {
                let index = u32::try_from(index)
                    .map_err(|_| invalid_lifecycle("reasoning summary index overflowed"))?;
                if part.get("type").and_then(Value::as_str) != Some("summary_text")
                    || part.get("text").and_then(Value::as_str)
                        != state
                            .reasoning_summaries
                            .get(&index)
                            .map(|summary| summary.text.as_str())
                {
                    return Err(invalid_lifecycle(
                        "reasoning completion does not match summary deltas",
                    ));
                }
            }
        }
        ReasoningLifecycleMode::FullParts => {
            // Typed summary length is always derived from native after parse; the
            // OpenRouter golden still checks each summary_text against deltas and
            // that every lifecycle index remains a summary_text part.
            for (index, part) in native_summary.iter().enumerate() {
                let index = u32::try_from(index)
                    .map_err(|_| invalid_lifecycle("reasoning summary index overflowed"))?;
                if part.get("type").and_then(Value::as_str) == Some("summary_text")
                    && part.get("text").and_then(Value::as_str)
                        != state
                            .reasoning_summaries
                            .get(&index)
                            .map(|summary| summary.text.as_str())
                {
                    return Err(invalid_lifecycle(
                        "reasoning completion does not match summary deltas",
                    ));
                }
            }
            if state.reasoning_summaries.keys().any(|index| {
                usize::try_from(*index)
                    .ok()
                    .and_then(|index| native_summary.get(index))
                    .is_none_or(|part| {
                        part.get("type").and_then(Value::as_str) != Some("summary_text")
                    })
            }) {
                return Err(invalid_lifecycle(
                    "reasoning summary lifecycle changed its typed part",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn soft_seed_function_arguments_from_done(state: &mut ItemLifecycle, done: &Value) {
    let kind = item_kind(&state.added);
    if !matches!(kind, Some("function_call" | "custom_tool_call")) {
        return;
    }
    if state.function_arguments_done.is_some() {
        return;
    }
    let args_field = if kind == Some("custom_tool_call") {
        "input"
    } else {
        "arguments"
    };
    let arguments = done
        .get(args_field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // SoftSeedEmptyOnly: seed from done only when no deltas; keep non-empty deltas
    // when Codex omits `function_call_arguments.done` but rewrites on item.done.
    if state.function_arguments.is_empty() {
        state.function_arguments = arguments;
    }
    state.function_arguments_done = Some(state.function_arguments.clone());
}

fn validate_function_done(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    require_done_status(item_status(done))?;
    let added_call_id = state
        .added
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let added_name = state
        .added
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let call_id = done.get("call_id").and_then(Value::as_str).unwrap_or("");
    let name = done.get("name").and_then(Value::as_str).unwrap_or("");
    let args_field = if item_kind(&state.added) == Some("custom_tool_call") {
        "input"
    } else {
        "arguments"
    };
    let arguments = done.get(args_field).and_then(Value::as_str).unwrap_or("");
    if call_id != added_call_id || name != added_name {
        return Err(invalid_lifecycle(
            "function completion changed correlation or arguments",
        ));
    }
    // SoftSeedEmptyOnly: when not strict, done item args may re-serialize vs
    // kept deltas; require lifecycle args tracking completed consistently.
    if policy.strict_terminal_output {
        if arguments != state.function_arguments
            || state.function_arguments_done.as_deref() != Some(arguments)
        {
            return Err(invalid_lifecycle(
                "function completion changed correlation or arguments",
            ));
        }
    } else if state.function_arguments_done.as_deref() != Some(state.function_arguments.as_str()) {
        return Err(invalid_lifecycle(
            "function completion changed correlation or arguments",
        ));
    }
    Ok(())
}

fn validate_compaction_done(done: &Value) -> Result<(), ResponsesLifecycleError> {
    require_done_status(item_status(done))?;
    let encrypted = done
        .get("encrypted_content")
        .and_then(Value::as_str)
        .unwrap_or("");
    if encrypted.is_empty() {
        return Err(invalid_lifecycle(
            "compaction item omitted encrypted_content",
        ));
    }
    Ok(())
}

fn validate_opaque_done(
    added_native: &Value,
    native: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    let Some(status) = item_status(native) else {
        if policy.strict_terminal_output {
            return Err(invalid_lifecycle("opaque completion status is missing"));
        }
        // Codex/ChatGPT may omit status on opaque `output_item.done`.
        let mut added = added_native.clone();
        let mut done = native.clone();
        added
            .as_object_mut()
            .ok_or_else(|| invalid_lifecycle("opaque added item is not an object"))?
            .remove("status");
        if let Some(object) = done.as_object_mut() {
            object.remove("status");
        }
        if added != done {
            return Err(invalid_lifecycle(
                "opaque item changed fields other than terminal status",
            ));
        }
        return Ok(());
    };
    if !matches!(status, "completed" | "incomplete") {
        return Err(invalid_lifecycle(
            "opaque completion status is not terminal",
        ));
    }
    let mut expected = added_native.clone();
    expected
        .as_object_mut()
        .ok_or_else(|| invalid_lifecycle("opaque added item is not an object"))?
        .insert("status".to_owned(), Value::String(status.to_owned()));
    if &expected != native {
        return Err(invalid_lifecycle(
            "opaque item changed fields other than terminal status",
        ));
    }
    Ok(())
}

pub(super) fn validate_terminal_semantics(
    event_status: &'static str,
    response_status: &str,
    has_incomplete_details: bool,
    has_error: bool,
) -> Result<(), ResponsesLifecycleError> {
    let valid = match event_status {
        "completed" => response_status == "completed" && !has_incomplete_details && !has_error,
        "incomplete" => response_status == "incomplete" && has_incomplete_details && !has_error,
        "failed" => response_status == "failed" && !has_incomplete_details && has_error,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_lifecycle(
            "terminal event and response outcome semantics disagree",
        ))
    }
}
