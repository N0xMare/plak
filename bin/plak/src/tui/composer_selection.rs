//! Char-indexed selection and editing for the composer draft.
//!
//! This state is deliberately separate from transcript selection: it edits the
//! authoritative draft `String`, while transcript selection snapshots painted
//! terminal cells for copying only.

use std::ops::Range;
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

/// Maximum interval between clicks in one multi-click sequence.
pub(crate) const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Pointer button identity used to qualify a multi-click sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PointerButton {
    Left,
    Middle,
    Right,
}

/// One composer click, already resolved by the view to a draft char index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ComposerClick {
    pub char_index: usize,
    pub button: PointerButton,
    pub column: u16,
    pub row: u16,
    /// Monotonic time since an arbitrary caller-owned epoch.
    pub at: Duration,
}

/// Selection action produced by a click sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClickSelection {
    Caret,
    Word,
    LogicalLine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviousClick {
    button: PointerButton,
    column: u16,
    row: u16,
    at: Duration,
    count: u8,
}

/// Editable composer caret and selection, expressed entirely in char indices.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ComposerSelection {
    anchor: usize,
    head: usize,
    selecting: bool,
    previous_click: Option<PreviousClick>,
}

impl ComposerSelection {
    /// Current caret/selection head as a char index.
    #[must_use]
    pub(crate) fn head(&self) -> usize {
        self.head
    }

    /// Fixed end of the current drag as a char index.
    #[must_use]
    pub(crate) fn anchor(&self) -> usize {
        self.anchor
    }

    /// Place the caret and clear any selected range.
    pub(crate) fn set_caret(&mut self, text: &str, char_index: usize) {
        let index = clamp_index(text, char_index);
        self.anchor = index;
        self.head = index;
        self.selecting = false;
    }

    /// Start a drag at a caller-resolved char index.
    pub(crate) fn begin_drag(&mut self, text: &str, char_index: usize) {
        let index = clamp_index(text, char_index);
        self.anchor = index;
        self.head = index;
        self.selecting = true;
    }

    /// Extend the active selection head, retaining its original anchor.
    pub(crate) fn drag_to(&mut self, text: &str, char_index: usize) {
        if !self.selecting {
            self.anchor = self.head.min(text.chars().count());
            self.selecting = true;
        }
        self.head = clamp_index(text, char_index);
    }

    /// Clear selection without moving the caret.
    pub(crate) fn clear(&mut self) {
        self.anchor = self.head;
        self.selecting = false;
    }

    /// Return the normalized, non-empty selected char range.
    #[must_use]
    pub(crate) fn range(&self, text: &str) -> Option<Range<usize>> {
        if !self.selecting {
            return None;
        }
        let len = text.chars().count();
        let anchor = self.anchor.min(len);
        let head = self.head.min(len);
        (anchor != head).then(|| anchor.min(head)..anchor.max(head))
    }

    /// Selected char range for the renderer.
    #[must_use]
    pub(crate) fn render_range(&self, text: &str) -> Option<Range<usize>> {
        self.range(text)
    }

    /// Apply a click, promoting qualified repeats to word and line selection.
    pub(crate) fn click(&mut self, text: &str, click: ComposerClick) -> ClickSelection {
        let count = self.click_count(click);
        let index = clamp_index(text, click.char_index);
        match count {
            2 => {
                let range = word_range(text, index);
                self.select_range(range, index);
                ClickSelection::Word
            }
            3 => {
                let range = logical_line_range(text, index);
                self.select_range(range, index);
                ClickSelection::LogicalLine
            }
            _ => {
                self.set_caret(text, index);
                ClickSelection::Caret
            }
        }
    }

    /// Delete the selected range, placing the caret at its normalized start.
    pub(crate) fn delete_selection(&mut self, text: &mut String) -> bool {
        let Some(range) = self.range(text) else {
            self.reconcile(text);
            return false;
        };
        replace_char_range(text, range.clone(), "");
        self.collapse(range.start);
        true
    }

    /// Replace a selection, or insert at the caret when no range is selected.
    pub(crate) fn replace_or_insert(&mut self, text: &mut String, replacement: &str) {
        let range = self.range(text).unwrap_or_else(|| {
            self.head.min(text.chars().count())..self.head.min(text.chars().count())
        });
        let start = range.start;
        replace_char_range(text, range, replacement);
        self.collapse(start + replacement.chars().count());
    }

    /// Backspace semantics: delete selection first, otherwise one preceding char.
    pub(crate) fn backspace(&mut self, text: &mut String) -> bool {
        if self.delete_selection(text) {
            return true;
        }
        if self.head == 0 {
            return false;
        }
        let start = previous_grapheme_boundary(text, self.head);
        replace_char_range(text, start..self.head, "");
        self.collapse(start);
        true
    }

    /// Delete-key semantics: delete selection first, otherwise one following char.
    pub(crate) fn delete_forward(&mut self, text: &mut String) -> bool {
        if self.delete_selection(text) {
            return true;
        }
        let len = text.chars().count();
        if self.head >= len {
            return false;
        }
        let start = self.head;
        let end = next_grapheme_boundary(text, start);
        replace_char_range(text, start..end, "");
        self.collapse(start);
        true
    }

    /// Cut semantics: return and delete selected text; do nothing without a range.
    pub(crate) fn cut(&mut self, text: &mut String) -> Option<String> {
        let range = self.range(text)?;
        let selected: String = text
            .chars()
            .skip(range.start)
            .take(range.end - range.start)
            .collect();
        replace_char_range(text, range.clone(), "");
        self.collapse(range.start);
        Some(selected)
    }

    fn select_range(&mut self, range: Range<usize>, fallback: usize) {
        self.anchor = range.start;
        self.head = range.end;
        self.selecting = range.start != range.end;
        if !self.selecting {
            self.anchor = fallback;
            self.head = fallback;
        }
    }

    fn click_count(&mut self, click: ComposerClick) -> u8 {
        let count = self.previous_click.map_or(1, |previous| {
            let elapsed = click.at.checked_sub(previous.at);
            if previous.button == click.button
                && previous.column == click.column
                && previous.row == click.row
                && elapsed.is_some_and(|elapsed| elapsed <= MULTI_CLICK_INTERVAL)
            {
                previous.count % 3 + 1
            } else {
                1
            }
        });
        self.previous_click = Some(PreviousClick {
            button: click.button,
            column: click.column,
            row: click.row,
            at: click.at,
            count,
        });
        count
    }

    fn reconcile(&mut self, text: &str) {
        let len = text.chars().count();
        self.collapse(self.head.min(len));
    }

    fn collapse(&mut self, index: usize) {
        self.anchor = index;
        self.head = index;
        self.selecting = false;
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WordClass {
    Word,
    Whitespace,
    Boundary,
    LineBreak,
}

fn word_class(grapheme: &str) -> WordClass {
    if grapheme.contains('\n') || grapheme.contains('\r') {
        WordClass::LineBreak
    } else if grapheme.chars().all(char::is_whitespace) {
        WordClass::Whitespace
    } else if grapheme
        .chars()
        .next()
        .is_some_and(|character| character.is_alphanumeric() || character == '_')
    {
        WordClass::Word
    } else {
        // Unicode punctuation and symbols are boundaries without requiring a
        // locale-specific word-break dependency.
        WordClass::Boundary
    }
}

fn word_range(text: &str, char_index: usize) -> Range<usize> {
    let mut graphemes = Vec::new();
    let mut offset = 0usize;
    for grapheme in text.graphemes(true) {
        let start = offset;
        offset = offset.saturating_add(grapheme.chars().count());
        graphemes.push((start, offset, grapheme, word_class(grapheme)));
    }
    if graphemes.is_empty() {
        return 0..0;
    }
    let cursor = char_index.min(offset.saturating_sub(1));
    let index = graphemes
        .iter()
        .position(|(start, end, _, _)| cursor >= *start && cursor < *end)
        .unwrap_or(graphemes.len() - 1);
    let class = graphemes[index].3;
    if class == WordClass::LineBreak {
        return graphemes[index].0..graphemes[index].1;
    }
    let mut start = index;
    while start > 0 && graphemes[start - 1].3 == class {
        start -= 1;
    }
    let mut end = index + 1;
    while end < graphemes.len() && graphemes[end].3 == class {
        end += 1;
    }
    graphemes[start].0..graphemes[end - 1].1
}

fn logical_line_range(text: &str, char_index: usize) -> Range<usize> {
    let chars: Vec<char> = text.chars().collect();
    let index = char_index.min(chars.len());
    let start = chars[..index]
        .iter()
        .rposition(|character| *character == '\n')
        .map_or(0, |newline| newline + 1);
    let end = chars[index..]
        .iter()
        .position(|character| *character == '\n')
        .map_or(chars.len(), |newline| index + newline + 1);
    start..end
}

fn clamp_index(text: &str, char_index: usize) -> usize {
    char_index.min(text.chars().count())
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.chars().count());
    text.graphemes(true)
        .scan(0usize, |index, grapheme| {
            let start = *index;
            *index = index.saturating_add(grapheme.chars().count());
            Some(start)
        })
        .take_while(|start| *start < cursor)
        .last()
        .unwrap_or(0)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.chars().count());
    let mut index = 0usize;
    for grapheme in text.graphemes(true) {
        index = index.saturating_add(grapheme.chars().count());
        if index > cursor {
            return index;
        }
    }
    index
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn replace_char_range(text: &mut String, range: Range<usize>, replacement: &str) {
    let bytes = byte_index(text, range.start)..byte_index(text, range.end);
    text.replace_range(bytes, replacement);
}

/// Bounded composer undo history (spec 02 §5.2).
///
/// Steps are whole-draft snapshots: the drafts this bounds are already capped
/// at `MAX_COMPOSER_BYTES`, so a delta encoding would buy nothing but bugs.
/// The ring is capped by both step count and retained bytes and is not durable
/// product data — `/clear` and exact resume drop it.
pub(crate) const UNDO_MAX_STEPS: usize = 128;

/// Retained-byte cap across the whole ring (past plus future).
pub(crate) const UNDO_MAX_BYTES: usize = 512 * 1024;

/// One recorded draft state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ComposerSnapshot {
    pub text: String,
    pub cursor: usize,
}

/// How the edit that is about to run should join the undo history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditKind {
    /// A single-grapheme insertion; consecutive ones coalesce into one step.
    Typing,
    /// A kill, paste, history load, or draft clear — always exactly one step.
    Discrete,
}

/// Bounded undo/redo ring for one composer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ComposerUndo {
    past: Vec<ComposerSnapshot>,
    future: Vec<ComposerSnapshot>,
    /// Whether the newest past step was a coalescing typing run still open.
    typing_run: bool,
}

impl ComposerUndo {
    /// Record the pre-edit draft state, unless it coalesces into the open
    /// typing run. Any recorded edit invalidates the redo stack.
    pub(crate) fn record(&mut self, text: &str, cursor: usize, kind: EditKind) {
        self.future.clear();
        if kind == EditKind::Typing && self.typing_run {
            return;
        }
        self.past.push(ComposerSnapshot {
            text: text.to_owned(),
            cursor,
        });
        self.typing_run = kind == EditKind::Typing;
        self.enforce_bounds();
    }

    /// Close an open typing run so the next insertion starts a new step.
    pub(crate) fn break_typing_run(&mut self) {
        self.typing_run = false;
    }

    /// Step back one edit, handing the caller the state to restore.
    pub(crate) fn undo(&mut self, text: &str, cursor: usize) -> Option<ComposerSnapshot> {
        let previous = self.past.pop()?;
        self.future.push(ComposerSnapshot {
            text: text.to_owned(),
            cursor,
        });
        self.typing_run = false;
        self.enforce_bounds();
        Some(previous)
    }

    /// Step forward one undone edit.
    pub(crate) fn redo(&mut self, text: &str, cursor: usize) -> Option<ComposerSnapshot> {
        let next = self.future.pop()?;
        self.past.push(ComposerSnapshot {
            text: text.to_owned(),
            cursor,
        });
        self.typing_run = false;
        self.enforce_bounds();
        Some(next)
    }

    /// Drop the whole history (`/clear`, exact resume, submit).
    pub(crate) fn reset(&mut self) {
        self.past.clear();
        self.future.clear();
        self.typing_run = false;
    }

    /// Retained payload bytes across both stacks.
    pub(crate) fn retained_bytes(&self) -> usize {
        self.past
            .iter()
            .chain(self.future.iter())
            .map(|snapshot| snapshot.text.len())
            .sum()
    }

    /// Number of undoable steps.
    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.past.len()
    }

    fn enforce_bounds(&mut self) {
        if self.past.len() > UNDO_MAX_STEPS {
            let drop = self.past.len() - UNDO_MAX_STEPS;
            self.past.drain(..drop);
        }
        if self.future.len() > UNDO_MAX_STEPS {
            let drop = self.future.len() - UNDO_MAX_STEPS;
            self.future.drain(..drop);
        }
        // Byte cap: shed the oldest undo steps first, then the oldest redo steps.
        while self.retained_bytes() > UNDO_MAX_BYTES {
            if self.past.len() > 1 {
                self.past.remove(0);
            } else if !self.future.is_empty() {
                self.future.remove(0);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(index: usize, millis: u64) -> ComposerClick {
        ComposerClick {
            char_index: index,
            button: PointerButton::Left,
            column: 7,
            row: 3,
            at: Duration::from_millis(millis),
        }
    }

    #[test]
    fn forward_and_reverse_drag_normalize_for_rendering() {
        let text = "abcdef";
        let mut selection = ComposerSelection::default();
        selection.begin_drag(text, 1);
        selection.drag_to(text, 5);
        assert_eq!(selection.anchor(), 1);
        assert_eq!(selection.head(), 5);
        assert_eq!(selection.render_range(text), Some(1..5));

        selection.begin_drag(text, 5);
        selection.drag_to(text, 1);
        assert_eq!(selection.anchor(), 5);
        assert_eq!(selection.head(), 1);
        assert_eq!(selection.range(text), Some(1..5));
    }

    #[test]
    fn drag_and_caret_indices_are_clamped() {
        let mut selection = ComposerSelection::default();
        selection.begin_drag("hi", 99);
        selection.drag_to("hi", 0);
        assert_eq!(selection.range("hi"), Some(0..2));
        selection.clear();
        assert_eq!(selection.head(), 0);
        assert_eq!(selection.range("hi"), None);
        selection.set_caret("hi", 99);
        assert_eq!(selection.head(), 2);
        assert_eq!(selection.range("hi"), None);
    }

    #[test]
    fn unicode_double_click_stops_at_unicode_punctuation() {
        let text = "naïve—café 世界";
        let mut selection = ComposerSelection::default();
        assert_eq!(selection.click(text, click(7, 0)), ClickSelection::Caret);
        assert_eq!(selection.click(text, click(7, 200)), ClickSelection::Word);
        assert_eq!(selection.range(text), Some(6..10));
    }

    #[test]
    fn combining_graphemes_are_selected_and_deleted_atomically() {
        let mut text = "cafe\u{301} noir".to_owned();
        let mut selection = ComposerSelection::default();
        selection.click(&text, click(3, 0));
        selection.click(&text, click(3, 100));
        assert_eq!(selection.range(&text), Some(0..5));
        assert!(selection.delete_selection(&mut text));
        assert_eq!(text, " noir");

        text = "e\u{301}x".to_owned();
        selection.set_caret(&text, 2);
        assert!(selection.backspace(&mut text));
        assert_eq!(text, "x");
    }

    #[test]
    fn double_click_respects_unicode_whitespace() {
        let text = "alpha\u{2003}βeta";
        let mut selection = ComposerSelection::default();
        selection.click(text, click(7, 0));
        selection.click(text, click(7, 100));
        assert_eq!(selection.range(text), Some(6..10));
    }

    #[test]
    fn punctuation_is_its_own_double_click_boundary() {
        let text = "one——two";
        let mut selection = ComposerSelection::default();
        selection.click(text, click(3, 0));
        selection.click(text, click(3, 100));
        assert_eq!(selection.range(text), Some(3..5));
    }

    #[test]
    fn double_click_requires_matching_time_button_and_coordinate() {
        let text = "alpha beta";
        let mut selection = ComposerSelection::default();
        selection.click(text, click(1, 0));

        let mut changed = click(1, 100);
        changed.column += 1;
        assert_eq!(selection.click(text, changed), ClickSelection::Caret);
        changed.at = Duration::from_millis(200);
        changed.button = PointerButton::Right;
        assert_eq!(selection.click(text, changed), ClickSelection::Caret);
        changed.at = Duration::from_millis(800);
        assert_eq!(selection.click(text, changed), ClickSelection::Caret);
        changed.at = Duration::from_millis(801);
        assert_eq!(selection.click(text, changed), ClickSelection::Word);
    }

    #[test]
    fn middle_button_is_a_distinct_click_identity() {
        let text = "word";
        let mut selection = ComposerSelection::default();
        let mut event = click(0, 0);
        event.button = PointerButton::Middle;
        selection.click(text, event);
        event.at = Duration::from_millis(1);
        assert_eq!(selection.click(text, event), ClickSelection::Word);
    }

    #[test]
    fn triple_click_selects_one_logical_line_including_newline() {
        let text = "first\nsecond λ\nthird";
        let mut selection = ComposerSelection::default();
        selection.click(text, click(9, 0));
        selection.click(text, click(9, 100));
        assert_eq!(
            selection.click(text, click(9, 200)),
            ClickSelection::LogicalLine
        );
        assert_eq!(selection.range(text), Some(6..15));
    }

    #[test]
    fn triple_click_last_line_does_not_overrun() {
        let text = "first\n最後";
        let mut selection = ComposerSelection::default();
        selection.click(text, click(7, 0));
        selection.click(text, click(7, 1));
        selection.click(text, click(7, 2));
        assert_eq!(selection.range(text), Some(6..8));
    }

    #[test]
    fn deleting_reverse_unicode_selection_uses_char_indices() {
        let mut text = "aé日z".to_owned();
        let mut selection = ComposerSelection::default();
        selection.begin_drag(&text, 3);
        selection.drag_to(&text, 1);
        assert!(selection.delete_selection(&mut text));
        assert_eq!(text, "az");
        assert_eq!(selection.head(), 1);
        assert_eq!(selection.range(&text), None);
    }

    #[test]
    fn replacement_handles_multiline_unicode_selection() {
        let mut text = "before\nλ中\nafter".to_owned();
        let mut selection = ComposerSelection::default();
        selection.begin_drag(&text, 7);
        selection.drag_to(&text, 10);
        selection.replace_or_insert(&mut text, "new🙂");
        assert_eq!(text, "before\nnew🙂after");
        assert_eq!(selection.head(), 11);
    }

    #[test]
    fn replacement_without_selection_inserts_at_caret() {
        let mut text = "ac".to_owned();
        let mut selection = ComposerSelection::default();
        selection.set_caret(&text, 1);
        selection.replace_or_insert(&mut text, "🙂b");
        assert_eq!(text, "a🙂bc");
        assert_eq!(selection.head(), 3);
    }

    #[test]
    fn backspace_and_delete_are_selection_first() {
        let mut text = "abcde".to_owned();
        let mut selection = ComposerSelection::default();
        selection.begin_drag(&text, 1);
        selection.drag_to(&text, 4);
        assert!(selection.backspace(&mut text));
        assert_eq!(text, "ae");
        assert_eq!(selection.head(), 1);

        selection.set_caret(&text, 1);
        assert!(selection.delete_forward(&mut text));
        assert_eq!(text, "a");
        assert!(!selection.delete_forward(&mut text));
        assert!(selection.backspace(&mut text));
        assert_eq!(text, "");
        assert!(!selection.backspace(&mut text));
    }

    #[test]
    fn cut_returns_exact_multibyte_text_and_collapses() {
        let mut text = "a🙂中z".to_owned();
        let mut selection = ComposerSelection::default();
        selection.begin_drag(&text, 3);
        selection.drag_to(&text, 1);
        assert_eq!(selection.cut(&mut text), Some("🙂中".to_owned()));
        assert_eq!(text, "az");
        assert_eq!(selection.head(), 1);
        assert_eq!(selection.cut(&mut text), None);
    }

    #[test]
    fn stale_indices_reconcile_before_editing() {
        let mut text = "short".to_owned();
        let mut selection = ComposerSelection::default();
        selection.set_caret(&text, 5);
        text.clear();
        assert!(!selection.delete_selection(&mut text));
        assert_eq!(selection.head(), 0);
    }

    #[test]
    fn undo_ring_coalesces_typing_and_keeps_kills_discrete() {
        let mut undo = ComposerUndo::default();
        // Three consecutive single-grapheme insertions are one step.
        undo.record("", 0, EditKind::Typing);
        undo.record("a", 1, EditKind::Typing);
        undo.record("ab", 2, EditKind::Typing);
        assert_eq!(undo.depth(), 1);
        // A kill always opens its own step.
        undo.record("abc", 3, EditKind::Discrete);
        assert_eq!(undo.depth(), 2);
        // The next typing run starts fresh rather than joining the kill.
        undo.record("", 0, EditKind::Typing);
        undo.record("x", 1, EditKind::Typing);
        assert_eq!(undo.depth(), 3);
    }

    #[test]
    fn undo_and_redo_round_trip_through_the_ring() {
        let mut undo = ComposerUndo::default();
        undo.record("one", 3, EditKind::Discrete);
        assert_eq!(
            undo.undo("two", 3).map(|snapshot| snapshot.text).as_deref(),
            Some("one")
        );
        assert_eq!(undo.depth(), 0);
        assert_eq!(
            undo.redo("one", 3).map(|snapshot| snapshot.text).as_deref(),
            Some("two")
        );
        assert_eq!(undo.depth(), 1);
        assert!(undo.redo("two", 3).is_none());
    }

    #[test]
    fn recording_a_new_edit_drops_the_redo_stack() {
        let mut undo = ComposerUndo::default();
        undo.record("a", 1, EditKind::Discrete);
        let _ = undo.undo("ab", 2);
        undo.record("a", 1, EditKind::Discrete);
        assert!(undo.redo("az", 2).is_none());
    }

    #[test]
    fn undo_ring_is_bounded_by_steps_and_bytes() {
        let mut undo = ComposerUndo::default();
        for index in 0..(UNDO_MAX_STEPS + 40) {
            undo.record(&format!("draft {index}"), 0, EditKind::Discrete);
        }
        assert_eq!(undo.depth(), UNDO_MAX_STEPS);
        // Spec floor: at least 100 steps must survive.
        assert!(undo.depth() >= 100);

        let mut heavy = ComposerUndo::default();
        let chunk = "x".repeat(64 * 1024);
        for _ in 0..40 {
            heavy.record(&chunk, 0, EditKind::Discrete);
        }
        assert!(heavy.retained_bytes() <= UNDO_MAX_BYTES);
        assert!(heavy.depth() >= 1);
    }

    #[test]
    fn reset_drops_the_whole_history() {
        let mut undo = ComposerUndo::default();
        undo.record("a", 1, EditKind::Discrete);
        let _ = undo.undo("ab", 2);
        undo.reset();
        assert_eq!(undo.depth(), 0);
        assert!(undo.redo("ab", 2).is_none());
    }
}
