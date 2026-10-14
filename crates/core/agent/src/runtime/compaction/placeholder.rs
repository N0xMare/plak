//! Offline replacement of tool-result bodies with placeholders.
//!
//! This is the third rung of the compaction ladder, below the provider-native
//! facet and below the semantic fold. It runs when the next model round would
//! overflow the admitted ceiling (or the 85 % fence is crossed between rounds)
//! and neither the native facet nor the fold can act, which is exactly the
//! single-turn shape where the latest user intent and its whole causal tail are
//! the entire session.
//!
//! The transformation is deterministic and offline: no model call, no paid
//! dispatch. Oldest completed tool results lose their bodies first; each
//! call-and-result group stays whole and paired, and only the result body is
//! replaced by a bounded placeholder naming the tool, the call id and the
//! original size in bytes. A placeholder-only pass never changes user intent,
//! assistant text, reasoning, encrypted replay or committed compaction outputs.
//! An explicitly authorized portable pass also records replay loss with a
//! steering note and the full source range. The history's sole compaction
//! segment records the pass, rather than leaving a silent deletion.

use crate::runtime::compaction::record::compaction_uuid;
use crate::runtime::driver::turn_position::summarized_turns;
use crate::{AgentError, AgentErrorKind, SessionWorkflowId};
use plakous_model::{
    Actor, CallId, CanonicalHistory, CompactionBlock, ContentBlock, ConversationItem, EntryId,
    HistorySegment, HistorySegmentKind, Provenance, SemanticCompactionCut, SummarizedTurns,
    TextBlock, ToolName, ToolResultBlock, ToolResultPart,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Marker that opens every replaced body, so a pass is idempotent and a
/// later pass can tell an already-replaced result from a live one.
pub(in crate::runtime) const REPLACED_TOOL_RESULT_MARKER: &str = "[plak: tool result replaced]";

/// One completed tool result whose body may be replaced, in history order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::runtime) struct PlaceholderCandidate {
    segment_index: usize,
    item_index: usize,
    block_index: usize,
    call_id: CallId,
    tool: Option<ToolName>,
    /// The call's arguments exactly as the model sent them, bounded.
    ///
    /// A placeholder naming only the tool is not enough to act on: after
    /// twenty reads, `read_file` identifies nothing. The model needs the
    /// arguments it wrote — the path, the offset, the limit, the command — to
    /// know which result went and to reissue the same call.
    arguments: Option<String>,
    bytes: u64,
}

impl PlaceholderCandidate {
    /// Returns the segment position of the group this candidate belongs to.
    pub(in crate::runtime) const fn segment_index(&self) -> usize {
        self.segment_index
    }
}

/// The exact successor of one placeholders pass, with its selection evidence.
pub(in crate::runtime) struct PlaceholderSuccessor {
    pub(in crate::runtime) history: CanonicalHistory,
    pub(in crate::runtime) notice: HistorySegment,
    pub(in crate::runtime) selected_start: u32,
    pub(in crate::runtime) selected_end: u32,
    pub(in crate::runtime) replaced_results: u32,
    pub(in crate::runtime) replaced_bytes: u64,
}

fn tool_result_part_bytes(part: &ToolResultPart) -> u64 {
    match part {
        ToolResultPart::Text(text) => u64::try_from(text.text().len()).unwrap_or(u64::MAX),
        other => serde_json::to_vec(other)
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
    }
}

fn tool_result_bytes(result: &ToolResultBlock) -> u64 {
    result
        .parts()
        .iter()
        .map(tool_result_part_bytes)
        .fold(0, u64::saturating_add)
}

fn already_replaced(result: &ToolResultBlock) -> bool {
    matches!(
        result.parts(),
        [ToolResultPart::Text(text)] if text.text().starts_with(REPLACED_TOOL_RESULT_MARKER)
    )
}

/// True when this segment carries semantic user intent.
///
/// This mirrors `is_semantic_user_intent` in the history planner exactly.
/// Steering is deliberately excluded there and here: the harness note an
/// placeholders leaves for the model is a steering segment, and counting it as
/// intent would move the turn boundary forward and leave the next pass with
/// nothing to replace.
fn is_user_intent_segment(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::RecentIntent
        || (segment.kind() == HistorySegmentKind::Conversation
            && segment
                .items()
                .iter()
                .any(|item| item.actor() == Actor::User))
}

/// The segment index at which the current turn's causal tail begins.
///
/// That is the last segment carrying user intent. In the single-turn shape the
/// defect describes, this is the first segment after the stable prefix, so the
/// eligible set is the whole protected tail.
fn current_turn_start(history: &CanonicalHistory) -> usize {
    let mut start = 0;
    for (index, segment) in history.segments().iter().enumerate() {
        if is_user_intent_segment(segment) {
            start = index;
        }
    }
    start
}

/// The newest complete tool exchange while no model round has answered it:
/// the batch the model just received.
///
/// Its results are what the current round asked for, so they are exactly the
/// ones the model is about to use. Replacing them guarantees the loss the rung
/// exists to avoid, and the admission path already clips an overflowing batch
/// before the fence ever runs. Once a later model round has answered it (the
/// turn's final reply, seen at the idle fence) the batch is consumed like any
/// older one, and it is the only body that idle fence can replace.
fn current_round_batch(history: &CanonicalHistory) -> Option<usize> {
    let mut newest = None;
    let mut answered = false;
    for (index, segment) in history.segments().iter().enumerate() {
        if segment.kind() == HistorySegmentKind::ToolExchange {
            newest = Some(index);
            answered = false;
        } else if newest.is_some()
            && matches!(
                segment.kind(),
                HistorySegmentKind::Conversation | HistorySegmentKind::ReasoningReplay
            )
            && segment
                .items()
                .iter()
                .any(|item| item.actor() == Actor::Assistant)
        {
            answered = true;
        }
    }
    newest.filter(|_| !answered)
}

/// Collects replaceable tool-result bodies of the current turn, oldest first.
///
/// Results already replaced are skipped so repeated passes make progress, and
/// compaction segments are never entered.
pub(in crate::runtime) fn collect_placeholder_candidates(
    history: &CanonicalHistory,
) -> Vec<PlaceholderCandidate> {
    let calls = collect_calls(history);
    let start = current_turn_start(history);
    let newest_batch = current_round_batch(history);
    // An unanswered newest batch has not yet been consumed by a model round.
    // Replacing it merely makes the model reissue the same reads and can turn
    // an irreducible overflow into an unbounded compact/read loop. Admission
    // has already bounded the batch itself, so only consumed results are
    // eligible here.
    collect_from(history, &calls, start, newest_batch)
}

fn collect_calls(history: &CanonicalHistory) -> BTreeMap<CallId, (ToolName, String)> {
    let mut calls: BTreeMap<CallId, (ToolName, String)> = BTreeMap::new();
    for segment in history.segments() {
        for item in segment.items() {
            for block in item.blocks() {
                if let ContentBlock::ToolCall(call) = block {
                    calls.insert(
                        call.call_id(),
                        (call.exposed_name().clone(), call.raw_arguments().to_owned()),
                    );
                }
            }
        }
    }
    calls
}

/// Summary-only elision uses the sealed selection, not the current-turn
/// placeholder rung. The newest batch is already retained by the planner;
/// neither the prior summary nor a copied latest intent is selected here.
pub(super) fn collect_compaction_reduction_candidates(
    cut: &SemanticCompactionCut,
) -> Vec<PlaceholderCandidate> {
    let history = cut.summary_input().history();
    let selected: std::collections::BTreeSet<_> = cut
        .selected_segments()
        .iter()
        .map(|segment| segment.identity().identity_bytes())
        .collect();
    collect_from(history, &collect_calls(history), 0, None)
        .into_iter()
        .filter(|candidate| {
            history
                .segments()
                .get(candidate.segment_index)
                .is_some_and(|segment| selected.contains(&segment.identity().identity_bytes()))
        })
        .collect()
}

fn collect_from(
    history: &CanonicalHistory,
    calls: &BTreeMap<CallId, (ToolName, String)>,
    start: usize,
    spare: Option<usize>,
) -> Vec<PlaceholderCandidate> {
    let mut candidates = Vec::new();
    for (segment_index, segment) in history.segments().iter().enumerate() {
        if segment_index < start
            || segment.kind() == HistorySegmentKind::Compaction
            || spare == Some(segment_index)
        {
            continue;
        }
        for (item_index, item) in segment.items().iter().enumerate() {
            for (block_index, block) in item.blocks().iter().enumerate() {
                let ContentBlock::ToolResult(result) = block else {
                    continue;
                };
                if already_replaced(result) {
                    continue;
                }
                let call = calls.get(&result.call_id());
                candidates.push(PlaceholderCandidate {
                    segment_index,
                    item_index,
                    block_index,
                    call_id: result.call_id(),
                    tool: call.map(|(name, _)| name.clone()),
                    arguments: call.map(|(_, arguments)| bounded_arguments(arguments)),
                    bytes: tool_result_bytes(result),
                });
            }
        }
    }
    candidates
}

/// The most argument text a placeholder will carry.
///
/// Long enough for a path plus an offset and a limit, or a shell command;
/// short enough that replacing a body never costs more than it saves.
const MAX_REPLACED_ARGUMENT_BYTES: usize = 512;

fn bounded_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.len() <= MAX_REPLACED_ARGUMENT_BYTES {
        return trimmed.to_owned();
    }
    let mut end = MAX_REPLACED_ARGUMENT_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

/// The bounded placeholder that replaces one tool-result body.
///
/// This is the only thing the model will see where the content used to be, so
/// it says, in the model's own vocabulary, exactly which call it stands for and
/// what to do about it. Live growth runs are why: a model
/// whose reads had been replaced wrote one invented line count for every file
/// rather than re-reading, and another spent 267 tool calls thrashing without
/// ever producing its answer. Naming the tool and the arguments as the model
/// sent them turns "something is missing" into "re-issue this exact call".
///
/// It must also be unmistakable as a placeholder. An empty or terse body reads
/// as a tool that returned nothing, which is a fact about the workspace; this
/// reads as a fact about the conversation.
fn placeholder_body(candidate: &PlaceholderCandidate) -> String {
    let tool = candidate
        .tool
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), |name| name.as_str().to_owned());
    let arguments = candidate
        .arguments
        .as_deref()
        .filter(|arguments| !arguments.is_empty())
        .unwrap_or("{}");
    format!(
        "{REPLACED_TOOL_RESULT_MARKER}\n\
         This is not the tool's output. The output of this call was removed from the \
         conversation to fit the context window; it is no longer available to you here.\n\
         tool: {tool}\n\
         arguments: {arguments}\n\
         call_id: {}\n\
         removed_bytes: {}\n\
         If you still need this content, call {tool} again with the same arguments. Do not \
         guess or reconstruct what it said.",
        candidate.call_id, candidate.bytes,
    )
}

fn invalid(message: &'static str) -> AgentError {
    AgentError::new(AgentErrorKind::Internal, message)
}

/// Rewrites one tool result in place, keeping its call pairing intact.
fn replace_result_body(
    item: &ConversationItem,
    block_index: usize,
    candidate: &PlaceholderCandidate,
) -> Result<ConversationItem, AgentError> {
    let mut blocks = item.blocks().to_vec();
    let block = blocks
        .get_mut(block_index)
        .ok_or_else(|| invalid("tool-result placeholders lost its exact block position"))?;
    let ContentBlock::ToolResult(result) = block else {
        return Err(invalid(
            "tool-result placeholders addressed a non-result block",
        ));
    };
    let replacement = ToolResultBlock::new(
        result.call_id(),
        result.owner().clone(),
        result.kind(),
        vec![ToolResultPart::Text(
            TextBlock::new(placeholder_body(candidate), None)
                .map_err(|_| invalid("replaced tool-result placeholder is invalid"))?,
        )],
        result.correlation().clone(),
    )
    .map_err(|_| invalid("replaced tool result is invalid"))?;
    *block = ContentBlock::ToolResult(replacement);
    let subject = item.presentation_subject().map(str::to_owned);
    ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
        .map(|built| built.with_presentation_subject(subject.as_deref()))
        .map_err(|_| invalid("replaced tool-result item is invalid"))
}

/// Body rewrite only: no steering, notice, or canonical publication. Positions
/// remain stable so source-derived candidates can be applied incrementally.
pub(super) fn replace_compaction_result_body(
    history: &CanonicalHistory,
    candidate: &PlaceholderCandidate,
) -> Result<CanonicalHistory, AgentError> {
    let mut segments = history.segments().to_vec();
    let segment = segments
        .get_mut(candidate.segment_index)
        .ok_or_else(|| invalid("summary reduction lost its exact segment"))?;
    let mut items = segment.items().to_vec();
    let item = items
        .get_mut(candidate.item_index)
        .ok_or_else(|| invalid("summary reduction lost its exact item"))?;
    *item = replace_result_body(item, candidate.block_index, candidate)?;
    *segment = HistorySegment::new(segment.kind(), items)
        .map_err(|_| invalid("summary reduction segment is invalid"))?;
    CanonicalHistory::new(segments).map_err(|_| invalid("summary reduction history is invalid"))
}

/// Opens the one line a placeholder pass contributes to the canonical summary.
///
/// Coalescing on this prefix keeps the notice to a single line however many
/// passes a session runs: the exact per-pass counts live in the compaction
/// provenance and the `context.placeholders` records, which is where an auditor
/// reads them. The model only needs to know that bodies were replaced and how
/// to get them back.
const PLACEHOLDER_NOTICE_PREFIX: &str = "Tool result placeholders:";

fn placeholder_notice_line() -> String {
    format!(
        "{PLACEHOLDER_NOTICE_PREFIX} tool result bodies in this conversation have been replaced \
         with bounded placeholders so it fits this model's context window. Every tool call and \
         its result remain paired; re-run a call to see its content again.",
    )
}

/// The canonical summary text after this pass, given the prior summary.
///
/// A prior semantic summary is kept verbatim and the placeholder pass line is added
/// under it. A prior placeholders line is superseded rather than stacked.
fn merged_notice_text(previous: Option<&str>) -> String {
    let line = placeholder_notice_line();
    let Some(previous) = previous else {
        return line;
    };
    let kept = previous
        .lines()
        .filter(|candidate| {
            !candidate
                .trim_start()
                .starts_with(PLACEHOLDER_NOTICE_PREFIX)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if kept.trim().is_empty() {
        line
    } else {
        format!("{kept}\n{line}")
    }
}

/// The single canonical compaction segment this pass publishes.
///
/// The planner admits exactly one compaction segment in a canonical history
/// (`HistoryPlanningError::NoncanonicalCompaction`), and a semantic cut refuses
/// a retained tail that holds one. A placeholder pass therefore never adds a segment
/// beside an existing summary: it merges into it, or creates the only one.
fn placeholders_notice_segment(
    workflow_id: SessionWorkflowId,
    previous: Option<&str>,
    turns: Option<SummarizedTurns>,
) -> Result<HistorySegment, AgentError> {
    let entry_id = EntryId::try_from(compaction_uuid(workflow_id, b"placeholders-notice-entry")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    let mut notice = CompactionBlock::new(merged_notice_text(previous))
        .map_err(|_| invalid("placeholders notice is invalid"))?;
    if let Some(turns) = turns {
        notice = notice.with_summarized_turns(turns);
    }
    let item = ConversationItem::new(
        entry_id,
        Actor::Assistant,
        vec![ContentBlock::Compaction(notice)],
        Provenance::ModelOutput,
    )
    .map_err(|_| invalid("placeholders notice item is invalid"))?;
    HistorySegment::new(HistorySegmentKind::Compaction, vec![item])
        .map_err(|_| invalid("placeholders notice segment is invalid"))
}

/// The position and text of the one compaction segment a history already has.
///
/// Refuses a summary carrying provider-native replay material. Merging builds
/// a fresh [`CompactionBlock`], which has no `native_item`, so merging into a
/// native compaction item would silently drop ciphertext the family contract
/// requires to be replayed byte-exactly — and prepending a second segment
/// instead would make the history unplannable. Failing closed is the only
/// correct answer. This is unreachable today because a route with the native
/// facet takes the facet branch before the ladder reaches this rung.
fn existing_compaction_summary(
    history: &CanonicalHistory,
) -> Result<Option<(usize, String)>, AgentError> {
    let mut found = None;
    for (index, segment) in history.segments().iter().enumerate() {
        if segment.kind() != HistorySegmentKind::Compaction {
            continue;
        }
        if found.is_some() {
            return Err(invalid(
                "tool-result placeholders found more than one compaction segment",
            ));
        }
        let mut text = String::new();
        for block in segment.items().iter().flat_map(ConversationItem::blocks) {
            if let ContentBlock::Compaction(compaction) = block {
                if compaction.native_item().is_some() {
                    return Err(invalid(
                        "tool-result placeholders cannot merge into a native compaction item",
                    ));
                }
                compaction.summary().clone_into(&mut text);
            }
        }
        found = Some((index, text));
    }
    Ok(found)
}

/// Opens the harness note a placeholder pass leaves for the next model round.
pub(in crate::runtime) const PLACEHOLDER_HARNESS_NOTE_MARKER: &str =
    "[plak: oldest tool results replaced this round]";

/// True when this segment is a previous pass's harness note.
fn is_placeholder_harness_note(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::Steering
        && segment
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| match block {
                ContentBlock::Text(text) => {
                    text.text().starts_with(PLACEHOLDER_HARNESS_NOTE_MARKER)
                }
                _ => false,
            })
}

/// The steering note the model reads in the round after a placeholder pass.
///
/// It is a steering segment, not user intent: the history planner's
/// `is_semantic_user_intent` ignores steering, so the note does not move the
/// turn boundary, does not become the protected latest intent and does not
/// make the current turn's own work eligible for the fold. It is still an
/// ordinary user-role message on every protocol, which `Actor::Harness` and
/// `Actor::Developer` are not — the Messages protocol refuses both in history.
///
/// It exists because a placeholder alone is passive. The live growth runs
/// showed models continuing past replaced reads and answering from memory; a
/// note at the tail, where the model is actually working, tells it what just
/// happened and asks it to write down what it still needs before it loses more.
fn placeholders_harness_note_segment(
    workflow_id: SessionWorkflowId,
    selected: &[&PlaceholderCandidate],
) -> Result<HistorySegment, AgentError> {
    let entry_id = EntryId::try_from(compaction_uuid(workflow_id, b"placeholders-harness-note")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    let mut text = format!(
        "{PLACEHOLDER_HARNESS_NOTE_MARKER}\nThe conversation exceeded this model's context \
         window, so the output of {} earlier tool call(s) was removed. Their results are \
         gone; a placeholder marks each one. Removed this round:",
        selected.len(),
    );
    for candidate in selected {
        let tool = candidate
            .tool
            .as_ref()
            .map_or("unknown", |name| name.as_str());
        let arguments = candidate
            .arguments
            .as_deref()
            .filter(|arguments| !arguments.is_empty())
            .unwrap_or("{}");
        let _ = write!(text, "\n  - {tool} {arguments}");
    }
    text.push_str(
        "\nBefore continuing, write down in your own words the facts you still need from \
         those results, so they survive the next removal. Anything you cannot state now, \
         re-read by calling the same tool with the same arguments. Do not guess at removed \
         content.",
    );
    harness_note_segment(entry_id, text)
}

pub(crate) fn harness_note_segment(
    entry_id: EntryId,
    text: String,
) -> Result<HistorySegment, AgentError> {
    let item = ConversationItem::new(
        entry_id,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(text, None).map_err(
            |_| invalid("placeholders harness note is invalid"),
        )?)],
        // Instruction, not `UserInput`: the operator did not type this. A
        // steering segment must carry `Actor::User` to lower as a user-role
        // message on every protocol, and `Actor::Harness` is refused in
        // history by Messages, so provenance is the only field left that can
        // tell a harness note from something the operator wrote. Surfaces
        // label it from that.
        Provenance::Instruction,
    )
    .map_err(|_| invalid("placeholders harness note item is invalid"))?;
    HistorySegment::new(HistorySegmentKind::Steering, vec![item])
        .map_err(|_| invalid("placeholders harness note segment is invalid"))
}

/// Builds the successor for replacing the first `count` candidate bodies.
///
/// The successor keeps every source segment in order and rewrites only the
/// selected tool-result bodies. The placeholder pass is recorded in the history's sole
/// compaction segment: merged into the existing summary when the source has
/// one, and otherwise created at position zero, which is where a semantic
/// summary sits and where the planner's barrier rule wants it. A second
/// compaction segment would make the whole history unplannable, so the rung
/// would work exactly once.
pub(in crate::runtime) fn placeholder_successor(
    history: &CanonicalHistory,
    candidates: &[PlaceholderCandidate],
    count: usize,
    workflow_id: SessionWorkflowId,
) -> Result<PlaceholderSuccessor, AgentError> {
    let selected = candidates
        .get(..count)
        .ok_or_else(|| invalid("tool-result placeholders selected more results than exist"))?;
    if selected.is_empty() {
        return Err(invalid(
            "tool-result placeholders require at least one result",
        ));
    }
    let selected_ordered: &[&PlaceholderCandidate] = &selected.iter().collect::<Vec<_>>();
    let mut by_item: BTreeMap<(usize, usize), Vec<&PlaceholderCandidate>> = BTreeMap::new();
    let mut replaced_bytes = 0_u64;
    for candidate in selected {
        by_item
            .entry((candidate.segment_index, candidate.item_index))
            .or_default()
            .push(candidate);
        replaced_bytes = replaced_bytes.saturating_add(candidate.bytes);
    }
    let mut segments = Vec::new();
    for (segment_index, segment) in history.segments().iter().enumerate() {
        // A previous pass's note is stale the moment this one runs: it names
        // results that are no longer the newest loss. One note, always at the
        // tail, always current.
        if is_placeholder_harness_note(segment) {
            continue;
        }
        let touched = by_item
            .keys()
            .any(|(candidate_segment, _)| *candidate_segment == segment_index);
        if !touched {
            segments.push(segment.clone());
            continue;
        }
        let mut items = Vec::with_capacity(segment.items().len());
        for (item_index, item) in segment.items().iter().enumerate() {
            match by_item.get(&(segment_index, item_index)) {
                None => items.push(item.clone()),
                Some(candidates) => {
                    let mut rewritten = item.clone();
                    for candidate in candidates {
                        rewritten =
                            replace_result_body(&rewritten, candidate.block_index, candidate)?;
                    }
                    items.push(rewritten);
                }
            }
        }
        segments.push(
            HistorySegment::new(segment.kind(), items)
                .map_err(|_| invalid("replaced history segment is invalid"))?,
        );
    }
    let selected_start = u32::try_from(
        selected
            .iter()
            .map(PlaceholderCandidate::segment_index)
            .min()
            .unwrap_or(0),
    )
    .map_err(|_| invalid("placeholders selection start is out of range"))?;
    let selected_end = u32::try_from(
        selected
            .iter()
            .map(PlaceholderCandidate::segment_index)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    )
    .map_err(|_| invalid("placeholders selection end is out of range"))?;
    let replaced_results =
        u32::try_from(selected.len()).map_err(|_| invalid("placeholders count is out of range"))?;
    let existing = existing_compaction_summary(history)?;
    // Placeholders remove no turn: the merged notice records what the
    // existing summary recorded, or nothing removed when there was none.
    let notice = placeholders_notice_segment(
        workflow_id,
        existing.as_ref().map(|(_, text)| text.as_str()),
        summarized_turns(history, segments.iter()),
    )?;
    match existing {
        Some((index, _)) => {
            let slot = segments
                .get_mut(index)
                .ok_or_else(|| invalid("tool-result placeholders lost the existing summary"))?;
            *slot = notice.clone();
        }
        None => segments.insert(0, notice.clone()),
    }
    // Exactly one note per pass, published in the same commit as the
    // placeholders, so a model can never see the placeholders without the note or
    // the note without the placeholders.
    segments.push(placeholders_harness_note_segment(
        workflow_id,
        selected_ordered,
    )?);
    let history = CanonicalHistory::new(segments)
        .map_err(|_| invalid("tool-result placeholders successor is invalid"))?;
    Ok(PlaceholderSuccessor {
        history,
        notice,
        selected_start,
        selected_end,
        replaced_results,
        replaced_bytes,
    })
}

pub(in crate::runtime) fn replay_omission_successor(
    source: &CanonicalHistory,
    history: &CanonicalHistory,
    workflow_id: SessionWorkflowId,
) -> Result<PlaceholderSuccessor, AgentError> {
    if source.identity() == history.identity() {
        return Err(invalid(
            "replay omission requires a distinct portable history",
        ));
    }
    let mut segments = history.segments().to_vec();
    let existing = segments
        .iter()
        .position(|segment| segment.kind() == HistorySegmentKind::Compaction);
    // An existing native compaction item remains byte-exact. The steering note
    // and offline provenance describe this pass without altering its ciphertext.
    let notice = if let Some(index) = existing {
        segments[index].clone()
    } else {
        let entry = EntryId::try_from(compaction_uuid(workflow_id, b"replay-omission-notice")?)
            .map_err(|error| AgentError::from_identity(&error))?;
        let mut block = CompactionBlock::new(
            "Earlier unbound opaque replay and reasoning were omitted to fit the context window. Portable tool calls and results are retained.",
        ).map_err(|_| invalid("replay omission notice is invalid"))?;
        if let Some(turns) = summarized_turns(history, segments.iter()) {
            block = block.with_summarized_turns(turns);
        }
        let item = ConversationItem::new(
            entry,
            Actor::Assistant,
            vec![ContentBlock::Compaction(block)],
            Provenance::ModelOutput,
        )
        .map_err(|_| invalid("replay omission item is invalid"))?;
        let notice = HistorySegment::new(HistorySegmentKind::Compaction, vec![item])
            .map_err(|_| invalid("replay omission segment is invalid"))?;
        segments.insert(0, notice.clone());
        notice
    };
    segments.push(replay_omission_note_segment(workflow_id, false)?);
    Ok(PlaceholderSuccessor {
        history: CanonicalHistory::new(segments)
            .map_err(|_| invalid("replay omission successor is invalid"))?,
        notice,
        selected_start: 0,
        selected_end: u32::try_from(source.segments().len())
            .map_err(|_| invalid("replay omission range is invalid"))?,
        replaced_results: 0,
        replaced_bytes: 0,
    })
}

/// A portable pass records the whole source range, including replay removed
/// outside the selected result bodies. The same successor is measured and published.
pub(in crate::runtime) fn portable_placeholder_successor(
    source: &CanonicalHistory,
    history: &CanonicalHistory,
    candidates: &[PlaceholderCandidate],
    count: usize,
    workflow_id: SessionWorkflowId,
) -> Result<PlaceholderSuccessor, AgentError> {
    if source.identity() == history.identity() {
        return Err(invalid(
            "portable placeholders require a distinct portable history",
        ));
    }
    let mut successor = placeholder_successor(history, candidates, count, workflow_id)?;
    let mut segments = successor.history.segments().to_vec();
    segments.push(replay_omission_note_segment(workflow_id, true)?);
    successor.history = CanonicalHistory::new(segments)
        .map_err(|_| invalid("portable placeholders successor is invalid"))?;
    successor.selected_start = 0;
    successor.selected_end = u32::try_from(source.segments().len())
        .map_err(|_| invalid("portable placeholders source range is invalid"))?;
    Ok(successor)
}

fn replay_omission_note_segment(
    workflow_id: SessionWorkflowId,
    replaced_results: bool,
) -> Result<HistorySegment, AgentError> {
    let entry = EntryId::try_from(compaction_uuid(workflow_id, b"replay-omission-steering")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    let retained = if replaced_results {
        "Portable tool calls and unreplaced results remain available."
    } else {
        "Every portable tool call and result remains available."
    };
    let item = ConversationItem::new(entry, Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            format!("[plak: earlier reasoning dropped to fit]\nEarlier unbound opaque replay and reasoning were dropped to fit the context window. {retained} Continue from those observations; do not guess at omitted reasoning."), None,
        ).map_err(|_| invalid("replay omission steering text is invalid"))?)], Provenance::Instruction)
        .map_err(|_| invalid("replay omission steering item is invalid"))?;
    HistorySegment::new(HistorySegmentKind::Steering, vec![item])
        .map_err(|_| invalid("replay omission steering segment is invalid"))
}
