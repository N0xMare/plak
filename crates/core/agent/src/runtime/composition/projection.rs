//! Canonical interpreter response and nested-result projection.

use crate::local_composition::{
    LocalCompositionContentItem, LocalCompositionImageDetail, LocalCompositionIntegration,
    LocalCompositionResponse, LocalCompositionResultProjection, LocalCompositionWaitOutcome,
};
use crate::runtime::prompts;
use crate::tool_runtime::ToolRuntime;
use crate::{AgentError, AgentErrorKind};
use plakous_model::{
    AudioBlock, CallId, ContentBlock, ConversationItem, Digest, ImageBlock, ImageDetailHint,
    MediaSource, ProjectedToolPresentationV2, TextBlock, ToolCallBlock, ToolExecutionOwner,
    ToolResultKind, ToolResultPart,
};
use plakous_tools::{ToolOwnerClass, ToolProjectionMap, ToolRuntimeError, ToolRuntimeErrorKind};
use std::time::Duration;
pub(in crate::runtime) fn local_composition_exec_registry_arguments<'a>(
    call: &'a ToolCallBlock,
    contract: &plakous_model::ToolInputContract,
) -> Result<&'a str, ToolRuntimeError> {
    match (contract, call.arguments()) {
        (plakous_model::ToolInputContract::Utf8(_), Some(serde_json::Value::String(source))) => {
            Ok(source)
        }
        (plakous_model::ToolInputContract::Structured(_), Some(serde_json::Value::Object(_))) => {
            Ok(call.raw_arguments())
        }
        (plakous_model::ToolInputContract::Utf8(_), _) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "local composition exec input is not a custom UTF-8 string",
        )),
        (plakous_model::ToolInputContract::Structured(_), _) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "local composition exec input is not a structured object",
        )),
        _ => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "local composition exec input contract is unsupported",
        )),
    }
}

/// Encode a nested interpreter value as a canonical tool-call JSON argument.
/// Custom tools carry a JSON string here and are decoded once at admission.
pub(in crate::runtime) fn local_composition_nested_call_arguments(
    projection: &ToolProjectionMap,
    name: &plakous_model::ToolName,
    input: Option<&serde_json::Value>,
) -> Result<String, AgentError> {
    let exposure = projection.by_exposed(name).ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "nested local composition tool is absent from the exact nested projection",
        )
    })?;
    match exposure.presentation() {
        ProjectedToolPresentationV2::CustomUtf8 {
            input: contract, ..
        } => match input {
            Some(serde_json::Value::String(text)) => {
                if contract.validate(text).is_err() {
                    return Err(AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "nested custom tool input exceeds its UTF-8 bound",
                    ));
                }
                serde_json::to_string(text).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "nested custom tool input is not canonical JSON",
                    )
                })
            }
            _ => Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "nested custom tool input must be UTF-8 text",
            )),
        },
        ProjectedToolPresentationV2::StructuredFunction { .. } => {
            let value = input.cloned().unwrap_or_else(|| serde_json::json!({}));
            serde_json::to_string(&value).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "nested input is not canonical JSON",
                )
            })
        }
    }
}

pub(in crate::runtime) fn local_composition_wait_response(
    outcome: LocalCompositionWaitOutcome,
) -> Result<LocalCompositionResponse, AgentError> {
    match outcome {
        LocalCompositionWaitOutcome::Live(response) => Ok(response),
        LocalCompositionWaitOutcome::Missing(response) if response.is_terminal() => Ok(response),
        LocalCompositionWaitOutcome::Missing(_) => Err(local_composition_started_error(
            "local composition host lost a live cell without terminal evidence",
        )),
        LocalCompositionWaitOutcome::Lost(error) => Err(local_composition_started_error(error)),
    }
}

pub(in crate::runtime) fn local_composition_response_terminal(
    response: &LocalCompositionResponse,
) -> bool {
    response.is_terminal()
}

pub(in crate::runtime) fn local_composition_response_cell(
    response: &LocalCompositionResponse,
) -> &str {
    response.cell_id()
}

pub(in crate::runtime) fn local_composition_response_parts(
    response: &LocalCompositionResponse,
    max_output_tokens: usize,
    wall_time: Duration,
    integration: &dyn LocalCompositionIntegration,
) -> Result<Vec<ToolResultPart>, AgentError> {
    if integration.result_projection() == LocalCompositionResultProjection::BytesPythonPtcV1 {
        return local_composition_python_response_parts(response, max_output_tokens, integration);
    }
    let (items, error) = match response {
        LocalCompositionResponse::Yielded { items, .. }
        | LocalCompositionResponse::Terminated { items, .. } => (items, None),
        LocalCompositionResponse::Result { items, error, .. } => (items, error.as_deref()),
    };
    let status = local_composition_projection_status(
        LocalCompositionResultProjection::TokensScriptV1,
        response,
    );
    let mut items = items.clone();
    if let Some(error) = error {
        items.push(LocalCompositionContentItem::Text(format!(
            "Script error:\n{error}"
        )));
    }
    let items = integration.truncate_items(items, max_output_tokens);
    let wall_time_seconds = ((wall_time.as_secs_f32()) * 10.0).round() / 10.0;
    let header = TextBlock::new(
        format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n"),
        None,
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Tool, "local composition status is invalid"))?;
    local_composition_items_to_parts(header, &items)
}

fn local_composition_items_to_parts(
    header: TextBlock,
    items: &[LocalCompositionContentItem],
) -> Result<Vec<ToolResultPart>, AgentError> {
    let mut parts = vec![ToolResultPart::Text(header)];
    for item in items {
        match item {
            // An empty text item is ordinary user output: `text("")` in a JS
            // cell, or a Python cell whose value is the empty string. It
            // carries no model content and a canonical text block cannot hold
            // it, so it is dropped rather than failing the cell. Text that is
            // genuinely unrepresentable — over the canonical block bound —
            // still fails here.
            LocalCompositionContentItem::Text(text) if text.is_empty() => {}
            LocalCompositionContentItem::Text(text) => parts.push(ToolResultPart::Text(
                TextBlock::new(text.clone(), None).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Tool,
                        "local composition text output is invalid",
                    )
                })?,
            )),
            LocalCompositionContentItem::Image { url, detail } => {
                let media_type = data_url_media_type(url, "image/png")?;
                let source = MediaSource::data_url(url.clone()).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Tool,
                        "local composition image URL is invalid",
                    )
                })?;
                let detail = detail.map(|detail| match detail {
                    LocalCompositionImageDetail::Auto => ImageDetailHint::Auto,
                    LocalCompositionImageDetail::Low => ImageDetailHint::Low,
                    LocalCompositionImageDetail::High => ImageDetailHint::High,
                    LocalCompositionImageDetail::Original => ImageDetailHint::Original,
                });
                let image = ImageBlock::with_detail(media_type, source, detail).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Tool,
                        "local composition image output is invalid",
                    )
                })?;
                parts.push(ToolResultPart::Image(image));
            }
            LocalCompositionContentItem::Audio(url) => {
                let media_type = data_url_media_type(url, "audio/wav")?;
                let source = MediaSource::data_url(url.clone()).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Tool,
                        "local composition audio URL is invalid",
                    )
                })?;
                let audio = AudioBlock::new(media_type, source).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Tool,
                        "local composition audio output is invalid",
                    )
                })?;
                parts.push(ToolResultPart::Audio(audio));
            }
        }
    }
    Ok(parts)
}

fn local_composition_python_response_parts(
    response: &LocalCompositionResponse,
    max_output_bytes: usize,
    integration: &dyn LocalCompositionIntegration,
) -> Result<Vec<ToolResultPart>, AgentError> {
    let (items, error) = match response {
        LocalCompositionResponse::Yielded { items, .. }
        | LocalCompositionResponse::Terminated { items, .. } => (items, None),
        LocalCompositionResponse::Result { items, error, .. } => (items, error.as_deref()),
    };
    let status = local_composition_projection_status(
        LocalCompositionResultProjection::BytesPythonPtcV1,
        response,
    );
    let mut items = items.clone();
    if let Some(error) = error {
        items.push(LocalCompositionContentItem::Text(error.to_owned()));
    }
    let output = format!("{status}\nOutput:\n");
    let contract_bytes = usize::try_from(plakous_tools::PYTHON_PTC_MAX_VISIBLE_OUTPUT_BYTES_V1)
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "Python output bound exceeds usize",
            )
        })?;
    let items = integration.truncate_items(
        items,
        max_output_bytes.min(contract_bytes.saturating_sub(output.len())),
    );
    let bound = contract_bytes.min(output.len().saturating_add(max_output_bytes));
    python_text_part(items, output, bound)
}

fn python_text_part(
    items: Vec<LocalCompositionContentItem>,
    mut output: String,
    bound: usize,
) -> Result<Vec<ToolResultPart>, AgentError> {
    // Python markers require one bounded UTF-8 part, including separators.
    for item in items {
        let LocalCompositionContentItem::Text(text) = item else {
            return Err(AgentError::new(
                AgentErrorKind::Tool,
                "Python cell output must be UTF-8 text",
            ));
        };
        if text.is_empty() {
            continue;
        }
        if !output.ends_with('\n') && output.len() < bound {
            output.push('\n');
        }
        let mut end = text.len().min(bound.saturating_sub(output.len()));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[..end]);
        if end < text.len() {
            break;
        }
    }
    let text = TextBlock::new(output, None).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Tool,
            "local composition Python status is invalid",
        )
    })?;
    Ok(vec![ToolResultPart::Text(text)])
}

pub(in crate::runtime) fn local_composition_projection_status(
    projection: LocalCompositionResultProjection,
    response: &LocalCompositionResponse,
) -> String {
    match (projection, response) {
        (
            LocalCompositionResultProjection::TokensScriptV1,
            LocalCompositionResponse::Yielded { cell_id, .. },
        ) => format!(
            "{}{cell_id}",
            prompts::PLAKOUS_RUNTIME_SCRIPT_STATUS_RUNNING_PREFIX
        ),
        (
            LocalCompositionResultProjection::TokensScriptV1,
            LocalCompositionResponse::Terminated { .. },
        ) => prompts::PLAKOUS_RUNTIME_SCRIPT_STATUS_TERMINATED.to_owned(),
        (
            LocalCompositionResultProjection::TokensScriptV1,
            LocalCompositionResponse::Result { error: Some(_), .. },
        ) => prompts::PLAKOUS_RUNTIME_SCRIPT_STATUS_FAILED.to_owned(),
        (
            LocalCompositionResultProjection::TokensScriptV1,
            LocalCompositionResponse::Result { error: None, .. },
        ) => prompts::PLAKOUS_RUNTIME_SCRIPT_STATUS_COMPLETED.to_owned(),
        (
            LocalCompositionResultProjection::BytesPythonPtcV1,
            LocalCompositionResponse::Yielded { cell_id, .. },
        ) => format!(
            "{}{cell_id}",
            prompts::PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_RUNNING_PREFIX
        ),
        (
            LocalCompositionResultProjection::BytesPythonPtcV1,
            LocalCompositionResponse::Terminated { .. },
        ) => prompts::PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_TERMINATED.to_owned(),
        (
            LocalCompositionResultProjection::BytesPythonPtcV1,
            LocalCompositionResponse::Result { error: Some(_), .. },
        ) => prompts::PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_FAILED.to_owned(),
        (
            LocalCompositionResultProjection::BytesPythonPtcV1,
            LocalCompositionResponse::Result { error: None, .. },
        ) => prompts::PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_COMPLETED.to_owned(),
    }
}

fn data_url_media_type<'a>(value: &'a str, fallback: &'a str) -> Result<&'a str, AgentError> {
    let body = value.strip_prefix("data:").ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Tool,
            "local composition media output is not a data URL",
        )
    })?;
    let end = body.find([';', ',']).unwrap_or(body.len());
    let media_type = &body[..end];
    if media_type.is_empty() {
        Ok(fallback)
    } else {
        Ok(media_type)
    }
}

pub(in crate::runtime) fn clip_local_composition_update_text(
    body: &str,
    budget: &mut crate::runtime::observation::budget::ObservationBudget,
) -> Result<TextBlock, AgentError> {
    let text = budget.clip_text(body);
    TextBlock::new(text, None)
        .map_err(|_| AgentError::new(AgentErrorKind::Tool, "local composition update is invalid"))
}

pub(in crate::runtime) fn local_composition_nested_result_value(
    item: &ConversationItem,
) -> Result<serde_json::Value, AgentError> {
    let result = item
        .blocks()
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "nested result omitted tool content",
            )
        })?;
    let value = if let [ToolResultPart::Text(text)] = result.parts() {
        serde_json::Value::String(text.text().to_owned())
    } else {
        serde_json::to_value(result.parts()).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "nested result content could not be encoded",
            )
        })?
    };
    if result.kind() == ToolResultKind::Failure {
        // The child is committed before this rejection reaches the interpreter.
        // A definite tool failure is catchable; infrastructure loss is not.
        return Err(AgentError::new(
            AgentErrorKind::Tool,
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned),
        ));
    }
    Ok(value)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used directly as a map_err function"
)]
pub(in crate::runtime) fn local_composition_prestart_error(error: impl ToString) -> AgentError {
    AgentError::new(
        AgentErrorKind::Tool,
        format!("local composition admission failed: {}", error.to_string()),
    )
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "accepts protocol errors and static text"
)]
pub(in crate::runtime) fn local_composition_started_error(error: impl ToString) -> AgentError {
    AgentError::new(
        AgentErrorKind::OutcomeUnknown,
        format!(
            "local composition host or cell outcome requires exact recovery: {}",
            error.to_string()
        ),
    )
}

pub(in crate::runtime) fn local_composition_recovery_authoritative(error: &AgentError) -> bool {
    matches!(
        error.kind(),
        AgentErrorKind::OutcomeUnknown | AgentErrorKind::Session | AgentErrorKind::Internal
    )
}

pub(in crate::runtime) fn local_composition_nested_decision_unsupported() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::UnsupportedCapability,
        "nested local composition decisions are an unsupported capability",
    )
}

pub(in crate::runtime) fn local_composition_child_call_id(
    binding: Digest,
) -> Result<CallId, AgentError> {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&binding.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CallId::try_from(uuid::Uuid::from_bytes(bytes))
        .map_err(|error| AgentError::from_identity(&error))
}

pub(in crate::runtime) fn local_composition_nested_execution_owner(
    registry: &ToolRuntime,
    projection: &ToolProjectionMap,
    name: &plakous_model::ToolName,
) -> Result<ToolExecutionOwner, AgentError> {
    let exposure = projection.by_exposed(name).ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "nested local composition tool is absent from the exact nested projection",
        )
    })?;
    if !exposure
        .canonical_operations()
        .values()
        .all(|identity| registry.owns_registration(identity))
    {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            "nested local composition projection widens the frozen executable registry",
        ));
    }
    // A nested call takes its registration's owner, as a landed Direct call
    // does, so an MCP tool's nested call and its result name the same server.
    let mut owners = exposure.canonical_operations().values().map(|identity| {
        registry
            .owner(identity.name())
            .map_or(ToolExecutionOwner::Local, ToolOwnerClass::execution_owner)
    });
    let owner = owners.next().unwrap_or(ToolExecutionOwner::Local);
    if owners.any(|other| other != owner) {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            "nested local composition exposure spans more than one tool owner",
        ));
    }
    Ok(owner)
}

#[cfg(test)]
mod python_tests {
    use super::*;

    #[test]
    fn python_projection_is_one_part_with_separated_output_and_error() -> Result<(), AgentError> {
        let parts = python_text_part(
            vec![
                LocalCompositionContentItem::Text("printed".into()),
                LocalCompositionContentItem::Text("42".into()),
                LocalCompositionContentItem::Text("failure".into()),
            ],
            "Python cell failed\nOutput:\n".into(),
            1_048_576,
        )?;
        let [ToolResultPart::Text(text)] = parts.as_slice() else {
            panic!("one text part")
        };
        assert_eq!(
            text.text(),
            "Python cell failed\nOutput:\nprinted\n42\nfailure"
        );
        Ok(())
    }

    #[test]
    fn python_projection_bounds_header_and_separators_on_a_char_boundary() -> Result<(), AgentError>
    {
        let header = "Python cell completed\nOutput:\n";
        let max = usize::try_from(plakous_tools::PYTHON_PTC_MAX_VISIBLE_OUTPUT_BYTES_V1).map_err(
            |_| AgentError::new(AgentErrorKind::Internal, "Python byte bound exceeds usize"),
        )?;
        let parts = python_text_part(
            vec![
                LocalCompositionContentItem::Text("a".repeat(max - header.len() - 2)),
                LocalCompositionContentItem::Text("\u{e9}z".into()),
            ],
            header.into(),
            max,
        )?;
        let [ToolResultPart::Text(text)] = parts.as_slice() else {
            panic!("one text part")
        };
        assert_eq!(text.text().len(), max - 1);
        assert!(text.text().ends_with("a\n"));
        Ok(())
    }
}
