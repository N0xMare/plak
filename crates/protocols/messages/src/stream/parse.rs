use plakous_model::{ContentBlock, FinishDetails, FinishReason, ToolExecutionOwner};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{
    MessageDeltaEvent, MessageStartEvent, MessagesStopDetails, MessagesStopDetailsKind,
    MessagesStopOutcome, MessagesStopReason, MessagesStreamDialectHooks, MessagesStreamError,
    MessagesStreamErrorKind, MessagesStreamErrorPhase, MessagesStreamExtensions, ThinkingDeltaWire,
    invalid_response,
};

pub(super) fn parse_event<T: for<'de> Deserialize<'de>>(
    data: &str,
) -> Result<T, MessagesStreamError> {
    serde_json::from_str(data)
        .map_err(|error| invalid_response(format!("invalid Messages event payload: {error}")))
}

pub(super) fn parse_message_start(
    data: &str,
    extensions: MessagesStreamExtensions,
) -> Result<
    (
        MessageStartEvent,
        Option<plakous_model::PromptCacheDiagnosis>,
    ),
    MessagesStreamError,
> {
    match extensions {
        MessagesStreamExtensions::Strict => {
            // API-key and Claude OAuth both admit `message.diagnostics` as
            // null or advisory cache telemetry. Malformed diagnostics record
            // nothing and do not fail the turn. Unknown keys inside
            // `diagnostics` and inside `cache_miss_reason` are ignored.
            // Every other unknown field still fails closed.
            let mut value = parse_event::<serde_json::Value>(data)?;
            let diagnosis = match value
                .get_mut("message")
                .and_then(serde_json::Value::as_object_mut)
            {
                Some(message) => admit_message_diagnostics(message),
                None => None,
            };
            let event = serde_json::from_value(value).map_err(|error| {
                invalid_response(format!("invalid Messages event payload: {error}"))
            })?;
            Ok((event, diagnosis))
        }
        MessagesStreamExtensions::ClaudeOauthV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let message = value
                .get_mut("message")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("Claude OAuth message_start omitted message"))?;
            if let Some(context_management) = message.remove("context_management") {
                validate_claude_context_management(&context_management)?;
            }
            let diagnosis = admit_message_diagnostics(message);
            if let Some(transformations) = message.remove("input_transformations") {
                validate_claude_input_transformations(&transformations)?;
            }
            let event = serde_json::from_value(value).map_err(|error| {
                invalid_response(format!(
                    "invalid Claude OAuth message_start payload: {error}"
                ))
            })?;
            Ok((event, diagnosis))
        }
        MessagesStreamExtensions::OpenRouterV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let message = value
                .get_mut("message")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("OpenRouter message_start omitted message"))?;
            if let Some(provider) = message.remove("provider") {
                let valid = provider
                    .as_str()
                    .is_some_and(|provider| !provider.is_empty() && provider.len() <= 256);
                if !valid {
                    return Err(invalid_response(
                        "OpenRouter message_start provider was not a bounded string",
                    ));
                }
            }
            if let Some(container) = message.remove("container")
                && !container.is_null()
            {
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "OpenRouter message_start non-null context container is not admitted",
                ));
            }
            let usage = message
                .get_mut("usage")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("OpenRouter message_start omitted usage"))?;
            normalize_openrouter_usage(usage)?;
            let event = serde_json::from_value(value).map_err(|error| {
                invalid_response(format!("invalid OpenRouter message_start payload: {error}"))
            })?;
            Ok((event, None))
        }
    }
}

fn admit_message_diagnostics(
    message: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<plakous_model::PromptCacheDiagnosis> {
    let diagnostics = message.remove("diagnostics")?;
    parse_message_diagnostics(&diagnostics)
}

/// Advisory cache telemetry. A malformed value records nothing and never fails the turn.
///
/// Unknown keys are ignored. Only `type` and `cache_missed_input_tokens` are
/// read, and the raw type string is not retained.
fn parse_message_diagnostics(
    diagnostics: &serde_json::Value,
) -> Option<plakous_model::PromptCacheDiagnosis> {
    let reason = diagnostics
        .as_object()?
        .get("cache_miss_reason")?
        .as_object()?;
    let kind = reason.get("type")?.as_str()?;
    let missed_input_tokens = match reason.get("cache_missed_input_tokens") {
        None => None,
        Some(value) => Some(non_negative_cache_miss_tokens(value)?),
    };
    Some(plakous_model::PromptCacheDiagnosis {
        reason: classify_cache_miss_reason(kind),
        missed_input_tokens,
    })
}

fn classify_cache_miss_reason(kind: &str) -> plakous_model::PromptCacheMissReason {
    match kind {
        "model_changed" => plakous_model::PromptCacheMissReason::ModelChanged,
        "system_changed" => plakous_model::PromptCacheMissReason::SystemChanged,
        "tools_changed" => plakous_model::PromptCacheMissReason::ToolsChanged,
        "messages_changed" => plakous_model::PromptCacheMissReason::MessagesChanged,
        "previous_message_not_found" => {
            plakous_model::PromptCacheMissReason::PreviousMessageNotFound
        }
        "unavailable" => plakous_model::PromptCacheMissReason::Unavailable,
        _ => plakous_model::PromptCacheMissReason::Other,
    }
}

fn non_negative_cache_miss_tokens(value: &serde_json::Value) -> Option<u64> {
    // `as_u64` rejects negatives, floats, and integers above `u64::MAX`.
    value.as_number().and_then(serde_json::Number::as_u64)
}

fn validate_claude_input_transformations(
    value: &serde_json::Value,
) -> Result<(), MessagesStreamError> {
    // The pinned native client continues and retains the original signed
    // thinking after this server-side input notification. It is not authority
    // for client-side history surgery or a context-window change.
    let valid = value.as_array().is_some_and(|items| {
        items.iter().all(|item| {
            let Some(object) = item.as_object() else {
                return false;
            };
            if object.len() != 3
                || object.get("type").and_then(serde_json::Value::as_str)
                    != Some("thinking_dropped")
                || object.get("reason").and_then(serde_json::Value::as_str)
                    != Some("prefix_binding_mismatch")
            {
                return false;
            }
            let Some((message, block)) = object
                .get("path")
                .and_then(serde_json::Value::as_str)
                .and_then(|path| path.strip_prefix("messages."))
                .and_then(|path| path.split_once(".content."))
            else {
                return false;
            };
            [message, block].into_iter().all(|index| {
                !index.is_empty()
                    && index.len() <= 6
                    && index.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
    });
    if !valid {
        return Err(invalid_response(
            "Claude OAuth input transformation had an unsupported shape",
        ));
    }
    Ok(())
}

pub(super) fn parse_thinking_delta(
    data: &str,
    extensions: MessagesStreamExtensions,
) -> Result<ThinkingDeltaWire, MessagesStreamError> {
    if extensions != MessagesStreamExtensions::ClaudeOauthV1 {
        return parse_event(data);
    }
    let mut value = parse_event::<serde_json::Value>(data)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_response("Claude OAuth thinking delta was not an object"))?;
    if let Some(estimated_tokens) = object.remove("estimated_tokens")
        && !estimated_tokens.is_null()
        && estimated_tokens.as_u64().is_none()
    {
        let shape = match estimated_tokens {
            serde_json::Value::Number(_) => "non-unsigned number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Null => "null",
        };
        return Err(invalid_response(format!(
            "Claude OAuth thinking token estimate had unsupported {shape} shape"
        )));
    }
    serde_json::from_value(value)
        .map_err(|error| invalid_response(format!("invalid Claude OAuth thinking delta: {error}")))
}

pub(super) fn parse_message_delta(
    data: &str,
    extensions: MessagesStreamExtensions,
) -> Result<MessageDeltaEvent, MessagesStreamError> {
    match extensions {
        MessagesStreamExtensions::Strict => parse_event(data),
        MessagesStreamExtensions::ClaudeOauthV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let event = value
                .as_object_mut()
                .ok_or_else(|| invalid_response("Claude OAuth message_delta was not an object"))?;
            if let Some(context_management) = event.remove("context_management") {
                validate_claude_context_management(&context_management)?;
            }
            serde_json::from_value(value).map_err(|error| {
                invalid_response(format!(
                    "invalid Claude OAuth message_delta payload: {error}"
                ))
            })
        }
        MessagesStreamExtensions::OpenRouterV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let event = value
                .as_object_mut()
                .ok_or_else(|| invalid_response("OpenRouter message_delta was not an object"))?;
            if let Some(context) = event.remove("context_management")
                && !context.is_null()
            {
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "OpenRouter non-null context_management is not admitted",
                ));
            }
            let usage = event
                .get_mut("usage")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("OpenRouter message_delta omitted usage"))?;
            normalize_openrouter_usage(usage)?;
            serde_json::from_value(value).map_err(|error| {
                invalid_response(format!("invalid OpenRouter message_delta payload: {error}"))
            })
        }
    }
}

fn validate_claude_context_management(
    value: &serde_json::Value,
) -> Result<(), MessagesStreamError> {
    if value.is_null() {
        return Ok(());
    }
    let object = value.as_object().ok_or_else(|| {
        invalid_response("Claude OAuth context_management was not null or an object")
    })?;
    if object.len() != 1 || !object.contains_key("applied_edits") {
        return Err(invalid_response(
            "Claude OAuth context_management contained an unknown field",
        ));
    }
    let edits = object["applied_edits"].as_array().ok_or_else(|| {
        invalid_response("Claude OAuth context_management applied_edits was not an array")
    })?;
    if edits.len() > 64 {
        return Err(invalid_response(
            "Claude OAuth context_management exceeded the applied edit bound",
        ));
    }
    for edit in edits {
        if !edit.is_object() {
            return Err(invalid_response(
                "Claude OAuth context_management applied edit was not an object",
            ));
        }
        let bytes = serde_json::to_vec(edit)
            .map_err(|_| invalid_response("Claude OAuth context_management edit was invalid"))?;
        if bytes.len() > 16 * 1024 {
            return Err(invalid_response(
                "Claude OAuth context_management edit exceeded the byte bound",
            ));
        }
    }
    // Applied edits describe provider-local input processing for this call. The
    // canonical request remains complete and the route has no continuation
    // locator, so this metadata grants no replay or recovery authority.
    Ok(())
}

fn normalize_openrouter_usage(
    usage: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), MessagesStreamError> {
    for field in ["cache_creation_input_tokens", "cache_read_input_tokens"] {
        if usage.get(field).is_some_and(serde_json::Value::is_null) {
            usage.remove(field);
        }
    }
    if let Some(cost) = usage.remove("cost")
        && !cost.is_number()
    {
        return Err(invalid_response("OpenRouter usage cost was not a number"));
    }
    if let Some(is_byok) = usage.remove("is_byok")
        && !is_byok.is_boolean()
    {
        return Err(invalid_response(
            "OpenRouter usage is_byok was not a boolean",
        ));
    }
    if let Some(details) = usage.remove("cost_details") {
        validate_openrouter_cost_details(&details)?;
    }
    Ok(())
}

fn validate_openrouter_cost_details(
    details: &serde_json::Value,
) -> Result<(), MessagesStreamError> {
    const FIELDS: [&str; 3] = [
        "upstream_inference_cost",
        "upstream_inference_prompt_cost",
        "upstream_inference_completions_cost",
    ];
    let Some(details) = details.as_object() else {
        return Err(invalid_response(
            "OpenRouter usage cost_details was not an object",
        ));
    };
    if details
        .iter()
        .any(|(name, value)| FIELDS.contains(&name.as_str()) && !value.is_number())
    {
        return Err(invalid_response(
            "OpenRouter usage cost_details had a non-numeric cost",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ToolCaller {
    AbsentDirect,
    ExplicitDirect,
    Programmatic { tool_id: String },
}

pub(super) fn parse_tool_caller(
    caller: Option<&RawValue>,
    dialect: &MessagesStreamDialectHooks,
) -> Result<ToolCaller, MessagesStreamError> {
    let Some(caller) = caller else {
        return Ok(ToolCaller::AbsentDirect);
    };
    let value: serde_json::Value = serde_json::from_str(caller.get())
        .map_err(|error| invalid_response(format!("invalid Messages tool caller: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response("Messages tool caller must be a typed JSON object"))?;
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_response("Messages tool caller requires a string type"))?;
    match kind {
        "direct" if object.len() == 1 => Ok(ToolCaller::ExplicitDirect),
        "direct" => Err(invalid_response(
            "Messages direct tool caller contained unsupported lineage fields",
        )),
        kind if dialect.programmatic_tool_caller() == Some(kind) => {
            if object.len() != 2 {
                return Err(invalid_response(
                    "Messages programmatic tool caller contained unsupported fields",
                ));
            }
            let tool_id = object
                .get("tool_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    invalid_response(
                        "Messages programmatic tool caller requires a nonempty tool_id",
                    )
                })?;
            Ok(ToolCaller::Programmatic {
                tool_id: tool_id.to_owned(),
            })
        }
        _ => Err(MessagesStreamError::new(
            MessagesStreamErrorKind::UnsupportedSemantic,
            MessagesStreamErrorPhase::Stream,
            "unsupported Messages tool caller type",
        )),
    }
}

pub(super) fn raw_block_start_has_hosted_effect(data: &str) -> bool {
    let Ok(serde_json::Value::Object(event)) = serde_json::from_str(data) else {
        return false;
    };
    if event.get("type").and_then(serde_json::Value::as_str) != Some("content_block_start") {
        return false;
    }
    let Some(serde_json::Value::Object(block)) = event.get("content_block") else {
        return false;
    };
    if block
        .get("container_id")
        .is_some_and(|value| !value.is_null())
    {
        return true;
    }
    if let Some(caller) = block.get("caller").filter(|caller| !caller.is_null()) {
        let proven_direct = matches!(caller,
            serde_json::Value::Object(caller)
                if caller.len() == 1
                    && caller.get("type").and_then(serde_json::Value::as_str) == Some("direct")
        );
        if !proven_direct {
            return true;
        }
    }
    block
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind.starts_with("server_tool") || kind == "web_search_tool_result")
}

pub(super) fn reject_citations(citations: Option<&RawValue>) -> Result<(), MessagesStreamError> {
    let Some(citations) = citations else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_str(citations.get())
        .map_err(|error| invalid_response(format!("invalid Messages citations: {error}")))?;
    if value.is_null() || value.as_array().is_some_and(Vec::is_empty) {
        Ok(())
    } else {
        Err(MessagesStreamError::new(
            MessagesStreamErrorKind::UnsupportedSemantic,
            MessagesStreamErrorPhase::Stream,
            "Messages citations are explicitly excluded by adapter revision 1",
        ))
    }
}

pub(super) fn validate_opaque_string(
    value: &RawValue,
    field: &str,
) -> Result<(), MessagesStreamError> {
    serde_json::from_str::<String>(value.get())
        .map(|_| ())
        .map_err(|_| {
            invalid_response(format!(
                "Messages {field} must be an exact JSON string payload"
            ))
        })
}

pub(super) fn nonempty_opaque_signature(
    value: Option<Box<RawValue>>,
    field: &str,
) -> Result<Option<Box<RawValue>>, MessagesStreamError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = serde_json::from_str::<String>(value.get()).map_err(|_| {
        invalid_response(format!(
            "Messages {field} must be an exact JSON string payload"
        ))
    })?;
    Ok((!parsed.is_empty()).then_some(value))
}

pub(super) fn parse_stop_details(
    raw: Box<RawValue>,
) -> Result<MessagesStopDetails, MessagesStreamError> {
    #[derive(Deserialize)]
    struct StopDetailsEnvelope {
        #[serde(rename = "type")]
        kind: String,
    }

    let envelope: StopDetailsEnvelope = serde_json::from_str(raw.get())
        .map_err(|error| invalid_response(format!("invalid Messages stop_details: {error}")))?;
    Ok(MessagesStopDetails {
        kind: MessagesStopDetailsKind::parse(&envelope.kind)?,
        raw,
    })
}

pub(super) fn validate_stop_blocks(
    reason: MessagesStopReason,
    blocks: &[ContentBlock],
    hosted_effect_capable: bool,
    container_id: Option<&str>,
) -> Result<(), MessagesStreamError> {
    let client_proposals = blocks
        .iter()
        .filter(|block| {
            matches!(
                block, ContentBlock::ToolCall(call)
                    if !matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. })
            )
        })
        .count();
    let provider_proposals = blocks
        .iter()
        .filter(|block| {
            matches!(
                block, ContentBlock::ToolCall(call)
                    if matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. })
            )
        })
        .count();
    let has_client_tool = client_proposals != 0;
    let has_refusal = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Refusal(_)));
    if reason != MessagesStopReason::Refusal
        && matches!(reason, MessagesStopReason::ToolUse) != has_client_tool
    {
        return Err(invalid_response(format!(
            "Messages terminal tool proposal mismatch: stop_reason={} client_proposals={client_proposals} provider_proposals={provider_proposals}; tool_use requires a client proposal and other non-refusal stop reasons cannot complete client proposals",
            reason.wire_name(),
        )));
    }
    if matches!(reason, MessagesStopReason::PauseTurn)
        && (!hosted_effect_capable || container_id.is_none() || has_client_tool)
    {
        return Err(invalid_response(
            "Messages pause_turn requires an admitted hosted request, an exact container, and no client tool calls",
        ));
    }
    if has_refusal && !matches!(reason, MessagesStopReason::Refusal) {
        return Err(invalid_response(
            "Messages refusal stop_reason did not match terminal refusal blocks",
        ));
    }
    Ok(())
}

pub(super) fn map_finish(
    stop_reason: MessagesStopReason,
    stop_sequence: Option<String>,
    stop_details: Option<MessagesStopDetails>,
) -> Result<(FinishDetails, MessagesStopOutcome), MessagesStreamError> {
    if matches!(stop_reason, MessagesStopReason::Fallback)
        || stop_details
            .as_ref()
            .is_some_and(|details| details.kind == MessagesStopDetailsKind::Fallback)
    {
        return Err(MessagesStreamError::new(
            MessagesStreamErrorKind::UnsupportedSemantic,
            MessagesStreamErrorPhase::Stream,
            "Messages model fallback is explicitly excluded by adapter revision 1",
        ));
    }
    if matches!(stop_reason, MessagesStopReason::StopSequence) != stop_sequence.is_some() {
        return Err(invalid_response(
            "Messages stop_sequence reason and value must be supplied together",
        ));
    }
    if let Some(details) = &stop_details {
        let compatible = matches!(
            (stop_reason, details.kind),
            (
                MessagesStopReason::ToolUse,
                MessagesStopDetailsKind::ClientTool
            ) | (
                MessagesStopReason::Refusal,
                MessagesStopDetailsKind::Refusal
            ) | (
                MessagesStopReason::ModelContextWindowExceeded,
                MessagesStopDetailsKind::ModelContextWindowExceeded
            )
        );
        if !compatible {
            return Err(invalid_response(
                "Messages stop_reason and stop_details type disagree",
            ));
        }
    }
    let reason = match stop_reason {
        MessagesStopReason::ToolUse => FinishReason::ToolCalls,
        MessagesStopReason::MaxTokens | MessagesStopReason::ModelContextWindowExceeded => {
            FinishReason::Length
        }
        MessagesStopReason::EndTurn | MessagesStopReason::StopSequence => FinishReason::Stop,
        MessagesStopReason::PauseTurn => FinishReason::ProviderContinuation,
        MessagesStopReason::Refusal => FinishReason::Refusal,
        MessagesStopReason::Fallback => {
            unreachable!("unsupported Messages stop reasons returned before canonical mapping")
        }
    };
    let mut detail = format!("stop_reason={}", stop_reason.wire_name());
    if let Some(value) = &stop_sequence {
        detail.push_str(";stop_sequence=");
        detail.push_str(value);
    }
    if let Some(value) = &stop_details {
        detail.push_str(";stop_details.type=");
        detail.push_str(value.kind.wire_name());
    }
    if reason == FinishReason::Refusal {
        append_refusal_diagnostic(&mut detail, stop_details.as_ref())?;
    }
    Ok((
        FinishDetails::new(reason, Some(detail)),
        MessagesStopOutcome {
            reason: stop_reason,
            stop_sequence,
            details: stop_details,
        },
    ))
}

fn append_refusal_diagnostic(
    detail: &mut String,
    stop_details: Option<&MessagesStopDetails>,
) -> Result<(), MessagesStreamError> {
    if let Some(stop_details) = stop_details {
        #[derive(Deserialize)]
        struct RefusalDiagnostic {
            category: Option<String>,
            explanation: Option<String>,
        }
        let diagnostic: RefusalDiagnostic = serde_json::from_str(stop_details.raw.get())
            .map_err(|_| invalid_response("invalid Messages refusal diagnostic fields"))?;
        for (label, value) in [
            ("category", diagnostic.category),
            ("explanation", diagnostic.explanation),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                detail.push(';');
                detail.push_str(label);
                detail.push('=');
                detail.extend(value.chars().map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                }));
            }
        }
    }
    plakous_model::SafeDiagnostic::new(&*detail)
        .as_str()
        .clone_into(detail);
    Ok(())
}
