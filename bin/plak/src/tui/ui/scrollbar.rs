//! One-column scroll geometry for the transcript on screen.

use super::{App, Frame, MouseEvent, MouseEventKind, Rect, event};
use crate::tui::theme::Theme;
use ratatui::style::Style;

#[derive(Clone, Copy, Debug)]
pub(super) struct Scrollbar {
    pub area: Rect,
    pub total: usize,
    pub viewport: usize,
    pub offset: usize,
}

impl Scrollbar {
    pub fn split(area: Rect) -> (Rect, Rect) {
        let content = Rect {
            width: area.width.saturating_sub(1),
            ..area
        };
        let track = Rect {
            x: content.right(),
            width: u16::from(area.width > 0),
            ..area
        };
        (content, track)
    }

    fn max_offset(self) -> usize {
        self.total.saturating_sub(self.viewport)
    }

    fn thumb(self) -> (usize, usize) {
        let height = usize::from(self.area.height);
        if height == 0 {
            return (0, 0);
        }
        let size = height
            .saturating_mul(self.viewport)
            .checked_div(self.total)
            .unwrap_or(height)
            .clamp(1, height.max(1));
        let top = (height - size)
            .saturating_mul(self.offset.min(self.max_offset()))
            .checked_div(self.max_offset())
            .unwrap_or(0);
        (top, size)
    }

    pub fn paint(self, frame: &mut Frame<'_>, theme: &Theme) {
        let (top, size) = self.thumb();
        for y in self.area.y..self.area.bottom() {
            if self.area.width == 0 {
                break;
            }
            let row = usize::from(y - self.area.y);
            let color = if (top..top + size).contains(&row) {
                theme.colors.scrollbar_thumb
            } else {
                theme.colors.scrollbar_track
            };
            frame.buffer_mut()[(self.area.x, y)]
                .set_symbol(" ")
                .set_style(Style::default().bg(color.terminal()));
        }
    }

    fn dragged_offset(self, row: u16, grab: usize) -> usize {
        let (_, size) = self.thumb();
        let travel = usize::from(self.area.height).saturating_sub(size);
        let top = usize::from(row.saturating_sub(self.area.y))
            .saturating_sub(grab)
            .min(travel);
        top.saturating_mul(self.max_offset())
            .checked_div(travel)
            .unwrap_or(0)
    }
}

impl App {
    pub(super) fn handle_scrollbar_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.quit_arm.is_some() {
            self.scrollbar_drag = None;
            return false;
        }
        let active = if self.transcript_track_is_live() {
            self.transcript_scrollbar
        } else {
            None
        };
        let Some(bar) = active else {
            self.scrollbar_drag = None;
            return false;
        };
        let left = event::MouseButton::Left;
        if mouse.kind == MouseEventKind::Up(left) && self.scrollbar_drag.take().is_some() {
            return true;
        }
        if mouse.kind == MouseEventKind::Drag(left)
            && let Some(grab) = self.scrollbar_drag
        {
            self.set_scrollbar_offset(bar.dragged_offset(mouse.row, grab));
            return true;
        }
        if !bar.area.contains((mouse.column, mouse.row).into()) {
            return false;
        }
        if mouse.kind == MouseEventKind::Down(left) {
            let row = usize::from(mouse.row - bar.area.y);
            let (top, size) = bar.thumb();
            if (top..top + size).contains(&row) {
                self.scrollbar_drag = Some(row - top);
            } else {
                let offset = if row < top {
                    bar.offset.saturating_sub(bar.viewport)
                } else {
                    bar.offset
                        .saturating_add(bar.viewport)
                        .min(bar.max_offset())
                };
                self.set_scrollbar_offset(offset);
            }
            return true;
        }
        false
    }

    /// The root transcript's track, and the agent transcript's track while it
    /// is the Activity > Agents body. A confirmation, Menu, overlay, resume
    /// picker or slash palette owns the pointer instead (spec 02 §5.4).
    fn transcript_track_is_live(&self) -> bool {
        if self.slash_open
            || self.destructive_confirm.is_some()
            || self.info_overlay.is_some()
            || self.resume_menu.is_some()
            || self.control_tab.is_some()
        {
            return false;
        }
        if self.activity_tab == Some(super::processes::ActivityTab::Agents)
            && self.viewed_agent_id().is_some()
        {
            return true;
        }
        self.activity_tab.is_none()
    }

    /// Drops transcript hit rectangles when this frame did not paint the body,
    /// so a wheel or track click cannot reach a body that is not on screen.
    pub(super) fn clear_transcript_hit_rects(&mut self) {
        self.transcript_area = None;
        self.transcript_body_area = None;
        self.transcript_scrollbar = None;
        self.scrollbar_drag = None;
    }

    fn set_scrollbar_offset(&mut self, offset: usize) {
        if let Some(bar) = self.transcript_scrollbar {
            self.pane.scroll_from_bottom = bar.max_offset().saturating_sub(offset);
        }
        self.pane.transcript_view_anchor = None;
        self.mouse_scroll_queue = 0;
        self.mark_dirty();
    }
}
