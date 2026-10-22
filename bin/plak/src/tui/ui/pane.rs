//! One transcript's presentation state: its blocks, render cache, camera and
//! live-stream bookkeeping. The root transcript and an open agent transcript
//! are each one pane; the renderer, scrolling, mouse, copy and item detail
//! act on whichever pane is on screen.

use std::collections::HashMap;
use std::time::Instant;

use super::App;
use crate::tui::{UserAction, UserActionSender};
use plakous_host_protocol::views::transcript::TranscriptRow;

use crate::tui::blocks::{self, ScrollBlock, TranscriptChange, TranscriptRenderCache};

pub(crate) struct TranscriptPane {
    pub(crate) blocks: Vec<ScrollBlock>,
    /// Monotonic identity for every transcript content/layout mutation.
    pub(super) transcript_revision: u64,
    /// Coalesced mutation consumed by the next transcript presentation.
    pub(super) pending_transcript_change: TranscriptChange,
    /// Retained source-block payload bytes (presentation read model only).
    pub(super) transcript_bytes: usize,
    /// Full transcript projection, reused across idle and viewport-only redraws.
    pub(super) transcript_cache: TranscriptRenderCache,
    /// Whether incoming deltas append to the trailing assistant block.
    pub(super) assistant_stream_active: bool,
    /// Lines scrolled up from the bottom of the scrollback (0 = follow tail).
    pub(crate) scroll_from_bottom: usize,
    /// `scroll_from_bottom` after the last paint. The next paint subtracts this
    /// from the requested offset to get a row-space wheel/key delta; the
    /// unfollowed camera remaps the block-space anchor first, then applies it.
    pub(super) scroll_from_bottom_at_last_paint: usize,
    /// Absolute first rendered-content row painted in the transcript body.
    pub(super) transcript_visible_start: usize,
    /// Total rows in the current width-sensitive plain projection.
    pub(super) transcript_total_rows: usize,
    /// `(block, sub-row)` the top of the viewport sits on while scrolled up.
    ///
    /// A row number does not survive a re-wrap or the measurement of an
    /// estimated block, so the operator's place is held in block space and
    /// converted back to rows once the geometry has settled.
    pub(super) transcript_view_anchor: Option<(usize, usize)>,
    /// Document rows the session banner contributed above the anchored row at
    /// the last paint.
    ///
    /// The banner is a fixed-height prefix of the document, not a block, so a
    /// viewport that starts inside it has no message row to anchor on and
    /// [`Self::transcript_view_anchor`] saturates to the first one. Holding
    /// the rows the banner put above that row is what lets the next paint put
    /// the camera back where it was rather than at the top of the banner.
    pub(super) transcript_anchor_banner_rows: usize,
    /// Focused transcript row for the §5.3 fold keymap.
    pub(super) focused_block: Option<usize>,
    /// Presentation-only start instants for correlated activity timing.
    ///
    /// Fallback only: the host stamps `duration_ms` on every line it can, and
    /// that stamp wins. This clock covers a call the host holds no start for.
    pub(super) tool_started: HashMap<String, Instant>,
    /// Live reasoning locations, keyed by the host's opaque location key.
    pub(super) thinking_started: HashMap<String, Instant>,
}

impl TranscriptPane {
    pub(super) fn new(blocks: Vec<ScrollBlock>) -> Self {
        let transcript_bytes = blocks.iter().map(ScrollBlock::retained_bytes).sum();
        Self {
            blocks,
            transcript_revision: 0,
            pending_transcript_change: TranscriptChange::Structural,
            transcript_bytes,
            transcript_cache: TranscriptRenderCache::default(),
            assistant_stream_active: false,
            scroll_from_bottom: 0,
            scroll_from_bottom_at_last_paint: 0,
            transcript_visible_start: 0,
            transcript_total_rows: 0,
            transcript_view_anchor: None,
            transcript_anchor_banner_rows: 0,
            focused_block: None,
            tool_started: HashMap::new(),
            thinking_started: HashMap::new(),
        }
    }
}

/// An agent's transcript, shown as the Activity > Agents body.
pub(super) struct AgentView {
    /// Exact agent id from the tree.
    pub(super) agent_id: String,
    /// The label the Agents list shows for this agent.
    pub(super) label: String,
    /// The agent's live turn whose ops follow its committed rows.
    live_turn: Option<String>,
    /// Bumped for every fresh agent pane. A pane installed for the agent
    /// the host already serves (Esc then Enter, or A -> B -> A, within one
    /// loop pass) is empty and has to be fetched again.
    serial: u64,
    /// The pane not on screen: the root's, except while
    /// [`App::with_root_pane`] has put the root pane on screen.
    hidden: TranscriptPane,
    root_in_front: bool,
}

/// The root pane and the agent view's pane on screen, borrowed from the
/// fields alone so the caller keeps the rest of the app mutable.
pub(super) fn split_panes<'a>(
    on_screen: &'a TranscriptPane,
    agent_view: Option<&'a AgentView>,
) -> (&'a TranscriptPane, Option<(&'a str, &'a TranscriptPane)>) {
    match agent_view {
        Some(view) if !view.root_in_front => {
            (&view.hidden, Some((view.agent_id.as_str(), on_screen)))
        }
        _ => (on_screen, None),
    }
}

impl App {
    /// The root transcript, whether it is on screen or behind an agent view.
    pub(super) fn root_pane(&self) -> &TranscriptPane {
        match &self.agent_view {
            Some(view) if !view.root_in_front => &view.hidden,
            _ => &self.pane,
        }
    }

    /// The open agent transcript, when Activity > Agents is showing one.
    #[cfg(test)]
    pub(super) const fn agent_view(&self) -> Option<&AgentView> {
        self.agent_view.as_ref()
    }

    /// Render preferences are identical for root and agent transcripts.
    pub(super) fn pane_prefs(&self) -> blocks::RenderPrefs {
        blocks::RenderPrefs {
            prompt_clock: blocks::PromptClock::in_zone(self.time_zone, self.unix_now_ms()),
            ..self.render_prefs
        }
    }

    /// The agent whose transcript is on screen; `None` while the root's is.
    pub(super) fn viewed_agent_id(&self) -> Option<&str> {
        self.agent_view
            .as_ref()
            .filter(|view| !view.root_in_front)
            .map(|view| view.agent_id.as_str())
    }

    /// Runs `apply` with the root pane on screen, so a root host event lands
    /// in the root pane while an agent view is shown. `apply` may close the
    /// agent view; the root pane then stays on screen.
    pub(super) fn with_root_pane<R>(&mut self, apply: impl FnOnce(&mut Self) -> R) -> R {
        self.put_root_in_front(true);
        let out = apply(self);
        self.put_root_in_front(false);
        out
    }

    fn put_root_in_front(&mut self, front: bool) {
        if let Some(view) = self.agent_view.as_mut()
            && view.root_in_front != front
        {
            std::mem::swap(&mut self.pane, &mut view.hidden);
            view.root_in_front = front;
        }
    }

    /// A root notice raised outside a host event (a disconnect, a send
    /// failure) lands in the root pane even while an agent view is shown.
    pub(super) fn push_root_block(&mut self, block: ScrollBlock) {
        self.with_root_pane(|app| app.push_block(block));
    }

    /// Shows `agent_id`'s transcript as the Activity > Agents body. The root
    /// pane keeps its blocks, camera and anchor behind the view; opening
    /// another agent replaces the agent pane. Esc returns to the Agents list.
    pub(super) fn open_agent_view(&mut self, agent_id: String, label: String) {
        // Root host events never switch views: with the root pane swapped in,
        // a switch would install the agent pane over the root's.
        if self
            .agent_view
            .as_ref()
            .is_some_and(|view| view.root_in_front)
        {
            return;
        }
        self.activity_tab = Some(super::processes::ActivityTab::Agents);
        self.activity_last_tab = super::processes::ActivityTab::Agents;
        if self.tree_navigator.is_none() {
            self.tree_navigator = Some(crate::tui::TreeNavigator::awaiting());
        }
        match self.agent_view.as_mut() {
            Some(view) if view.agent_id == agent_id => {
                view.label = label;
                self.mark_dirty();
                return;
            }
            Some(view) => {
                self.agent_pane_serial += 1;
                view.agent_id = agent_id;
                view.label = label;
                view.live_turn = None;
                view.serial = self.agent_pane_serial;
                self.pane = TranscriptPane::new(Vec::new());
            }
            None => {
                self.agent_pane_serial += 1;
                let root = std::mem::replace(&mut self.pane, TranscriptPane::new(Vec::new()));
                self.agent_view = Some(AgentView {
                    agent_id,
                    label,
                    live_turn: None,
                    serial: self.agent_pane_serial,
                    hidden: root,
                    root_in_front: false,
                });
            }
        }
        self.reset_transcript_gestures();
        self.mark_transcript_structural();
    }

    /// Returns to the root transcript at the camera it had when the agent
    /// view opened (following the tail if it was). False when no agent view
    /// was open.
    pub(super) fn close_agent_view(&mut self) -> bool {
        let Some(view) = self.agent_view.take() else {
            return false;
        };
        if !view.root_in_front {
            self.pane = view.hidden;
        }
        self.reset_transcript_gestures();
        // Render preferences may have changed while the root pane was hidden.
        self.mark_transcript_structural();
        true
    }

    /// Whether `turn_id` is the live turn of the agent on screen, so its ops
    /// apply to the agent pane.
    pub(super) fn shows_live_turn(&self, agent_id: &str, turn_id: &str) -> bool {
        self.viewed_agent_id() == Some(agent_id)
            && self
                .agent_view
                .as_ref()
                .is_some_and(|view| view.live_turn.as_deref() == Some(turn_id))
    }

    /// Replaces the open agent view's rows with that agent's committed
    /// history, converted exactly as the root's restored snapshot is, and
    /// binds the live turn whose ops follow. A `note` renders as a notice
    /// row. The camera keeps its anchor, or
    /// follows the tail when it was there. Rows for an agent not on screen
    /// are dropped.
    pub(super) fn replace_agent_rows(
        &mut self,
        agent_id: &str,
        rows: Vec<TranscriptRow>,
        live_turn: Option<String>,
        note: Option<String>,
    ) {
        // `viewed_agent_id` is `None` while the root pane is swapped in, so
        // agent rows never land in it.
        if self.viewed_agent_id() != Some(agent_id) {
            return;
        }
        if let Some(view) = self.agent_view.as_mut() {
            view.live_turn = live_turn;
        }
        self.pane.tool_started.clear();
        self.pane.thinking_started.clear();
        self.pane.assistant_stream_active = false;
        self.restore_tip_clocks(&rows);
        let mut blocks = blocks::blocks_from_rows(rows);
        blocks.extend(note.map(ScrollBlock::System));
        let old = std::mem::replace(&mut self.pane.blocks, blocks);
        self.carry_item_state(&old);
        self.mark_transcript_replaced();
    }

    /// Carries what the operator did to the agent's rows across a re-sent
    /// transcript (every turn end): open items and their fetched detail,
    /// folded thoughts, the focused row and the camera's anchor, each found
    /// again by its identity (a tool's call id, a thought's location, or a
    /// typed content key and occurrence for rows without a durable id). A
    /// selection addresses old rows and is dropped.
    pub(super) fn carry_item_state(&mut self, old: &[ScrollBlock]) {
        carry_mail_state(&mut self.pane.blocks, old);
        let by_identity: HashMap<(bool, String), &ScrollBlock> = old
            .iter()
            .filter_map(|block| {
                item_identity(block).map(|(tool, id)| ((tool, id.to_owned()), block))
            })
            .collect();
        let by_content: HashMap<_, _> = old
            .iter()
            .filter(|block| matches!(block, ScrollBlock::Thinking { .. }))
            .map(|block| ((std::mem::discriminant(block), block.body()), block))
            .collect();
        for block in &mut self.pane.blocks {
            let Some(id) = item_identity(block).map(|(tool, id)| (tool, id.to_owned())) else {
                continue;
            };
            let Some(was) = by_identity.get(&id).copied().or_else(|| {
                by_content
                    .get(&(std::mem::discriminant(block), block.body()))
                    .copied()
            }) else {
                continue;
            };
            match (block, was) {
                (
                    ScrollBlock::Tool {
                        mode,
                        manual,
                        group_open,
                        view,
                        ..
                    },
                    ScrollBlock::Tool {
                        mode: old_mode,
                        manual: old_manual,
                        group_open: old_group_open,
                        view: old_view,
                        ..
                    },
                ) => {
                    *mode = *old_mode;
                    *manual = *old_manual;
                    *group_open = *old_group_open;
                    view.clone_from(old_view);
                }
                (
                    ScrollBlock::Thinking { mode, manual, .. },
                    ScrollBlock::Thinking {
                        mode: old_mode,
                        manual: old_manual,
                        ..
                    },
                ) if *old_manual => {
                    *mode = *old_mode;
                    *manual = true;
                }
                _ => {}
            }
        }
        let new = &self.pane.blocks;
        let remap = |idx| remap_block(old, new, idx);
        self.pane.focused_block = self.pane.focused_block.and_then(remap);
        self.pane.transcript_view_anchor =
            self.pane.transcript_view_anchor.and_then(|(idx, row)| {
                remap(idx)
                    .map(|at| (at, row))
                    .or_else(|| {
                        // If the row was removed, hold the nearest surviving
                        // content, never reinterpret the old bottom offset.
                        (1..old.len()).find_map(|distance| {
                            idx.checked_add(distance)
                                .and_then(remap)
                                .or_else(|| idx.checked_sub(distance).and_then(remap))
                                .map(|at| (at, 0))
                        })
                    })
                    .or_else(|| (!new.is_empty()).then_some((0, 0)))
            });
        self.selection.clear();
        self.pending_tool_click = None;
    }

    /// Asks the host to serve the agent transcript on screen, or to close the
    /// one it serves, when that changed since it was last told. Every way in
    /// or out of an agent view (Enter, Esc, a submit, a session switch) goes
    /// through here on the next loop pass.
    pub(crate) fn sync_agent_transcript(&mut self, actions: Option<&UserActionSender>) {
        let Some(tx) = actions else {
            return;
        };
        let wanted = self
            .agent_view
            .as_ref()
            .map(|view| (view.agent_id.clone(), view.serial));
        if self.host_agent_transcript == wanted {
            return;
        }
        let served = self.host_agent_transcript.take();
        match &wanted {
            None => {
                let _ = tx.send(UserAction::CloseAgentTranscript);
            }
            Some((agent_id, _)) => {
                // Opening the agent the host already serves is a no-op there,
                // so a fresh pane for it closes first to get its rows again.
                if served.is_some_and(|(served, _)| served == *agent_id) {
                    let _ = tx.send(UserAction::CloseAgentTranscript);
                }
                let _ = tx.send(UserAction::OpenAgentTranscript {
                    agent_id: agent_id.clone(),
                });
            }
        }
        self.host_agent_transcript = wanted;
    }

    /// Pointer, selection and click state address the pane that was on
    /// screen; none of it carries over to another pane.
    fn reset_transcript_gestures(&mut self) {
        self.reset_composer_drag();
        self.selection.clear();
        self.pending_tool_click = None;
        self.transcript_drag_pointer = None;
        self.transcript_down_cell = None;
        self.copy_selection_after_draw = false;
        self.tool_hit_rows.clear();
        self.tool_hit_targets.clear();
        self.mouse_scroll_queue = 0;
        self.scrollbar_drag = None;
    }
}

fn carry_mail_state(blocks: &mut [ScrollBlock], old: &[ScrollBlock]) {
    // Notices have no wire identity; retain folds by content and occurrence.
    let mut mail_states: HashMap<&str, std::collections::VecDeque<(bool, bool)>> = HashMap::new();
    for block in old {
        if let ScrollBlock::Mail {
            text,
            open,
            group_open,
        } = block
        {
            mail_states
                .entry(text)
                .or_default()
                .push_back((*open, *group_open));
        }
    }
    for block in blocks {
        if let ScrollBlock::Mail {
            text,
            open,
            group_open,
        } = block
            && let Some((was_open, was_group)) = mail_states
                .get_mut(text.as_str())
                .and_then(std::collections::VecDeque::pop_front)
        {
            *open = was_open;
            *group_open = was_group;
        }
    }
}

fn remap_block(old: &[ScrollBlock], new: &[ScrollBlock], idx: usize) -> Option<usize> {
    let block = old.get(idx)?;
    if let Some(id) = item_identity(block)
        && let Some(at) = new
            .iter()
            .position(|block| item_identity(block) == Some(id))
    {
        return Some(at);
    }
    // Snapshots can replace live location keys. Prose and notices
    // have no wire id at all. Their typed content plus occurrence is
    // a stable presentation key, independent of distance to the tail.
    let same = |other: &&ScrollBlock| {
        std::mem::discriminant(*other) == std::mem::discriminant(block)
            && other.body() == block.body()
            && match (block, *other) {
                (
                    ScrollBlock::Tool {
                        subject,
                        parent_call_id,
                        ..
                    },
                    ScrollBlock::Tool {
                        subject: other_subject,
                        parent_call_id: other_parent,
                        ..
                    },
                ) => subject == other_subject && parent_call_id == other_parent,
                _ => true,
            }
    };
    let occurrence = old[..idx].iter().filter(same).count();
    new.iter()
        .enumerate()
        .filter(|(_, other)| same(other))
        .nth(occurrence)
        .map(|(at, _)| at)
}

/// The identity a row keeps across re-sent transcripts: a tool's call id or
/// a thought's location key.
fn item_identity(block: &ScrollBlock) -> Option<(bool, &str)> {
    match block {
        ScrollBlock::Tool {
            call_id: Some(id), ..
        } => Some((true, id.as_str())),
        ScrollBlock::Thinking { location_key, .. } => Some((false, location_key.as_str())),
        _ => None,
    }
}
