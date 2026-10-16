//! Composer text editing, history, and paste.

use super::terminal::copy_text_osc52;
use super::{
    App, ControlTab, EditKind, MAX_COMPOSER_BYTES, ScrollBlock, SlashEffect, TreeNavigator,
    UserAction, char_range_bytes, cursor_from_visual_click, modal_chrome, sanitize_composer_text,
    sanitize_single_line, truncate_utf8_bytes, utf8_prefix, visual_cursor_xy, visual_line_count,
};

impl App {
    /// Record the pre-edit draft so the next mutation is undoable.
    /// Record the pre-edit draft so the next mutation is undoable.
    pub(super) fn record_undo(&mut self, kind: EditKind) {
        self.composer_undo
            .record(&self.input, self.input_cursor, kind);
    }

    /// Load a draft from outside the editor (history shelf, queue edit,
    /// restored draft). One discrete undo step; the text is sanitized like any
    /// other input path.
    pub(super) fn set_input(&mut self, text: String) {
        self.record_undo(EditKind::Discrete);
        self.set_input_raw(text);
    }

    pub(super) fn set_input_raw(&mut self, text: String) {
        // Only reallocate when the input actually carried a rejected scalar.
        let mut text = match sanitize_composer_text(&text) {
            sanitized if sanitized == text => text,
            sanitized => sanitized,
        };
        truncate_utf8_bytes(&mut text, MAX_COMPOSER_BYTES);
        self.input = text;
        self.input_cursor = self.input.chars().count();
        self.composer_selection
            .set_caret(&self.input, self.input_cursor);
        self.composer_wheel_scroll = None;
    }

    /// Clear the draft and drop the undo history (submit, `/clear`, resume).
    pub(super) fn clear_input(&mut self) {
        self.clear_input_text();
        self.composer_undo.reset();
    }

    /// Clear the draft as one undoable step (Esc-Esc idle, first Ctrl+C).
    pub(super) fn clear_input_undoable(&mut self) {
        if self.input.is_empty() {
            return;
        }
        self.record_undo(EditKind::Discrete);
        self.clear_input_text();
    }

    pub(super) fn clear_input_text(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.composer_selection.set_caret(&self.input, 0);
        self.composer_wheel_scroll = None;
    }

    pub(super) fn insert_chars(&mut self, s: &str) {
        self.insert_chars_kind(s, EditKind::Discrete);
    }

    pub(super) fn insert_chars_kind(&mut self, s: &str, kind: EditKind) {
        let sanitized = sanitize_composer_text(s);
        if sanitized.is_empty() {
            return;
        }
        // A typed grapheme over a selection replaces it: that is one discrete
        // edit, not a coalescing keystroke.
        let kind = if self.composer_selection.range(&self.input).is_some() {
            EditKind::Discrete
        } else {
            kind
        };
        self.record_undo(kind);
        if self.composer_selection.range(&self.input).is_none() {
            self.composer_selection
                .set_caret(&self.input, self.input_cursor);
        }
        let selected_bytes = self
            .composer_selection
            .range(&self.input)
            .map_or(0, |range| char_range_bytes(&self.input, range));
        let retained_bytes = self.input.len().saturating_sub(selected_bytes);
        let available = MAX_COMPOSER_BYTES.saturating_sub(retained_bytes);
        let replacement = utf8_prefix(&sanitized, available);
        self.composer_selection
            .replace_or_insert(&mut self.input, replacement);
        self.input_cursor = self.composer_selection.head();
        self.composer_wheel_scroll = None;
    }

    pub(super) fn backspace_char(&mut self) {
        if self.input.is_empty() {
            return;
        }
        self.record_undo(EditKind::Discrete);
        if self.composer_selection.range(&self.input).is_none() {
            self.composer_selection
                .set_caret(&self.input, self.input_cursor);
        }
        self.composer_selection.backspace(&mut self.input);
        self.input_cursor = self.composer_selection.head();
        self.composer_wheel_scroll = None;
    }

    /// Move the caret, optionally extending the selection.
    /// Selects the whole draft and parks the caret at its end (Ctrl+Shift+A).
    ///
    /// The mouse has had this since the triple-click; the keyboard did not, so
    /// a cut or copy of the whole draft needed a pointer.
    /// `Delete` and `Ctrl+D`: the selection, or the character after the caret.
    pub(super) fn delete_forward(&mut self) {
        self.record_undo(EditKind::Discrete);
        if self.composer_selection.range(&self.input).is_none() {
            self.composer_selection
                .set_caret(&self.input, self.input_cursor);
        }
        self.composer_selection.delete_forward(&mut self.input);
        self.input_cursor = self.composer_selection.head();
        self.composer_wheel_scroll = None;
        self.dismiss_history();
        self.sync_slash_open();
        self.mark_dirty();
    }

    pub(super) fn select_all_composer(&mut self) {
        let end = self.input.chars().count();
        self.composer_selection.begin_drag(&self.input, 0);
        self.composer_selection.drag_to(&self.input, end);
        self.input_cursor = end;
        self.composer_wheel_scroll = None;
        self.composer_undo.break_typing_run();
        self.mark_dirty();
    }

    pub(super) fn move_caret_to(&mut self, target: usize, extend: bool) {
        let target = target.min(self.input.chars().count());
        if extend {
            if self.composer_selection.range(&self.input).is_none() {
                self.composer_selection
                    .begin_drag(&self.input, self.input_cursor);
            }
            self.composer_selection.drag_to(&self.input, target);
        } else {
            self.composer_selection.set_caret(&self.input, target);
        }
        self.input_cursor = target;
        self.composer_wheel_scroll = None;
        self.composer_undo.break_typing_run();
        self.mark_dirty();
    }

    /// Delete the char range `[start, end)` as one undoable kill and put the
    /// removed text on the same clipboard target as an explicit cut (§4).
    pub(super) fn kill_range(&mut self, start: usize, end: usize) {
        let count = self.input.chars().count();
        let start = start.min(count);
        let end = end.min(count);
        if start >= end {
            return;
        }
        self.record_undo(EditKind::Discrete);
        let killed: String = self.input.chars().skip(start).take(end - start).collect();
        let retained: String = self
            .input
            .chars()
            .take(start)
            .chain(self.input.chars().skip(end))
            .collect();
        self.input = retained;
        self.input_cursor = start;
        self.composer_selection.set_caret(&self.input, start);
        self.composer_wheel_scroll = None;
        copy_text_osc52(&killed);
    }

    pub(super) fn composer_undo_step(&mut self) {
        let Some(snapshot) = self.composer_undo.undo(&self.input, self.input_cursor) else {
            return;
        };
        self.apply_composer_snapshot(snapshot);
    }

    pub(super) fn composer_redo_step(&mut self) {
        let Some(snapshot) = self.composer_undo.redo(&self.input, self.input_cursor) else {
            return;
        };
        self.apply_composer_snapshot(snapshot);
    }

    pub(super) fn apply_composer_snapshot(
        &mut self,
        snapshot: crate::tui::composer_selection::ComposerSnapshot,
    ) {
        self.input = snapshot.text;
        self.input_cursor = snapshot.cursor.min(self.input.chars().count());
        self.composer_selection
            .set_caret(&self.input, self.input_cursor);
        self.composer_wheel_scroll = None;
        self.dismiss_history();
        self.sync_slash_open();
        self.mark_dirty();
    }

    pub(super) fn move_composer_cursor(&mut self, row_delta: isize, extend: bool) {
        let width = self
            .composer_inner
            .map_or(1, |inner| usize::from(inner.width.saturating_sub(2).max(1)));
        let (column, row) = visual_cursor_xy(&self.input, width, self.input_cursor);
        let last_row = visual_line_count(&self.input, width).saturating_sub(1);
        let target_row = row.saturating_add_signed(row_delta).min(last_row);
        let next = cursor_from_visual_click(&self.input, width, target_row, column);
        if extend {
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
    }

    /// `/reload`: hand the idle session to the installed `plak`. A running
    /// turn or live sub-agents would be cut off, and with nothing installed
    /// the TUI would close for nothing, so all three refuse. The loop performs
    /// the handoff, because it owns the host mailbox the explicit quit uses.
    fn request_reload(&mut self) -> Option<UserAction> {
        let refusal = self.reload_refusal().or_else(|| {
            crate::build_identity::installed_executable()
                .is_none()
                .then_some("reload refused: no installed plak found on PATH")
        });
        if let Some(refusal) = refusal {
            self.push_block(ScrollBlock::System(refusal.to_owned()));
            return None;
        }
        self.pending_reload = true;
        None
    }

    /// Session state that refuses `/reload`, checked again when the
    /// installed-build probe answers.
    pub(super) fn reload_refusal(&self) -> Option<&'static str> {
        if self.busy {
            Some("reload refused: wait for the turn to finish or cancel it")
        } else if self.background_processes > 0 {
            Some("reload refused: background processes are running; stop them first (/ps)")
        } else if self.tree_live_descendants > 0 {
            Some("reload refused: sub-agents are still running")
        } else {
            None
        }
    }

    pub(super) fn apply_slash_effect(&mut self, effect: SlashEffect) -> Option<UserAction> {
        match effect {
            SlashEffect::Quit => self.request_quit(),
            SlashEffect::OpenProcesses => {
                self.clear_input();
                self.open_processes();
                None
            }
            SlashEffect::Reload => self.request_reload(),
            SlashEffect::AttachImage(path) => {
                self.attach_image(&expand_home(&path));
                None
            }
            SlashEffect::OpenSettings => self.open_control_tab(ControlTab::Settings),
            SlashEffect::OpenProviders => self.open_control_tab(ControlTab::Providers),
            SlashEffect::OpenUsage => self.open_control_tab(ControlTab::Usage),
            SlashEffect::OpenContext => self.open_control_tab(ControlTab::Context),
            SlashEffect::OpenSession => self.open_control_tab(ControlTab::Session),
            SlashEffect::OpenAgents => {
                self.slash_open = false;
                self.clear_input();
                self.open_control_tab(ControlTab::Models)
            }
            SlashEffect::ConfirmProfile(name) => {
                self.slash_open = false;
                self.clear_input();
                self.open_profile_confirm(&name)
            }
            SlashEffect::OpenKeybinds => self.open_control_tab(ControlTab::Keybinds),
            SlashEffect::OpenInfoOverlay { title, body } => {
                self.slash_open = false;
                self.clear_input();
                self.info_overlay = Some((title, body));
                self.info_overlay_scroll = modal_chrome::ScrollState::default();
                self.mark_dirty();
                None
            }
            SlashEffect::LocalNote(note) => {
                // Prefer a single system block (word-wrap handles width); avoid
                // dumping multi-line help walls into the permanent transcript.
                let compact = note.trim();
                if !compact.is_empty() {
                    self.push_block(ScrollBlock::System(compact.to_owned()));
                }
                None
            }
            SlashEffect::Action(UserAction::ClearSession) => Some(UserAction::ClearSession),
            SlashEffect::Action(UserAction::DropQueued) => self.queue_drop_last(),
            SlashEffect::Action(UserAction::OpenTopologyNavigator) => {
                self.activity_tab = Some(super::processes::ActivityTab::Agents);
                self.activity_last_tab = super::processes::ActivityTab::Agents;
                self.slash_open = false;
                self.clear_input();
                self.close_resume_menu();
                if self.tree_navigator.is_none() {
                    self.tree_navigator = Some(TreeNavigator::awaiting());
                }
                self.mark_dirty();
                Some(UserAction::OpenTopologyNavigator)
            }
            SlashEffect::Action(UserAction::StopAllTopology) => {
                self.slash_open = false;
                self.clear_input();
                self.confirm_destructive(UserAction::StopAllTopology)
            }
            SlashEffect::Action(action) => Some(action),
        }
    }

    pub(super) fn complete_slash_entry(&mut self) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }
        let idx = self.slash_selected.min(matches.len() - 1);
        let (name, _) = matches[idx];
        let filter = self.slash_filter();
        let args = filter
            .split_once(char::is_whitespace)
            .map(|(_, a)| a.trim().to_owned())
            .unwrap_or_default();
        let line = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        self.set_input(line);
        self.sync_slash_open();
        self.mark_dirty();
    }

    pub(super) fn select_slash_entry(&mut self) -> Option<UserAction> {
        self.complete_slash_entry();
        self.submit_line(false)
    }

    pub(super) fn history_up(&mut self) {
        if self.prompt_history.is_empty() {
            self.pane.scroll_from_bottom = self.pane.scroll_from_bottom.saturating_add(1);
            self.begin_transcript_camera();
            self.mark_dirty();
            return;
        }
        let next = match self.history_browse {
            None => self.prompt_history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_browse = Some(next);
        self.mark_dirty();
    }

    pub(super) fn history_down(&mut self) {
        let Some(i) = self.history_browse else {
            self.pane.scroll_from_bottom = self.pane.scroll_from_bottom.saturating_sub(1);
            self.begin_transcript_camera();
            self.mark_dirty();
            return;
        };
        if i + 1 < self.prompt_history.len() {
            self.history_browse = Some(i + 1);
        } else {
            self.clear_input();
            self.dismiss_history();
            return;
        }
        self.mark_dirty();
    }

    pub(super) fn load_history_selected(&mut self) {
        let Some(index) = self.history_browse else {
            return;
        };
        let Some(prompt) = self.prompt_history.get(index).cloned() else {
            self.dismiss_history();
            return;
        };
        self.set_input(prompt);
        self.dismiss_history();
        self.sync_slash_open();
        self.mark_dirty();
    }

    pub(super) fn dismiss_history(&mut self) {
        self.history_browse = None;
        self.prompt_history_hits.clear();
        self.mark_dirty();
    }

    /// Bracketed paste is an input event like any other: it traverses the same
    /// modal chain as a key (§4 "Paste routing"). Paste never reaches the
    /// composer while a modal owns input, and every path applies the one
    /// [`crate::tui::text_input`] admission rule.
    pub(crate) fn handle_paste(&mut self, text: &str) -> Option<UserAction> {
        // A pop-up or dialog owns input: a paste never reaches the draft
        // under it, and never presses one of its buttons.
        if self.destructive_confirm.is_some() || self.info_overlay.is_some() {
            return None;
        }
        // A view with no prompt has no draft: a paste does nothing, and the
        // hidden root draft is never pasted into.
        if self.has_no_prompt() || self.resume_menu.is_some() {
            return None;
        }
        if self.control_tab.is_some() {
            self.paste_into_control_center(text);
            return None;
        }
        self.typing.observe_paste(self.now());
        // The queue never takes typing: a paste leaves it for the composer.
        self.queue_focused = false;
        // A dropped or pasted absolute path to an existing image file
        // attaches the image instead of typing its path.
        if let Some(path) = pasted_image_path(text) {
            self.attach_image(&path);
            return None;
        }
        self.insert_chars(text);
        self.after_composer_edit();
        None
    }

    /// The Menu's only text sink is the Settings search filter; every other
    /// tab drops the paste rather than leaking it through.
    pub(super) fn paste_into_control_center(&mut self, text: &str) {
        if self.control_tab != Some(ControlTab::Settings) {
            return;
        }
        let filtered = sanitize_single_line(text);
        if let Some(menu) = self.settings_menu.as_mut() {
            for ch in filtered.chars() {
                menu.push_filter_char(ch);
            }
            self.mark_dirty();
        }
    }
}

impl App {
    /// Attaches one image file to the draft; a refusal is a system note
    /// naming the file, and the draft is unchanged.
    pub(super) fn attach_image(&mut self, path: &std::path::Path) {
        self.mark_dirty();
        if self.attachments.len() >= plakous::agent::MAX_USER_IMAGES {
            self.push_block(ScrollBlock::System(format!(
                "image not attached: a message carries at most {} images",
                plakous::agent::MAX_USER_IMAGES
            )));
            return;
        }
        match crate::attachments::read_image(path) {
            Ok(image) => self.attachments.push(image),
            Err(error) => self.push_block(ScrollBlock::System(format!("not attached: {error}"))),
        }
    }
}

/// `~/x` relative to `HOME`; anything else as typed.
fn expand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::Path::new(&home).join(rest),
        _ => std::path::PathBuf::from(path),
    }
}

/// The path a terminal pastes when a file is dropped on it, when that path
/// is absolute and names an existing png, jpeg, gif or webp file. Surrounding
/// quotes and backslash-escaped spaces are undone.
fn pasted_image_path(text: &str) -> Option<std::path::PathBuf> {
    use std::io::Read as _;
    let text = text.trim();
    if text.contains('\n') {
        return None;
    }
    let unquoted = ['\'', '"']
        .iter()
        .find_map(|quote| {
            text.strip_prefix(*quote)
                .and_then(|rest| rest.strip_suffix(*quote))
        })
        .map_or_else(|| text.replace("\\ ", " "), str::to_owned);
    let path = std::path::PathBuf::from(unquoted);
    if !path.is_absolute() || !path.is_file() {
        return None;
    }
    let mut head = Vec::new();
    std::fs::File::open(&path)
        .ok()?
        .take(4096)
        .read_to_end(&mut head)
        .ok()?;
    let sniffed = plakous::tools::sniff_image(&head);
    // A header too short to state dimensions still names the type.
    matches!(
        sniffed,
        Ok(_) | Err(plakous::tools::ImageSniffError::Dimensions)
    )
    .then_some(path)
}
