//! Turn projection bookkeeping.

use super::reasoning::{reasoning_location_key, reasoning_visibility_admits, sealed_reasoning};
use crate::runtime::{
    ComposeProjection, MAX_PENDING_REASONING_BYTES, ReasoningLocation, SealedReasoning,
    TurnProjection,
};
use crate::session_log::redact_text_preview;
use std::collections::HashMap;
use std::time::Instant;

impl<P: plakous_host_protocol::ProtocolExtension> TurnProjection<P> {
    pub(in crate::runtime) fn observe_block_start(
        &mut self,
        started: &plakous::model::BlockStarted,
    ) {
        let key = (
            started.location().item_index(),
            started.location().block_index(),
        );
        match started.block() {
            plakous::model::BlockStart::Reasoning { visibility, .. } => {
                let state = if reasoning_visibility_admits(*visibility) {
                    ReasoningLocation::Admitted {
                        emitted: false,
                        text: String::new(),
                        started: Instant::now(),
                    }
                } else {
                    ReasoningLocation::Suppressed
                };
                self.reasoning.insert(key, state);
            }
            plakous::model::BlockStart::ReasoningPending { .. } => {
                self.reasoning.insert(
                    key,
                    ReasoningLocation::Pending {
                        buffered: Vec::new(),
                    },
                );
            }
            _ => {}
        }
    }

    /// Returns the text to project, or `None` when the location does not admit.
    pub(in crate::runtime) fn observe_reasoning_delta(
        &mut self,
        delta: &plakous::model::ReasoningDelta,
    ) -> Option<String> {
        let key = (
            delta.location().item_index(),
            delta.location().block_index(),
        );
        let state = self
            .reasoning
            .entry(key)
            // A delta for a location the host never saw start has no bound, and
            // an unbound location is non-admitting.
            .or_insert(ReasoningLocation::Suppressed);
        match state {
            ReasoningLocation::Pending { buffered } => {
                if buffered.len().saturating_add(delta.bytes().len()) > MAX_PENDING_REASONING_BYTES
                {
                    *state = ReasoningLocation::Suppressed;
                } else {
                    buffered.extend_from_slice(delta.bytes());
                }
                None
            }
            ReasoningLocation::Admitted {
                emitted,
                text,
                started,
            } => {
                let piece = std::str::from_utf8(delta.bytes()).ok()?;
                if piece.is_empty() {
                    return None;
                }
                let preview = redact_text_preview(piece);
                if !*emitted {
                    *started = Instant::now();
                }
                *emitted = true;
                text.push_str(&preview);
                Some(preview)
            }
            ReasoningLocation::Suppressed => None,
        }
    }

    /// Resolves one pending location, releasing or dropping its buffer.
    ///
    /// Only a `Pending` location transitions. A `Suppressed` location stays
    /// suppressed for the remainder of the block: re-admitting it
    /// on a later bound would emit a partial reasoning tail whose head the host
    /// already dropped. An `Admitted` location keeps the `emitted` flag it
    /// earned, so a redundant bound cannot cost it its seal.
    pub(in crate::runtime) fn bind_reasoning_visibility(
        &mut self,
        bound: plakous::model::ReasoningVisibilityBound,
    ) -> Option<String> {
        let key = (
            bound.location().item_index(),
            bound.location().block_index(),
        );
        let admits = reasoning_visibility_admits(bound.visibility());
        match self.reasoning.get(&key) {
            Some(ReasoningLocation::Suppressed) => return None,
            // Already admitted at `BlockStarted`; a later bound cannot un-emit
            // what shipped, and must not cost the block its seal.
            Some(ReasoningLocation::Admitted { .. }) => return None,
            Some(ReasoningLocation::Pending { .. }) | None => {}
        }
        let previous = self.reasoning.remove(&key);
        if !admits {
            // Buffered bytes are dropped, never flushed.
            self.reasoning.insert(key, ReasoningLocation::Suppressed);
            return None;
        }
        let released = match previous {
            Some(ReasoningLocation::Pending { buffered }) => String::from_utf8(buffered)
                .ok()
                .filter(|text| !text.is_empty()),
            _ => None,
        };
        let preview = released.map(|text| redact_text_preview(&text));
        self.reasoning.insert(
            key,
            ReasoningLocation::Admitted {
                emitted: preview.is_some(),
                text: preview.clone().unwrap_or_default(),
                started: Instant::now(),
            },
        );
        preview
    }

    /// Retires one location, returning its body when a seal is owed.
    pub(in crate::runtime) fn seal_reasoning(
        &mut self,
        location: plakous::model::OutputLocation,
    ) -> Option<SealedReasoning> {
        let key = (location.item_index(), location.block_index());
        match self.reasoning.remove(&key) {
            Some(ReasoningLocation::Admitted {
                emitted: true,
                text,
                started,
            }) => Some(sealed_reasoning(
                reasoning_location_key(location),
                text,
                started,
            )),
            _ => None,
        }
    }

    /// Holds a sealed block until its response settles.
    pub(in crate::runtime) fn hold_thinking(&mut self, sealed: SealedReasoning) {
        self.unsettled_thinking.push(sealed);
    }

    /// Releases the held blocks of a response that completed or a turn that
    /// ended, for persisting.
    pub(in crate::runtime) fn settle_thinking(&mut self) -> Vec<SealedReasoning> {
        std::mem::take(&mut self.unsettled_thinking)
    }

    /// Drops the held blocks and open locations of the in-flight response the
    /// retry policy discarded, returning the open keys a renderer had opened.
    /// Earlier responses of the turn settled at their completion and are not
    /// here.
    pub(in crate::runtime) fn discard_reasoning(&mut self) -> Vec<String> {
        self.unsettled_thinking.clear();
        let mut keys = self
            .reasoning
            .drain()
            .filter_map(|(key, state)| {
                matches!(state, ReasoningLocation::Admitted { emitted: true, .. })
                    .then(|| format!("{}:{}", key.0, key.1))
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    /// Retires every location at response completion.
    ///
    /// A location that never received a bound is non-admitting, so its buffer
    /// is dropped rather than flushed.
    pub(in crate::runtime) fn seal_all_reasoning(&mut self) -> Vec<SealedReasoning> {
        let mut sealed = self
            .reasoning
            .drain()
            .filter_map(|(key, state)| match state {
                ReasoningLocation::Admitted {
                    emitted: true,
                    text,
                    started,
                } => Some(sealed_reasoning(
                    format!("{}:{}", key.0, key.1),
                    text,
                    started,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        sealed.sort_by(|left, right| left.location_key.cmp(&right.location_key));
        sealed
    }

    /// The marker call owning `cell_id`, when the host has observed both.
    pub(in crate::runtime) fn marker_for_cell(&self, cell_id: Option<&str>) -> Option<String> {
        self.marker_by_cell.get(cell_id?).cloned()
    }

    pub(in crate::runtime) fn learn_cell_owner(
        &mut self,
        marker_call: &str,
        cell_id: Option<&str>,
    ) {
        if let Some(cell_id) = cell_id {
            self.marker_by_cell
                .insert(cell_id.to_owned(), marker_call.to_owned());
        }
    }

    pub(in crate::runtime) fn open_compose(
        &mut self,
        marker_call: &str,
        name: &str,
        cell_id: Option<&str>,
    ) {
        self.learn_cell_owner(marker_call, cell_id);
        self.cells.insert(
            marker_call.to_owned(),
            ComposeProjection {
                name: name.to_owned(),
                cell_id: cell_id.map(str::to_owned),
                observed_children: std::collections::BTreeSet::new(),
                terminal_children: HashMap::new(),
                highest_ordinal: 0,
                terminal_state: None,
            },
        );
    }

    /// Learns the cell a marker owns from the marker's own lineage.
    ///
    /// A cell that yields before it dispatches anything named no cell on its
    /// start event — the interpreter mints the cell after the marker begins —
    /// so `open_compose` learnt nothing, the `wait*` continuation had no row to
    /// fold under, and the wait terminal never re-terminalised the compose. The
    /// terminal lineage carries the retired cell.
    pub(in crate::runtime) fn learn_marker_cell(
        &mut self,
        marker_call: &str,
        cell_id: Option<&str>,
    ) {
        let Some(compose) = self.cells.get_mut(marker_call) else {
            return;
        };
        if compose.cell_id.is_none() {
            compose.cell_id = cell_id.map(str::to_owned);
        }
        let learnt = compose.cell_id.clone();
        self.learn_cell_owner(marker_call, learnt.as_deref());
    }

    /// Records the marker's own observed terminal row state.
    ///
    /// A `wait*` continuation of the same cell reports the cell's later state,
    /// so the last observed terminal wins and the row never regresses to
    /// `running` behind it.
    pub(in crate::runtime) fn observe_compose_terminal(&mut self, marker_call: &str, state: &str) {
        if let Some(compose) = self.cells.get_mut(marker_call) {
            compose.terminal_state = Some(state.to_owned());
        }
    }

    /// Records one child start, and reports whether it put a yielded cell back
    /// in flight — a compose whose marker already terminalised now owns a
    /// non-terminal child, so its row must paint `running` again (§5.8).
    pub(in crate::runtime) fn observe_child_start(
        &mut self,
        parent_call_id: &str,
        child_call_id: Option<&str>,
        cell_id: Option<&str>,
        ordinal: Option<u32>,
    ) -> bool {
        let Some(compose) = self.cells.get_mut(parent_call_id) else {
            return false;
        };
        if compose.cell_id.is_none() {
            compose.cell_id = cell_id.map(str::to_owned);
        }
        let learn = compose.cell_id.clone();
        if let Some(child) = child_call_id {
            compose.observed_children.insert(child.to_owned());
            compose.terminal_children.remove(child);
        }
        if let Some(ordinal) = ordinal {
            compose.highest_ordinal = compose.highest_ordinal.max(ordinal);
        }
        let reopened =
            compose.terminal_state.is_some() && !compose.all_observed_children_terminal();
        self.learn_cell_owner(parent_call_id, learn.as_deref());
        reopened
    }

    /// Records one child's terminal state and returns the owning compose.
    pub(in crate::runtime) fn observe_child_terminal(
        &mut self,
        parent_call_id: &str,
        child_call_id: Option<&str>,
        failed: bool,
    ) -> Option<&ComposeProjection> {
        let compose = self.cells.get_mut(parent_call_id)?;
        let child = child_call_id?.to_owned();
        compose.observed_children.insert(child.clone());
        compose.terminal_children.insert(child, failed);
        Some(&*compose)
    }
}
