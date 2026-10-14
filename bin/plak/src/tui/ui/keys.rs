//! Key dispatch: modal cancel, top-level keys, bindings, Ctrl keys.

use super::terminal::copy_text_osc52;
use super::typing_guard::Typing;
use super::{
    App, CtrlCOutcome, CtrlKey, EditKind, Instant, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    QuitArm, ResumePickerOutcome, UserAction, classify_ctrl_c, logical_line_end,
    logical_line_start, modal_chrome, next_word_boundary, previous_word_boundary,
};

impl App {
    /// Ctrl+C reaches every modal (§6.1). When one owns input it cancels that
    /// surface — never the turn, never the draft, and never as an inserted `c`.
    /// [`CtrlKey::Pass`] means no modal owned the key.
    pub(super) fn cancel_focused_modal(&mut self) -> CtrlKey {
        if self.destructive_confirm.take().is_some() {
            self.mark_dirty();
            return CtrlKey::Consumed;
        }
        let consumed =
            |action: Option<UserAction>| action.map_or(CtrlKey::Consumed, CtrlKey::Action);
        if self.activity_tab == Some(super::processes::ActivityTab::Processes) {
            self.handle_process_key(KeyCode::Esc);
            return CtrlKey::Consumed;
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Agents)
            && self.viewed_agent_id().is_some()
        {
            return consumed(self.agent_view_back());
        }
        if self.info_overlay.is_some() {
            self.info_overlay = None;
            self.mark_dirty();
            return CtrlKey::Consumed;
        }
        if self.control_tab.is_some() {
            self.close_control_center();
            return CtrlKey::Consumed;
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Agents) {
            return consumed(self.handle_tree_navigator_key(KeyCode::Esc));
        }
        if self.resume_menu.is_some() {
            return consumed(self.handle_resume_key(KeyCode::Esc));
        }
        CtrlKey::Pass
    }

    /// Handle a key event; returns a control-plane action when one is produced.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        self.dispatch_key(key)
    }

    fn dispatch_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        self.semantic_revision = self.semantic_revision.saturating_add(1);
        // Ignore key-repeat on Ctrl+C so holding does not auto-confirm quit.
        if key.kind == KeyEventKind::Repeat
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return None;
        }
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return None;
        }
        self.key_typing = self.typing.observe(&key, self.now());
        self.route_key(key)
    }

    /// A click on a painted key label presses its key: a deliberate
    /// gesture, never part of a typed run.
    pub(super) fn press_key_label(&mut self, code: KeyCode) -> Option<UserAction> {
        self.typing.forget_run();
        self.key_typing = Typing::SETTLED;
        self.route_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Routes one key to the surface that owns input, after
    /// [`Self::key_typing`] says what came before it.
    #[allow(
        clippy::too_many_lines,
        reason = "one precedence chain from the outermost owner to the composer"
    )]
    pub(super) fn route_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        // Kitty reports Command as SUPER. Never let such a key reach text insertion.
        if key.modifiers.contains(KeyModifiers::SUPER) {
            let composer = self.control_tab.is_none()
                && self.info_overlay.is_none()
                && self.activity_tab.is_none()
                && self.resume_menu.is_none();
            match key.code {
                KeyCode::Char('c' | 'C') => {
                    self.copy_current_selection();
                }
                KeyCode::Char('x' | 'X')
                    if composer && self.composer_selection.range(&self.input).is_some() =>
                {
                    self.handle_ctrl_key(KeyCode::Char('x'), false);
                }
                KeyCode::Char('a' | 'A') if composer => self.select_all_composer(),
                KeyCode::Char('z' | 'Z') if composer => {
                    self.handle_composer_ctrl_key(
                        KeyCode::Char('z'),
                        key.modifiers.contains(KeyModifiers::SHIFT)
                            || key.code == KeyCode::Char('Z'),
                    );
                }
                _ => {}
            }
            return None;
        }
        // Typing means the operator is back in the composer: show its cursor.
        self.transcript_gesture = false;
        // Any non-Ctrl+C press cancels quit arm (except Esc handled below).
        let is_ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'));
        if !is_ctrl_c && !matches!(key.code, KeyCode::Esc) && key.kind == KeyEventKind::Press {
            self.quit_arm = None;
            self.esc_clear_arm = None;
        }
        // Esc and Ctrl+C are the two dismissals. They retire an outstanding
        // `/resume` too, so a reply still in flight cannot open a picker the
        // operator has already walked away from.
        if is_ctrl_c || matches!(key.code, KeyCode::Esc) {
            self.pending_open_resume = false;
        }

        // Quit and the Ctrl+C ladder outrank every modal guard (§6.1): a stuck
        // modal must never be able to trap the operator in the TUI.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q' | 'Q'))
        {
            return self.request_quit();
        }
        // The destructive confirmation owns every key while it is open.
        if self.destructive_confirm.is_some() {
            return self.handle_destructive_confirm_key(key);
        }
        if is_ctrl_c {
            match self.cancel_focused_modal() {
                CtrlKey::Pass => {}
                CtrlKey::Consumed => return None,
                CtrlKey::Action(action) => return Some(action),
            }
            if self.copy_current_selection() {
                self.composer_selection
                    .set_caret(&self.input, self.input_cursor);
                self.selection.clear();
                self.mark_dirty();
                return None;
            }
        }

        // A modal that confirms on `Enter` reads an `Alt` chord as `Esc`: a
        // legacy terminal delivers `Esc` and the next key as one, and that
        // must close or decline, never confirm or resume.
        if key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && (self.info_overlay.is_some()
                || self.control_tab.is_some()
                || self.resume_menu.is_some())
        {
            return self.route_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        // Where there is no prompt an Alt or Super chord is neither a command
        // nor typing: it does nothing, so `Alt+s` never reaches `s`.
        if self.has_no_prompt()
            && self.control_tab.is_none()
            && key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Processes) {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if matches!(key.code, KeyCode::Char('x' | 'X')) {
                    self.close_activity();
                }
                return self.handle_promptless_ctrl(key);
            }
            return self.handle_process_key(key.code);
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Agents)
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            // Leaving Agents closes an open transcript. Coming back shows the
            // list at that agent (spec 02 §5.7).
            self.close_agent_transcript_onto_list();
            self.open_processes();
            return None;
        }
        if self.info_overlay.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'Q') => {
                    self.info_overlay = None;
                    self.mark_dirty();
                }
                // The overlay owns a scrollable body (§5.2 `help overlay`).
                KeyCode::Up => self.scroll_info_overlay(|scroll| scroll.scroll_by(-1)),
                KeyCode::Down => self.scroll_info_overlay(|scroll| scroll.scroll_by(1)),
                KeyCode::PageUp => self.scroll_info_overlay(modal_chrome::ScrollState::page_up),
                KeyCode::PageDown => self.scroll_info_overlay(modal_chrome::ScrollState::page_down),
                KeyCode::Home => self.scroll_info_overlay(modal_chrome::ScrollState::home),
                KeyCode::End => self.scroll_info_overlay(modal_chrome::ScrollState::end),
                _ => {}
            }
            return None;
        }
        if self.control_tab.is_some() {
            return self.handle_control_key(key);
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Agents) {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return self.handle_promptless_ctrl(key);
            }
            if self.viewed_agent_id().is_some() {
                return self.handle_agent_view_key(key);
            }
            return self.handle_tree_navigator_key(key.code);
        }
        if self.resume_menu.is_some() {
            // The picker maps no letter: an unmapped key does nothing, and
            // `Enter` resumes the selected row at once.
            return self.handle_resume_key(key.code);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Fold-all on a terminal without the kitty keyboard protocol.
        // There Ctrl+Shift+O arrives indistinguishably as Ctrl+O and runs the
        // *other* action — expanding one row instead of every block — so the
        // chord needs a plain-encoding alias, and the sheet names both.
        if alt && !ctrl && matches!(key.code, KeyCode::Char('o' | 'O')) {
            self.toggle_all_blocks();
            return None;
        }
        if ctrl {
            match self.handle_ctrl_key(key.code, shift) {
                CtrlKey::Pass => {}
                CtrlKey::Consumed => return None,
                CtrlKey::Action(action) => return Some(action),
            }
            if !self.queue_focused && self.handle_composer_ctrl_key(key.code, shift) {
                return None;
            }
        }
        self.handle_plain_key(key.code, key.modifiers)
    }

    /// `Ctrl` chords where there is no prompt (spec 02 §5.7): only those that
    /// are not composer commands. `Ctrl+S`, `Ctrl+Enter`, `Ctrl+D` and the
    /// draft-editing chords belong to the composer and do nothing here, so
    /// the hidden draft is never sent, edited, or used to drop a queued one.
    fn handle_promptless_ctrl(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            // With no prompt there is no selection to cut: always the Menu.
            KeyCode::Char('x' | 'X') => self.open_control_tab(self.last_control_tab),
            KeyCode::Char('t' | 'T' | 'o' | 'O' | 'b' | 'B') => {
                match self.handle_ctrl_key(key.code, key.modifiers.contains(KeyModifiers::SHIFT)) {
                    CtrlKey::Action(action) => Some(action),
                    CtrlKey::Pass | CtrlKey::Consumed => None,
                }
            }
            _ => None,
        }
    }

    /// Activity or the agent view: views with no prompt (spec 02 §5.7). A key
    /// they do not map does nothing, and it is not typed into the composer.
    pub(super) fn has_no_prompt(&self) -> bool {
        self.activity_tab.is_some() || self.viewed_agent_id().is_some()
    }

    fn copy_current_selection(&mut self) -> bool {
        if let Some(range) = self.composer_selection.range(&self.input) {
            let selected: String = self
                .input
                .chars()
                .skip(range.start)
                .take(range.end - range.start)
                .collect();
            copy_text_osc52(&selected);
            return true;
        }
        if self.selection.has_range()
            && let Some(selected) = self.take_transcript_selection_text()
        {
            copy_text_osc52(&selected);
            return true;
        }
        false
    }

    pub(super) fn handle_resume_key(&mut self, code: KeyCode) -> Option<UserAction> {
        let menu = self.resume_menu.as_mut()?;
        match code {
            KeyCode::Esc => self.decline_resume_menu(),
            KeyCode::Up => {
                menu.move_selection(-1);
                self.mark_dirty();
                None
            }
            KeyCode::Down => {
                menu.move_selection(1);
                self.mark_dirty();
                None
            }
            KeyCode::Enter => self.accept_resume_selection(),
            _ => None,
        }
    }

    /// Resumes the picker's selected session: `Enter` or a double-click on
    /// its row.
    pub(super) fn accept_resume_selection(&mut self) -> Option<UserAction> {
        let outcome = self.resume_menu.as_mut()?.accept();
        self.launch_resume_picker = false;
        self.close_resume_menu();
        self.arm_launch_intro();
        match outcome {
            ResumePickerOutcome::Selected(entry) => {
                if entry.in_place {
                    Some(UserAction::ResumeSession { id: Some(entry.id) })
                } else {
                    Some(UserAction::ResumeSessionHandoff { id: entry.id })
                }
            }
            ResumePickerOutcome::Empty => None,
            #[cfg(test)]
            ResumePickerOutcome::Cancelled => None,
        }
    }

    /// Readline-class composer editing (spec 02 §5.2).
    ///
    /// Only reachable on the composer input path, so a modal or the queue panel
    /// can never have its keys silently rewrite a hidden draft. Returns whether
    /// the chord was consumed.
    pub(super) fn handle_composer_ctrl_key(&mut self, code: KeyCode, shift: bool) -> bool {
        let cursor = self.input_cursor;
        match code {
            KeyCode::Char('a' | 'A') => {
                // Without the kitty enhancement flags a terminal reports
                // Ctrl-Shift-A as an uppercase char with no SHIFT bit. Accept
                // that as the shift signal, exactly as Ctrl-Shift-O does.
                if shift || code == KeyCode::Char('A') {
                    // §5.2: the keyboard route to a whole-draft selection, so
                    // Ctrl+X / Ctrl+C reach the entire draft without a mouse.
                    self.select_all_composer();
                } else {
                    self.move_caret_to(logical_line_start(&self.input, cursor), false);
                }
            }
            KeyCode::Char('e' | 'E') => {
                self.move_caret_to(logical_line_end(&self.input, cursor), shift);
            }
            KeyCode::Char('u' | 'U') => {
                self.kill_range(logical_line_start(&self.input, cursor), cursor);
                self.after_composer_edit();
            }
            KeyCode::Char('k' | 'K') => {
                self.kill_range(cursor, logical_line_end(&self.input, cursor));
                self.after_composer_edit();
            }
            KeyCode::Char('w' | 'W') => {
                self.kill_range(previous_word_boundary(&self.input, cursor), cursor);
                self.after_composer_edit();
            }
            KeyCode::Char('d') => self.delete_forward(),
            KeyCode::Char('z') if shift => self.composer_redo_step(),
            KeyCode::Char('Z' | 'y' | 'Y') => self.composer_redo_step(),
            KeyCode::Char('z') => self.composer_undo_step(),
            KeyCode::Left => {
                self.move_caret_to(previous_word_boundary(&self.input, cursor), shift);
            }
            KeyCode::Right => {
                self.move_caret_to(next_word_boundary(&self.input, cursor), shift);
            }
            _ => return false,
        }
        true
    }

    /// Shared tail for every composer mutation: history browse ends, the slash
    /// palette re-syncs, and the frame repaints.
    pub(super) fn after_composer_edit(&mut self) {
        self.dismiss_history();
        self.sync_slash_open();
        self.mark_dirty();
    }

    /// Emit the cancel intent for the running turn and mark it in flight, so a
    /// further Ctrl+C can escalate (§6.1) and a further Esc is a no-op.
    pub(super) fn cancel_current_turn(&mut self) -> UserAction {
        self.cancel_pending = true;
        self.quit_arm = None;
        self.esc_clear_arm = None;
        self.mark_dirty();
        UserAction::Interrupt
    }

    /// Ctrl combos that are not composer editing.
    pub(super) fn handle_ctrl_key(&mut self, code: KeyCode, shift: bool) -> CtrlKey {
        match code {
            KeyCode::Char('b' | 'B') => {
                if self.busy && self.root_pane().blocks.iter().any(|block| matches!(block,
                    super::ScrollBlock::Tool { kind: crate::tui::presentation::ActivityKind::Process, state, .. }
                    if super::tool_is_running(state))) {
                    CtrlKey::Action(UserAction::Background)
                } else {
                    CtrlKey::Consumed
                }
            }
            KeyCode::Char('c' | 'C') => {
                if let Some(range) = self.composer_selection.range(&self.input) {
                    let selected: String = self
                        .input
                        .chars()
                        .skip(range.start)
                        .take(range.end - range.start)
                        .collect();
                    copy_text_osc52(&selected);
                    return CtrlKey::Consumed;
                }
                if self.history_browse.is_some() {
                    self.clear_input_undoable();
                    self.slash_open = false;
                    self.dismiss_history();
                    return CtrlKey::Consumed;
                }
                let now = Instant::now();
                let draft_empty = self.input.trim().is_empty();
                match classify_ctrl_c(
                    draft_empty,
                    self.busy,
                    self.cancel_pending,
                    self.quit_arm.as_ref(),
                    now,
                ) {
                    CtrlCOutcome::ClearDraft => {
                        self.clear_input_undoable();
                        self.slash_open = false;
                        self.history_browse = None;
                        self.quit_arm = None;
                        self.mark_dirty();
                        CtrlKey::Consumed
                    }
                    // The second press of a typed `Ctrl+C Ctrl+C` asks, as
                    // a busy `Esc` does (spec 02 §5.3).
                    CtrlCOutcome::Interrupt => {
                        self.confirm_turn_stop();
                        CtrlKey::Consumed
                    }
                    CtrlCOutcome::ArmQuit => {
                        self.quit_arm = Some(QuitArm::arm(now));
                        // Floating confirm is drawn in draw(); no tacky transcript spam.
                        self.mark_dirty();
                        CtrlKey::Consumed
                    }
                    CtrlCOutcome::Quit => {
                        self.quit_arm = None;
                        self.request_quit()
                            .map_or(CtrlKey::Consumed, CtrlKey::Action)
                    }
                }
            }
            // Spec 02 §5.2: the queue's commands are chords, never typing,
            // and only on the focused queue; elsewhere `Ctrl+D` is the
            // composer's forward delete.
            KeyCode::Char('d') if self.queue_focused => self
                .queue_drop_selected()
                .map_or(CtrlKey::Consumed, CtrlKey::Action),
            KeyCode::Char('e' | 'E') if self.queue_focused => {
                self.queue_edit_selected();
                CtrlKey::Consumed
            }
            KeyCode::Char('r' | 'R') if self.queue_focused => {
                if self.queue_drain == super::QueueDrainState::RestoredPaused {
                    CtrlKey::Action(UserAction::ResumeQueueDrain)
                } else {
                    CtrlKey::Consumed
                }
            }
            KeyCode::Char('x' | 'X') => {
                // §2.1: with a selection this cuts; with no selection it is the
                // documented Menu chord (it replaced Ctrl+.).
                if self.composer_selection.range(&self.input).is_some() {
                    self.record_undo(EditKind::Discrete);
                    if let Some(selected) = self.composer_selection.cut(&mut self.input) {
                        self.input_cursor = self.composer_selection.head();
                        copy_text_osc52(&selected);
                    }
                    self.mark_dirty();
                    CtrlKey::Consumed
                } else {
                    match self.open_control_tab(self.last_control_tab) {
                        Some(action) => CtrlKey::Action(action),
                        None => CtrlKey::Consumed,
                    }
                }
            }
            // On the focused queue they send the selected row now, whatever
            // the draft holds.
            KeyCode::Enter | KeyCode::Char('s') if self.queue_focused => self
                .queue_activate_selected(true)
                .map_or(CtrlKey::Consumed, CtrlKey::Action),
            // Ctrl+Enter / Ctrl+S → steer when busy; send when idle.
            KeyCode::Enter | KeyCode::Char('s') => match self.submit_line(true) {
                Some(a) => CtrlKey::Action(a),
                None => CtrlKey::Consumed,
            },
            // Spec 02 §5.4: Ctrl+O opens or closes the focused row;
            // Ctrl+Shift+O folds everything open (or opens every group, cell
            // and thought) and clears the per-row manual overrides.
            KeyCode::Char('o' | 'O') => {
                if shift || code == KeyCode::Char('O') {
                    self.toggle_all_blocks();
                } else {
                    self.toggle_last_tool_expand();
                }
                CtrlKey::Consumed
            }
            KeyCode::Char('t' | 'T') => {
                // Activity, including an agent transcript in it, toggles closed.
                if self.activity_tab.is_some() || self.viewed_agent_id().is_some() {
                    self.close_activity();
                    return CtrlKey::Consumed;
                }
                if self.activity_last_tab == super::processes::ActivityTab::Processes {
                    self.open_processes();
                    return CtrlKey::Consumed;
                }
                self.open_tree_navigator().map_or(CtrlKey::Consumed, CtrlKey::Action)
            }
            _ => CtrlKey::Pass,
        }
    }
}
