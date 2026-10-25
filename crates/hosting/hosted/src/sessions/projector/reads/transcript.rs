//! The one canonical-history part reader for root projection and child views.

use plakous::agent::{ToolCallObservation, ToolCallerLineageView};
use plakous::model::{Actor, CallId, ReasoningVisibility};
use plakous::sessions::{SessionTranscriptItem, SessionTranscriptPart, SessionTranscriptTail};

/// Borrowed, redacted canonical data; no presentation or runtime ownership.
pub(crate) enum TranscriptPart<'a> {
    Text(&'a str),
    Image(String),
    Reasoning {
        visibility: ReasoningVisibility,
        text: &'a str,
    },
    Call {
        call_id: String,
        canonical: &'a str,
        exposed: &'a str,
        observation: Option<&'a ToolCallObservation>,
        lineage: Option<&'a ToolCallerLineageView>,
        composition_owner: Option<String>,
    },
    Result {
        call_id: String,
        succeeded: bool,
        preview: &'a str,
        subject: Option<&'a str>,
        lineage: Option<&'a ToolCallerLineageView>,
    },
    Compaction,
}

pub(crate) struct TranscriptRead<'a> {
    pub(crate) actor: Actor,
    pub(crate) harness_authored: bool,
    pub(crate) parts: Vec<TranscriptPart<'a>>,
}

fn transcript_item<'a>(
    item: &'a SessionTranscriptItem,
    owner: impl Fn(CallId) -> Option<CallId> + 'a,
) -> TranscriptRead<'a> {
    TranscriptRead {
        actor: item.actor(),
        harness_authored: item.is_harness_authored(),
        parts: transcript_parts(item, owner).collect(),
    }
}

pub(super) fn transcript_parts<'a>(
    item: &'a SessionTranscriptItem,
    owner: impl Fn(CallId) -> Option<CallId> + 'a,
) -> impl Iterator<Item = TranscriptPart<'a>> {
    item.parts()
        .iter()
        .filter_map(move |part| transcript_part(part, &owner))
}

pub(crate) fn transcript_tail(tail: &SessionTranscriptTail) -> Vec<TranscriptRead<'_>> {
    tail.items()
        .iter()
        .map(|item| transcript_item(item, |id| tail.composition_owner(id)))
        .collect()
}

fn transcript_part<'a>(
    part: &'a SessionTranscriptPart,
    owner: &impl Fn(CallId) -> Option<CallId>,
) -> Option<TranscriptPart<'a>> {
    match part {
        SessionTranscriptPart::Text(text) => Some(TranscriptPart::Text(text)),
        SessionTranscriptPart::Reasoning { visibility, text } => Some(TranscriptPart::Reasoning {
            visibility: *visibility,
            text,
        }),
        SessionTranscriptPart::ToolCall {
            call_id,
            canonical_name,
            exposed_name,
            observation,
            lineage,
        } => Some(TranscriptPart::Call {
            call_id: call_id.to_string(),
            canonical: canonical_name.as_str(),
            exposed: exposed_name.as_str(),
            observation: Some(observation),
            lineage: Some(lineage),
            composition_owner: owner(*call_id)
                .filter(|owner| owner != call_id)
                .map(|owner| owner.to_string()),
        }),
        SessionTranscriptPart::ToolResult {
            call_id,
            observation,
            lineage,
        } => Some(TranscriptPart::Result {
            call_id: call_id.to_string(),
            succeeded: observation.succeeded(),
            preview: observation.preview(),
            subject: observation.subject(),
            lineage: Some(lineage),
        }),
        SessionTranscriptPart::Compaction => Some(TranscriptPart::Compaction),
        SessionTranscriptPart::Image {
            media_type,
            byte_len,
            dimensions,
        } => Some(TranscriptPart::Image(crate::sessions::image_chip(
            media_type,
            *byte_len,
            *dimensions,
        ))),
        _ => None,
    }
}
