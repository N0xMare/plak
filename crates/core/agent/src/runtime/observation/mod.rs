//! Bounded observation projection and sensitive-value redaction.

use crate::runtime::checkpoint::owner::CheckpointOwner;
use crate::{
    AgentCompactSessionOutcome, AgentCompactionReason, AgentError, AgentErrorKind,
    AgentSessionEndpointError, CompactionEventOutcome, CompactionEventReason, ControlError,
    ControlErrorKind, EffectDecisionRef, TelemetryTerminalStatus, ToolCallObservation,
    ToolResultObservation,
};
use plakous_model::{
    Actor, CallId, ContentBlock, ContextOverflow, ConversationItem, EntryId, MediaSource,
    ModelError, ModelEvent, Provenance, TextBlock, ToolCallBlock, ToolExecutionOwner,
    ToolResultBlock, ToolResultKind, ToolResultPart, TurnId, Usage,
};
use plakous_tools::{ToolRuntimeError, internal::preview};
/// First output item or delta, including activity from suppressed tool input.
pub(super) const fn model_event_stamps_ttft(event: &ModelEvent) -> bool {
    match event {
        ModelEvent::StreamActivity(activity) => activity.output_started,
        ModelEvent::TextDelta(_)
        | ModelEvent::ReasoningDelta(_)
        | ModelEvent::ToolInputDelta(_)
        | ModelEvent::BlockStarted(_) => true,
        _ => false,
    }
}

pub(super) fn compaction_event_reason(reason: AgentCompactionReason) -> CompactionEventReason {
    match reason {
        AgentCompactionReason::Manual => CompactionEventReason::Manual,
        AgentCompactionReason::Scheduled => CompactionEventReason::Scheduled,
    }
}

pub(super) fn compaction_session_event_outcome(
    outcome: &Result<AgentCompactSessionOutcome, AgentSessionEndpointError>,
) -> CompactionEventOutcome {
    match outcome {
        Ok(AgentCompactSessionOutcome::Compacted { .. }) => CompactionEventOutcome::Committed,
        Ok(AgentCompactSessionOutcome::RecoveryRequired { .. }) => {
            CompactionEventOutcome::RecoveryRequired
        }
        Ok(AgentCompactSessionOutcome::Failed { .. }) => CompactionEventOutcome::Failed,
        Ok(AgentCompactSessionOutcome::NoEligibleRange) => CompactionEventOutcome::NoEligibleRange,
        Ok(AgentCompactSessionOutcome::NotNeeded) => CompactionEventOutcome::NotNeeded,
        Ok(AgentCompactSessionOutcome::Unavailable) | Err(_) => CompactionEventOutcome::Unavailable,
    }
}

pub(super) fn compaction_telemetry_tokens(
    authority: &CheckpointOwner,
    outcome: &Result<AgentCompactSessionOutcome, AgentSessionEndpointError>,
) -> (Option<u64>, Option<u64>, Option<Usage>) {
    if let Ok(AgentCompactSessionOutcome::Failed { usage, .. }) = outcome {
        return (None, None, *usage);
    }
    let Ok(AgentCompactSessionOutcome::Compacted { summary, .. }) = outcome else {
        return (None, None, None);
    };
    let checkpoint = authority.checkpoint();
    let Some(provenance) = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
    else {
        return (None, None, None);
    };
    // The unanchored whole-request estimate (the bound without parts), so
    // before and after share one basis; only fences and the meter anchor.
    // The audited bound stands in when the summary carries no figure.
    let occupancy_before = summary.and_then(|summary| summary.occupancy_tokens_before);
    let occupancy_after = summary.and_then(|summary| summary.occupancy_tokens_after);
    (
        Some(occupancy_before.unwrap_or_else(|| provenance.before_audit().input_tokens())),
        Some(occupancy_after.unwrap_or_else(|| provenance.after_audit().input_tokens())),
        provenance.usage(),
    )
}

fn media_source_content_bytes(source: &MediaSource) -> usize {
    source
        .as_uri()
        .map(str::len)
        .or_else(|| source.as_data_url().map(str::len))
        .unwrap_or_else(|| source.as_bytes().map_or(0, <[u8]>::len))
}

fn tool_result_part_content_bytes(part: &ToolResultPart) -> usize {
    match part {
        ToolResultPart::Text(text) => text.text().len(),
        ToolResultPart::Image(image) => image
            .media_type()
            .len()
            .saturating_add(media_source_content_bytes(image.source())),
        ToolResultPart::Document(document) => document
            .media_type()
            .len()
            .saturating_add(media_source_content_bytes(document.source())),
        ToolResultPart::Audio(audio) => audio
            .media_type()
            .len()
            .saturating_add(media_source_content_bytes(audio.source())),
        ToolResultPart::Opaque(opaque) => opaque.replay().payload().get().len(),
        _ => 0,
    }
}

pub(super) fn tool_result_content_bytes(result: &ToolResultBlock) -> u64 {
    u64::try_from(result.parts().iter().fold(0_usize, |total, part| {
        total.saturating_add(tool_result_part_content_bytes(part))
    }))
    .unwrap_or(u64::MAX)
}

pub(super) fn conversation_item_tool_result(item: &ConversationItem) -> Option<&ToolResultBlock> {
    item.blocks().iter().rev().find_map(|block| match block {
        ContentBlock::ToolResult(result) => Some(result),
        _ => None,
    })
}

pub(super) fn tool_result_telemetry_status(
    result: &ToolResultBlock,
    shutting_down: bool,
) -> TelemetryTerminalStatus {
    match result.kind() {
        ToolResultKind::Success => TelemetryTerminalStatus::Completed,
        ToolResultKind::Failure if shutting_down => TelemetryTerminalStatus::Shutdown,
        // `ToolResultKind` is non-exhaustive; any future kind is not a success.
        _ => TelemetryTerminalStatus::Failed,
    }
}

pub(super) fn composition_tool_error_telemetry_status(
    error: &AgentError,
    shutting_down: bool,
) -> TelemetryTerminalStatus {
    match error.kind() {
        AgentErrorKind::Cancelled if shutting_down => TelemetryTerminalStatus::Shutdown,
        AgentErrorKind::Cancelled => TelemetryTerminalStatus::Cancelled,
        _ => TelemetryTerminalStatus::Failed,
    }
}

pub(super) fn retained_tool_exchange_failure(
    shrunk_admit: Result<(), AgentError>,
) -> Result<Option<AgentError>, AgentError> {
    match shrunk_admit {
        Ok(()) => Ok(None),
        Err(error)
            if error
                .model_error()
                .and_then(ModelError::context_capacity)
                .is_some() =>
        {
            Ok(Some(error))
        }
        Err(error) => Err(error),
    }
}

pub(super) struct ObservationReplacement {
    pub(super) call_id: CallId,
    pub(super) bytes_before: u64,
    pub(super) bytes_after: u64,
}

fn tool_result_text_bytes(result: &ToolResultBlock) -> u64 {
    result.parts().iter().fold(0_u64, |bytes, part| match part {
        ToolResultPart::Text(text) => bytes.saturating_add(observation_bytes(text.text().len())),
        _ => bytes,
    })
}

/// Smallest share worth clipping a result to. Below it the head and tail are
/// too short to carry a diagnosis, so the result gives its space up entirely.
const SHRINK_FLOOR_BYTES: usize = 2 * 1024;

/// The text budget left for a batch whose admission reported `overflow`: its
/// text less the excess, converted back from the conservative `ceil(bytes / 4)`
/// estimate. `None` when the overflow is on an axis
/// that shrinking observations cannot move or nothing would remain.
pub(super) fn tool_exchange_shrink_budget(
    items: &[ConversationItem],
    overflow: ContextOverflow,
) -> Option<usize> {
    let excess_tokens = [overflow.shared(), overflow.input()]
        .into_iter()
        .flatten()
        .map(|axis| axis.required().saturating_sub(axis.available()))
        .max()?;
    let sizes = batch_result_text_sizes(items);
    let total: usize = sizes.iter().sum();
    let excess = usize::try_from(excess_tokens).ok()?;
    // Markers, JSON escaping and estimator slack: a tenth of the batch plus a
    // fixed allowance per result.
    let margin = total / 10 + 256 * sizes.len();
    let budget = total.checked_sub(excess)?.checked_sub(margin)?;
    (budget > 0).then_some(budget)
}

fn batch_result_text_sizes(items: &[ConversationItem]) -> Vec<usize> {
    items
        .iter()
        .filter(|item| item.actor() == Actor::Tool)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(tool_result_text_len(result)),
            _ => None,
        })
        .collect()
}

fn tool_result_text_len(result: &ToolResultBlock) -> usize {
    result.parts().iter().fold(0, |bytes, part| match part {
        ToolResultPart::Text(text) => bytes.saturating_add(text.text().len()),
        _ => bytes,
    })
}

/// The per-result cap that splits `budget` fairly: results at or under it keep
/// their whole body and the larger ones share what remains equally.
fn fair_share_cap(sizes: &[usize], budget: usize) -> usize {
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    let mut remaining = budget;
    for (index, size) in sorted.iter().enumerate() {
        let share = remaining / (sorted.len() - index);
        if *size > share {
            return share;
        }
        remaining -= size;
    }
    usize::MAX
}

/// Shrinks a batch that overflowed admission. With a `budget` the batch's
/// text is split fairly across its results, so a small result survives whole
/// beside a huge one; without one large results get the exhausted notice.
pub(super) fn shrink_tool_exchange_observations(
    items: Vec<ConversationItem>,
    budget: Option<usize>,
) -> Result<(Vec<ConversationItem>, Vec<ObservationReplacement>), AgentError> {
    let cap = budget.map_or(0, |budget| {
        fair_share_cap(&batch_result_text_sizes(&items), budget)
    });
    let mut replacements: Vec<ObservationReplacement> = Vec::new();
    let items = items
        .into_iter()
        .map(|item| {
            if item.actor() != Actor::Tool {
                return Ok(item);
            }
            let mut blocks = Vec::with_capacity(item.blocks().len());
            for block in item.blocks() {
                match block {
                    ContentBlock::ToolResult(result) => {
                        let size = tool_result_text_len(result);
                        let limit = if size <= cap {
                            None
                        } else if cap >= SHRINK_FLOOR_BYTES {
                            Some(cap)
                        } else {
                            Some(0)
                        };
                        let mut budget = match limit {
                            Some(capacity) => budget::ObservationBudget::new(capacity, None),
                            None => budget::ObservationBudget::capped(None),
                        };
                        let shrunk = clip_tool_result(result.clone(), &mut budget)?.0;
                        if result.parts() != shrunk.parts() {
                            let before = tool_result_text_bytes(result);
                            let after = tool_result_text_bytes(&shrunk);
                            if let Some(existing) = replacements
                                .iter_mut()
                                .find(|existing| existing.call_id == result.call_id())
                            {
                                existing.bytes_before =
                                    existing.bytes_before.saturating_add(before);
                                existing.bytes_after = existing.bytes_after.saturating_add(after);
                            } else {
                                replacements.push(ObservationReplacement {
                                    call_id: result.call_id(),
                                    bytes_before: before,
                                    bytes_after: after,
                                });
                            }
                        }
                        blocks.push(ContentBlock::ToolResult(shrunk));
                    }
                    other => blocks.push(other.clone()),
                }
            }
            let subject = item.presentation_subject().map(str::to_owned);
            ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
                .map(|built| built.with_presentation_subject(subject.as_deref()))
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "shrunk tool observation item is invalid",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((items, replacements))
}

/// What the observation budget removed from one tool result.
///
/// A clipped result is a fact the operator has to be able to see: the model is
/// answering from a truncated read, and until this existed the only trace was
/// the model's own prose. `bound` is the budget the clip was measured against,
/// which is what makes `bytes_dropped` interpretable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) struct ObservationClip {
    /// Observation bytes that survived into canonical history.
    pub(in crate::runtime) bytes_kept: u64,
    /// Observation bytes the budget removed.
    pub(in crate::runtime) bytes_dropped: u64,
    /// Bytes still available to this result before it was clipped.
    pub(in crate::runtime) bound: u64,
}

pub(super) fn clip_tool_result(
    result: ToolResultBlock,
    budget: &mut budget::ObservationBudget,
) -> Result<(ToolResultBlock, Option<ObservationClip>), AgentError> {
    let original_bytes = tool_result_text_len(&result);
    let capacity_limited = budget.capacity_limited();
    let available = budget.remaining.unwrap_or(usize::MAX);
    let allowance = available.min(budget::CANONICAL_RESULT_BYTES);
    if original_bytes <= allowance
        || (capacity_limited && original_bytes <= OBSERVATION_EXHAUSTED.len())
    {
        budget.remaining = budget
            .remaining
            .map(|remaining| remaining.saturating_sub(original_bytes));
        budget.record_result(
            original_bytes,
            original_bytes,
            capacity_limited && original_bytes > allowance,
        );
        return Ok((result, None));
    }
    let mut result_remaining = allowance;
    let remaining = &mut result_remaining;
    let bound = u64::try_from(*remaining).unwrap_or(u64::MAX);
    let mut kept = 0_u64;
    let mut dropped = 0_u64;
    let exhausted = || {
        TextBlock::new(OBSERVATION_EXHAUSTED, None).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "observation budget message is invalid",
            )
        })
    };
    let mut parts = Vec::with_capacity(result.parts().len());
    for part in result.parts() {
        match part {
            ToolResultPart::Text(text) => {
                let body = text.text();
                if *remaining == 0 {
                    let clipped = clip_observation_text(body, 0);
                    dropped = dropped.saturating_add(observation_bytes(body.len()));
                    parts.push(ToolResultPart::Text(
                        TextBlock::new(clipped, text.phase()).map_err(|_| {
                            AgentError::new(AgentErrorKind::Tool, "clipped text is invalid")
                        })?,
                    ));
                    continue;
                }
                if body.len() <= *remaining {
                    *remaining -= body.len();
                    kept = kept.saturating_add(observation_bytes(body.len()));
                    parts.push(ToolResultPart::Text(text.clone()));
                    continue;
                }
                let before = *remaining;
                let (clipped, next_remaining) = budgeted_observation_text(body, *remaining);
                *remaining = next_remaining;
                // Budget consumed is how much of the original survived. The
                // clipped string also carries a truncation notice, so its own
                // length is longer than what it kept and cannot be used here.
                let survived = before.saturating_sub(next_remaining);
                kept = kept.saturating_add(observation_bytes(survived));
                dropped =
                    dropped.saturating_add(observation_bytes(body.len().saturating_sub(survived)));
                parts.push(ToolResultPart::Text(
                    TextBlock::new(clipped, text.phase()).map_err(|_| {
                        AgentError::new(AgentErrorKind::Tool, "clipped text is invalid")
                    })?,
                ));
            }
            other => parts.push(other.clone()),
        }
    }
    if parts.is_empty() {
        parts.push(ToolResultPart::Text(exhausted()?));
    }
    let clipped = ToolResultBlock::new(
        result.call_id(),
        result.owner().clone(),
        result.kind(),
        parts,
        result.correlation().clone(),
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Tool, "clipped tool result is invalid"))?;
    // Nothing dropped is not an event. Only a truncated observation is.
    let clip = (dropped > 0).then_some(ObservationClip {
        bytes_kept: kept,
        bytes_dropped: dropped,
        bound,
    });
    if budget.remaining.is_some() {
        budget.remaining =
            Some(available.saturating_sub(allowance.saturating_sub(result_remaining)));
    }
    budget.record_result(
        original_bytes,
        tool_result_text_len(&clipped),
        clip.is_some() && capacity_limited,
    );
    Ok((clipped, clip))
}

/// Bytes as a count, saturating rather than refusing on a 32-bit `usize`.
fn observation_bytes(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

pub(super) fn local_composition_result_observation(
    item: &ConversationItem,
) -> Option<ToolResultObservation> {
    item.blocks().iter().rev().find_map(|block| match block {
        ContentBlock::ToolResult(result) => Some(tool_result_observation_with_subject(
            result,
            item.presentation_subject(),
        )),
        _ => None,
    })
}

pub(crate) fn tool_call_observation(call: &ToolCallBlock) -> ToolCallObservation {
    let subject = call.arguments().and_then(call_subject).map(|value| {
        let value = preview::text(&value)
            .replace("\r\n", "↵")
            .replace(['\r', '\n'], "↵");
        if value.len() <= TOOL_OBSERVATION_SUBJECT_BYTES {
            return value;
        }
        let mut end = TOOL_OBSERVATION_SUBJECT_BYTES - "…".len();
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
    });
    let preview = call.arguments().map_or_else(
        || String::from("[arguments unavailable]"),
        |arguments| {
            let mut redacted = arguments.clone();
            preview::json(&mut redacted);
            serde_json::to_string_pretty(&redacted)
                .unwrap_or_else(|_| String::from("[arguments unavailable]"))
        },
    );
    ToolCallObservation::new(
        subject,
        bounded_text(&preview, TOOL_OBSERVATION_PREVIEW_BYTES),
    )
}

/// The bounded redacted observation for a call that was refused.
///
/// Same bound and same redaction as [`tool_call_observation`], with one
/// difference that is the point of it: a call refused for a malformed or
/// schema-invalid body has no parsed `arguments`, and reporting
/// "[arguments unavailable]" there loses the only evidence of what the model
/// sent. This falls back to the raw argument text, redacted and bounded the
/// same way, so a rejection stays diagnosable after the fact.
pub(in crate::runtime) fn rejected_tool_call_observation(
    call: &ToolCallBlock,
) -> ToolCallObservation {
    if call.arguments().is_some() {
        return tool_call_observation(call);
    }
    let raw = call.raw_arguments().trim();
    let preview = if raw.is_empty() {
        String::from("[arguments unavailable]")
    } else {
        preview::text(raw)
    };
    ToolCallObservation::new(None, bounded_text(&preview, TOOL_OBSERVATION_PREVIEW_BYTES))
}

/// Bounds and redacts a rejection's descriptive message.
pub(in crate::runtime) fn bounded_rejection_detail(message: &str) -> String {
    bounded_text(&preview::text(message), TOOL_OBSERVATION_PREVIEW_BYTES)
}

pub(crate) fn tool_result_observation(result: &ToolResultBlock) -> ToolResultObservation {
    tool_result_observation_with_subject(result, None)
}

/// [`tool_result_observation`] with a tool-provided host subject.
///
/// The subject is the outcome's presentation subject, already redacted by the
/// tool. It is not read out of the result text. `None` stays absent.
pub(crate) fn tool_result_observation_with_subject(
    result: &ToolResultBlock,
    subject: Option<&str>,
) -> ToolResultObservation {
    let subject = subject
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(|subject| bounded_observation_subject(&preview::text(subject)));
    let mut preview = String::new();
    for part in result.parts() {
        if !preview.is_empty() {
            preview.push('\n');
        }
        match part {
            ToolResultPart::Text(text) => preview.push_str(text.text()),
            ToolResultPart::Image(_) => preview.push_str("[image result]"),
            ToolResultPart::Document(_) => preview.push_str("[document result]"),
            ToolResultPart::Audio(_) => preview.push_str("[audio result]"),
            ToolResultPart::Opaque(_) => preview.push_str("[opaque result]"),
            _ => preview.push_str("[result part]"),
        }
        if preview.len() >= TOOL_OBSERVATION_PREVIEW_BYTES {
            break;
        }
    }
    let preview = if let Ok(mut structured) = serde_json::from_str::<serde_json::Value>(&preview) {
        preview::json(&mut structured);
        bounded_structured(&mut structured, TOOL_OBSERVATION_PREVIEW_BYTES)
    } else {
        preview::text(&preview)
    };
    ToolResultObservation::new(
        result.kind() == ToolResultKind::Success,
        bounded_text(&preview, TOOL_OBSERVATION_PREVIEW_BYTES),
        subject,
    )
}

fn bounded_observation_subject(value: &str) -> String {
    let value = value.replace("\r\n", "↵").replace(['\r', '\n'], "↵");
    if value.len() <= TOOL_OBSERVATION_SUBJECT_BYTES {
        return value;
    }
    let mut end = TOOL_OBSERVATION_SUBJECT_BYTES - "…".len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// Extra bytes a history read redacts past the preview bound before it clips
/// a string, as a call-detail read does (spec 04 §5): a credential shorter
/// than this margin is redacted whole, and when the source ran past the
/// window this many redacted bytes are held back from its end.
const HISTORY_REDACTION_MARGIN_BYTES: usize = 16 * 1024;

/// The most bytes of one string a history read redacts: the margin plus
/// twice the preview bound, so redaction may shrink the window by a whole
/// preview and the read still shows what the live preview shows.
const HISTORY_WINDOW_BYTES: usize =
    2 * TOOL_OBSERVATION_PREVIEW_BYTES + HISTORY_REDACTION_MARGIN_BYTES;

/// Sources up to this size are observed by the live functions themselves, so
/// a history read of them is byte-identical to the live observation. JSON
/// within it keeps its structured preview and the facts a host reads from it.
const HISTORY_EXACT_BYTES: usize = 64 * 1024;

/// [`tool_call_observation`] for a call read back from committed history.
/// Arguments up to [`HISTORY_EXACT_BYTES`] observe exactly as live; larger
/// ones have every string leaf cut to the history window before redaction.
pub(crate) fn history_tool_call_observation(call: &ToolCallBlock) -> ToolCallObservation {
    let Some(arguments) = call
        .arguments()
        .filter(|_| call.raw_arguments().len() > HISTORY_EXACT_BYTES)
    else {
        return tool_call_observation(call);
    };
    let subject = history_call_subject(arguments).map(|value| {
        let value = preview::text(&value)
            .replace("\r\n", "↵")
            .replace(['\r', '\n'], "↵");
        if value.len() <= TOOL_OBSERVATION_SUBJECT_BYTES {
            return value;
        }
        let mut end = TOOL_OBSERVATION_SUBJECT_BYTES - "…".len();
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
    });
    let mut budget = HISTORY_WINDOW_BYTES;
    let mut redacted = windowed_leaves(arguments, &mut budget);
    preview::json(&mut redacted);
    let preview = serde_json::to_string_pretty(&redacted)
        .unwrap_or_else(|_| String::from("[arguments unavailable]"));
    ToolCallObservation::new(
        subject,
        bounded_text(&preview, TOOL_OBSERVATION_PREVIEW_BYTES),
    )
}

/// The presentation subject a call's arguments would give its live
/// `ToolStarted`, with the history variant above [`HISTORY_EXACT_BYTES`] of
/// raw arguments, for a call read back without its call block.
pub(crate) fn arguments_presentation_subject(
    arguments: &serde_json::Value,
    raw_bytes: usize,
) -> Option<String> {
    let subject = if raw_bytes > HISTORY_EXACT_BYTES {
        history_call_subject(arguments)
    } else {
        call_subject(arguments)
    };
    subject.map(|value| bounded_observation_subject(&preview::text(&value)))
}

/// [`call_subject`] without copying a long subject string: a patch is only
/// counted, and any other subject is cut to the history window first.
fn history_call_subject(arguments: &serde_json::Value) -> Option<String> {
    let patch = arguments
        .get("patch")
        .or_else(|| arguments.get("input"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|patch| patch.starts_with("*** Begin Patch"));
    if patch {
        return call_subject(arguments);
    }
    tool_subject(arguments).map(windowed_text)
}

/// A copy of `value` within a byte `budget`: a string longer than the history
/// window keeps only its redacted window, less the held-back margin, every
/// leaf and container spends at least one byte, and once the budget is spent,
/// later strings are empty and later array items and object fields are left
/// out. The preview shows less than the budget, so
/// nothing left out could have been shown.
fn windowed_leaves(value: &serde_json::Value, budget: &mut usize) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            let kept = if *budget == 0 {
                String::new()
            } else {
                windowed_text(text)
            };
            *budget = budget.saturating_sub(kept.len().max(1));
            serde_json::Value::String(kept)
        }
        serde_json::Value::Array(items) => {
            *budget = budget.saturating_sub(1);
            let mut kept = Vec::new();
            for item in items {
                if *budget == 0 {
                    break;
                }
                kept.push(windowed_leaves(item, budget));
            }
            serde_json::Value::Array(kept)
        }
        serde_json::Value::Object(fields) => {
            *budget = budget.saturating_sub(1);
            let mut kept = serde_json::Map::new();
            for (key, value) in fields {
                if *budget == 0 {
                    break;
                }
                *budget = budget.saturating_sub(key.len().max(1));
                kept.insert(key.clone(), windowed_leaves(value, budget));
            }
            serde_json::Value::Object(kept)
        }
        other => {
            *budget = budget.saturating_sub(8);
            other.clone()
        }
    }
}

/// `text` when it fits the history window; otherwise its window, redacted,
/// with the last margin of the redacted text held back.
fn windowed_text(text: &str) -> String {
    if text.len() <= HISTORY_WINDOW_BYTES {
        return text.to_owned();
    }
    let mut end = HISTORY_WINDOW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut redacted = preview::text(&text[..end]);
    let mut keep = redacted
        .len()
        .saturating_sub(HISTORY_REDACTION_MARGIN_BYTES);
    while !redacted.is_char_boundary(keep) {
        keep -= 1;
    }
    redacted.truncate(keep);
    redacted
}

/// [`tool_result_observation_with_subject`] for a result read back from
/// committed history. `subject` is the conversation item's stored
/// presentation subject, not a parse of the result. A result whose text fits
/// [`HISTORY_EXACT_BYTES`] observes exactly as live; a larger JSON one is
/// parsed with each string cut to the history window and previewed as live,
/// and any other is read as text over the history window only.
pub(crate) fn history_tool_result_observation_with_subject(
    result: &ToolResultBlock,
    subject: Option<&str>,
) -> ToolResultObservation {
    let mut bytes = 0_usize;
    for part in result.parts() {
        if let ToolResultPart::Text(text) = part {
            bytes = bytes.saturating_add(text.text().len());
        }
        if bytes > HISTORY_EXACT_BYTES {
            break;
        }
    }
    if bytes <= HISTORY_EXACT_BYTES {
        return tool_result_observation_with_subject(result, subject);
    }
    let succeeded = result.kind() == ToolResultKind::Success;
    let subject = subject
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(|subject| bounded_observation_subject(&preview::text(subject)));
    if let Some(preview) = history_structured_preview(result) {
        return ToolResultObservation::new(succeeded, preview, subject);
    }
    let mut source = String::new();
    for part in result.parts() {
        if !source.is_empty() {
            source.push('\n');
        }
        let room = HISTORY_WINDOW_BYTES
            .saturating_add(1)
            .saturating_sub(source.len());
        match part {
            ToolResultPart::Text(text) => {
                let mut end = text.text().len().min(room);
                while !text.text().is_char_boundary(end) {
                    end -= 1;
                }
                source.push_str(&text.text()[..end]);
            }
            ToolResultPart::Image(_) => source.push_str("[image result]"),
            ToolResultPart::Document(_) => source.push_str("[document result]"),
            ToolResultPart::Audio(_) => source.push_str("[audio result]"),
            ToolResultPart::Opaque(_) => source.push_str("[opaque result]"),
            _ => source.push_str("[result part]"),
        }
        if source.len() > HISTORY_WINDOW_BYTES {
            break;
        }
    }
    // The source ran past the exact bound, so it is always cut here and the
    // preview always says so, even when redaction shrank it below the bound.
    let preview = windowed_text(&source);
    let preview = if preview.len() + "\n[preview truncated]".len() <= TOOL_OBSERVATION_PREVIEW_BYTES
    {
        format!("{preview}\n[preview truncated]")
    } else {
        bounded_text(&preview, TOOL_OBSERVATION_PREVIEW_BYTES)
    };
    ToolResultObservation::new(succeeded, preview, subject)
}

/// The live structured preview of a large JSON result, over a bounded read:
/// the source the live function parses (parts joined until one reaches the
/// preview bound, so a short prefix and at most one large text part) is
/// parsed with every string cut to its redacted window, then redacted and
/// bounded as live. `None` when that source is not JSON.
fn history_structured_preview(result: &ToolResultBlock) -> Option<String> {
    let mut prefix = String::new();
    let mut large = "";
    for part in result.parts() {
        if !prefix.is_empty() {
            prefix.push('\n');
        }
        match part {
            ToolResultPart::Text(text)
                if prefix.len() + text.text().len() >= TOOL_OBSERVATION_PREVIEW_BYTES =>
            {
                large = text.text();
                break;
            }
            ToolResultPart::Text(text) => prefix.push_str(text.text()),
            ToolResultPart::Image(_) => prefix.push_str("[image result]"),
            ToolResultPart::Document(_) => prefix.push_str("[document result]"),
            ToolResultPart::Audio(_) => prefix.push_str("[audio result]"),
            ToolResultPart::Opaque(_) => prefix.push_str("[opaque result]"),
            _ => prefix.push_str("[result part]"),
        }
        if prefix.len() >= TOOL_OBSERVATION_PREVIEW_BYTES {
            break;
        }
    }
    let json_whitespace = [' ', '\t', '\n', '\r'];
    let start = match prefix.trim_start_matches(json_whitespace) {
        "" => large.trim_start_matches(json_whitespace),
        prefix => prefix,
    };
    if !start.starts_with(['{', '[']) {
        return None;
    }
    let mut structured = if prefix.is_empty() {
        history_json::windowed_json(serde_json::Deserializer::from_str(large))
    } else {
        let source = std::io::Read::chain(prefix.as_bytes(), large.as_bytes());
        history_json::windowed_json(serde_json::Deserializer::from_reader(source))
    }?;
    preview::json(&mut structured);
    let preview = bounded_structured(&mut structured, TOOL_OBSERVATION_PREVIEW_BYTES);
    Some(bounded_text(&preview, TOOL_OBSERVATION_PREVIEW_BYTES))
}

fn tool_subject(arguments: &serde_json::Value) -> Option<&str> {
    let object = arguments.as_object()?;
    [
        "path",
        "file_path",
        "cmd",
        "command",
        "query",
        "q",
        "url",
        "pattern",
        "source",
        "code",
    ]
    .into_iter()
    .find_map(|key| object.get(key).and_then(serde_json::Value::as_str))
}

fn call_subject(arguments: &serde_json::Value) -> Option<String> {
    // Count complete patch arguments before the observation preview clips them.
    // This recognizes an input shape, not a provider or tool identity.
    if let Some(patch) = arguments
        .get("patch")
        .or_else(|| arguments.get("input"))
        .and_then(serde_json::Value::as_str)
        .filter(|patch| patch.starts_with("*** Begin Patch"))
    {
        let mut paths = Vec::new();
        let (mut added, mut removed) = (0usize, 0usize);
        for line in patch.lines() {
            for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
                if let Some(path) = line.strip_prefix(prefix).map(str::trim)
                    && !path.is_empty()
                    && !paths.contains(&path)
                {
                    paths.push(path);
                }
            }
            added += usize::from(line.starts_with('+'));
            removed += usize::from(line.starts_with('-'));
        }
        let subject = match paths.as_slice() {
            [path] => (*path).to_owned(),
            [] => "patch".to_owned(),
            paths => format!("{} files", paths.len()),
        };
        return Some(format!("{subject} +{added}/-{removed}"));
    }
    tool_subject(arguments).map(str::to_owned)
}

/// Pretty JSON within `maximum` bytes that is still JSON: the longest string
/// leaves give up their middles first, so the small fields beside a large
/// `output` (an exit code, a status) and the output's own last lines survive
/// for presentation. Falls back to plain truncation when no string is left to
/// cut.
fn bounded_structured(value: &mut serde_json::Value, maximum: usize) -> String {
    const KEEP_AT_LEAST: usize = 64;
    for _ in 0..8 {
        let Ok(text) = serde_json::to_string_pretty(value) else {
            return String::from("[result unavailable]");
        };
        let excess = text.len().saturating_sub(maximum);
        if excess == 0 {
            return text;
        }
        let Some(longest) = longest_string(value).filter(|leaf| leaf.len() > KEEP_AT_LEAST) else {
            return bounded_text(&text, maximum);
        };
        // Escapes make the encoded leaf longer than the string, so cut by the
        // excess plus a margin and measure again.
        let keep = longest
            .len()
            .saturating_sub(excess.saturating_add(excess / 4).saturating_add(48))
            .max(KEEP_AT_LEAST);
        *longest = clip_head_tail(longest, keep);
    }
    serde_json::to_string_pretty(value).map_or_else(
        |_| String::from("[result unavailable]"),
        |text| bounded_text(&text, maximum),
    )
}

fn longest_string(value: &mut serde_json::Value) -> Option<&mut String> {
    match value {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Array(items) => items
            .iter_mut()
            .filter_map(longest_string)
            .max_by_key(|text| text.len()),
        serde_json::Value::Object(fields) => fields
            .values_mut()
            .filter_map(longest_string)
            .max_by_key(|text| text.len()),
        _ => None,
    }
}

/// A third of `keep` from the head and the rest from the tail, cut on
/// character boundaries around one marker.
fn clip_head_tail(text: &str, keep: usize) -> String {
    let mut head = keep / 3;
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(keep - keep / 3);
    while tail < text.len() && !text.is_char_boundary(tail) {
        tail += 1;
    }
    let tail = tail.max(head);
    format!(
        "{}\n[{} bytes omitted]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn bounded_text(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    let suffix = "\n[preview truncated]";
    let mut end = maximum.saturating_sub(suffix.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &text[..end])
}

pub(crate) fn normalized_tool_failure(
    call: &ToolCallBlock,
    error: &ToolRuntimeError,
) -> Result<ToolResultBlock, AgentError> {
    let text = TextBlock::new(format!("tool invocation failed: {}", error.message()), None)
        .map_err(|_| AgentError::from_tool(error))?;
    ToolResultBlock::new(
        call.call_id(),
        call.owner().clone(),
        ToolResultKind::Failure,
        vec![ToolResultPart::Text(text)],
        call.correlation().clone(),
    )
    .map_err(|_| AgentError::from_tool(error))
}

/// The answer to a composition `wait` made while no cell is running.
///
/// A successful, empty, typed result: no cell is active, so there is nothing to
/// wait for and nothing was dispatched. It is deliberately not a failure. The
/// model asked a reasonable question and gets the reasonable answer, and the
/// turn keeps the tool call it would otherwise have spent on a rejection.
pub(super) fn no_active_cell_wait_result(
    call: &ToolCallBlock,
) -> Result<ToolResultBlock, AgentError> {
    let internal = || {
        AgentError::new(
            AgentErrorKind::Internal,
            "local composition wait result could not be constructed",
        )
    };
    let text = TextBlock::new(
        "no local composition cell is active; there is nothing to wait for".to_owned(),
        None,
    )
    .map_err(|_| internal())?;
    ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(text)],
        call.correlation().clone(),
    )
    .map_err(|_| internal())
}

pub(super) fn canonical_tool_result_item(
    entry_id: EntryId,
    result: ToolResultBlock,
) -> Result<ConversationItem, AgentError> {
    ConversationItem::new(
        entry_id,
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Tool,
            "canonical tool-result item is invalid",
        )
    })
}

pub(super) fn validate_pending_effect_decision(
    control_turn: TurnId,
    pending: &EffectDecisionRef,
    supplied: &EffectDecisionRef,
) -> Result<(), ControlError> {
    if control_turn != pending.turn_id() || supplied.turn_id() != control_turn {
        return Err(ControlError::new(
            ControlErrorKind::ForeignTurn,
            "tool decision belongs to a different turn",
        ));
    }
    if supplied.event_id() != pending.event_id() {
        return Err(ControlError::new(
            ControlErrorKind::Stale,
            "tool effect-decision event is no longer pending",
        ));
    }
    if supplied.attempt_id() != pending.attempt_id()
        || supplied.call_id() != pending.call_id()
        || supplied.binding_digest() != pending.binding_digest()
    {
        return Err(ControlError::new(
            ControlErrorKind::WrongEffectDecision,
            "tool decision does not match the pending attempt, call, and binding",
        ));
    }
    Ok(())
}

pub(in crate::runtime) mod budget;
mod history_json;
use budget::{OBSERVATION_EXHAUSTED, budgeted_observation_text, clip_observation_text};

#[cfg(test)]
mod tests;

const TOOL_OBSERVATION_PREVIEW_BYTES: usize = 2 * 1024;

const TOOL_OBSERVATION_SUBJECT_BYTES: usize = 240;

/// What the committed compaction on this checkpoint did, as it records it.
///
/// Read from the successor's own compaction provenance rather than recomputed,
/// so the figures an operator is shown are the ones the checkpoint commits to.
/// `None` when the checkpoint carries no provenance, which a committed
/// compaction should not produce; the caller reports "committed, figures
/// unknown" rather than refusing.
pub(super) fn committed_compaction_summary(
    authority: &CheckpointOwner,
) -> Option<crate::control::CompactionSummary> {
    let checkpoint = authority.checkpoint();
    let provenance = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()?;
    let rung = match provenance.strategy() {
        crate::manifest::RuntimeCompactionStrategy::NativeResponsesTriggerV1 => {
            crate::control::CompactionRung::NativeFacet
        }
        crate::manifest::RuntimeCompactionStrategy::SemanticSummaryPrefixV1 => {
            crate::control::CompactionRung::SemanticFold
        }
        crate::manifest::RuntimeCompactionStrategy::ToolResultPlaceholdersV1 => {
            crate::control::CompactionRung::ToolResultPlaceholders
        }
        crate::manifest::RuntimeCompactionStrategy::ReplayOmissionV1 => {
            crate::control::CompactionRung::ReplayOmission
        }
    };
    Some(crate::control::CompactionSummary {
        rung,
        segments: provenance.selected_range().2,
        input_tokens_before: provenance.before_audit().input_tokens(),
        input_tokens_after: provenance.after_audit().input_tokens(),
        occupancy_tokens_before: None,
        occupancy_tokens_after: None,
    })
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    #[test]
    fn suppressed_custom_tool_output_activity_stamps_first_output() {
        let activity = plakous_model::ModelStreamActivity {
            output_started: true,
            ..plakous_model::ModelStreamActivity::default()
        };
        assert!(model_event_stamps_ttft(&ModelEvent::StreamActivity(
            activity
        )));
        assert!(!model_event_stamps_ttft(&ModelEvent::StreamActivity(
            plakous_model::ModelStreamActivity::default()
        )));
        assert!(!model_event_stamps_ttft(&ModelEvent::PromptCacheDiagnosis(
            plakous_model::PromptCacheDiagnosis {
                reason: plakous_model::PromptCacheMissReason::MessagesChanged,
                missed_input_tokens: Some(587),
            }
        )));
    }
}
