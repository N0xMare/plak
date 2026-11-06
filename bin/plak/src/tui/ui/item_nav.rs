//! Keyboard focus among transcript items (spec 02 §5.2).
//!
//! The marker is the pane's focused block. It is active only while
//! [`TranscriptPane::item_nav`] is set, so a mouse focus does not take `Esc`
//! or `Space` away from the composer.

use super::{App, ContentPosition, KeyCode, KeyModifiers, ScrollBlock};
use crate::tui::blocks::LineLayout;

/// Whether a key belonged to transcript-item navigation.
pub(super) enum ItemKey {
    /// Not one of this mode's chords.
    Pass,
    /// Consumed. `Some` is a host action; navigation itself produces none.
    Done(Option<crate::tui::UserAction>),
}

impl App {
    /// Transcript-item chords.
    ///
    /// `Ctrl` and Super never match, so the §5.3 ladder is untouched.
    pub(super) fn handle_item_nav_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> ItemKey {
        let alt = modifiers.contains(KeyModifiers::ALT);
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        let super_key = modifiers.contains(KeyModifiers::SUPER);
        if ctrl || super_key {
            return ItemKey::Pass;
        }
        if alt && matches!(code, KeyCode::Char('c' | 'C')) {
            if self.pane.item_nav && !self.slash_open && self.history_browse.is_none() {
                self.copy_focused_item();
            }
            return ItemKey::Done(None);
        }
        if self.slash_open || self.history_browse.is_some() {
            return ItemKey::Pass;
        }
        if alt && matches!(code, KeyCode::Up | KeyCode::Down) {
            if self.queue_focused {
                self.queue_focused = false;
            }
            self.move_item_focus(if code == KeyCode::Up {
                NavStep::Previous
            } else {
                NavStep::Next
            });
            return ItemKey::Done(None);
        }
        if !self.pane.item_nav || self.queue_focused {
            return ItemKey::Pass;
        }
        match code {
            // Shifted arrows stay with the queue. While the marker is active
            // and the queue is not, they neither move it nor fall through to
            // history or the caret.
            KeyCode::Up | KeyCode::Down if shift => ItemKey::Done(None),
            KeyCode::Up | KeyCode::Down => {
                self.move_item_focus(if code == KeyCode::Up {
                    NavStep::Previous
                } else {
                    NavStep::Next
                });
                ItemKey::Done(None)
            }
            KeyCode::Enter | KeyCode::Char(' ') if !alt && !shift => {
                self.toggle_focused_nav_item();
                ItemKey::Done(None)
            }
            _ => ItemKey::Pass,
        }
    }

    /// Printable editing returns to the composer. Navigation chords do not.
    pub(super) fn composer_editing_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) || modifiers.contains(KeyModifiers::SUPER) {
            return false;
        }
        let alt = modifiers.contains(KeyModifiers::ALT);
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        match code {
            KeyCode::Char(_)
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End => true,
            KeyCode::Enter if alt || shift => true,
            _ => false,
        }
    }

    pub(super) fn leave_item_nav(&mut self) {
        if !self.pane.item_nav {
            return;
        }
        self.pane.item_nav = false;
        self.pane.reveal_nav = false;
        self.pane.focused_block = None;
        self.mark_dirty();
    }

    /// Move to the previous or next drawn item.
    ///
    /// From the composer, previous is the last item and next is the first.
    /// The ends clamp. An empty transcript does not enter navigation.
    pub(super) fn move_item_focus(&mut self, step: NavStep) {
        let items = self.navigable_items();
        let Some(next) = next_nav_index(&items, self.pane.item_nav, self.pane.focused_block, step)
        else {
            return;
        };
        self.pane.item_nav = true;
        self.pane.focused_block = Some(next);
        self.pane.reveal_nav = true;
        self.mark_dirty();
    }

    fn navigable_items(&self) -> Vec<usize> {
        let plans = crate::tui::blocks::plan_rows(&self.pane.blocks, self.pane_prefs());
        self.pane
            .blocks
            .iter()
            .enumerate()
            .filter(|(index, block)| {
                !matches!(block, ScrollBlock::Header(_))
                    && plans.get(*index).is_some_and(|plan| !plan.hidden)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn toggle_focused_nav_item(&mut self) {
        let Some(index) = self.pane.focused_block else {
            return;
        };
        if self
            .pane
            .blocks
            .get(index)
            .is_some_and(ScrollBlock::is_foldable)
        {
            self.toggle_tool_expand_at(index);
        }
    }

    /// Copy the focused item with the same text a drag across each of its
    /// rows produces. `None` when navigation is off or the item draws nothing.
    pub(super) fn copy_focused_item(&mut self) -> Option<String> {
        if !self.pane.item_nav {
            return None;
        }
        let block = self.pane.focused_block?;
        let width = self.last_transcript_width;
        if width == 0 {
            return None;
        }
        self.warm_transcript_projection();
        self.pane
            .transcript_cache
            .materialize_all(&self.pane.blocks);
        let revision = self.pane.transcript_revision;
        let (first, last) = item_row_span(self.pane.transcript_cache.layout(), block)?;
        self.pane.selection.begin(
            ContentPosition {
                row: first,
                column: 0,
            },
            width,
            revision,
        );
        self.pane.selection.drag(ContentPosition {
            row: last,
            column: width.saturating_sub(1),
        });
        self.pane
            .selection
            .capture_blocks(self.pane.transcript_cache.layout());
        let text = self.take_transcript_selection_text();
        // A leftover selection would make the next Ctrl+C copy instead of
        // walking the §5.3 ladder.
        self.pane.selection.clear();
        let text = text?;
        super::terminal::copy_text_osc52(&text);
        #[cfg(test)]
        {
            self.last_nav_copy = Some(text.clone());
        }
        Some(text)
    }

    /// Remember a width change so the next paint keeps the marker on screen.
    pub(super) fn note_nav_width(&mut self, width: usize) {
        if self.pane.item_nav && self.pane.nav_width != width {
            self.pane.reveal_nav = true;
            self.pane.nav_width = width;
        }
    }

    /// Document `vstart` that puts the focused item on screen, when a move
    /// or a resize asked for it. Other paints leave the camera where it is.
    pub(super) fn revealed_vstart(
        &mut self,
        vstart: usize,
        banner_h: usize,
        body_h: usize,
        vtotal: usize,
        total_messages: usize,
    ) -> usize {
        if !self.pane.reveal_nav || !self.pane.item_nav {
            return vstart;
        }
        let Some(block) = self.pane.focused_block else {
            self.pane.reveal_nav = false;
            return vstart;
        };
        let Some((first, last)) = item_row_span(self.pane.transcript_cache.layout(), block) else {
            return vstart;
        };
        self.pane.reveal_nav = false;
        let vend = vstart.saturating_add(body_h).min(vtotal);
        let vstart = vend.saturating_sub(body_h);
        let start = vstart.saturating_sub(banner_h);
        let end = vend.saturating_sub(banner_h).min(total_messages);
        if first < end && last.saturating_add(1) > start {
            return vstart;
        }
        let window = end.saturating_sub(start).max(1);
        let new_start = if last < start {
            first
        } else {
            last.saturating_add(1).saturating_sub(window)
        };
        new_start
            .saturating_add(banner_h)
            .min(vtotal.saturating_sub(body_h))
    }
}

/// One step along the drawn items.
#[derive(Clone, Copy)]
pub(super) enum NavStep {
    Previous,
    Next,
}

/// The block to focus, or `None` when the move clamps or nothing is drawn.
fn next_nav_index(
    items: &[usize],
    navigating: bool,
    focused: Option<usize>,
    step: NavStep,
) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let edge = match step {
        NavStep::Previous => items[items.len() - 1],
        NavStep::Next => items[0],
    };
    if !navigating {
        return Some(edge);
    }
    let Some(current) = focused else {
        return Some(edge);
    };
    let Some(at) = items.iter().position(|index| *index == current) else {
        return Some(edge);
    };
    match step {
        NavStep::Previous => at.checked_sub(1).map(|next| items[next]),
        NavStep::Next => items.get(at + 1).copied(),
    }
}

/// Inclusive plain rows that belong to `block`, excluding the blank separator.
pub(super) fn item_row_span(layout: &LineLayout, block: usize) -> Option<(usize, usize)> {
    let mut rows = layout
        .block_index
        .iter()
        .enumerate()
        .filter_map(|(row, owner)| (*owner == Some(block)).then_some(row));
    let first = rows.next()?;
    Some((first, rows.next_back().unwrap_or(first)))
}
