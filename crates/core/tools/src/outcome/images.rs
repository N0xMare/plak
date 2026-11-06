//! The final route and decoder boundary for every tool-result image.

use base64::Engine as _;
use plakous_model::{CancellationToken, ImageBlock, MediaSource, TextBlock, ToolResultPart};

use super::{ToolOutcome, ToolOutcomeKind};
use crate::{ExecutionEnvironment, ToolRuntimeError};

pub(crate) async fn prepare_tool_images(
    mut outcome: ToolOutcome,
    environment: &ExecutionEnvironment,
    cancellation: &CancellationToken,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let images = outcome
        .parts
        .iter()
        .filter(|part| matches!(part, ToolResultPart::Image(_)))
        .count();
    if images == 0 {
        return Ok(outcome);
    }
    let session = crate::internal::visual_read(environment);
    let normalizer = session
        .filter(|session| session.accepts_images() && environment.tool_result_images())
        .and_then(crate::internal::VisualReadSession::normalizer);
    let Some(normalizer) = normalizer else {
        return omit_images(outcome, "this tool's result carries text only here");
    };
    if outcome.images_prepared {
        return Ok(outcome);
    }
    let mut parts = Vec::with_capacity(outcome.parts.len());
    for part in &outcome.parts {
        let ToolResultPart::Image(image) = part else {
            parts.push(part.clone());
            continue;
        };
        let normalizer = normalizer.clone();
        let bytes = inline_bytes(image);
        let prepared =
            match crate::internal::normalize_tool_image(normalizer, bytes, cancellation).await {
                Ok(prepared) => prepared,
                Err(error) if error.kind() == crate::ToolRuntimeErrorKind::Cancelled => {
                    return Err(error);
                }
                Err(error) => Err(error.message().as_str().to_owned()),
            };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut parts = without_images(&outcome)?;
                append_note(
                    &mut parts,
                    &format!("[image normalization failed: {error}]"),
                )?;
                outcome.kind = ToolOutcomeKind::Failure;
                return replace_parts(outcome, parts);
            }
        };
        let source =
            MediaSource::bytes(prepared.bytes().to_vec()).map_err(|error| content_error(&error))?;
        parts.push(ToolResultPart::Image(
            ImageBlock::with_detail(prepared.media_type(), source, image.detail())
                .map_err(|error| content_error(&error))?,
        ));
    }
    replace_parts(outcome, parts).map(crate::internal::mark_tool_images_prepared)
}

pub(crate) fn omit_images(
    outcome: ToolOutcome,
    reason: &str,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let images = outcome
        .parts
        .iter()
        .filter(|part| matches!(part, ToolResultPart::Image(_)))
        .count();
    let mut parts = without_images(&outcome)?;
    append_note(
        &mut parts,
        &format!("[{images} image(s) omitted: {reason}]"),
    )?;
    replace_parts(outcome, parts)
}

fn inline_bytes(image: &ImageBlock) -> Result<Vec<u8>, String> {
    if let Some(bytes) = image.source().as_bytes() {
        return Ok(bytes.to_vec());
    }
    let data = image
        .source()
        .as_data_url()
        .and_then(|url| url.split_once(','))
        .filter(|(head, _)| head.ends_with(";base64"))
        .ok_or_else(|| "tool images require inline bytes or base64 data URLs".to_owned())?;
    base64::engine::general_purpose::STANDARD
        .decode(data.1)
        .map_err(|_| "tool image has invalid base64 data".to_owned())
}

fn without_images(outcome: &ToolOutcome) -> Result<Vec<ToolResultPart>, ToolRuntimeError> {
    let images = outcome
        .parts
        .iter()
        .filter(|part| matches!(part, ToolResultPart::Image(_)))
        .count();
    outcome
        .parts
        .iter()
        .filter(|part| !matches!(part, ToolResultPart::Image(_)))
        .map(|part| {
            Ok(match part {
                ToolResultPart::Text(text) => {
                    let strip = |body: &str| {
                        body.split('\n')
                            .filter(|line| {
                                !line
                                    .strip_prefix("[image ")
                                    .and_then(|line| line.strip_suffix(']'))
                                    .and_then(|number| number.parse::<usize>().ok())
                                    .is_some_and(|number| number > 0 && number <= images)
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    let body = if let Ok(mut value) =
                        serde_json::from_str::<serde_json::Value>(text.text())
                        && let Some(result) =
                            value.get("result").and_then(serde_json::Value::as_str)
                    {
                        value["result"] = serde_json::Value::String(strip(result));
                        value.to_string()
                    } else {
                        strip(text.text())
                    };
                    if body.is_empty() {
                        None
                    } else {
                        Some(ToolResultPart::Text(
                            TextBlock::new(body, None).map_err(|error| content_error(&error))?,
                        ))
                    }
                }
                _ => Some(part.clone()),
            })
        })
        .collect::<Result<Vec<_>, ToolRuntimeError>>()
        .map(|parts| parts.into_iter().flatten().collect())
}

fn append_note(parts: &mut Vec<ToolResultPart>, note: &str) -> Result<(), ToolRuntimeError> {
    if let Some(ToolResultPart::Text(text)) = parts.last_mut() {
        let mut body = text.text().to_owned();
        if let Ok(mut object) = serde_json::from_str::<serde_json::Value>(&body)
            && object.as_object().is_some_and(|object| object.len() == 1)
            && let Some(result) = object
                .get("result")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        {
            object["result"] = serde_json::Value::String(with_note(result, note));
            body = object.to_string();
        } else {
            body = with_note(body, note);
        }
        *text = TextBlock::new(body, None).map_err(|error| content_error(&error))?;
    } else {
        parts.push(ToolResultPart::Text(
            TextBlock::new(note, None).map_err(|error| content_error(&error))?,
        ));
    }
    Ok(())
}

fn with_note(mut text: String, note: &str) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(note);
    text
}

fn replace_parts(
    mut outcome: ToolOutcome,
    parts: Vec<ToolResultPart>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let rebuilt = ToolOutcome::raised_content(parts)?;
    outcome.parts = rebuilt.parts;
    outcome.retained_bytes = rebuilt.retained_bytes;
    Ok(outcome)
}

fn content_error(error: &plakous_model::ContentError) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        crate::ToolRuntimeErrorKind::Execution,
        "normalized tool image could not be represented",
        error,
    )
}

#[cfg(test)]
mod tests;
