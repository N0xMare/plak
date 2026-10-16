//! Frame composition and chrome panels.

use super::{
    Alignment, App, Block, BorderType, Borders, Clear, Constraint, Duration, Frame,
    HUD_SCAN_BLOCKS, Instant, Layout, Line, PROMPT_PREFIX, Paragraph, QUITTING_STATUS,
    QueueDrainState, Rect, ScrollBlock, Span, Style, TREE_PAUSED_STATUS, UnicodeWidthStr,
    bounded_panel_height, desired_block_height, modal_chrome, tool_is_running, user_phase,
    user_status_detail, visual_cursor_xy_from_rows,
};
use unicode_segmentation::UnicodeSegmentation;

impl App {
    /// Compose one full frame.
    ///
    /// `pub(crate)` so the crate-internal §11 gate can score a composed
    /// `ratatui` buffer rather than the transcript layout alone.
    pub(crate) fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let _frame_span =
            tracing::debug_span!("tui.frame", width = area.width, height = area.height,).entered();
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.app_bg)),
            area,
        );
        self.key_label_hits.clear();
        // Menu and Activity are the same full-page frame (spec 02 §5.5, §5.7):
        // nothing of the main view is drawn or hit-tested beneath either.
        // The agent transcript is the Agents tab's body, not a main view.
        if self.activity_tab.is_some() || self.control_tab.is_some() {
            self.clear_main_view_hits();
        } else {
            self.draw_root_view(frame, area);
        }
        self.draw_profile_hover(frame);

        if self.slash_open {
            self.draw_slash_overlay(frame, area);
        }
        if self.resume_menu.is_some() {
            self.draw_resume_overlay(frame, area);
        }
        // Menu covers Activity. Do not leave Activity's hits live underneath.
        if self.activity_tab.is_some() && self.control_tab.is_none() {
            self.draw_activity_overlay(frame, area);
        } else {
            self.clear_activity_hits();
            // Menu replaced the frame and did not paint a transcript. Drop the
            // previous frame's track so it cannot page under the menu.
            if self.activity_tab.is_some() || self.control_tab.is_some() {
                self.clear_transcript_hit_rects();
            }
        }
        if self.control_tab.is_some() {
            self.draw_control_overlay(frame, area);
        }
        if self.info_overlay.is_some() {
            self.draw_info_overlay(frame, area);
        }
        if self.destructive_confirm.is_some() {
            self.draw_destructive_confirm(frame, area);
        }
        if self.quit_arm.is_some() {
            self.draw_quit_confirm(frame, area);
        }
    }

    /// The root view: transcript, queue and history shelves, composer.
    fn draw_root_view(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // The agent bar is the agent view's; its last rectangle overlaps the
        // chat block's bottom lines and must not outlive the view.
        self.agent_bar_hit = None;
        // Transcript-only top; floating chat block at bottom (max ⅓ height).
        let content_w = area.width.saturating_sub(4).max(1);
        let block_h = desired_block_height(area.height, content_w, &self.input);
        let queue_n = self.local_queue.len() + self.composer_chrome.sent_steers;
        let desired_queue_h = if queue_n == 0 {
            0
        } else {
            u16::try_from((queue_n.min(4)).saturating_add(2)).unwrap_or(6)
        };
        let desired_history_h = if self.history_browse.is_some() { 6 } else { 0 };
        let desired_recovery_h = if self.capacity_recovery.is_some() {
            if self.prompt_history.is_empty() { 4 } else { 6 }
        } else {
            0
        };
        let bottom_gap = u16::from(area.height > block_h);
        let composer_h = block_h.min(area.height);
        let mut remaining = area
            .height
            .saturating_sub(composer_h)
            .saturating_sub(bottom_gap);
        let spacer_h = u16::from(remaining > 0);
        remaining = remaining.saturating_sub(spacer_h);
        let history_h = bounded_panel_height(desired_history_h, remaining);
        remaining = remaining.saturating_sub(history_h);
        let recovery_h = bounded_panel_height(desired_recovery_h, remaining);
        remaining = remaining.saturating_sub(recovery_h);
        let queue_h = bounded_panel_height(desired_queue_h, remaining);
        let chunks = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(spacer_h),
            Constraint::Length(queue_h),
            Constraint::Length(history_h),
            Constraint::Length(recovery_h),
            Constraint::Length(composer_h),
            Constraint::Length(bottom_gap),
        ])
        .split(area);

        self.draw_scrollback(frame, chunks[0]);
        self.draw_jump_to_live_spacer(frame, chunks[1]);
        if queue_h > 0 {
            self.queue_area = Some(chunks[2]);
            self.draw_queue_panel(frame, chunks[2]);
        } else {
            self.queue_area = None;
        }
        if history_h > 0 {
            self.draw_prompt_history_shelf(frame, chunks[3]);
        } else {
            self.prompt_history_hits.clear();
        }
        if recovery_h > 0 {
            self.draw_capacity_recovery(frame, chunks[4]);
        } else {
            self.capacity_recovery_action = None;
        }
        self.draw_floating_block(frame, chunks[5]);
        self.draw_composer_hint(frame, chunks[6]);
    }

    /// Braille frame for the §5.4 composer HUD.
    pub(super) fn status_spinner(&self) -> &'static str {
        if self.render_prefs.reduced_motion {
            return "⠿";
        }
        ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"][self.status_spinner_frame % 8]
    }

    pub(super) fn advance_status_spinner(&mut self, now: Instant) -> bool {
        // With motion off the glyph is static, but the row still carries the
        // phase and turn timers: repaint once a second so they are not frozen
        // between host events (§5.4).
        let interval = if self.render_prefs.reduced_motion {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(100)
        };
        if now.duration_since(self.status_spinner_at) < interval {
            return false;
        }
        if self.turn_status_label().is_none()
            && self.tree_live_descendants == 0
            && self.background_processes == 0
        {
            // Idle: only the prompt clock can move, and it repaints only when
            // its text changes.
            let changed = self.idle_prompt_clock(now) != self.idle_clock_painted;
            if changed {
                self.status_spinner_at = now;
            }
            return changed;
        }
        if self.render_prefs.reduced_motion {
            self.status_spinner_at = now;
            return true;
        }
        self.status_spinner_frame = self.status_spinner_frame.wrapping_add(1);
        self.status_spinner_at = now;
        true
    }

    /// The §5.4 HUD state, or `None` when the root is idle.
    ///
    /// Every value is host-observed: the running tool's own §5.2 header, the
    /// host phase, or the startup row. Nothing is invented.
    pub(super) fn turn_status_label(&self) -> Option<String> {
        if self.quitting {
            return Some(QUITTING_STATUS.to_owned());
        }
        if self.session_open_started.is_some() {
            return Some("starting…".to_owned());
        }
        if self.pending_approval().is_some() {
            return Some("approval needed".to_owned());
        }
        if matches!(self.phase.as_str(), "error" | "not_ready") {
            let reason = self
                .status_detail
                .as_deref()
                .map_or_else(|| user_phase(&self.phase).to_owned(), user_status_detail);
            return Some(format!("error: {reason}"));
        }
        if self.phase == "interrupted" {
            return Some("interrupted".to_owned());
        }
        if self.phase == "rebind" {
            return Some("switching model…".to_owned());
        }
        // Pending results, questions, process exits, and pauses are idle
        // chrome. A running turn hides them even when the facts remain, and
        // a problem label outranks them.
        if !self.busy
            && let Some(label) =
                pending_indicator_label(self.pending_continuation, self.tree_restored_paused)
        {
            return Some(label);
        }
        if !self.busy {
            return None;
        }
        if self.cancel_pending || self.phase == "interrupting" {
            return Some("cancelling…".to_owned());
        }
        if self.phase == "compacting" {
            return Some("compacting".to_owned());
        }
        if let Some((retry, announced)) = &self.pending_retry {
            return Some(retry_status_label(
                retry,
                self.now().saturating_duration_since(*announced),
            ));
        }
        // A running tool names the row with its own §5.2 header. The scan is
        // bounded: a live call is at the transcript tail, and the frame clock
        // must never walk the whole projection.
        if let Some(block) = self
            .root_pane()
            .blocks
            .iter()
            .rev()
            .take(HUD_SCAN_BLOCKS)
            .find(
                |block| matches!(block, ScrollBlock::Tool { state, .. } if tool_is_running(state)),
            )
            && let ScrollBlock::Tool {
                kind,
                name,
                subject,
                ..
            } = block
        {
            return Some(super::composer_chrome::activity_label(
                name,
                *kind,
                subject.as_deref(),
            ));
        }
        if self
            .root_pane()
            .blocks
            .iter()
            .rev()
            .take(HUD_SCAN_BLOCKS)
            .any(|block| matches!(block, ScrollBlock::Thinking { sealed: false, .. }))
        {
            return Some("thinking".to_owned());
        }
        if self.root_pane().assistant_stream_active {
            return Some("responding".to_owned());
        }
        // Busy with no unsealed thinking block is waiting on the model, not
        // reasoning. Codex never renders thought blocks (spec 02 §9).
        Some("waiting for model".to_owned())
    }

    /// The idle pending line, without the steady `◆` the border adds.
    pub(super) fn idle_pending_label(&self) -> Option<String> {
        if self.busy {
            return None;
        }
        pending_indicator_label(self.pending_continuation, self.tree_restored_paused)
    }
}

fn pending_indicator_label(
    pending: plakous_host_protocol::PendingContinuation,
    tree_restored_paused: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    push_pending_count(&mut parts, pending.results, "result", "results");
    push_pending_count(&mut parts, pending.questions, "question", "questions");
    match pending.process_exits {
        0 => {}
        1 => parts.push("1 process exited".to_owned()),
        count => parts.push(format!("{count} processes exited")),
    }
    if pending.auto_continue_paused {
        parts.push("auto-continue paused".to_owned());
    }
    if pending.tree_paused || tree_restored_paused {
        parts.push(TREE_PAUSED_STATUS.to_owned());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn push_pending_count(parts: &mut Vec<String>, count: u32, singular: &str, plural: &str) {
    if count == 0 {
        return;
    }
    let noun = if count == 1 { singular } else { plural };
    parts.push(format!("{count} {noun} pending"));
}

impl App {
    pub(super) fn draw_capacity_recovery(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(recovery) = self.capacity_recovery else {
            return;
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" context recovery ")
            .title_style(self.theme.menu_title)
            .border_style(self.theme.menu_title)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let (message, action) = match recovery {
            crate::tui::ContextCapacityRecovery::ResendShorter => (
                "Prompt exceeds context capacity; shorten the restored draft and resend.",
                "",
            ),
            crate::tui::ContextCapacityRecovery::Compactable => (
                "Context limit reached; failed turn will not replay.",
                "[ compact this checkpoint ]",
            ),
            crate::tui::ContextCapacityRecovery::Protected => (
                "Protected context cannot be compacted; choose a larger model, fork, or /clear.",
                "",
            ),
        };
        frame.render_widget(Paragraph::new(message), inner);
        let has_saved_prompt = !self.prompt_history.is_empty();
        if has_saved_prompt {
            let guidance = Rect {
                y: inner.y.saturating_add(1),
                height: inner.height.saturating_sub(1).min(2),
                ..inner
            };
            frame.render_widget(
                Paragraph::new(
                    "Prompt saved in history.\nEmpty composer: ↑ history; Enter/← loads only.",
                ),
                guidance,
            );
        }
        let action_row = if has_saved_prompt { 3 } else { 1 };
        self.capacity_recovery_action =
            (!action.is_empty() && inner.height > action_row).then(|| Rect {
                x: inner.x,
                y: inner.y.saturating_add(action_row),
                width: u16::try_from(action.len())
                    .unwrap_or(inner.width)
                    .min(inner.width),
                height: 1,
            });
        if let Some(hit) = self.capacity_recovery_action {
            frame.render_widget(Paragraph::new(action).style(self.theme.slash_selected), hit);
        }
    }

    pub(super) fn draw_prompt_history_shelf(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(selected) = self.history_browse else {
            return;
        };
        let start = selected.saturating_sub(3);
        let end = (start + 4).min(self.prompt_history.len());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" prompt history · ↑ older · ↓ newer · Enter use/edit ")
            .title_style(self.theme.menu_title)
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(area);
        self.prompt_history_hits.clear();
        frame.render_widget(block, area);
        let width = usize::from(inner.width.saturating_sub(3));
        let lines = self.prompt_history[start..end]
            .iter()
            .enumerate()
            .map(|(offset, prompt)| {
                let index = start + offset;
                self.prompt_history_hits.push((
                    index,
                    Rect {
                        x: inner.x,
                        y: inner.y.saturating_add(u16::try_from(offset).unwrap_or(0)),
                        width: inner.width,
                        height: 1,
                    },
                ));
                let marker = if index == selected { "❯ " } else { "  " };
                let one_line = prompt.replace('\n', " ");
                let shown: String = one_line.chars().take(width).collect();
                let padding = usize::from(inner.width)
                    .saturating_sub(marker.chars().count() + shown.chars().count());
                let style = if index == selected {
                    self.theme.slash_selected
                } else {
                    self.theme.slash_item
                };
                Line::from(Span::styled(
                    format!("{marker}{shown}{}", " ".repeat(padding)),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The floating text overlay (`/help`).
    ///
    /// The body is scrolled through the same renderer the Keybinds tab uses.
    /// The overlay used to size itself to the body and then clip whatever did
    /// not fit, so a body longer than the terminal simply lost its tail with
    /// nothing on screen saying so.
    pub(super) fn draw_info_overlay(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some((title, body)) = self.info_overlay.clone() else {
            return;
        };
        let width =
            (area.width.saturating_mul(3) / 4).clamp(28, area.width.saturating_sub(2).max(20));
        // Take the height the body wants, capped by the frame; the scroll state
        // owns whatever is left over.
        let wanted = u16::try_from(body.lines().count().saturating_add(2)).unwrap_or(u16::MAX);
        let height = wanted.min(area.height.saturating_sub(2).max(4)).max(4);
        let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
        let y = area
            .y
            .saturating_add(area.height.saturating_sub(height) / 3);
        let popup = Rect {
            x,
            y,
            width,
            height,
        };
        // §5.5: every overlay dims what it covers, so the modal reads as
        // raised and the transcript behind it stops competing with it.
        modal_chrome::render_scrim(frame, area, &self.theme);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(" {title} "))
            .title_alignment(Alignment::Center)
            .title_style(self.theme.menu_title)
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let item = self.theme.slash_item;
        let bg = self.theme.menu_bg;
        Self::render_scrolled_text(frame, inner, &mut self.info_overlay_scroll, &body, item, bg);
    }

    pub(super) fn draw_quit_confirm(&self, frame: &mut Frame<'_>, area: Rect) {
        // Tact-style compact confirm above the chat block (top-right of lower third).
        let width = 36u16
            .min(area.width.saturating_sub(2))
            .max(24.min(area.width));
        let height = 5u16.min(area.height);
        let x = area
            .x
            .saturating_add(area.width.saturating_sub(width).saturating_sub(1));
        let y = area
            .y
            .saturating_add(area.height.saturating_sub(height).saturating_sub(8).max(1));
        let popup = Rect {
            x,
            y,
            width,
            height,
        };
        // §5.5: every overlay dims what it covers, so the modal reads as
        // raised and the transcript behind it stops competing with it.
        modal_chrome::render_scrim(frame, area, &self.theme);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" quit ")
            .title_alignment(Alignment::Center)
            .title_style(self.theme.menu_title)
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let lines = vec![
            Line::from(Span::styled(
                " cancel stuck — Ctrl+C to quit",
                self.theme.slash_item,
            )),
            Line::from(Span::styled(
                " Esc keeps waiting · 1s",
                self.theme.slash_item_help,
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    pub(super) fn draw_queue_panel(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let paused = self.queue_drain == QueueDrainState::RestoredPaused;
        let title = if self.local_queue.is_empty() {
            " sent steers "
        } else if self.queue_editing.is_some() && !self.queue_focused {
            " queue · editing queued follow-up · Enter saves · Esc cancels "
        } else if !self.agent_enabled && self.queue_focused {
            " queue · offline edit (Ctrl+E edit · ⇧↑↓ move · Ctrl+D drop · Esc) "
        } else if !self.agent_enabled {
            " queue · delivery unavailable · Tab to edit "
        } else if self.queue_focused && paused {
            " queue · paused with the tree (Ctrl+R resume · Enter send now · Ctrl+E edit · ⇧↑↓ move · Ctrl+D drop) "
        } else if self.queue_focused {
            " queue · focused (↑↓ · Enter send now · Ctrl+E edit · ⇧↑↓ move · Ctrl+D drop · Esc) "
        } else if paused {
            " queue · paused with the tree · Tab focus "
        } else {
            " queue · Ctrl+S sends now · Tab focus "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .title_style(self.theme.menu_title)
            .border_style(if self.queue_focused {
                self.theme.menu_title
            } else {
                self.theme.menu_border
            })
            .style(Style::default().bg(self.theme.app_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let painted_at = self.now();
        let previous = std::mem::take(&mut self.queue_button_hits);
        let mut lines = Vec::new();
        // FIFO display oldest-first with #1 = next to send.
        let n = self.local_queue.len();
        let visible = if self.composer_chrome.sent_steers > 0 {
            3
        } else {
            4
        };
        let start = self
            .queue_selected
            .saturating_sub(visible - 1)
            .min(n.saturating_sub(visible));
        let end = (start + visible).min(n);
        for (disp_i, abs_i) in (start..end).enumerate() {
            let item = &self.local_queue[abs_i];
            let budget = usize::from(inner.width).saturating_sub(20);
            let preview =
                super::composer_chrome::clip_text(&item.text.replace('\n', " "), budget.min(40));
            let selected = self.queue_focused && abs_i == self.queue_selected;
            let style = if selected {
                self.theme.slash_selected
            } else {
                self.theme.queue_item
            };
            let help = if selected {
                self.theme.slash_selected_help
            } else {
                self.theme.queue_help
            };
            let mark = if selected { "❯" } else { " " };
            let state = if item.rejected { "rejected: " } else { "" };
            let left = format!("{mark}#{} {state}{preview}", abs_i + 1);
            // Hit targets follow the painted chips, not the panel's right edge.
            let row_y = inner.y.saturating_add(u16::try_from(disp_i).unwrap_or(0));
            let left_w = u16::try_from(UnicodeWidthStr::width(left.as_str())).unwrap_or(0);
            let drop_w = 6u16; // "[drop]"
            let now_w = if self.agent_enabled { 5u16 } else { 0u16 };
            let lead = 2u16; // "  " before the first chip
            let between = 1u16; // space between [now] and [drop]
            let chips_x = inner.x.saturating_add(left_w).saturating_add(lead);
            let drop_x = if now_w == 0 {
                chips_x
            } else {
                chips_x.saturating_add(now_w).saturating_add(between)
            };
            let now_rect = Rect {
                x: chips_x,
                y: row_y,
                width: now_w,
                height: 1,
            };
            let drop_rect = Rect {
                x: drop_x,
                y: row_y,
                width: drop_w,
                height: 1,
            };
            let now_rect = now_rect.intersection(inner);
            let drop_rect = drop_rect.intersection(inner);
            let since = previous
                .iter()
                .find(|hit| hit.id == item.id && hit.now == now_rect && hit.drop == drop_rect)
                .map_or(painted_at, |hit| hit.since);
            self.queue_button_hits.push(super::QueueChipHit {
                abs_idx: abs_i,
                id: item.id.clone(),
                now: now_rect,
                drop: drop_rect,
                since,
            });
            let mut spans = vec![Span::styled(left, style), Span::raw("  ")];
            if self.agent_enabled {
                spans.push(Span::styled(
                    "[now]",
                    if self.chrome_hovered(now_rect) {
                        self.theme.slash_selected
                    } else {
                        help
                    },
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                "[drop]",
                if self.chrome_hovered(drop_rect) {
                    self.theme.slash_selected
                } else {
                    help
                },
            ));
            lines.push(Line::from(spans));
        }
        for _ in 0..self
            .composer_chrome
            .sent_steers
            .min(4usize.saturating_sub(lines.len()))
        {
            lines.push(Line::from(Span::styled(
                " sent · waiting for the current reply",
                self.theme.chrome_dim,
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "composer chrome, exact hit geometry, wrapping, and cursor share one frame area"
    )]
    pub(super) fn draw_floating_block(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.assistant)
            .style(Style::default().bg(self.theme.app_bg));
        let inner = block.inner(area);
        self.composer_inner = Some(inner);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        self.draw_composer_top(frame, area);
        self.draw_composer_bottom(frame, area);
        if inner.height < 1 || inner.width < 2 {
            return;
        }

        // Prompt: leading `❯` (U+276F) then draft — not ASCII `>`.
        let glyph_cols = 2u16; // "❯ "
        let wrap_w = usize::from(inner.width.saturating_sub(glyph_cols).max(1));
        let max_rows = usize::from(inner.height);
        let (mut body, cursor_row) = {
            let wrap_rows = self.composer_wrap.rows(&self.input, wrap_w);
            let body: Vec<String> = wrap_rows.iter().map(|(_, _, line)| line.clone()).collect();
            let cursor_row = visual_cursor_xy_from_rows(wrap_rows, self.input_cursor).1;
            (body, cursor_row)
        };
        let max_scroll = body.len().saturating_sub(max_rows);
        let scroll = self.composer_wheel_scroll.map_or_else(
            || {
                let current = self.composer_visual_scroll.min(max_scroll);
                if cursor_row < current {
                    cursor_row
                } else if cursor_row >= current.saturating_add(max_rows) {
                    cursor_row.saturating_sub(max_rows.saturating_sub(1))
                } else {
                    current
                }
            },
            |scroll| scroll.min(max_scroll),
        );
        self.composer_visual_scroll = scroll;
        if body.len() > max_rows {
            let end = (scroll + max_rows).min(body.len());
            let start = end.saturating_sub(max_rows);
            body = body[start..end].to_vec();
        }
        while body.len() < max_rows {
            body.push(String::new());
        }
        let para_lines: Vec<Line<'_>> = body
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let g = if i == 0 && scroll == 0 {
                    PROMPT_PREFIX
                } else {
                    "  "
                };
                if l.is_empty() && self.input.is_empty() && i == 0 {
                    Line::from(vec![
                        Span::styled(g, self.theme.composer_prompt),
                        Span::styled(" ", self.theme.assistant),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(g, self.theme.composer_prompt),
                        Span::styled(l, self.theme.assistant),
                    ])
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(para_lines), inner);

        if let Some(range) = self.composer_selection.render_range(&self.input) {
            let wrap_rows = self.composer_wrap.rows(&self.input, wrap_w);
            let mut char_index = 0usize;
            for grapheme in self.input.graphemes(true) {
                let grapheme_start = char_index;
                char_index = char_index.saturating_add(grapheme.chars().count());
                if grapheme_start >= range.end || char_index <= range.start {
                    continue;
                }
                let (selected_col, selected_row) =
                    visual_cursor_xy_from_rows(wrap_rows, grapheme_start);
                if selected_row < scroll || selected_row >= scroll.saturating_add(max_rows) {
                    continue;
                }
                let y = inner.y.saturating_add(
                    u16::try_from(selected_row.saturating_sub(scroll)).unwrap_or(u16::MAX),
                );
                let cells = UnicodeWidthStr::width(grapheme).max(1);
                for offset in 0..cells {
                    let x = inner
                        .x
                        .saturating_add(glyph_cols)
                        .saturating_add(u16::try_from(selected_col + offset).unwrap_or(u16::MAX));
                    if x < inner.x.saturating_add(inner.width)
                        && y < inner.y.saturating_add(inner.height)
                    {
                        frame.buffer_mut()[(x, y)].set_style(self.theme.slash_selected);
                    }
                }
            }
        }

        // The cursor is shown only while the composer owns the operator's
        // attention; a transcript wheel, click or drag hides it for the frame.
        if inner.width > 0
            && inner.height > 0
            && !self.transcript_gesture
            && cursor_row >= scroll
            && cursor_row < scroll.saturating_add(max_rows)
        {
            let (cursor_col, cursor_row) = visual_cursor_xy_from_rows(
                self.composer_wrap.rows(&self.input, wrap_w),
                self.input_cursor,
            );
            let vis_row = cursor_row.saturating_sub(scroll);
            let cx = inner.x.saturating_add(glyph_cols).saturating_add(
                u16::try_from(cursor_col)
                    .unwrap_or(0)
                    .min(inner.width.saturating_sub(1)),
            );
            let cy = inner.y.saturating_add(u16::try_from(vis_row).unwrap_or(0));
            frame.set_cursor_position(ratatui::layout::Position { x: cx, y: cy });
        }
    }
}

/// The HUD countdown; retry causes belong in the transcript.
fn retry_status_label(
    retry: &plakous_host_protocol::ModelRetryNotice,
    elapsed: Duration,
) -> String {
    let remaining = Duration::from_millis(retry.delay_ms).saturating_sub(elapsed);
    let seconds = remaining.as_millis().div_ceil(1_000);
    format!(
        "retrying {seconds}s ({}/{})",
        retry.next_attempt, retry.max_attempts
    )
}
