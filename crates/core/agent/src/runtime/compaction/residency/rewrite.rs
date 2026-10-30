use super::{ImageSelection, ResidencyPlan, invalid};
use crate::runtime::compaction::placeholder::{
    bounded_arguments, harness_note_segment, is_placeholder_harness_note,
    placeholders_notice_segment,
};
use crate::runtime::compaction::record::compaction_uuid;
use crate::runtime::driver::turn_position::summarized_turns;
use crate::{AgentError, SessionWorkflowId};
use plakous_model::{
    CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind,
    ImageBlock, TextBlock, ToolResultBlock, ToolResultPart, sha256_hex,
};
use std::fmt::Write as _;

pub(in crate::runtime) const NOTE_MARKER: &str = "[plak: older context retired]";

pub(in crate::runtime) struct ResidencySuccessor {
    pub history: CanonicalHistory,
    pub notice: HistorySegment,
}

pub(in crate::runtime) fn residency_successor(
    source: &CanonicalHistory,
    plan: &ResidencyPlan,
    workflow: SessionWorkflowId,
) -> Result<ResidencySuccessor, AgentError> {
    plan.validate_source(source)?;
    let mut segments = source.segments().to_vec();
    for selection in &plan.selection {
        let segment = segments
            .get_mut(selection.segment as usize)
            .ok_or_else(|| invalid("image residency segment missing"))?;
        let mut items = segment.items().to_vec();
        let item = items
            .get_mut(selection.item as usize)
            .ok_or_else(|| invalid("image residency item missing"))?;
        *item = rewrite_item(source, item, selection)?;
        *segment = HistorySegment::new(segment.kind(), items)
            .map_err(|_| invalid("image residency segment invalid"))?;
    }
    let existing: Vec<_> = segments
        .iter()
        .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
        .collect();
    if existing.len() > 1 {
        return Err(invalid(
            "image residency requires a single compaction segment",
        ));
    }
    let notice = match existing.first() {
        Some(segment) if segment.items().iter().flat_map(ConversationItem::blocks).any(|block| matches!(block, ContentBlock::Compaction(compaction) if compaction.native_item().is_some())) => (*segment).clone(),
        previous => {
            let summary = previous.and_then(|segment| segment.items().iter().flat_map(ConversationItem::blocks).find_map(|block| match block {
                ContentBlock::Compaction(compaction) => Some(compaction.summary()), _ => None,
            }));
            placeholders_notice_segment(workflow, summary, summarized_turns(source, segments.iter()))?
        }
    };
    if let Some(index) = segments
        .iter()
        .position(|segment| segment.kind() == HistorySegmentKind::Compaction)
    {
        segments[index] = notice.clone();
    } else {
        segments.insert(0, notice.clone());
    }
    segments.retain(|segment| !is_placeholder_harness_note(segment));
    segments.push(note(source, plan, workflow)?);
    Ok(ResidencySuccessor {
        history: CanonicalHistory::new(segments)
            .map_err(|_| invalid("image residency successor invalid"))?,
        notice,
    })
}

fn rewrite_item(
    source: &CanonicalHistory,
    item: &ConversationItem,
    selected: &ImageSelection,
) -> Result<ConversationItem, AgentError> {
    let mut blocks = item.blocks().to_vec();
    let block = blocks
        .get_mut(selected.block as usize)
        .ok_or_else(|| invalid("image residency block missing"))?;
    match (block, selected.part) {
        (block @ ContentBlock::Image(_), None) => {
            let ContentBlock::Image(image) = block else {
                unreachable!()
            };
            let text = replacement(source, selected, image)?;
            *block = ContentBlock::Text(text);
        }
        (ContentBlock::ToolResult(result), Some(part_index)) => {
            let mut parts = result.parts().to_vec();
            let part = parts
                .get_mut(part_index as usize)
                .ok_or_else(|| invalid("image residency part missing"))?;
            let ToolResultPart::Image(image) = part else {
                return Err(invalid("image residency source kind mismatch"));
            };
            *part = ToolResultPart::Text(replacement(source, selected, image)?);
            *result = ToolResultBlock::new(
                result.call_id(),
                result.owner().clone(),
                result.kind(),
                parts,
                result.correlation().clone(),
            )
            .map_err(|_| invalid("image residency result invalid"))?;
        }
        _ => return Err(invalid("image residency source kind mismatch")),
    }
    ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
        .map(|rewritten| rewritten.with_presentation_subject(item.presentation_subject()))
        .map_err(|_| invalid("image residency item invalid"))
}

fn origin(source: &CanonicalHistory, selected: &ImageSelection) -> String {
    let Some(call_id) = selected.call_id else {
        return "user attachment (earlier turn)".to_owned();
    };
    let call = source
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) if call.call_id() == call_id => Some(call),
            _ => None,
        });
    match call {
        Some(call) => format!(
            "tool {}, call_id {call_id}, arguments {}",
            call.exposed_name(),
            bounded_arguments(call.raw_arguments())
        ),
        None => format!("tool unknown, call_id {call_id}, arguments unavailable"),
    }
}

fn replacement(
    source: &CanonicalHistory,
    selected: &ImageSelection,
    image: &ImageBlock,
) -> Result<TextBlock, AgentError> {
    let bytes = image.source().as_bytes();
    let dimensions = bytes
        .and_then(|bytes| plakous_tools::sniff_image(bytes).ok())
        .and_then(|header| header.dimensions())
        .map_or_else(
            || "unknown".to_owned(),
            |(width, height)| format!("{width}x{height}"),
        );
    let length = bytes.map_or_else(|| "unknown".to_owned(), |bytes| bytes.len().to_string());
    let hash = bytes.map_or_else(|| "unavailable".to_owned(), |bytes| sha256_hex(&[bytes]));
    let remedy = if selected.call_id.is_some() {
        "reissue the identified tool safely; check its effects before running it again"
    } else {
        "ask the user to attach it again"
    };
    TextBlock::new(format!("[plak: image replaced]\nThis is not the image. It was removed to bound request size; you cannot see it here.\norigin: {}\nmedia_type: {}\ndimensions: {dimensions}\nbytes: {length}\nsha256: {hash}\nTo see it again: {remedy}.\nDo not describe it beyond what you already wrote down.", origin(source, selected), image.media_type()), None)
        .map_err(|_| invalid("image residency placeholder invalid"))
}

fn note(
    source: &CanonicalHistory,
    plan: &ResidencyPlan,
    workflow: SessionWorkflowId,
) -> Result<HistorySegment, AgentError> {
    let mut text = format!(
        "{NOTE_MARKER}\n{} earlier images were replaced to bound request size. These placeholders are not the images. Removed this pass:",
        plan.selection.len()
    );
    for selected in plan.selection.iter().take(20) {
        let _ = write!(text, "\n  - {}", origin(source, selected));
    }
    if plan.selection.len() > 20 {
        let _ = write!(text, "\n  - and {} more", plan.selection.len() - 20);
    }
    text.push_str("\nUse only facts you already wrote down. Ask for a new attachment or safely reissue the identified tool if you need an image again; do not guess at removed content.");
    let entry = EntryId::try_from(compaction_uuid(workflow, b"residency-harness-note")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    harness_note_segment(entry, text)
}
