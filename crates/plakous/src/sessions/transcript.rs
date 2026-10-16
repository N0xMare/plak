//! Bounded, newest-first read of committed canonical history as typed
//! presentation parts, for a host that shows one session's transcript.

use super::{CallId, SessionLocator, ToolName, fmt};
use plakous_agent::internal::RuntimeOperationLedger;
use plakous_agent::internal::facade_support::{
    attach_with_committed_history, history_composition_calls, history_composition_child_owners,
    history_nested_tools, history_tool_call_observation, history_tool_result_observation,
};
use plakous_agent::{
    Agent, ToolCallObservation, ToolCallerLineageView, ToolResultObservation, ViewAttachment,
    ViewError, ViewRequest,
};
use plakous_model::{
    Actor, CanonicalHistory, ContentBlock, ConversationItem, ConversationView, Provenance,
    ReasoningVisibility, ToolCallerLineage, WorkingProjection,
};
use std::collections::{HashMap, HashSet};

const MAX_COMPOSITION_RECORDS: usize = 2048;
type CompositionChildOwners = HashMap<(CallId, u64), CallId>;

/// The newest committed history items of one exact session head, oldest
/// first. Tool calls and results carry the same bounded, redacted
/// observations the live tool events carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTranscriptTail {
    locator: SessionLocator,
    items: Vec<SessionTranscriptItem>,
    truncated: bool,
    composition_owners: HashMap<CallId, CallId>,
}

impl SessionTranscriptTail {
    /// Returns the exact head that was read.
    #[must_use]
    pub const fn locator(&self) -> SessionLocator {
        self.locator
    }

    /// Returns the retained items, oldest first.
    #[must_use]
    pub fn items(&self) -> &[SessionTranscriptItem] {
        &self.items
    }

    /// Whether older items were left out by the item or byte bound.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Returns the explicit retained evaluation owner of a marker or
    /// continuation in this window. An opening marker owns itself.
    /// No owner is inferred when the durable binding was not retained.
    #[must_use]
    pub fn composition_owner(&self, call_id: CallId) -> Option<CallId> {
        self.composition_owners.get(&call_id).copied()
    }
}

/// One committed conversation item as presentation parts.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionTranscriptItem {
    actor: Actor,
    provenance: Provenance,
    parts: Vec<SessionTranscriptPart>,
}

impl SessionTranscriptItem {
    /// Returns the responsible actor.
    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// True when the harness authored this item rather than the operator.
    #[must_use]
    pub const fn is_harness_authored(&self) -> bool {
        matches!(self.actor, Actor::User) && matches!(self.provenance, Provenance::Instruction)
    }

    /// Returns the item's parts in canonical block order.
    #[must_use]
    pub fn parts(&self) -> &[SessionTranscriptPart] {
        &self.parts
    }

    fn retained_bytes(&self) -> usize {
        self.parts.iter().fold(128, |total, part| {
            total.saturating_add(match part {
                SessionTranscriptPart::Text(text)
                | SessionTranscriptPart::Reasoning { text, .. } => text.len(),
                SessionTranscriptPart::ToolCall {
                    canonical_name,
                    exposed_name,
                    observation,
                    lineage,
                    ..
                } => 64_usize
                    .saturating_add(lineage.cell_id().map_or(0, str::len))
                    .saturating_add(canonical_name.as_str().len())
                    .saturating_add(exposed_name.as_str().len())
                    .saturating_add(observation.subject().map_or(0, str::len))
                    .saturating_add(observation.arguments_preview().len()),
                SessionTranscriptPart::ToolResult {
                    observation,
                    lineage,
                    ..
                } => 64_usize
                    .saturating_add(lineage.cell_id().map_or(0, str::len))
                    .saturating_add(observation.preview().len())
                    .saturating_add(observation.subject().map_or(0, str::len)),
                SessionTranscriptPart::Compaction => 64,
                SessionTranscriptPart::Image { media_type, .. } => 64 + media_type.len(),
            })
        })
    }
}

impl fmt::Debug for SessionTranscriptItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTranscriptItem")
            .field("actor", &self.actor)
            .field("provenance", &self.provenance)
            .field("parts", &self.parts)
            .finish()
    }
}

/// One presentation part of a committed item. Inline images become
/// [`SessionTranscriptPart::Image`] metadata; referenced images, documents,
/// audio, citation and opaque blocks have no part.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionTranscriptPart {
    /// Text or refusal text.
    Text(String),
    /// Reasoning that carries display text, with the visibility its block
    /// declared; the reader decides whether that visibility is shown.
    Reasoning {
        /// Declared visibility of the block.
        visibility: ReasoningVisibility,
        /// The block's display text.
        text: String,
    },
    /// A tool call and the observation its live start event carried.
    ToolCall {
        /// Canonical call id.
        call_id: CallId,
        /// Canonical executable name; classification follows it.
        canonical_name: ToolName,
        /// Exact name the model called; the call is labelled by it.
        exposed_name: ToolName,
        /// Bounded, redacted subject and argument preview.
        observation: ToolCallObservation,
        /// Redaction-safe caller class and cell identity from canonical history.
        lineage: ToolCallerLineageView,
    },
    /// A tool result and the observation its live completion event carried.
    ToolResult {
        /// The call this result answers.
        call_id: CallId,
        /// Success, the bounded redacted result preview, and an await
        /// receipt's subject when the result is one.
        observation: ToolResultObservation,
        /// Redaction-safe caller class and cell identity from canonical history.
        lineage: ToolCallerLineageView,
    },
    /// A committed compaction marker.
    Compaction,
    /// An inline image a user message carried: its media type, raw size and,
    /// when its header states them, its dimensions. The bytes stay in history.
    Image {
        /// Declared media type, such as `image/png`.
        media_type: String,
        /// Raw encoded size in bytes.
        byte_len: usize,
        /// Width and height in pixels from the image header.
        dimensions: Option<(u32, u32)>,
    },
}

impl fmt::Debug for SessionTranscriptPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Reasoning { visibility, .. } => formatter
                .debug_struct("Reasoning")
                .field("visibility", visibility)
                .finish_non_exhaustive(),
            Self::ToolCall {
                call_id,
                canonical_name,
                exposed_name,
                ..
            } => formatter
                .debug_struct("ToolCall")
                .field("call_id", call_id)
                .field("canonical_name", canonical_name)
                .field("exposed_name", exposed_name)
                .finish_non_exhaustive(),
            Self::ToolResult { call_id, .. } => formatter
                .debug_struct("ToolResult")
                .field("call_id", call_id)
                .finish_non_exhaustive(),
            Self::Compaction => formatter.write_str("Compaction"),
            Self::Image {
                media_type,
                byte_len,
                dimensions,
            } => formatter
                .debug_struct("Image")
                .field("media_type", media_type)
                .field("byte_len", byte_len)
                .field("dimensions", dimensions)
                .finish(),
        }
    }
}

fn project_item(
    item: &ConversationItem,
    child_owners: &CompositionChildOwners,
) -> SessionTranscriptItem {
    let parts = item
        .blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(block) => Some(SessionTranscriptPart::Text(block.text().to_owned())),
            ContentBlock::Refusal(block) => {
                Some(SessionTranscriptPart::Text(block.text().to_owned()))
            }
            ContentBlock::Reasoning(block) => {
                block.text().map(|text| SessionTranscriptPart::Reasoning {
                    visibility: block.visibility(),
                    text: text.to_owned(),
                })
            }
            ContentBlock::ToolCall(call) => Some(SessionTranscriptPart::ToolCall {
                call_id: call.call_id(),
                canonical_name: call.canonical_name().clone(),
                exposed_name: call.exposed_name().clone(),
                observation: history_tool_call_observation(call),
                lineage: history_lineage(
                    call.call_id(),
                    call.correlation().caller_lineage(),
                    child_owners,
                ),
            }),
            ContentBlock::ToolResult(result) => Some(SessionTranscriptPart::ToolResult {
                call_id: result.call_id(),
                observation: history_tool_result_observation(result, item.presentation_subject()),
                lineage: history_lineage(
                    result.call_id(),
                    result.correlation().caller_lineage(),
                    child_owners,
                ),
            }),
            ContentBlock::Compaction(_) => Some(SessionTranscriptPart::Compaction),
            ContentBlock::Image(image) => {
                image
                    .source()
                    .as_bytes()
                    .map(|bytes| SessionTranscriptPart::Image {
                        media_type: image.media_type().to_owned(),
                        byte_len: bytes.len(),
                        dimensions: plakous_tools::sniff_image(bytes)
                            .ok()
                            .and_then(|header| header.dimensions()),
                    })
            }
            _ => None,
        })
        .collect();
    SessionTranscriptItem {
        actor: item.actor(),
        provenance: item.provenance(),
        parts,
    }
}

fn history_lineage(
    call_id: CallId,
    lineage: &ToolCallerLineage,
    child_owners: &CompositionChildOwners,
) -> ToolCallerLineageView {
    let owner = match lineage {
        ToolCallerLineage::LocalProgrammatic {
            evaluation_operation_id,
            child_call_id,
            ..
        } if *child_call_id == call_id => child_owners
            .get(&(call_id, *evaluation_operation_id))
            .copied(),
        _ => None,
    };
    ToolCallerLineageView::from_lineage(lineage, owner)
}

/// One session's committed history, read once and projected on demand. The
/// projection is synchronous CPU work over shared history, so a host can run
/// it off its async workers.
#[derive(Clone)]
pub struct SessionTranscriptSource {
    locator: SessionLocator,
    history: CanonicalHistory,
    ledger: RuntimeOperationLedger,
}

impl fmt::Debug for SessionTranscriptSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTranscriptSource")
            .field("locator", &self.locator)
            .finish_non_exhaustive()
    }
}

impl SessionTranscriptSource {
    pub(super) const fn new(
        locator: SessionLocator,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
    ) -> Self {
        Self {
            locator,
            history,
            ledger,
        }
    }

    /// Attaches to `agent`'s live view and captures its committed history at
    /// the attachment's cursor, atomically: every item in the history was
    /// presented at or before the cursor, and every later commit lies after
    /// it, so the snapshot partitions committed rows from the live tail. The
    /// source's locator is that history's own checkpoint, read in the same
    /// instant.
    ///
    /// # Errors
    ///
    /// Returns the attachment's subscriber-admission or view-scope error.
    pub fn attach(
        agent: &Agent,
        request: ViewRequest,
    ) -> Result<(ViewAttachment, Self), ViewError> {
        let (attachment, history, topology, ledger) =
            attach_with_committed_history(agent, request)?;
        let locator = SessionLocator::new(
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
        );
        Ok((attachment, Self::new(locator, history, ledger)))
    }

    /// Returns the session head the history belongs to.
    #[must_use]
    pub const fn locator(&self) -> SessionLocator {
        self.locator
    }

    /// Projects the newest items as typed presentation parts, oldest first.
    /// Items are projected newest first and projection stops at `max_items`
    /// or `max_bytes` (the newest item is always kept), so older history is
    /// never projected. Tool calls and results carry the observations the
    /// live tool events carry, read over a bounded redaction window.
    #[must_use]
    pub fn tail(&self, max_items: usize, max_bytes: usize) -> SessionTranscriptTail {
        project_transcript_tail(
            &self.history,
            &self.ledger,
            self.locator,
            max_items,
            max_bytes,
        )
    }
}

fn inject_nested_items(
    items: Vec<SessionTranscriptItem>,
    ledger: &RuntimeOperationLedger,
) -> Vec<SessionTranscriptItem> {
    let mut present: HashSet<_> = items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| match part {
            SessionTranscriptPart::ToolCall { call_id, .. }
            | SessionTranscriptPart::ToolResult { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .collect();
    let owners: HashSet<_> = items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| {
            let SessionTranscriptPart::ToolCall { call_id, .. } = part else {
                return None;
            };
            Some(*call_id)
        })
        .collect();
    let mut nested: HashMap<_, Vec<_>> = HashMap::new();
    for call in history_nested_tools(ledger, &owners, MAX_COMPOSITION_RECORDS, 8 * 1_048_576) {
        if present.insert(call.call_id) {
            nested.entry(call.owner).or_default().push(call);
        }
    }
    let mut expanded = Vec::new();
    for item in items {
        let mut parts = Vec::new();
        for part in item.parts {
            let inserted = if let SessionTranscriptPart::ToolCall { call_id, .. } = &part {
                nested.remove(call_id)
            } else {
                None
            };
            parts.push(part);
            if let Some(calls) = inserted {
                expanded.push(SessionTranscriptItem {
                    actor: item.actor,
                    provenance: item.provenance,
                    parts: std::mem::take(&mut parts),
                });
                for call in calls {
                    expanded.push(SessionTranscriptItem {
                        actor: Actor::Tool,
                        provenance: Provenance::ToolExecution,
                        parts: vec![
                            SessionTranscriptPart::ToolCall {
                                call_id: call.call_id,
                                canonical_name: call.canonical_name,
                                exposed_name: call.exposed_name,
                                observation: call.call,
                                lineage: call.lineage.clone(),
                            },
                            SessionTranscriptPart::ToolResult {
                                call_id: call.call_id,
                                observation: call.result,
                                lineage: call.lineage,
                            },
                        ],
                    });
                }
            }
        }
        if !parts.is_empty() {
            expanded.push(SessionTranscriptItem {
                actor: item.actor,
                provenance: item.provenance,
                parts,
            });
        }
    }
    expanded
}

fn composition_child_owners(ledger: &RuntimeOperationLedger) -> CompositionChildOwners {
    history_composition_child_owners(ledger, MAX_COMPOSITION_RECORDS)
        .into_iter()
        .map(|(child, evaluation, owner)| ((child, evaluation), owner))
        .collect()
}

/// Rewrites the lineage of every Code Cell marker or continuation in `items`
/// and returns the composition bindings it applied.
fn mark_composition(
    items: &mut [SessionTranscriptItem],
    ledger: &RuntimeOperationLedger,
) -> HashMap<CallId, CallId> {
    // The ledger scan is bounded independently of the history window; nested
    // ownership uses indexed lookups even if its evaluation is older than it.
    let mut bindings: HashMap<_, _> = history_composition_calls(ledger, MAX_COMPOSITION_RECORDS)
        .into_iter()
        .collect();
    let mut cells = HashMap::new();
    for part in items.iter().flat_map(|item| &item.parts) {
        if let SessionTranscriptPart::ToolCall { lineage, .. }
        | SessionTranscriptPart::ToolResult { lineage, .. } = part
            && let (Some(owner), Some(cell)) = (lineage.parent_call_id(), lineage.cell_id())
        {
            cells.insert(owner, cell.to_owned());
            bindings.entry(owner).or_insert(owner);
        }
    }
    for part in items.iter_mut().flat_map(|item| &mut item.parts) {
        if let SessionTranscriptPart::ToolCall {
            call_id, lineage, ..
        }
        | SessionTranscriptPart::ToolResult {
            call_id, lineage, ..
        } = part
            && let Some(owner) = bindings.get(call_id)
        {
            *lineage = ToolCallerLineageView::local_composition_marker(
                cells.get(owner).map(String::as_str),
            );
        }
    }
    bindings
}

/// Projects exactly `items` as the transcript projects them, with the same
/// nested Code Cell items and composition markers.
pub(super) fn project_items(
    items: &[ConversationItem],
    ledger: &RuntimeOperationLedger,
) -> Vec<SessionTranscriptItem> {
    let child_owners = composition_child_owners(ledger);
    let projected = items
        .iter()
        .map(|item| project_item(item, &child_owners))
        .collect();
    let mut projected = inject_nested_items(projected, ledger);
    mark_composition(&mut projected, ledger);
    projected
}

fn project_transcript_tail(
    history: &CanonicalHistory,
    ledger: &RuntimeOperationLedger,
    locator: SessionLocator,
    max_items: usize,
    max_bytes: usize,
) -> SessionTranscriptTail {
    let view = ConversationView::from_shared(history.clone(), WorkingProjection::default());
    let all = view.items();
    let child_owners = composition_child_owners(ledger);
    let mut items = Vec::new();
    let mut retained = 0_usize;
    let mut index = all.len();
    while index > 0 && items.len() < max_items {
        let item = project_item(&all[index - 1], &child_owners);
        let bytes = item.retained_bytes();
        if !items.is_empty() && retained.saturating_add(bytes) > max_bytes {
            break;
        }
        retained = retained.saturating_add(bytes);
        items.push(item);
        index -= 1;
    }
    items.reverse();
    let mut items = inject_nested_items(items, ledger);
    let bindings = mark_composition(&mut items, ledger);
    // Marker cell identities add bytes, so enforce the item byte bound again.
    let mut retained = items.iter().fold(0_usize, |bytes, item| {
        bytes.saturating_add(item.retained_bytes())
    });
    let mut dropped = 0;
    while items.len().saturating_sub(dropped) > 1
        && (retained > max_bytes || items.len().saturating_sub(dropped) > max_items)
    {
        retained = retained.saturating_sub(items[dropped].retained_bytes());
        dropped += 1;
    }
    items.drain(..dropped);
    let composition_owners = items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| {
            let (SessionTranscriptPart::ToolCall { call_id, .. }
            | SessionTranscriptPart::ToolResult { call_id, .. }) = part
            else {
                return None;
            };
            bindings.get(call_id).map(|owner| (*call_id, *owner))
        })
        .collect();
    SessionTranscriptTail {
        locator,
        items,
        truncated: index > 0 || dropped > 0,
        composition_owners,
    }
}

#[cfg(test)]
mod tests;
