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
