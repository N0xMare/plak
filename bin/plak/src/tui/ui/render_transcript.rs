//! Scrollback and slash overlay rendering.

use super::{
    Alignment, App, Block, BorderType, Borders, Clear, Color, ContentPosition, Frame, Line,
    PROMPT_PREFIX, Paragraph, Rect, ScrollBlock, Span, Style, TranscriptChange, UnicodeWidthStr,
    blocks, layout_anchor, sticky_user_lines,
};

/// A block's head row, its click targets and its spinner cells.
type BlockRows = (usize, usize, Vec<blocks::RowTarget>, Vec<(usize, usize)>);

/// The focused block's first visible row takes one subtle highlight,
/// whatever the row is; hover paints over it.
fn paint_focus_row(
    frame: &mut Frame<'_>,
    area: Rect,
    start: usize,
    end: usize,
    layout: &blocks::LineLayout,
    focused: Option<usize>,
    style: Style,
) {
    let Some(idx) = focused else {
        return;
    };
    let Some(line_i) = (start..end).find(|row| {
        layout.block_index.get(*row) == Some(&Some(idx))
            && layout
                .plain
                .get(*row)
                .is_some_and(|line| !line.trim().is_empty())
    }) else {
        return;
    };
    let y = area
        .y
        .saturating_add(u16::try_from(line_i - start).unwrap_or(u16::MAX));
    if y < area.bottom() {
        frame
            .buffer_mut()
            .set_style(Rect::new(area.x, y, area.width, 1), style);
    }
}

pub(super) fn paint_transcript_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: &[Line<'static>],
    start: usize,
    end: usize,
    background: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = frame.buffer_mut();
    let fill = Style::default().bg(background);
    let height = usize::from(area.height);
    for row in 0..height {
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        for x in area.left()..area.right() {
            buf[(x, y)].reset();
            buf[(x, y)].set_style(fill);
        }
        let src = start.saturating_add(row);
        if src < end && src < lines.len() {
            buf.set_line(area.x, y, &lines[src], area.width);
        }
    }
}

impl App {
    #[allow(
        clippy::too_many_lines,
        reason = "viewport, sticky, hit rows, selection, and position share exact row geometry"
    )]
    pub(super) fn draw_scrollback(&mut self, frame: &mut Frame<'_>, area: Rect) {
        const STICKY_MAX: usize = 3;

        self.transcript_area = Some(area);
        let (area, track) = super::scrollbar::Scrollbar::split(area);
        self.sticky_prompt_jump = None;
        self.tool_hit_rows.clear();
        let content_w = usize::from(area.width.max(1));
        self.last_transcript_width = content_w;
        let change = std::mem::take(&mut self.pane.pending_transcript_change);
        if change != TranscriptChange::Unchanged {
            self.pane.transcript_revision = self.pane.transcript_revision.wrapping_add(1);
        }
        let follow_live = self.pane.scroll_from_bottom == 0;
        let toward_tail = toward_tail_delta(
            self.pane.scroll_from_bottom_at_last_paint,
            self.pane.scroll_from_bottom,
        );
        let held_anchor = (!follow_live)
            .then_some(self.pane.transcript_view_anchor)
            .flatten();
        let stable_prefix_rows = self.pane.transcript_cache.stable_prefix_rows(change);
        // Selection carries content rows from one frame to the next and copy
        // reads the whole plain projection, so both need every block measured;
        // nothing else does. Declared before the projection is asked for, so a
        // rebuild this frame measures everything in the one pass.
        let selecting = self.selection.has_range();
        self.pane.transcript_cache.require_exact(selecting);
        let prefs = self.pane_prefs();
        let _ = self.pane.transcript_cache.get(
            self.pane.transcript_revision,
            &self.pane.blocks,
            &self.theme,
            content_w,
            change,
            prefs,
        );
        if selecting {
            self.pane
                .transcript_cache
                .materialize_all(&self.pane.blocks);
        }

        // The session banner is a prefix of the document, not a sticky
        // header: follow-live and scroll offsets count it in the total, so it
        // moves off once the transcript fills the pane.
        let banner_h = self.session_banner_rows(area);
        // Sticky prompt: when scrolled up, pin the user message above the viewport
        // into a 2–3 line bar at the top of the transcript region. Sticky
        // height feeds back into the body height, so it settles in two rounds,
        // and the estimated region is measured inside the same loop — the
        // window the operator reads has to be exact before anything reads it.
        let height = usize::from(area.height.max(1));
        let mut sticky_h = 0;
        let mut sticky = None;
        let mut start;
        let mut vstart = 0;
        for _ in 0..2 {
            let body_h = height.saturating_sub(sticky_h).max(1);
            vstart = self.viewport_start(follow_live, held_anchor, toward_tail, banner_h, body_h);
            let message_rows = self.pane.transcript_cache.layout().plain.len();
            let total = message_rows.saturating_add(banner_h);
            let vend = vstart.saturating_add(body_h).min(total);
            vstart = vend.saturating_sub(body_h);
            start = vstart.saturating_sub(banner_h);
            sticky = sticky_user_lines(
                &self.pane.blocks,
                self.pane.transcript_cache.layout(),
                start,
                STICKY_MAX,
                content_w,
            );
            sticky_h = sticky
                .as_ref()
                .map_or(0, |preview| preview.lines.len().min(STICKY_MAX));
        }

        let body_h = height.saturating_sub(sticky_h).max(1);
        let total_messages = self.pane.transcript_cache.layout().plain.len();
        let vtotal = total_messages.saturating_add(banner_h);
        let vend = vstart.saturating_add(body_h).min(vtotal);
        vstart = vend.saturating_sub(body_h);
        let start = vstart.saturating_sub(banner_h);
        let end = vend.saturating_sub(banner_h).min(total_messages);
        let from_bottom = vtotal.saturating_sub(vend);
        let banner_skip = vstart.min(banner_h);
        let banner_visible = banner_h.saturating_sub(banner_skip);
        self.pane.scroll_from_bottom = from_bottom;
        self.pane.scroll_from_bottom_at_last_paint = from_bottom;
        self.pane.transcript_view_anchor = (from_bottom > 0)
            .then(|| layout_anchor(self.pane.transcript_cache.layout(), start))
            .flatten();
        self.pane.transcript_anchor_banner_rows = banner_skip;
        // Intent the camera could not spend is not banked against the
        // reversal: a paint that clamped at an end is the authority on that
        // direction being exhausted.
        self.discard_clamped_scroll_queue(vstart == 0, from_bottom == 0);
        // Hit-test only a visible summary. If scrolling clips the summary and
        // begins on an expanded detail rail, neither mouse nor spinner may
        // reinterpret that detail row as the lifecycle row.
        let sticky_u16 = u16::try_from(sticky_h).unwrap_or(0);
        let banner_u16 = u16::try_from(banner_visible).unwrap_or(0);
        let body_origin_y = area.y.saturating_add(sticky_u16).saturating_add(banner_u16);
        let body_area = Rect {
            x: area.x,
            y: body_origin_y,
            width: area.width,
            height: area
                .height
                .saturating_sub(sticky_u16)
                .saturating_sub(banner_u16),
        };
        self.transcript_body_area = Some(body_area);
        self.pane.transcript_visible_start = start;
        self.pane.transcript_total_rows = vtotal;
        self.selection
            .resolve_blocks(self.pane.transcript_cache.layout());
        self.selection.reconcile_projection(
            self.pane.transcript_revision,
            content_w,
            total_messages,
            stable_prefix_rows,
        );
        if let Some((column, row)) = self.transcript_drag_pointer
            && !body_area.is_empty()
            && total_messages > 0
        {
            let screen_row = row.clamp(body_area.y, body_area.bottom().saturating_sub(1));
            let screen_column = column.clamp(body_area.x, body_area.right().saturating_sub(1));
            let position = ContentPosition {
                row: start
                    .saturating_add(usize::from(screen_row.saturating_sub(body_area.y)))
                    .min(total_messages.saturating_sub(1)),
                column: usize::from(screen_column.saturating_sub(body_area.x)),
            };
            self.selection.drag(position);
            self.selection
                .capture_blocks(self.pane.transcript_cache.layout());
        }
        if banner_visible > 0 {
            let banner_area = Rect {
                x: area.x,
                y: area.y.saturating_add(sticky_u16),
                width: area.width,
                height: banner_u16.min(area.height.saturating_sub(sticky_u16)),
            };
            self.draw_launch_intro(frame, banner_area, banner_skip);
        }
        // Byte-bounded projection: rows the operator is about to see may have
        // been evicted, so re-project them before anything reads them, and let
        // the cache re-apply its ceiling around the window now known.
        self.pane
            .transcript_cache
            .ensure_rows(&self.pane.blocks, start, end);
        let render = self.pane.transcript_cache.render();
        let layout = &render.layout;
        let all_lines = &render.lines;

        // Clickable rows (spec 02 §5.4): a block's leading summary, merged,
        // item, thinking or milestone rows, and an open item's hidden-lines
        // row. Targets come from the plan alone, so this stays cheap.
        let plans = self.pane.transcript_cache.row_plans();
        let mut hits = Vec::new();
        let mut spinners = Vec::new();
        // One block's head row, click targets and spinner cells.
        let mut block_targets: Option<BlockRows> = None;
        for (vis_i, line_i) in (start..end).enumerate() {
            let Some(Some(bi)) = layout.block_index.get(line_i).copied() else {
                continue;
            };
            if block_targets
                .as_ref()
                .is_none_or(|(block, ..)| *block != bi)
            {
                let head = layout.block_index[..line_i]
                    .iter()
                    .rposition(|index| *index != Some(bi))
                    .map_or(0, |row| row + 1);
                let spinner_cells = match (self.pane.blocks.get(bi), plans.get(bi)) {
                    (Some(block), Some(plan)) => blocks::spinner_cells(block, plan, prefs),
                    _ => Vec::new(),
                };
                block_targets =
                    Some((bi, head, self.row_targets(bi, plans.get(bi)), spinner_cells));
            }
            let Some((_, head, targets, spinner_cells)) = block_targets.as_ref() else {
                continue;
            };
            let screen_row = body_origin_y.saturating_add(u16::try_from(vis_i).unwrap_or(0));
            let target = targets
                .get(line_i - head)
                .copied()
                .or_else(|| {
                    blocks::cell_footer_target(
                        &all_lines[*head..],
                        line_i - head,
                        plans.get(bi)?,
                        &self.pane.blocks,
                    )
                })
                .or_else(|| {
                    all_lines
                        .get(line_i)
                        .filter(|line| blocks::is_hidden_lines_row(line))
                        .map(|_| blocks::RowTarget::HiddenLines)
                });
            if let Some(target) = target {
                hits.push((bi, screen_row, target));
            }
            for (_, column) in spinner_cells
                .iter()
                .filter(|(row, _)| *row == line_i - head)
            {
                spinners.push((line_i, screen_row, *column, bi));
            }
        }
        self.tool_hit_targets = hits.iter().map(|(_, _, target)| *target).collect();
        self.tool_hit_rows = hits.into_iter().map(|(bi, y, _)| (bi, y)).collect();
        // The open approval's `[allow]` and `[deny]` chips (spec 02 §5.2),
        // at the columns its header row paints them.
        let painted_at = self.now();
        let previous = std::mem::take(&mut self.approval_chip_hits);
        if let Some(open) = self.open_approval.as_deref() {
            for (vis_i, line_i) in (start..end).enumerate() {
                let Some(Some(bi)) = layout.block_index.get(line_i).copied() else {
                    continue;
                };
                let Some(ScrollBlock::Approval {
                    approval_id,
                    tool_name,
                    subject,
                    resolution: None,
                    ..
                }) = self.pane.blocks.get(bi)
                else {
                    continue;
                };
                if approval_id != open {
                    continue;
                }
                let first = (0..line_i)
                    .rev()
                    .take_while(|i| layout.block_index.get(*i).copied().flatten() == Some(bi))
                    .last()
                    .unwrap_or(line_i);
                let y = body_origin_y.saturating_add(u16::try_from(vis_i).unwrap_or(0));
                for (row, column, label) in
                    blocks::approval_chip_cells(tool_name, subject.as_deref(), content_w)
                {
                    if row != line_i - first {
                        continue;
                    }
                    let choice = if label == blocks::APPROVAL_ALLOW_CHIP {
                        crate::tui::ApprovalChoice::Allow
                    } else {
                        crate::tui::ApprovalChoice::Deny
                    };
                    let x = body_area
                        .x
                        .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
                    let width = u16::try_from(label.width()).unwrap_or(u16::MAX);
                    let rect = Rect::new(x, y, width, 1).intersection(body_area);
                    if !rect.is_empty() {
                        let since = previous
                            .iter()
                            .find(|hit| {
                                hit.approval_id == *approval_id
                                    && hit.rect == rect
                                    && hit.choice == choice
                            })
                            .map_or(painted_at, |hit| hit.since);
                        self.approval_chip_hits.push(super::ApprovalChipHit {
                            approval_id: approval_id.clone(),
                            rect,
                            choice,
                            since,
                        });
                    }
                }
            }
        }

        if let Some(sticky_preview) = sticky {
            let sticky_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: u16::try_from(sticky_h).unwrap_or(2).min(area.height),
            };
            let target_end = sticky_preview
                .source_start
                .saturating_add(banner_h)
                .saturating_add(body_h)
                .min(vtotal);
            let target_from_bottom = vtotal.saturating_sub(target_end);
            self.sticky_prompt_jump = Some((sticky_area, target_from_bottom));
            // Sticky prompt: warm theme-owned wash + `›` (U+203A).
            let pinned_bg = self.theme.sticky_user_bg;
            let sticky_styled: Vec<Line<'static>> = sticky_preview
                .lines
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    let body = l
                        .strip_prefix("┃ ")
                        .or_else(|| l.strip_prefix(PROMPT_PREFIX))
                        .or_else(|| l.strip_prefix("> "))
                        .unwrap_or(l.as_str());
                    let glyph = if i == 0 { PROMPT_PREFIX } else { "  " };
                    Line::from(vec![
                        Span::styled(glyph, self.theme.composer_prompt.bg(pinned_bg)),
                        Span::styled(body.to_owned(), self.theme.sticky_user.bg(pinned_bg)),
                    ])
                    .style(Style::default().bg(pinned_bg))
                })
                .collect();
            // Fill full sticky rect so it reads as a translucent bar.
            frame.render_widget(
                Block::default().style(Style::default().bg(pinned_bg)),
                sticky_area,
            );
            frame.render_widget(Paragraph::new(sticky_styled), sticky_area);
        }
        paint_transcript_lines(frame, body_area, all_lines, start, end, self.theme.app_bg);
        paint_focus_row(
            frame,
            body_area,
            start,
            end,
            layout,
            self.pane.focused_block,
            self.theme.row_focus,
        );
        // Motion is paint-only: it must not invalidate selection's text
        // projection. Spinner cells come from block state and plans.
        for (line_i, y, column, block) in spinners {
            let Some(plain) = layout.plain.get(line_i) else {
                continue;
            };
            let x = body_area
                .x
                .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
            if x < body_area.right() && y < body_area.bottom() {
                if frame.buffer_mut()[(x, y)].symbol() == "⠼" {
                    frame.buffer_mut()[(x, y)].set_symbol(self.status_spinner());
                }
                self.paint_tip_elapsed(frame, body_area, y, plain, Some(block));
            }
        }
        // Hover highlights the one clickable row under the pointer.
        for (_, y) in &self.tool_hit_rows {
            let rect = Rect::new(body_area.x, *y, body_area.width, 1);
            if self.chrome_hovered(rect) {
                frame.buffer_mut().set_style(rect, self.theme.row_hover);
            }
        }
        // Highlight only the real body. Copy uses the complete cached plain
        // projection.
        self.selection.render(
            frame.buffer_mut(),
            body_area,
            start,
            &layout.plain,
            self.theme.transcript_selection,
        );
        let bar = super::scrollbar::Scrollbar {
            area: track,
            total: vtotal,
            viewport: body_h,
            offset: vstart,
        };
        bar.paint(frame, &self.theme);
        self.transcript_scrollbar = Some(bar);
    }

    /// Place the viewport in document space for this paint.
    ///
    /// Follow-live pins to the tail. Unfollowed always remaps the last
    /// block-space anchor first, then applies this frame's wheel/key delta in
    /// row space, so a growing stream cannot fight a down-scroll.
    fn viewport_start(
        &mut self,
        follow_live: bool,
        held_anchor: Option<(usize, usize)>,
        toward_tail: i64,
        banner_h: usize,
        body_h: usize,
    ) -> usize {
        let message_rows = self.pane.transcript_cache.layout().plain.len();
        let total = message_rows.saturating_add(banner_h);
        if follow_live {
            let vstart = total.saturating_sub(body_h);
            let _ = self.pane.transcript_cache.settle_window(
                &self.pane.blocks,
                layout_anchor(
                    self.pane.transcript_cache.layout(),
                    vstart.saturating_sub(banner_h),
                ),
                body_h,
            );
            return vstart;
        }
        if let Some(held) = held_anchor {
            let msg_start =
                self.pane
                    .transcript_cache
                    .settle_window(&self.pane.blocks, Some(held), body_h);
            // The banner is a fixed-height prefix, so it needs no remap — but
            // it does need counting. A viewport that starts inside it has no
            // message row of its own: the anchor saturates to row 0 however
            // many banner rows are scrolled off, so document space is
            // recovered from the banner rows held with the anchor rather than
            // from the anchor alone. Below the banner that held count is the
            // whole banner, which is the plain `msg_start + banner_h` case. A
            // banner armed while the camera sat on message row 0 has no held
            // rows, so it opens with the camera at its top: it is the row the
            // operator was already reading, with the new prefix above it.
            let banner_above = if msg_start == 0 {
                self.pane.transcript_anchor_banner_rows.min(banner_h)
            } else {
                banner_h
            };
            let remapped = msg_start.saturating_add(banner_above);
            let vstart = apply_toward_tail(remapped, toward_tail, total, body_h);
            let _ = self.pane.transcript_cache.settle_window(
                &self.pane.blocks,
                layout_anchor(
                    self.pane.transcript_cache.layout(),
                    vstart.saturating_sub(banner_h),
                ),
                body_h,
            );
            return vstart;
        }
        // First unfollow from live: no stored anchor, so the requested
        // from-bottom offset is the camera.
        let from_bottom = self
            .pane
            .scroll_from_bottom
            .min(total.saturating_sub(body_h));
        let vstart = total.saturating_sub(from_bottom).saturating_sub(body_h);
        let _ = self.pane.transcript_cache.settle_window(
            &self.pane.blocks,
            layout_anchor(
                self.pane.transcript_cache.layout(),
                vstart.saturating_sub(banner_h),
            ),
            body_h,
        );
        vstart
    }

    pub(super) fn draw_jump_to_live_spacer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.jump_to_live_hit = None;
        if area.height == 0 || area.width == 0 {
            return;
        }
        if self.pane.scroll_from_bottom == 0 {
            return;
        }
        self.jump_to_live_hit = Some(area);
        frame.render_widget(
            Paragraph::new(Span::styled("↓", self.theme.chrome_dim)).alignment(Alignment::Center),
            area,
        );
    }

    pub(super) fn draw_slash_overlay(&mut self, frame: &mut Frame<'_>, viewport: Rect) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            self.slash_popup = None;
            self.slash_view_start = 0;
            return;
        }
        let max_rows = 10.min(matches.len());
        let desired_height = u16::try_from(max_rows)
            .unwrap_or(10)
            .saturating_add(2)
            .min(viewport.height.saturating_sub(2))
            .max(4);
        let composer = self.composer_inner.map_or(
            Rect {
                x: viewport.x,
                y: viewport.bottom().saturating_sub(3),
                width: viewport.width,
                height: 3,
            },
            |inner| Rect {
                x: inner.x.saturating_sub(1),
                y: inner.y.saturating_sub(1),
                width: inner.width.saturating_add(2),
                height: inner.height.saturating_add(2),
            },
        );
        let available = composer.y.saturating_sub(viewport.y);
        let height = desired_height.min(available);
        if height < 3 {
            self.slash_popup = None;
            self.slash_view_start = 0;
            return;
        }
        let area = Rect {
            x: composer.x,
            y: composer.y.saturating_sub(height),
            width: composer.width,
            height,
        };
        self.slash_popup = Some(area);
        let visible_rows = usize::from(area.height.saturating_sub(2));
        let max_start = matches.len().saturating_sub(visible_rows);
        self.slash_view_start = self
            .slash_selected
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(max_start);
        frame.render_widget(Clear, area);
        let inner_w = usize::from(area.width.saturating_sub(3));
        let mut lines = Vec::new();
        for (offset, (name, help)) in matches
            .iter()
            .skip(self.slash_view_start)
            .take(visible_rows)
            .enumerate()
        {
            let selected = self.slash_view_start + offset == self.slash_selected;
            let mark = if selected { "❯" } else { " " };
            let cmd = format!("{mark}/{name}");
            // Pad command column so descriptions align.
            let cmd_col = format!("{cmd:<16}");
            let rest = inner_w.saturating_sub(cmd_col.chars().count());
            let help_shown: String = help.chars().take(rest.saturating_sub(1)).collect();
            let pad = rest.saturating_sub(help_shown.chars().count());
            let (name_style, help_style) = if selected {
                (self.theme.slash_selected, self.theme.slash_selected_help)
            } else {
                (self.theme.slash_item, self.theme.slash_item_help)
            };
            // Full-width background via trailing spaces on the help span.
            let help_padded = format!("{help_shown}{}", " ".repeat(pad));
            lines.push(Line::from(vec![
                Span::styled(cmd_col, name_style),
                Span::styled(help_padded, help_style),
            ]));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.menu_border)
            .title(" commands ")
            .title_alignment(Alignment::Center)
            .title_style(self.theme.menu_title)
            .style(Style::default().bg(self.theme.menu_bg));
        let content = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(self.theme.menu_bg)),
            content,
        );
    }

    /// A live tip's elapsed, painted into the blank meta cells before its
    /// `▸` like the spinner: paint-only, so the row plan and layout are never
    /// rebuilt for a clock. A subject too long to leave room shows none.
    fn paint_tip_elapsed(
        &self,
        frame: &mut Frame<'_>,
        body: Rect,
        y: u16,
        plain: &str,
        block: Option<usize>,
    ) {
        let Some(ScrollBlock::Tool {
            call_id,
            name,
            state,
            ..
        }) = block.and_then(|index| self.pane.blocks.get(index))
        else {
            return;
        };
        if !blocks::tool_is_running(state) {
            return;
        }
        let key = call_id.as_ref().unwrap_or(name);
        let Some(label) = self.pane.tool_started.get(key).and_then(|started| {
            crate::tui::activity::tip_elapsed(self.now().saturating_duration_since(*started))
        }) else {
            return;
        };
        let Some(before_caret) = plain
            .trim_end()
            .strip_suffix(" ▸")
            .or_else(|| plain.trim_end().strip_suffix(" ▾"))
        else {
            return;
        };
        let cells = label.width();
        if before_caret.width() + 1 > usize::from(body.width)
            || !before_caret.ends_with(&" ".repeat(cells + 1))
        {
            return;
        }
        let end = body
            .x
            .saturating_add(u16::try_from(before_caret.width()).unwrap_or(u16::MAX));
        let x = end.saturating_sub(u16::try_from(cells).unwrap_or(u16::MAX));
        if end <= body.right() {
            frame
                .buffer_mut()
                .set_string(x, y, label, self.theme.tool_label);
        }
    }
}

/// Rows toward the tail implied by a change in `scroll_from_bottom`.
///
/// A decrease is a down-scroll (positive). `from_bottom` can be `usize::MAX`
/// in tests, so the subtraction saturates in `i64`.
fn toward_tail_delta(last_from_bottom: usize, requested: usize) -> i64 {
    let last = i64::try_from(last_from_bottom).unwrap_or(i64::MAX);
    let requested = i64::try_from(requested).unwrap_or(i64::MAX);
    last.saturating_sub(requested)
}

/// Move `vstart` toward the tail by `toward_tail` rows, clamped to the pane.
fn apply_toward_tail(vstart: usize, toward_tail: i64, total: usize, body_h: usize) -> usize {
    let max_start = total.saturating_sub(body_h);
    if toward_tail >= 0 {
        vstart
            .saturating_add(usize::try_from(toward_tail).unwrap_or(usize::MAX))
            .min(max_start)
    } else {
        let up = toward_tail.checked_neg().map_or(usize::MAX, |delta| {
            usize::try_from(delta).unwrap_or(usize::MAX)
        });
        vstart.saturating_sub(up).min(max_start)
    }
}

#[cfg(test)]
mod camera_delta_tests {
    use super::{apply_toward_tail, toward_tail_delta};

    #[test]
    fn a_from_bottom_decrease_is_rows_toward_the_tail() {
        assert_eq!(toward_tail_delta(100, 97), 3);
        assert_eq!(toward_tail_delta(100, 103), -3);
        assert_eq!(toward_tail_delta(0, 3), -3);
    }

    #[test]
    fn a_toward_tail_delta_is_clamped_to_the_pane() {
        assert_eq!(apply_toward_tail(10, 3, 100, 20), 13);
        assert_eq!(apply_toward_tail(10, -3, 100, 20), 7);
        assert_eq!(apply_toward_tail(70, 50, 100, 20), 80);
        assert_eq!(apply_toward_tail(2, -10, 100, 20), 0);
    }
}
