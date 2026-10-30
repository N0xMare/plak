//! Parsers and validators for output items, content, annotations and provider errors.

use std::collections::BTreeSet;

use crate::{
    optional_provider_label, optional_string, required_provider_label, required_string,
    required_u64,
};
use serde_json::{Map, Value};

use super::{
    MAX_URL_CITATION_TITLE_BYTES, ResponsesAnnotation, ResponsesContentPart,
    ResponsesHostDecodeError, ResponsesIncompleteDetails, ResponsesOutputItem,
    ResponsesProviderError, ResponsesProviderErrorClass, ResponsesProviderErrorPolicy,
    ResponsesReasoningContentPart, ResponsesReasoningSummaryPart, ResponsesResponseLimits,
};

/// Parses one output item from a JSON value.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for structural failures.
pub(crate) fn parse_output_item(
    value: &Value,
) -> Result<ResponsesOutputItem, ResponsesHostDecodeError> {
    let object = value
        .as_object()
        .ok_or_else(|| ResponsesHostDecodeError::invalid_field("output item"))?;
    let kind = required_provider_label(object, "type")?;
    match kind {
        "message" => parse_message(object, value),
        "reasoning" => parse_reasoning(object, value),
        "function_call" => parse_function_call(object, value),
        "custom_tool_call" => parse_custom_tool_call(object, value),
        "program" => {
            required_provider_label(object, "id")?;
            required_provider_label(object, "call_id")?;
            required_string(object, "code")?;
            required_string(object, "fingerprint")?;
            Ok(ResponsesOutputItem::Opaque {
                kind: kind.to_owned(),
                native: value.clone(),
            })
        }
        "program_output" => {
            required_provider_label(object, "id")?;
            required_provider_label(object, "call_id")?;
            required_string(object, "result")?;
            match required_provider_label(object, "status")? {
                "in_progress" | "completed" | "incomplete" => {}
                _ => {
                    return Err(ResponsesHostDecodeError::invalid_field(
                        "program_output status",
                    ));
                }
            }
            Ok(ResponsesOutputItem::Opaque {
                kind: kind.to_owned(),
                native: value.clone(),
            })
        }
        "compaction" => {
            let encrypted_content = required_string(object, "encrypted_content")?.to_owned();
            Ok(ResponsesOutputItem::Compaction {
                id: optional_provider_label(object, "id")?.map(str::to_owned),
                encrypted_content,
                native: value.clone(),
            })
        }
        other => {
            required_provider_label(object, "id")?;
            Ok(ResponsesOutputItem::Opaque {
                kind: other.to_owned(),
                native: value.clone(),
            })
        }
    }
}

/// Parses a message output item.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for structural failures.
fn parse_message(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, ResponsesHostDecodeError> {
    let content = object
        .get("content")
        .ok_or_else(|| ResponsesHostDecodeError::missing_field("content"))?
        .as_array()
        .ok_or_else(|| ResponsesHostDecodeError::invalid_field("content"))?
        .iter()
        .map(parse_content_part)
        .collect::<Result<Vec<_>, ResponsesHostDecodeError>>()?;
    Ok(ResponsesOutputItem::Message {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        role: required_provider_label(object, "role")?.to_owned(),
        content,
        native: native.clone(),
    })
}

/// Parses one message content part.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for structural failures.
fn parse_content_part(value: &Value) -> Result<ResponsesContentPart, ResponsesHostDecodeError> {
    let object = value
        .as_object()
        .ok_or_else(|| ResponsesHostDecodeError::invalid_field("message content part"))?;
    let kind = required_provider_label(object, "type")?;
    match kind {
        "output_text" => Ok(ResponsesContentPart::OutputText {
            text: required_string(object, "text")?.to_owned(),
            annotations: match object.get("annotations") {
                None => Vec::new(),
                Some(value) => value
                    .as_array()
                    .ok_or_else(|| ResponsesHostDecodeError::invalid_field("annotations"))?
                    .iter()
                    .map(parse_annotation)
                    .collect::<Result<Vec<_>, ResponsesHostDecodeError>>()?,
            },
        }),
        "refusal" => Ok(ResponsesContentPart::Refusal {
            refusal: required_string(object, "refusal")?.to_owned(),
        }),
        other => Ok(ResponsesContentPart::Opaque {
            kind: other.to_owned(),
            native: value.clone(),
        }),
    }
}

/// Parses one output-text annotation into a typed or opaque family value.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for malformed documented citation fields.
pub(crate) fn parse_annotation(
    value: &Value,
) -> Result<ResponsesAnnotation, ResponsesHostDecodeError> {
    let object = value
        .as_object()
        .ok_or_else(|| ResponsesHostDecodeError::invalid_field("annotation"))?;
    let kind = required_provider_label(object, "type")?;
    if kind != "url_citation" {
        return Ok(ResponsesAnnotation::Opaque {
            kind: kind.to_owned(),
            native: value.clone(),
        });
    }
    let start_index = required_u64(object, "start_index")?;
    let end_index = required_u64(object, "end_index")?;
    if start_index > end_index {
        return Err(ResponsesHostDecodeError::invalid_field("citation range"));
    }
    let url = required_string(object, "url")?;
    let title = required_string(object, "title")?;
    if url.is_empty() || title.is_empty() || title.len() > MAX_URL_CITATION_TITLE_BYTES {
        return Err(ResponsesHostDecodeError::invalid_field("citation"));
    }
    Ok(ResponsesAnnotation::UrlCitation {
        start_index,
        end_index,
        url: url.to_owned(),
        title: title.to_owned(),
        native: value.clone(),
    })
}

/// Parses a reasoning output item.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for structural failures.
fn parse_reasoning(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, ResponsesHostDecodeError> {
    let summary = match object.get("summary") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .cloned()
            .ok_or_else(|| ResponsesHostDecodeError::invalid_field("summary"))?,
    }
    .into_iter()
    .map(|value| {
        let object = value
            .as_object()
            .ok_or_else(|| ResponsesHostDecodeError::invalid_field("reasoning summary part"))?;
        let kind = required_provider_label(object, "type")?;
        if kind == "summary_text" {
            Ok(ResponsesReasoningSummaryPart::SummaryText(
                required_string(object, "text")?.to_owned(),
            ))
        } else {
            Ok(ResponsesReasoningSummaryPart::Opaque {
                kind: kind.to_owned(),
                native: value,
            })
        }
    })
    .collect::<Result<Vec<_>, ResponsesHostDecodeError>>()?;
    let content = match object.get("content") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| ResponsesHostDecodeError::invalid_field("reasoning content"))?
            .iter()
            .map(|value| {
                let object = value.as_object().ok_or_else(|| {
                    ResponsesHostDecodeError::invalid_field("reasoning content part")
                })?;
                if required_provider_label(object, "type")? != "reasoning_text" {
                    return Err(ResponsesHostDecodeError::invalid_field(
                        "reasoning content part",
                    ));
                }
                Ok(ResponsesReasoningContentPart::ReasoningText(
                    required_string(object, "text")?.to_owned(),
                ))
            })
            .collect::<Result<Vec<_>, ResponsesHostDecodeError>>()?,
    };
    Ok(ResponsesOutputItem::Reasoning {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        summary,
        content,
        encrypted_content: optional_string(object, "encrypted_content")?.map(str::to_owned),
        native: native.clone(),
    })
}

/// Parses a function-call output item.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for structural failures.
fn parse_function_call(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, ResponsesHostDecodeError> {
    Ok(ResponsesOutputItem::FunctionCall {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        call_id: required_provider_label(object, "call_id")?.to_owned(),
        name: required_provider_label(object, "name")?.to_owned(),
        arguments: required_string(object, "arguments")?.to_owned(),
        native: native.clone(),
    })
}

/// Parses a custom-tool-call output item.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` for structural failures.
fn parse_custom_tool_call(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, ResponsesHostDecodeError> {
    Ok(ResponsesOutputItem::CustomToolCall {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        call_id: required_provider_label(object, "call_id")?.to_owned(),
        name: required_provider_label(object, "name")?.to_owned(),
        input: required_string(object, "input")?.to_owned(),
        native: native.clone(),
    })
}

/// Returns the provider item status when present.
#[must_use]
pub(crate) fn output_item_status(item: &ResponsesOutputItem) -> Option<&str> {
    match item {
        ResponsesOutputItem::Message { status, .. }
        | ResponsesOutputItem::Reasoning { status, .. }
        | ResponsesOutputItem::FunctionCall { status, .. }
        | ResponsesOutputItem::CustomToolCall { status, .. } => status.as_deref(),
        ResponsesOutputItem::Opaque { native, .. }
        | ResponsesOutputItem::Compaction { native, .. } => {
            native.get("status").and_then(Value::as_str)
        }
    }
}

/// Validates assistant roles and unique item/call identities.
///
/// # Errors
///
/// Returns `ResponsesHostDecodeError` when roles or correlation IDs are inconsistent.
pub(crate) fn validate_output_identity_roles(
    output: &[ResponsesOutputItem],
) -> Result<(), ResponsesHostDecodeError> {
    let mut item_ids = BTreeSet::new();
    let mut remote_call_ids = BTreeSet::new();
    for item in output {
        let item_id = match item {
            ResponsesOutputItem::Message { id, role, .. } => {
                if role != "assistant" {
                    return Err(ResponsesHostDecodeError::invalid_field("output role"));
                }
                id.as_deref()
            }
            ResponsesOutputItem::Reasoning { id, .. }
            | ResponsesOutputItem::Compaction { id, .. } => id.as_deref(),
            ResponsesOutputItem::FunctionCall { id, call_id, .. }
            | ResponsesOutputItem::CustomToolCall { id, call_id, .. } => {
                if !remote_call_ids.insert(call_id.as_str()) {
                    return Err(ResponsesHostDecodeError::invalid_field("function call_id"));
                }
                validate_program_caller(item.native())?;
                id.as_deref()
            }
            ResponsesOutputItem::Opaque { kind, native } => {
                if kind == "program" {
                    let _call_id =
                        native
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ResponsesHostDecodeError::missing_field("program call_id")
                            })?;
                } else if kind == "program_output" {
                    let _call_id =
                        native
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ResponsesHostDecodeError::missing_field("program_output call_id")
                            })?;
                }
                native.get("id").and_then(Value::as_str)
            }
        };
        if item_id.is_some_and(|item_id| !item_ids.insert(item_id)) {
            return Err(ResponsesHostDecodeError::invalid_field("output item id"));
        }
    }
    Ok(())
}

fn validate_program_caller(native: &Value) -> Result<(), ResponsesHostDecodeError> {
    let Some(caller) = native.get("caller") else {
        return Ok(());
    };
    if caller.is_null() {
        return Ok(());
    }
    let object = caller
        .as_object()
        .ok_or_else(|| ResponsesHostDecodeError::invalid_field("tool caller"))?;
    if object.len() == 1 && object.get("type").and_then(Value::as_str) == Some("direct") {
        return Ok(());
    }
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("program") {
        return Err(ResponsesHostDecodeError::invalid_field("tool caller"));
    }
    let _caller_id = object
        .get("caller_id")
        .and_then(Value::as_str)
        .filter(|caller_id| !caller_id.is_empty())
        .ok_or_else(|| ResponsesHostDecodeError::missing_field("tool caller_id"))?;
    Ok(())
}

/// Parses a bounded provider error using one exact classification allowlist.
///
/// # Errors
///
/// Fails closed for malformed labels or an over-limit payload.
pub fn parse_provider_error_with_policy(
    value: &Value,
    limits: ResponsesResponseLimits,
    policy: ResponsesProviderErrorPolicy,
) -> Result<ResponsesProviderError, ResponsesHostDecodeError> {
    parse_provider_error_with_canonical_type(value, limits, policy, None)
}

/// Selects the object to parse from a provider HTTP error body.
///
/// OpenAI-compatible bodies nest `{ "error": { ... } }`. SuperGrok/xAI uses
/// a top-level `{ "code": "...", "error": "<string>" }`. A string `error`
/// field is the operator message, not a nested object.
pub(crate) fn provider_error_object(value: &Value) -> &Value {
    match value.get("error") {
        Some(nested @ Value::Object(_)) => nested,
        _ => value,
    }
}

pub(crate) fn parse_provider_error_with_canonical_type(
    value: &Value,
    limits: ResponsesResponseLimits,
    policy: ResponsesProviderErrorPolicy,
    canonical_error_type: Option<&str>,
) -> Result<ResponsesProviderError, ResponsesHostDecodeError> {
    if encoded_len(value) > limits.max_unknown_bytes() {
        return Err(ResponsesHostDecodeError::ResourceLimit(
            "provider error bytes",
        ));
    }
    let object = value
        .as_object()
        .ok_or(ResponsesHostDecodeError::InvalidField("error"))?;
    let payment_required = policy == ResponsesProviderErrorPolicy::OpenRouter
        && object.get("code").and_then(Value::as_u64) == Some(402);
    let code = if payment_required {
        None
    } else {
        optional_provider_label(object, "code")?.map(str::to_owned)
    };
    let native_kind = optional_provider_label(object, "type")?
        .or(optional_provider_label(object, "kind")?)
        .map(str::to_owned);
    let native_usage_limit = native_kind
        .as_deref()
        .is_some_and(plakous_attempt::terminal_usage_error);
    let kind = canonical_error_type.map(str::to_owned).or(native_kind);
    let class = if payment_required
        || code
            .as_deref()
            .is_some_and(plakous_attempt::terminal_usage_error)
        || kind
            .as_deref()
            .is_some_and(plakous_attempt::terminal_usage_error)
        || native_usage_limit
    {
        ResponsesProviderErrorClass::Quota
    } else {
        match policy {
            ResponsesProviderErrorPolicy::OpenAiCompatible => {
                match code.as_deref().or(kind.as_deref()) {
                    Some("authentication_error" | "invalid_api_key" | "unauthorized") => {
                        ResponsesProviderErrorClass::Authentication
                    }
                    Some("rate_limit_exceeded" | "rate_limit_error") => {
                        ResponsesProviderErrorClass::RateLimited
                    }
                    Some("server_error" | "internal_error" | "server_is_overloaded") => {
                        ResponsesProviderErrorClass::Server
                    }
                    _ => ResponsesProviderErrorClass::Unknown,
                }
            }
            ResponsesProviderErrorPolicy::OpenRouter => match canonical_error_type {
                Some("authentication" | "permission_denied") => {
                    ResponsesProviderErrorClass::Authentication
                }
                Some("rate_limit_exceeded") => ResponsesProviderErrorClass::RateLimited,
                Some("provider_overloaded" | "provider_unavailable" | "timeout" | "server") => {
                    ResponsesProviderErrorClass::Server
                }
                Some(_) => ResponsesProviderErrorClass::Unknown,
                None => match code.as_deref().or(kind.as_deref()) {
                    Some("authentication_error" | "invalid_api_key" | "unauthorized") => {
                        ResponsesProviderErrorClass::Authentication
                    }
                    Some("rate_limit_exceeded" | "rate_limit_error") => {
                        ResponsesProviderErrorClass::RateLimited
                    }
                    Some("server_error" | "internal_error" | "server_is_overloaded") => {
                        ResponsesProviderErrorClass::Server
                    }
                    _ => ResponsesProviderErrorClass::Unknown,
                },
            },
            ResponsesProviderErrorPolicy::Grok => match (kind.as_deref(), code.as_deref()) {
                // xAI's usage-guideline check answers HTTP 403 with exactly
                // this code and a refusal sentence before any generation;
                // credential failures carry `unauthenticated:*` codes.
                (_, Some("permission-denied")) => ResponsesProviderErrorClass::ContentRefusal,
                (Some("authentication_error"), _)
                | (_, Some("invalid_api_key" | "authentication_error")) => {
                    ResponsesProviderErrorClass::Authentication
                }
                (_, Some("rate_limit_exceeded")) | (Some("rate_limit_error"), _) => {
                    ResponsesProviderErrorClass::RateLimited
                }
                (_, Some("server_error")) | (Some("server_error"), _) => {
                    ResponsesProviderErrorClass::Server
                }
                _ => ResponsesProviderErrorClass::Unknown,
            },
        }
    };
    Ok(ResponsesProviderError {
        code,
        kind,
        request_id: None,
        class,
        native: value.clone(),
    })
}

pub(super) fn parse_incomplete_details(
    value: &Value,
    limits: ResponsesResponseLimits,
) -> Result<ResponsesIncompleteDetails, ResponsesHostDecodeError> {
    if encoded_len(value) > limits.max_unknown_bytes() {
        return Err(ResponsesHostDecodeError::ResourceLimit(
            "incomplete detail bytes",
        ));
    }
    let object = value
        .as_object()
        .ok_or(ResponsesHostDecodeError::InvalidField("incomplete_details"))?;
    let reason = optional_provider_label(object, "reason")?.map(str::to_owned);
    Ok(ResponsesIncompleteDetails {
        reason,
        native: value.clone(),
    })
}

pub(crate) fn validate_bounded_unknown_item(
    item: &ResponsesOutputItem,
    limits: ResponsesResponseLimits,
) -> Result<(), ResponsesHostDecodeError> {
    match item {
        ResponsesOutputItem::Opaque { native, .. }
            if encoded_len(native) > limits.max_unknown_bytes() =>
        {
            Err(ResponsesHostDecodeError::ResourceLimit(
                "unknown output item bytes",
            ))
        }
        ResponsesOutputItem::Message { content, .. } => {
            for part in content {
                match part {
                    ResponsesContentPart::Opaque { native, .. }
                        if encoded_len(native) > limits.max_unknown_bytes() =>
                    {
                        return Err(ResponsesHostDecodeError::ResourceLimit(
                            "unknown content part bytes",
                        ));
                    }
                    ResponsesContentPart::OutputText { annotations, .. } => {
                        for annotation in annotations {
                            if matches!(annotation, ResponsesAnnotation::Opaque { native, .. } if encoded_len(native) > limits.max_unknown_bytes())
                            {
                                return Err(ResponsesHostDecodeError::ResourceLimit(
                                    "unknown annotation bytes",
                                ));
                            }
                        }
                    }
                    ResponsesContentPart::Refusal { .. } | ResponsesContentPart::Opaque { .. } => {}
                }
            }
            Ok(())
        }
        ResponsesOutputItem::Reasoning { summary, .. } => {
            for part in summary {
                if matches!(part, ResponsesReasoningSummaryPart::Opaque { native, .. } if encoded_len(native) > limits.max_unknown_bytes())
                {
                    return Err(ResponsesHostDecodeError::ResourceLimit(
                        "unknown reasoning part bytes",
                    ));
                }
            }
            Ok(())
        }
        ResponsesOutputItem::FunctionCall { .. }
        | ResponsesOutputItem::CustomToolCall { .. }
        | ResponsesOutputItem::Opaque { .. }
        | ResponsesOutputItem::Compaction { .. } => Ok(()),
    }
}

pub(super) fn encoded_len(value: &Value) -> usize {
    value.to_string().len()
}
