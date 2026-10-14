//! Mouse dispatch.

use super::typing_guard::CHIP_SETTLE;
use super::{
    App, ClickSelection, ComposerClick, Instant, MouseEvent, MouseEventKind, PointerButton, Rect,
    UserAction, cursor_from_visual_rows, event, picker, visual_line_count,
};

impl App {
    pub(super) fn reset_composer_drag(&mut self) {
        self.composer_dragging = false;
        self.composer_drag_pointer = None;
        self.composer_down_cell = None;
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<UserAction> {
        self.dispatch_mouse(mouse)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "mouse event routing keeps hit-test precedence explicit"
    )]
    fn dispatch_mouse(&mut self, mouse: MouseEvent) -> Option<UserAction> {
        if self.a_modal_owns_input() || self.quit_arm.is_some() {
            self.reset_composer_drag();
        }
        self.track_chrome_pointer(mouse);
        if self.destructive_confirm.is_some() {
            return self.handle_destructive_confirm_mouse(mouse);
        }
        // A painted key label presses its key.
        if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left))
            && let Some(code) = self
                .key_label_hits
                .iter()
                .find(|(hit, _)| hit.contains((mouse.column, mouse.row).into()))
                .map(|(_, code)| *code)
        {
            return self.press_key_label(code);
        }
        if self.handle_scrollbar_mouse(mouse) {
            return None;
        }
        if self.activity_tab.is_some() {
            let close_hover = self
                .activity_close_hit
                .is_some_and(|hit| hit.contains((mouse.column, mouse.row).into()));
            if self.activity_close_hover != close_hover {
                self.activity_close_hover = close_hover;
                self.mark_dirty();
            }
            if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left)) {
                let point = (mouse.column, mouse.row).into();
                if self
                    .activity_close_hit
                    .is_some_and(|hit| hit.contains(point))
                    || self.tree_popup.is_some_and(|popup| !popup.contains(point))
                {
                    self.close_activity();
                    return None;
                }
                if self
                    .activity_agents_hit
                    .is_some_and(|hit| hit.contains(point))
                {
                    // A click on Agents, including while a transcript is open,
                    // shows the list at that agent.
                    self.close_agent_transcript_onto_list();
                    return self.open_tree_navigator();
                }
                if self
                    .activity_processes_hit
                    .is_some_and(|hit| hit.contains(point))
                {
                    self.close_agent_transcript_onto_list();
                    self.open_processes();
                    return None;
                }
            }
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Processes) {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.handle_process_key(super::KeyCode::Up);
                }
                MouseEventKind::ScrollDown => {
                    self.handle_process_key(super::KeyCode::Down);
                }
                MouseEventKind::Down(event::MouseButton::Left) if !self.process_output => {
                    if let Some(id) = self
                        .process_hits
                        .iter()
                        .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                        .map(|(id, _)| id.clone())
                    {
                        let double = self.list_clicks.click(
                            picker::ClickList::Processes,
                            &id,
                            Instant::now(),
                        );
                        self.process_selected = Some(id);
                        self.mark_dirty();
                        if double {
                            return self.handle_process_key(super::KeyCode::Enter);
                        }
                    }
                }
                _ => {}
            }
            return None;
        }
        if self.info_overlay.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll_info_overlay(|scroll| scroll.scroll_by(-1));
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_info_overlay(|scroll| scroll.scroll_by(1));
                }
                MouseEventKind::Down(event::MouseButton::Left) => {
                    self.info_overlay = None;
                    self.mark_dirty();
                }
                _ => {}
            }
            return None;
        }
        if self.quit_arm.is_some() {
            return None;
        }
        if self.control_tab.is_some() {
            return self.handle_control_mouse(mouse);
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Agents) {
            if self.viewed_agent_id().is_some() {
                // The bar returns to the list. Transcript clicks and the wheel
                // fall through to the scrollback, which is this body.
                if self.is_agent_bar_click(mouse) {
                    return self.agent_view_back();
                }
            } else {
                return self.handle_tree_navigator_mouse(mouse);
            }
        }
        if self.resume_menu.is_some() {
            return self.handle_resume_mouse(mouse);
        }
        if self.slash_open
            && let Some(popup) = self.slash_popup
        {
            let inner = Rect {
                x: popup.x.saturating_add(1),
                y: popup.y.saturating_add(1),
                width: popup.width.saturating_sub(2),
                height: popup.height.saturating_sub(2),
            };
            if inner.contains((mouse.column, mouse.row).into()) {
                let index = self
                    .slash_view_start
                    .saturating_add(usize::from(mouse.row.saturating_sub(inner.y)));
                if index < self.slash_matches().len() {
                    self.slash_selected = index;
                    self.mark_dirty();
                    if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left))
                        && self.list_clicks.click(
                            picker::ClickList::Slash,
                            &index.to_string(),
                            Instant::now(),
                        )
                    {
                        return self.select_slash_entry();
                    }
                }
                return None;
            }
            if popup.contains((mouse.column, mouse.row).into()) {
                return None;
            }
        }

        if let Some(inner) = self.composer_inner
            && inner.contains((mouse.column, mouse.row).into())
            && matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            let wrap_w = usize::from(inner.width.saturating_sub(2).max(1));
            let max_scroll =
                visual_line_count(&self.input, wrap_w).saturating_sub(usize::from(inner.height));
            let current = self
                .composer_wheel_scroll
                .unwrap_or(self.composer_visual_scroll)
                .min(max_scroll);
            let next = match mouse.kind {
                MouseEventKind::ScrollUp => current.saturating_sub(3),
                MouseEventKind::ScrollDown => current.saturating_add(3).min(max_scroll),
                _ => current,
            };
            self.composer_wheel_scroll = Some(next);
            self.composer_visual_scroll = next;
            self.mark_dirty();
            return None;
        }

        // Composer gesture ownership is separate from transcript selection.
        if matches!(mouse.kind, MouseEventKind::Drag(event::MouseButton::Left))
            && let Some(down) = self.composer_down_cell
        {
            if down == (mouse.column, mouse.row) && !self.composer_dragging {
                return None;
            }
            self.composer_dragging = true;
            self.composer_drag_pointer = Some((mouse.column, mouse.row));
            if let Some(index) = self.composer_index_at(mouse.column, mouse.row) {
                self.composer_selection.drag_to(&self.input, index);
                self.input_cursor = self.composer_selection.head();
                self.mark_dirty();
            }
            return None;
        }
        if matches!(mouse.kind, MouseEventKind::Up(event::MouseButton::Left))
            && self.composer_down_cell.is_some()
        {
            if self.composer_dragging
                && let Some(index) = self.composer_index_at(mouse.column, mouse.row)
            {
                self.composer_selection.drag_to(&self.input, index);
                self.input_cursor = self.composer_selection.head();
            }
            self.reset_composer_drag();
            self.mark_dirty();
            return None;
        }

        if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left))
            && self
                .status_processes_hit
                .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
        {
            self.open_processes();
            return None;
        }
        // §5.4 segment 5: clicking `· N agents` opens the canvas.
        if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left))
            && self
                .status_agents_hit
                .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
        {
            return self.open_tree_navigator();
        }

        // Queue [now] / [drop] clicks (Grok Build-style send-now / drop).
        let in_transcript = self
            .transcript_area
            .is_some_and(|area| area.contains((mouse.column, mouse.row).into()));
        if matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left)) {
            if self
                .capacity_recovery_action
                .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
            {
                return Some(UserAction::Compact);
            }
            if let Some((rect, target)) = self.sticky_prompt_jump
                && rect.contains((mouse.column, mouse.row).into())
            {
                self.pane.scroll_from_bottom = target;
                self.mark_dirty();
                return None;
            }
            if let Some((index, _)) = self
                .prompt_history_hits
                .iter()
                .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                .copied()
            {
                self.history_browse = Some(index);
                self.load_history_selected();
                return None;
            }
            if let Some(action) = self.handle_queue_click(mouse.column, mouse.row) {
                return Some(action);
            }
            if let Some(hit) = self
                .approval_chip_hits
                .iter()
                .find(|hit| hit.rect.contains((mouse.column, mouse.row).into()))
                .cloned()
            {
                if self.now().saturating_duration_since(hit.since) < CHIP_SETTLE {
                    return None;
                }
                return self.resolve_pending_approval(hit.choice);
            }
            if let Some(rect) = self.jump_to_live_hit
                && rect.contains((mouse.column, mouse.row).into())
            {
                self.pane.scroll_from_bottom = 0;
                self.mark_dirty();
                return None;
            }
            // A click on a clickable row toggles it on release; a drag from
            // it still selects, so a collapsed row can be copied.
            self.pending_tool_click = self
                .tool_hit_rows
                .iter()
                .position(|&(_, r)| r == mouse.row)
                .and_then(|hit| {
                    self.tool_hit_targets
                        .get(hit)
                        .map(|target| (self.tool_hit_rows[hit].0, *target))
                });
            // Click composer → focus + place cursor.
            if let Some(inner) = self.composer_inner
                && mouse.column >= inner.x
                && mouse.column < inner.x.saturating_add(inner.width)
                && mouse.row >= inner.y
                && mouse.row < inner.y.saturating_add(inner.height)
            {
                self.queue_focused = false;
                let index = self.composer_index_at(mouse.column, mouse.row)?;
                let action = self.composer_selection.click(
                    &self.input,
                    ComposerClick {
                        char_index: index,
                        button: PointerButton::Left,
                        column: mouse.column,
                        row: mouse.row,
                        at: Instant::now().duration_since(self.composer_click_epoch),
                    },
                );
                if action == ClickSelection::Caret {
                    self.composer_selection.begin_drag(&self.input, index);
                    self.composer_down_cell = Some((mouse.column, mouse.row));
                    self.composer_dragging = false;
                    self.composer_drag_pointer = None;
                }
                self.transcript_gesture = false;
                self.input_cursor = self.composer_selection.head();
                self.mark_dirty();
                return None;
            }
        }

        // Transcript: scroll + drag-select.
        match mouse.kind {
            MouseEventKind::ScrollUp if in_transcript => {
                self.transcript_gesture = true;
                self.transcript_gesture_is_wheel = true;
                self.enqueue_transcript_scroll(true);
            }
            MouseEventKind::ScrollDown if in_transcript => {
                self.transcript_gesture = true;
                self.transcript_gesture_is_wheel = true;
                self.enqueue_transcript_scroll(false);
            }
            MouseEventKind::Down(event::MouseButton::Left) if in_transcript => {
                self.transcript_gesture = true;
                self.transcript_gesture_is_wheel = false;
                if let Some(area) = self.transcript_body_area
                    && area.contains((mouse.column, mouse.row).into())
                    && let Some(position) = self.transcript_position_at(mouse.column, mouse.row)
                {
                    self.selection.begin(
                        position,
                        usize::from(area.width),
                        self.pane.transcript_revision,
                    );
                    self.selection
                        .capture_blocks(self.pane.transcript_cache.layout());
                    self.transcript_down_cell = Some((mouse.column, mouse.row));
                    self.mark_dirty();
                }
            }
            MouseEventKind::Drag(event::MouseButton::Left)
                if self.transcript_down_cell.is_some() =>
            {
                let down = self.transcript_down_cell?;
                if down
                    .0
                    .abs_diff(mouse.column)
                    .max(down.1.abs_diff(mouse.row))
                    < 2
                    && self.transcript_drag_pointer.is_none()
                {
                    return None;
                }
                self.transcript_gesture = true;
                self.transcript_gesture_is_wheel = false;
                self.pending_tool_click = None;
                self.transcript_drag_pointer = Some((mouse.column, mouse.row));
                if let Some(position) = self.transcript_position_at(mouse.column, mouse.row)
                    && self.selection.drag(position)
                {
                    self.selection
                        .capture_blocks(self.pane.transcript_cache.layout());
                    self.mark_dirty();
                }
            }
            MouseEventKind::Up(event::MouseButton::Left) => {
                let was_dragging = self.transcript_drag_pointer.is_some();
                self.transcript_down_cell = None;
                self.transcript_drag_pointer = None;
                let selected = was_dragging
                    && self
                        .transcript_position_at(mouse.column, mouse.row)
                        .is_some_and(|position| self.selection.finish(position));
                if selected {
                    self.selection
                        .capture_blocks(self.pane.transcript_cache.layout());
                    self.mark_dirty();
                    // Copy runs after the next paint from the complete cache.
                    self.copy_selection_after_draw = true;
                    self.pending_tool_click = None;
                } else if let Some((block_index, target)) = self.pending_tool_click.take() {
                    self.selection.clear();
                    self.toggle_row(block_index, target);
                } else {
                    self.selection.clear();
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn composer_index_at(&mut self, column: u16, row: u16) -> Option<usize> {
        let inner = self.composer_inner?;
        let clamped_column = column.clamp(
            inner.x,
            inner.x.saturating_add(inner.width.saturating_sub(1)),
        );
        let clamped_row = row.clamp(
            inner.y,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
        );
        let wrap_w = usize::from(inner.width.saturating_sub(2).max(1));
        let col = usize::from(clamped_column.saturating_sub(inner.x)).saturating_sub(2);
        let visual_row = usize::from(clamped_row.saturating_sub(inner.y))
            .saturating_add(self.composer_visual_scroll);
        let text_chars = self.input.chars().count();
        let rows = self.composer_wrap.rows(&self.input, wrap_w);
        Some(cursor_from_visual_rows(rows, text_chars, visual_row, col))
    }

    /// Hit-test queue row chips: `[now]` steers/sends that item, `[drop]` removes it.
    pub(super) fn handle_queue_click(&mut self, col: u16, row: u16) -> Option<UserAction> {
        let area = self.queue_area?;
        if col < area.x
            || col >= area.x.saturating_add(area.width)
            || row < area.y
            || row >= area.y.saturating_add(area.height)
        {
            return None;
        }
        // Inner content starts one cell in from rounded border.
        let inner_y = area.y.saturating_add(1);
        let inner_x = area.x.saturating_add(1);
        let inner_w = area.width.saturating_sub(2);
        if row < inner_y || col < inner_x {
            return None;
        }
        // Prefer exact button hit rects painted this frame.
        // A chip fires only once it has held its place for CHIP_SETTLE: a
        // click aimed at a row the drain just moved never hits its neighbour.
        let now = self.now();
        let point = (col, row).into();
        for hit in &self.queue_button_hits {
            let settled = now.saturating_duration_since(hit.since) >= CHIP_SETTLE;
            let abs_idx = hit.abs_idx;
            if hit.drop.contains(point) {
                if !settled {
                    return None;
                }
                self.queue_selected = abs_idx;
                self.queue_focused = true;
                return self.queue_drop_selected();
            }
            if hit.now.contains(point) {
                if !settled {
                    return None;
                }
                self.queue_selected = abs_idx;
                self.queue_focused = true;
                return self.queue_send_now_at(abs_idx);
            }
        }
        let rel_row = usize::from(row.saturating_sub(inner_y));
        let n = self.local_queue.len();
        let start = self
            .queue_selected
            .saturating_sub(3)
            .min(n.saturating_sub(4));
        let shown = n - start;
        if shown == 0 || rel_row >= shown {
            return None;
        }
        let abs_idx = start + rel_row;
        self.queue_selected = abs_idx;
        self.queue_focused = true;
        self.mark_dirty();
        let _ = inner_w;
        let item = &self.local_queue.get(abs_idx)?.id;
        let double = self
            .list_clicks
            .click(picker::ClickList::Queue, item, Instant::now());
        // A double-click on a row sends it now while busy, as `Ctrl+S` on
        // the focused queue does.
        double
            .then(|| self.queue_activate_selected(self.busy))
            .flatten()
    }
}
