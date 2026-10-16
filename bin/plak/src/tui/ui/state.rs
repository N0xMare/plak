//! App bookkeeping: dirty tracking, redraw demand, transcript retention, slash sync.

#[cfg(test)]
use super::Rect;
use super::{
    App, BUILD_CHECK_INTERVAL, Instant, MAX_LIVE_TRANSCRIPT_BLOCKS, MAX_LIVE_TRANSCRIPT_BYTES,
    RedrawDemand, ScrollBlock, SlashContext, TRANSCRIPT_TRUNCATION_MARKER, TranscriptChange,
    UserAction, WHEEL_FAST_BONUS, WHEEL_FAST_WINDOW, WHEEL_NOTCH_DEFAULT, WHEEL_QUEUE_CAP,
    fuzzy_list, render_prefs_for, user_phase,
};
use std::time::Duration;

impl App {
    /// Enter the quitting state. The frame painted before the loop exits shows
    /// `quitting…` on the HUD (spec 02 §5.3). The launcher then restores the
    /// terminal and exits the process; it does not wait on host release.
    pub(super) fn begin_quit(&mut self) {
        crate::tui_timing::quit_requested();
        self.dismiss_launch_intro();
        self.should_quit = true;
        self.quitting = true;
        self.mark_dirty();
    }

    /// Poll whether a newer `plak` was installed; true when the hint changed.
    pub(super) fn recheck_installed_build(&mut self, now: Instant) -> bool {
        if now < self.next_build_check {
            return false;
        }
        self.next_build_check = now + BUILD_CHECK_INTERVAL;
        let newer = crate::build_identity::newer_build_installed();
        let changed = newer != self.newer_build_installed;
        self.newer_build_installed = newer;
        changed
    }

    pub(super) fn mark_dirty(&mut self) {
        self.semantic_revision = self.semantic_revision.saturating_add(1);
        self.dirty = true;
    }

    /// What the renderer needs from the clock right now.
    ///
    /// Every field is O(1): the loop must never walk the transcript to decide
    /// how long to wait. Running tools are covered by `busy`, which the host
    /// owns, rather than by scanning blocks for a running state.
    pub(crate) fn redraw_demand(&self) -> RedrawDemand {
        RedrawDemand {
            streaming: self.pane.assistant_stream_active
                && (self.busy || self.viewed_agent_id().is_some()),
            pointer_drag: self.transcript_drag_pointer.is_some()
                || self.composer_drag_pointer.is_some(),
            // Glyph motion is not liveness: a working turn still needs a live
            // cadence for its timers and rows even with motion reduced.
            spinner_visible: self.session_open_started.is_some()
                || self.startup_pending
                || self.busy
                || self.tree_live_descendants > 0
                || self.background_processes > 0
                || self.open_approval.is_some()
                || self.reload_probe.is_some(),
            // An open Agents list does not keep the idle cadence. Ages dim
            // and expire on a one-shot wake; running rows use the spinner.
            animating: self.busy
                || self.tree_live_descendants > 0
                || self.background_processes > 0
                || self.open_approval.is_some(),
            timer_armed: self.quit_arm.is_some() || self.esc_clear_arm.is_some(),
            wheel_draining: self.mouse_scroll_queue != 0,
        }
    }

    /// Structural change after the block vector was rewritten wholesale.
    ///
    /// Re-sums retained bytes, which is O(blocks). Only snapshot replacement
    /// and `/clear` may call it: the per-event paths keep `transcript_bytes`
    /// incrementally so no O(blocks) sum runs on the hot path.
    pub(super) fn mark_transcript_replaced(&mut self) {
        self.pane.transcript_bytes = self
            .pane
            .blocks
            .iter()
            .map(ScrollBlock::retained_bytes)
            .sum();
        self.mark_transcript_structural();
    }

    /// Recomputes [`Self::render_prefs`] from the settings the operator is
    /// currently looking at: the open draft while the Settings tab previews,
    /// otherwise the persisted snapshot. §5.5's Show thinking / Group tool
    /// calls / Reduced motion rows are otherwise
    /// misleading chrome — they would preview, persist and change nothing.
    pub(super) fn sync_render_prefs(&mut self) {
        let source = self
            .settings_menu
            .as_ref()
            .map(|menu| &menu.draft)
            .or(self.settings_snapshot.as_ref());
        let next = render_prefs_for(source);
        if next == self.render_prefs {
            return;
        }
        self.render_prefs = next;
        self.mark_transcript_structural();
    }

    /// Structural change with `transcript_bytes` already accounted by the caller.
    pub(crate) fn mark_transcript_structural(&mut self) {
        self.enforce_transcript_retention();
        self.pane.pending_transcript_change = TranscriptChange::Structural;
        self.mark_dirty();
    }

    pub(crate) fn mark_assistant_tail_changed(&mut self) {
        self.pane.pending_transcript_change = self
            .pane
            .pending_transcript_change
            .merge(TranscriptChange::TailAssistant);
        self.mark_dirty();
    }

    /// Scrollback body rect the last paint used, without the sticky preview.
    ///
    /// The §11 composed-frame gate reads the transcript rows out of a painted
    /// buffer, which needs the exact rect the transcript body occupied.
    #[cfg(test)]
    pub(crate) const fn transcript_body_area(&self) -> Option<Rect> {
        self.transcript_body_area
    }

    /// Retained transcript blocks, for gates that drive a synthetic session.
    ///
    /// Callers own the change bookkeeping: mark the mutation with
    /// [`Self::mark_transcript_structural`] or
    /// [`Self::mark_assistant_tail_changed`] exactly as the host paths do.
    #[cfg(test)]
    pub(crate) const fn transcript_blocks_mut(&mut self) -> &mut Vec<ScrollBlock> {
        &mut self.pane.blocks
    }

    pub(super) fn mark_block_changed(&mut self, idx: usize) {
        self.pane.pending_transcript_change = self
            .pane
            .pending_transcript_change
            .merge(TranscriptChange::Block(idx));
        self.mark_dirty();
    }

    /// Transcript lines one wheel notch moves (§5.5 `tui.scroll_lines`).
    ///
    /// The row used to be inert chrome: it previewed, persisted and moved
    /// nothing, because the wheel handler carried its own 3/5 literals. The
    /// open Settings draft is preferred over the persisted snapshot so the row
    /// previews live, exactly like the render-preference rows.
    pub(super) fn wheel_notch(&self, fast: bool) -> i16 {
        let configured = self
            .settings_menu
            .as_ref()
            .map(|menu| &menu.draft)
            .or(self.settings_snapshot.as_ref())
            .map_or(WHEEL_NOTCH_DEFAULT, |settings| settings.scroll_lines);
        let notch = i16::try_from(configured).unwrap_or(i16::MAX).max(1);
        if fast {
            notch.saturating_add(WHEEL_FAST_BONUS)
        } else {
            notch
        }
    }

    pub(super) fn enqueue_transcript_scroll(&mut self, up: bool) {
        let now = self.now();
        let fast = self
            .last_wheel_at
            .is_some_and(|prior| now.duration_since(prior) <= WHEEL_FAST_WINDOW);
        self.last_wheel_at = Some(now);
        let intent = self.wheel_notch(fast);
        let delta = if up { intent } else { -intent };
        self.mouse_scroll_queue =
            (self.mouse_scroll_queue + delta).clamp(-WHEEL_QUEUE_CAP, WHEEL_QUEUE_CAP);
        self.mark_dirty();
    }

    pub(super) fn drain_mouse_scroll(&mut self) {
        if self.mouse_scroll_queue == 0 {
            return;
        }
        let mag = self.mouse_scroll_queue.unsigned_abs();
        let step = if mag >= 12 {
            3
        } else if mag >= 6 {
            2
        } else {
            mag
        };
        let step = i16::try_from(step).unwrap_or(1);
        if self.mouse_scroll_queue > 0 {
            self.pane.scroll_from_bottom = self
                .pane
                .scroll_from_bottom
                .saturating_add(usize::try_from(step).unwrap_or(0));
            self.mouse_scroll_queue -= step;
        } else {
            self.pane.scroll_from_bottom = self
                .pane
                .scroll_from_bottom
                .saturating_sub(usize::try_from(step).unwrap_or(0));
            self.mouse_scroll_queue += step;
        }
        self.mark_dirty();
    }

    /// Drop wheel intent a paint has already refused.
    ///
    /// The queue caps at 30 rows and drains at most 3 a frame, so notches
    /// delivered after the camera reached an end used to sit there and had to
    /// be burned off before a reversal moved anything — the wheel read as
    /// locked. Only the exhausted direction is cleared, so a reversal in the
    /// same frame still spends.
    pub(super) fn discard_clamped_scroll_queue(&mut self, at_top: bool, at_bottom: bool) {
        if (at_top && self.mouse_scroll_queue > 0) || (at_bottom && self.mouse_scroll_queue < 0) {
            self.mouse_scroll_queue = 0;
        }
    }

    pub(super) fn warm_transcript_projection(&mut self) {
        let width = self.last_transcript_width;
        if width == 0 {
            return;
        }
        let change = std::mem::take(&mut self.pane.pending_transcript_change);
        if change != TranscriptChange::Unchanged {
            self.pane.transcript_revision = self.pane.transcript_revision.wrapping_add(1);
        }
        let prefs = self.pane_prefs();
        let _ = self.pane.transcript_cache.get(
            self.pane.transcript_revision,
            &self.pane.blocks,
            &self.theme,
            width,
            change,
            prefs,
        );
    }

    /// Hide the composer caret while the transcript camera is moving.
    pub(super) fn begin_transcript_camera(&mut self) {
        self.transcript_gesture = true;
        self.transcript_gesture_is_wheel = false;
    }

    /// Hands the cursor back when a wheel burst has finished draining.
    ///
    /// The cursor is hidden on the first wheel event so a burst of frames does
    /// not flicker a caret. Before this it stayed hidden until the next
    /// keystroke — including after the view was scrolled back to the bottom
    /// with the composer focused. A drag-select keeps it hidden: that
    /// gesture ends on the button release, not on an empty scroll queue.
    pub(super) fn settle_wheel_gesture(&mut self) {
        if self.mouse_scroll_queue != 0
            || self
                .last_wheel_at
                .is_some_and(|last| self.now().duration_since(last) < Duration::from_millis(100))
            || !self.transcript_gesture
            || !self.transcript_gesture_is_wheel
            || self.composer_dragging
            || self.transcript_drag_pointer.is_some()
        {
            return;
        }
        self.transcript_gesture = false;
        self.mark_dirty();
    }

    /// The system and error notes the host published during the open that
    /// is now publishing its presentation snapshot, in order. The snapshot
    /// replaces the transcript; these postdate it and must survive it.
    pub(super) fn take_open_notes(&mut self) -> Vec<ScrollBlock> {
        let Some(from) = self.open_notes_from.take() else {
            return Vec::new();
        };
        self.pane
            .blocks
            .iter()
            .skip(from)
            .filter(|block| matches!(block, ScrollBlock::System(_) | ScrollBlock::Error(_)))
            .cloned()
            .collect()
    }

    pub(super) fn push_block(&mut self, block: ScrollBlock) {
        self.flush_assistant_stream();
        self.pane.transcript_bytes = self
            .pane
            .transcript_bytes
            .saturating_add(block.retained_bytes());
        self.pane.blocks.push(block);
        self.mark_transcript_structural();
    }

    /// Restores a refused draft: its text, and its images ahead of any
    /// attached since, within the per-message image bound.
    pub(super) fn restore_draft(&mut self, draft: plakous_host_protocol::Draft) {
        let mut images = draft.images;
        images.append(&mut self.attachments);
        images.truncate(plakous::agent::MAX_USER_IMAGES);
        self.attachments = images;
        self.restore_draft_text(draft.text);
    }

    pub(super) fn restore_draft_text(&mut self, text: String) {
        if self.input == text {
            return;
        }
        if text
            .len()
            .saturating_add(self.input.len())
            .saturating_add(if self.input.is_empty() { 0 } else { 2 })
            > super::MAX_COMPOSER_BYTES
        {
            if self.prompt_history.contains(&text) {
                self.push_block(ScrollBlock::System(
                    "prompt remains in history; the newer draft was kept because restoration exceeds the composer limit".into(),
                ));
            } else {
                self.recovered_drafts.push(text.clone());
                self.prompt_history.push(text);
                self.history_browse = None;
                self.push_block(ScrollBlock::System(
                    "refused prompt kept in local history; restoration exceeds the composer limit"
                        .into(),
                ));
            }
            return;
        }
        let merged = if self.input.is_empty() {
            text
        } else {
            format!("{text}\n\n{}", self.input)
        };
        self.set_input(merged);
    }

    pub(super) fn remove_pending_draft(&mut self, action: &UserAction) -> Option<UserAction> {
        let index = self
            .pending_drafts
            .iter()
            .position(|pending| pending == action)?;
        self.pending_drafts.remove(index)
    }

    pub(super) fn restore_pending_drafts(&mut self) {
        while let Some(action) = self.pending_drafts.pop_front() {
            if let UserAction::Prompt(draft)
            | UserAction::FollowUp(draft)
            | UserAction::Steer(draft) = action
            {
                self.restore_draft(draft);
            }
        }
        self.pending_queue_edit = None;
    }

    pub(super) fn note_host_disconnected(&mut self) {
        if self.host_disconnected {
            return;
        }
        self.host_disconnected = true;
        self.restore_pending_drafts();
        self.push_root_block(ScrollBlock::Error(
            "product host disconnected — unsafely unacknowledged draft text was restored; quit and retry"
                .to_owned(),
        ));
    }

    /// Blocks the current projection is still carrying as an estimate.
    ///
    /// The anchor gate needs to know that the frames it scored actually crossed
    /// the estimated→measured transition, rather than running entirely inside a
    /// fully measured layout.
    #[cfg(test)]
    pub(crate) fn transcript_estimated_blocks(&self) -> usize {
        self.pane.transcript_cache.layout().estimated.len()
    }

    pub(super) fn enforce_transcript_retention(&mut self) {
        let old_tail_bytes = self
            .pane
            .blocks
            .last()
            .map_or(0, ScrollBlock::retained_bytes);
        if let Some(last) = self.pane.blocks.last_mut() {
            last.truncate_oversized_tail();
        }
        let new_tail_bytes = self
            .pane
            .blocks
            .last()
            .map_or(0, ScrollBlock::retained_bytes);
        self.pane.transcript_bytes = self
            .pane
            .transcript_bytes
            .saturating_sub(old_tail_bytes)
            .saturating_add(new_tail_bytes);
        if new_tail_bytes != old_tail_bytes {
            // Truncation rewrote the tail block; callers that mark only their
            // own block must still see it.
            let tail = self.pane.blocks.len().saturating_sub(1);
            self.pane.pending_transcript_change = self
                .pane
                .pending_transcript_change
                .merge(TranscriptChange::Block(tail));
        }

        let marker_bytes = TRANSCRIPT_TRUNCATION_MARKER.len();
        let mut remove = 0;
        while remove < self.pane.blocks.len()
            && (self.pane.blocks.len() - remove > MAX_LIVE_TRANSCRIPT_BLOCKS
                || self.pane.transcript_bytes
                    > MAX_LIVE_TRANSCRIPT_BYTES.saturating_sub(if remove > 0 {
                        marker_bytes
                    } else {
                        0
                    }))
        {
            self.pane.transcript_bytes = self
                .pane
                .transcript_bytes
                .saturating_sub(self.pane.blocks[remove].retained_bytes());
            remove += 1;
        }
        if remove == 0 {
            return;
        }
        self.pane.blocks.drain(..remove);
        // Every block-space index the operator's place and the render cache
        // hold names a block that just moved. `dropped_below` is the lowest old
        // index that survives; `shift` is how far the survivors moved down.
        let mut dropped_below = remove;
        let mut shift = remove;
        if self.pane.blocks.first().map(ScrollBlock::body) != Some(TRANSCRIPT_TRUNCATION_MARKER) {
            self.pane.blocks.insert(
                0,
                ScrollBlock::System(TRANSCRIPT_TRUNCATION_MARKER.to_owned()),
            );
            self.pane.transcript_bytes = self.pane.transcript_bytes.saturating_add(marker_bytes);
            shift = shift.saturating_sub(1);
        }
        if self.pane.blocks.len() > MAX_LIVE_TRANSCRIPT_BLOCKS {
            self.pane.transcript_bytes = self
                .pane
                .transcript_bytes
                .saturating_sub(self.pane.blocks[1].retained_bytes());
            self.pane.blocks.remove(1);
            dropped_below = dropped_below.saturating_add(1);
            shift = shift.saturating_add(1);
        }
        // Anchoring must not drift. A pinned viewport at the
        // retention cap drifted one block per host event because the anchor was
        // a raw block index that retention never rebased.
        self.pane.transcript_view_anchor =
            self.pane.transcript_view_anchor.map(|(block, sub_row)| {
                if block >= dropped_below {
                    (block.saturating_sub(shift), sub_row)
                } else {
                    // The anchored block itself was evicted: hold the oldest
                    // content still retained rather than jumping to live.
                    (dropped_below.saturating_sub(shift), 0)
                }
            });
        self.pane
            .transcript_cache
            .shift_blocks(dropped_below, shift);
        self.pane.pending_transcript_change = TranscriptChange::Structural;
    }

    pub(super) fn flush_assistant_stream(&mut self) {
        // Streaming already mutates a trailing `Assistant` block in place; only
        // clear the cursor so the next delta starts a new segment.
        if self.pane.assistant_stream_active {
            self.pane.assistant_stream_active = false;
            self.mark_dirty();
        }
    }

    pub(super) fn slash_ctx(&self) -> SlashContext {
        SlashContext {
            session_id: self.session_id.clone(),
            account: self.account.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            phase: user_phase(&self.phase).to_owned(),
            queue_len: self.queue_len,
            busy: self.busy,
        }
    }

    pub(super) fn slash_filter(&self) -> String {
        self.input
            .trim_start()
            .strip_prefix('/')
            .unwrap_or("")
            .to_owned()
    }

    pub(super) fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        fuzzy_list(&self.slash_filter())
    }

    pub(super) fn sync_slash_open(&mut self) {
        let open = self.input.starts_with('/');
        if open != self.slash_open {
            self.slash_open = open;
            self.slash_selected = 0;
            self.mark_dirty();
        } else if open {
            // Keep selection in range as filter changes.
            let n = self.slash_matches().len();
            if n == 0 {
                self.slash_selected = 0;
            } else if self.slash_selected >= n {
                self.slash_selected = n - 1;
            }
        }
    }
}
