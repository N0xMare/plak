//! Unmodified key handling and transcript selection helpers.

use super::{
    App, ContentPosition, ESC_CLEAR_WINDOW, EditKind, EscOutcome, Instant, KeyCode, KeyModifiers,
    QuitArm, ScrollBlock, UserAction, classify_esc, is_accepted_input_char, next_grapheme_boundary,
    next_word_boundary, previous_grapheme_boundary, previous_word_boundary,
};

/// An irreversible palette command completed by `Enter` ignores the next
/// `Enter` for this long (spec 02 §5.1): a doubled press runs nothing.
const SLASH_DOUBLED_ENTER: std::time::Duration = std::time::Duration::from_millis(600);

impl App {
    #[allow(
        clippy::too_many_lines,
        reason = "central key event dispatch is clearer as one exhaustive match"
    )]
    pub(super) fn handle_plain_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<UserAction> {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        let key_alt = modifiers.contains(KeyModifiers::ALT);
        let key_super = modifiers.contains(KeyModifiers::SUPER);
        // Spec 02 §5.2: the focused queue takes the arrows (and `Esc` and
        // `Tab` to leave it). Every other key leaves it for the composer, so
        // no typing or editing key drops, edits or sends a queued prompt.
        // Plain `Enter` is `Ctrl+S`: it sends the selected prompt now,
        // including on a paused drain, and returns focus here, leaving the
        // draft untouched. Where delivery is unavailable it only leaves, so
        // the next `Enter` reaches the composer.
        if self.queue_focused
            && !ctrl
            && !matches!(
                code,
                KeyCode::Up | KeyCode::Down | KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab
            )
        {
            if code == KeyCode::Enter && !shift && !key_alt {
                let sends = self.queue_enter_sends();
                self.queue_focused = false;
                self.mark_dirty();
                if sends {
                    return self.queue_activate_selected(true);
                }
                return None;
            }
            self.queue_focused = false;
            self.mark_dirty();
        }
        match code {
            KeyCode::Esc => {
                // §6.1 precedence: an owner consumes Esc first; then a mid-turn
                // Esc cancels the turn; then Esc-Esc clears an idle draft. Esc
                // never quits at any level.
                if self.quit_arm.take().is_some() {
                    self.mark_dirty();
                    return None;
                }
                if self.history_browse.is_some() {
                    self.dismiss_history();
                    return None;
                }
                if self.slash_open {
                    self.slash_open = false;
                    self.mark_dirty();
                    return None;
                }
                if self.queue_focused {
                    self.queue_focused = false;
                    self.mark_dirty();
                    return None;
                }
                if self.cancel_queue_edit() {
                    return None;
                }
                if self.composer_selection.range(&self.input).is_some() {
                    self.composer_selection
                        .set_caret(&self.input, self.input_cursor);
                    self.mark_dirty();
                    return None;
                }
                let now = Instant::now();
                match classify_esc(
                    self.input.trim().is_empty(),
                    self.busy,
                    self.cancel_pending,
                    self.esc_clear_arm.as_ref(),
                    now,
                ) {
                    EscOutcome::Cancel => {
                        self.confirm_turn_stop();
                        None
                    }
                    EscOutcome::ArmClearDraft => {
                        self.esc_clear_arm = Some(QuitArm::arm_for(now, ESC_CLEAR_WINDOW));
                        self.mark_dirty();
                        None
                    }
                    EscOutcome::ClearDraft => {
                        self.esc_clear_arm = None;
                        self.clear_input_undoable();
                        self.slash_open = false;
                        self.history_browse = None;
                        self.mark_dirty();
                        None
                    }
                    EscOutcome::Ignore => {
                        self.esc_clear_arm = None;
                        (!self.busy).then_some(UserAction::Interrupt)
                    }
                }
            }
            // §5.3/§5.8 fold-all. `Alt+O` is the same effect as `Ctrl-Shift-O`
            // on terminals that swallow the Ctrl-Shift chord, and it is the
            // only gesture that folds a Compose's child tree away.
            KeyCode::Char('o' | 'O') if key_alt && !ctrl => {
                self.toggle_all_blocks();
                None
            }
            // Alt word ops (§4). These precede the plain `Char` arm so Alt+B
            // never types a `b` into the draft.
            KeyCode::Char('b' | 'B') if key_alt && !ctrl => {
                self.move_caret_to(
                    previous_word_boundary(&self.input, self.input_cursor),
                    shift,
                );
                None
            }
            KeyCode::Char('f' | 'F') if key_alt && !ctrl => {
                self.move_caret_to(next_word_boundary(&self.input, self.input_cursor), shift);
                None
            }
            KeyCode::Backspace if key_alt => {
                self.kill_range(
                    previous_word_boundary(&self.input, self.input_cursor),
                    self.input_cursor,
                );
                self.after_composer_edit();
                None
            }
            // Newline: Shift+Enter (when terminal reports Shift) or Alt+Enter.
            // Many SSH / VTE terminals drop Shift on Enter — use Alt+Enter there.
            KeyCode::Enter if shift || key_alt => {
                self.insert_chars("\n");
                self.dismiss_history();
                self.sync_slash_open();
                self.mark_dirty();
                None
            }
            KeyCode::Enter | KeyCode::Left if self.history_browse.is_some() => {
                self.load_history_selected();
                None
            }
            KeyCode::Enter => {
                // §5.9: Enter on Spawn Agent opens the child transcript.
                if self.input.is_empty()
                    && let Some(action) = self.open_focused_agent_transcript()
                {
                    return Some(action);
                }
                // §5.3: with an empty draft, Enter is the fold key for the
                // focused transcript row rather than a submit.
                if self.input.is_empty()
                    && let Some(idx) = self.pane.focused_block
                    && self
                        .pane
                        .blocks
                        .get(idx)
                        .is_some_and(ScrollBlock::is_foldable)
                {
                    self.toggle_tool_expand_at(idx);
                    return None;
                }
                // Enter runs the highlighted command only when the typed word
                // is its prefix: a fuzzy match (`/tmp` reaching
                // `/fork-then-compact`) is submitted as typed instead.
                if self.slash_open
                    && !self.slash_filter().contains(char::is_whitespace)
                    && self.slash_highlight_is_prefix()
                {
                    return self.enter_on_slash_entry();
                }
                self.submit_line(false)
            }
            // Tab completes the same prefix match Enter would run, and
            // nothing else: a fuzzy match never rewrites the draft.
            KeyCode::Tab if self.slash_open => {
                if self.slash_highlight_is_prefix() {
                    self.complete_slash_entry();
                }
                None
            }
            KeyCode::Tab if !self.local_queue.is_empty() => {
                self.queue_focused = !self.queue_focused;
                if self.queue_focused && self.queue_selected >= self.local_queue.len() {
                    self.queue_selected = self.local_queue.len().saturating_sub(1);
                }
                self.mark_dirty();
                None
            }
            // §5.2: `Alt+A`/`Alt+D` ask to allow or deny an open effect
            // decision in the destructive pop-up: a legacy terminal reads a
            // typed `Esc` and the next letter as one `Alt` chord. A bare
            // letter is always typing.
            KeyCode::Char('a' | 'A') if !ctrl && key_alt && self.pending_approval().is_some() => {
                let action = self.resolve_pending_approval(crate::tui::ApprovalChoice::Allow)?;
                self.confirm_destructive(action)
            }
            KeyCode::Char('d' | 'D') if !ctrl && key_alt => {
                if let Some(action) =
                    self.resolve_pending_approval(crate::tui::ApprovalChoice::Deny)
                {
                    return self.confirm_destructive(action);
                }
                // Readline's kill-word-forward.
                self.kill_range(
                    self.input_cursor,
                    next_word_boundary(&self.input, self.input_cursor),
                );
                self.after_composer_edit();
                None
            }
            KeyCode::Up if self.queue_focused && shift => {
                let item = self.local_queue.get(self.queue_selected)?;
                Some(UserAction::MoveQueued {
                    id: item.id.clone(),
                    revision: item.revision,
                    new_index: self.queue_selected.saturating_sub(1),
                })
            }
            KeyCode::Down if self.queue_focused && shift => {
                let item = self.local_queue.get(self.queue_selected)?;
                Some(UserAction::MoveQueued {
                    id: item.id.clone(),
                    revision: item.revision,
                    new_index: (self.queue_selected + 1).min(self.local_queue.len() - 1),
                })
            }
            KeyCode::Up if self.queue_focused => {
                self.queue_selected = self.queue_selected.saturating_sub(1);
                self.mark_dirty();
                None
            }
            KeyCode::Down if self.queue_focused => {
                if !self.local_queue.is_empty() {
                    self.queue_selected = (self.queue_selected + 1).min(self.local_queue.len() - 1);
                }
                self.mark_dirty();
                None
            }
            KeyCode::Backspace => {
                self.backspace_char();
                self.dismiss_history();
                self.sync_slash_open();
                self.mark_dirty();
                None
            }
            KeyCode::Delete => {
                self.delete_forward();
                None
            }
            KeyCode::Left => {
                let next = previous_grapheme_boundary(&self.input, self.input_cursor);
                if shift {
                    if self.composer_selection.range(&self.input).is_none() {
                        self.composer_selection
                            .begin_drag(&self.input, self.input_cursor);
                    }
                    self.composer_selection.drag_to(&self.input, next);
                } else {
                    self.composer_selection.set_caret(&self.input, next);
                }
                self.input_cursor = next;
                self.composer_wheel_scroll = None;
                self.mark_dirty();
                None
            }
            KeyCode::Right => {
                let n = self.input.chars().count();
                let next = next_grapheme_boundary(&self.input, self.input_cursor).min(n);
                if shift {
                    if self.composer_selection.range(&self.input).is_none() {
                        self.composer_selection
                            .begin_drag(&self.input, self.input_cursor);
                    }
                    self.composer_selection.drag_to(&self.input, next);
                } else {
                    self.composer_selection.set_caret(&self.input, next);
                }
                self.input_cursor = next;
                self.composer_wheel_scroll = None;
                self.mark_dirty();
                None
            }
            KeyCode::Home => {
                // Start of current visual line: approximate start of last \n before cursor.
                let before: String = self.input.chars().take(self.input_cursor).collect();
                self.input_cursor = before.rfind('\n').map_or(0, |i| {
                    // convert byte to char idx
                    before[..=i].chars().count()
                });
                self.composer_selection
                    .set_caret(&self.input, self.input_cursor);
                self.composer_wheel_scroll = None;
                self.mark_dirty();
                None
            }
            KeyCode::End => {
                let after_start = self.input_cursor;
                let rest: String = self.input.chars().skip(after_start).collect();
                let add = rest.find('\n').unwrap_or(rest.chars().count());
                self.input_cursor = after_start + add;
                self.composer_selection
                    .set_caret(&self.input, self.input_cursor);
                self.composer_wheel_scroll = None;
                self.mark_dirty();
                None
            }
            KeyCode::Up if self.slash_open => {
                self.move_slash_selection(-1);
                None
            }
            KeyCode::Down if self.slash_open => {
                self.move_slash_selection(1);
                None
            }
            KeyCode::Up => {
                if self.input.is_empty() || self.history_browse.is_some() {
                    self.history_up();
                } else {
                    self.move_composer_cursor(-1, shift);
                }
                None
            }
            KeyCode::Down => {
                if self.history_browse.is_some() {
                    self.history_down();
                } else if !self.input.is_empty() {
                    self.move_composer_cursor(1, shift);
                } else {
                    self.pane.scroll_from_bottom = self.pane.scroll_from_bottom.saturating_sub(1);
                    self.begin_transcript_camera();
                    self.mark_dirty();
                }
                None
            }
            KeyCode::PageUp => {
                if self.input.is_empty() {
                    self.pane.scroll_from_bottom = self.pane.scroll_from_bottom.saturating_add(10);
                    self.begin_transcript_camera();
                    self.mark_dirty();
                } else {
                    let rows = self.composer_inner.map_or(10, |inner| inner.height.max(1));
                    self.move_composer_cursor(-isize::try_from(rows).unwrap_or(10), shift);
                }
                None
            }
            KeyCode::PageDown => {
                if self.input.is_empty() {
                    self.pane.scroll_from_bottom = self.pane.scroll_from_bottom.saturating_sub(10);
                    self.begin_transcript_camera();
                    self.mark_dirty();
                } else {
                    let rows = self.composer_inner.map_or(10, |inner| inner.height.max(1));
                    self.move_composer_cursor(isize::try_from(rows).unwrap_or(10), shift);
                }
                None
            }
            // Only after a pause with no typing: a `?` typed into a draft
            // just emptied stays text.
            KeyCode::Char('?')
                if !ctrl
                    && !key_alt
                    && !key_super
                    && self.input.is_empty()
                    && self.key_typing.settled() =>
            {
                self.activate_control_tab(super::ControlTab::Keybinds);
                None
            }
            KeyCode::Char(ch) if !ctrl && !key_super && is_accepted_input_char(ch) => {
                let open_palette = (self.slash_open && self.input.starts_with('/'))
                    || (ch == '/' && self.input.is_empty());
                self.insert_chars_kind(&ch.to_string(), EditKind::Typing);
                self.after_composer_edit();
                if !open_palette {
                    self.slash_open = false;
                }
                None
            }
            _ => None,
        }
    }

    /// `Enter` on a highlighted palette row the typed word is a prefix of.
    /// An irreversible command (spec 02 §5.1) runs only when its full name
    /// was typed: a shorter word is completed, as `Tab` does, and the
    /// `Enter` that follows within [`SLASH_DOUBLED_ENTER`] (a doubled press) runs
    /// nothing either.
    fn enter_on_slash_entry(&mut self) -> Option<UserAction> {
        let now = self.now();
        if let Some((at, draft)) = self.slash_enter_completion.take()
            && draft == self.input
            && now.saturating_duration_since(at) < SLASH_DOUBLED_ENTER
        {
            self.slash_enter_completion = Some((at, draft));
            return None;
        }
        let word = self.slash_typed_word();
        if let Some((name, _)) = self.slash_matches().get(self.slash_selected).copied()
            && super::slash::is_irreversible(name)
            && word != name
        {
            self.complete_slash_entry();
            self.slash_enter_completion = Some((now, self.input.clone()));
            return None;
        }
        self.select_slash_entry()
    }

    /// The typed command word, lowercased, without its slash.
    fn slash_typed_word(&self) -> String {
        self.slash_filter()
            .trim_start_matches('/')
            .split(char::is_whitespace)
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// Whether the highlighted palette row's name begins with the typed
    /// command word.
    fn slash_highlight_is_prefix(&self) -> bool {
        let filter = self.slash_filter();
        let word = filter
            .trim_start_matches('/')
            .split(char::is_whitespace)
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        self.slash_matches()
            .get(self.slash_selected)
            .is_some_and(|(name, _)| name.starts_with(word.as_str()))
    }

    pub(super) fn move_slash_selection(&mut self, delta: i8) {
        let n = self.slash_matches().len();
        if n == 0 {
            return;
        }
        if delta < 0 {
            self.slash_selected = if self.slash_selected == 0 {
                n - 1
            } else {
                self.slash_selected - 1
            };
        } else {
            self.slash_selected = (self.slash_selected + 1) % n;
        }
        self.mark_dirty();
    }

    /// Map a terminal pointer into absolute rendered-content coordinates. The
    /// pointer is clamped to the real body so dragging across a sticky preview
    /// or outside the viewport extends the selection while edge scrolling.
    pub(super) fn transcript_position_at(&self, column: u16, row: u16) -> Option<ContentPosition> {
        let area = self.transcript_body_area?;
        let rows = self.pane.transcript_cache.layout().plain.len();
        if area.is_empty() || rows == 0 {
            return None;
        }
        let screen_row = row.clamp(area.y, area.bottom().saturating_sub(1));
        let screen_column = column.clamp(area.x, area.right().saturating_sub(1));
        Some(ContentPosition {
            row: self
                .pane
                .transcript_visible_start
                .saturating_add(usize::from(screen_row.saturating_sub(area.y)))
                .min(rows.saturating_sub(1)),
            column: usize::from(screen_column.saturating_sub(area.x)),
        })
    }

    /// Timer hook for composer drag autoscroll, at the transcript's cadence.
    pub(super) fn advance_composer_drag_autoscroll(&mut self) -> bool {
        if self.a_modal_owns_input() || self.quit_arm.is_some() {
            self.reset_composer_drag();
            return false;
        }
        let Some((column, row)) = self.composer_drag_pointer else {
            return false;
        };
        let Some(inner) = self.composer_inner.filter(|inner| !inner.is_empty()) else {
            return false;
        };
        let width = usize::from(inner.width.saturating_sub(2).max(1));
        let max_scroll = self
            .composer_wrap
            .rows(&self.input, width)
            .len()
            .saturating_sub(usize::from(inner.height));
        let current = self.composer_visual_scroll.min(max_scroll);
        let next = if row < inner.y {
            current.saturating_sub(1)
        } else if row >= inner.bottom() {
            current.saturating_add(1).min(max_scroll)
        } else {
            return false;
        };
        if next == current {
            return false;
        }
        self.composer_visual_scroll = next;
        if self.composer_wheel_scroll.is_some() {
            self.composer_wheel_scroll = Some(next);
        }
        if let Some(index) = self.composer_index_at(column, row) {
            self.composer_selection.drag_to(&self.input, index);
            self.input_cursor = self.composer_selection.head();
        }
        self.mark_dirty();
        true
    }

    /// Timer hook for transcript drag autoscroll. Two rendered rows per poll
    /// keeps selection controllable and works even when no new mouse event is
    /// emitted while the pointer rests at an edge.
    pub(super) fn advance_transcript_drag_autoscroll(&mut self) -> bool {
        let Some((_, row)) = self.transcript_drag_pointer else {
            return false;
        };
        let Some(area) = self.transcript_body_area else {
            return false;
        };
        if !self.selection.has_range() {
            return false;
        }
        let max_from_bottom = self.pane.transcript_total_rows.saturating_sub(usize::from(
            self.transcript_area.unwrap_or(area).height.max(1),
        ));
        let next = if row <= area.y && self.pane.scroll_from_bottom < max_from_bottom {
            self.pane.scroll_from_bottom.saturating_add(2)
        } else if row >= area.bottom().saturating_sub(1) && self.pane.scroll_from_bottom > 0 {
            self.pane.scroll_from_bottom.saturating_sub(2)
        } else {
            return false;
        };
        self.pane.scroll_from_bottom = next;
        self.mark_dirty();
        true
    }

    pub(super) fn take_transcript_selection_text(&mut self) -> Option<String> {
        // Copy reads the complete plain projection, so any block the byte
        // ceiling evicted has to come back first.
        self.pane
            .transcript_cache
            .materialize_all(&self.pane.blocks);
        let layout = self.pane.transcript_cache.layout();
        let plans = self.pane.transcript_cache.row_plans();
        let (blocks, theme) = (&self.pane.blocks, &self.theme);
        // Rows arrive in order, so each block's head row and lead rows are
        // found once, not once per selected row.
        let mut current: Option<(usize, usize, Vec<crate::tui::blocks::LeadRow>)> = None;
        let mut full = |row: usize| -> Option<String> {
            let block = (*layout.block_index.get(row)?)?;
            if current.as_ref().is_none_or(|(at, ..)| *at != block) {
                let head = layout.block_index[..row]
                    .iter()
                    .rposition(|index| *index != Some(block))
                    .map_or(0, |at| at + 1);
                let leads =
                    crate::tui::blocks::lead_rows(blocks.get(block)?, plans.get(block)?, theme);
                current = Some((block, head, leads));
            }
            let (_, head, leads) = current.as_ref()?;
            let lead = leads.get(row - head)?;
            Some(lead.full_text())
        };
        self.selection.take_text(
            self.pane.transcript_revision,
            &layout.plain,
            &layout.copy,
            &mut full,
        )
    }
}
