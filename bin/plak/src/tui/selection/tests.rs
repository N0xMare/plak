use super::*;
use ratatui::{buffer::Buffer, layout::Rect};

#[test]
fn reverse_drag_copies_offscreen_rows_from_full_projection() {
    let lines = vec!["zero".into(), "one".into(), "two".into(), "three".into()];
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 3, column: 2 }, 20, 7);
    assert!(selection.drag(ContentPosition { row: 0, column: 1 }));
    assert_eq!(
        selection
            .take_text(7, &lines, &[], &mut |_| None)
            .as_deref(),
        Some("ero\none\ntwo\nthr")
    );
}

#[test]
fn wide_and_combining_graphemes_are_never_split() {
    assert_eq!(slice_cells("a界e\u{301}z", 2, 2), "界");
    assert_eq!(slice_cells("a界e\u{301}z", 3, 3), "e\u{301}");
}

#[test]
fn width_change_clears_but_same_width_redraw_preserves_anchor() {
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 4, column: 2 }, 30, 1);
    selection.drag(ContentPosition { row: 5, column: 3 });
    selection.reconcile_projection(1, 30, 10, None);
    assert!(selection.ordered().is_some());
    selection.reconcile_projection(1, 29, 10, None);
    assert!(selection.ordered().is_none());
}

#[test]
fn every_theme_paints_visible_selection_cells() {
    for id in crate::tui::theme::ThemeId::ALL {
        let theme = crate::tui::theme::Theme::for_id(id);
        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        let mut selection = TranscriptSelection::default();
        selection.begin(ContentPosition { row: 0, column: 0 }, 2, 1);
        selection.drag(ContentPosition { row: 0, column: 1 });
        selection.render(
            &mut buffer,
            area,
            0,
            &["ok".into()],
            theme.transcript_selection,
        );
        for x in 0..2 {
            assert_eq!(Some(buffer[(x, 0)].fg), theme.transcript_selection.fg);
            assert_eq!(Some(buffer[(x, 0)].bg), theme.transcript_selection.bg);
        }
    }
}

#[test]
fn revision_change_requires_a_proven_stable_prefix() {
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 2, column: 0 }, 20, 4);
    selection.drag(ContentPosition { row: 3, column: 1 });
    selection.reconcile_projection(5, 20, 8, Some(4));
    assert!(selection.ordered().is_some());

    selection.reconcile_projection(6, 20, 8, None);
    assert!(selection.ordered().is_none());

    selection.begin(ContentPosition { row: 3, column: 0 }, 20, 6);
    selection.drag(ContentPosition { row: 4, column: 1 });
    selection.reconcile_projection(7, 20, 8, Some(4));
    assert!(selection.ordered().is_none());
}

#[test]
fn copy_preserves_selected_trailing_spaces_exactly() {
    let lines = vec!["safe  ".to_owned(), "tail ".to_owned()];
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 0, column: 0 }, 8, 9);
    selection.drag(ContentPosition { row: 1, column: 4 });
    assert_eq!(
        selection
            .take_text(9, &lines, &[], &mut |_| None)
            .as_deref(),
        Some("safe  \ntail ")
    );

    selection.begin(ContentPosition { row: 0, column: 4 }, 8, 10);
    selection.drag(ContentPosition { row: 0, column: 5 });
    assert_eq!(
        selection
            .take_text(10, &lines, &[], &mut |_| None)
            .as_deref(),
        Some("  ")
    );
}

#[test]
fn copy_drops_leading_decoration_and_keeps_wide_graphemes() {
    // Gutter plus a code rail, then a wide character and a combining mark.
    let lines = vec!["  │ 界e\u{301}".to_owned(), "  │ next".to_owned()];
    let copy = vec![
        RowCopy {
            decoration: 4,
            code: Some("界e\u{301}".to_owned()),
            language: false,
            code_continuation: false,
        },
        RowCopy {
            decoration: 4,
            code: Some("next".to_owned()),
            language: false,
            code_continuation: false,
        },
    ];
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 0, column: 0 }, 20, 1);
    selection.drag(ContentPosition { row: 1, column: 10 });
    assert_eq!(
        selection
            .take_text(1, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("界e\u{301}\nnext")
    );

    // A selection that starts on the wide character keeps it whole, and
    // does not expand to a clipped source.
    selection.begin(ContentPosition { row: 0, column: 4 }, 20, 2);
    selection.drag(ContentPosition { row: 0, column: 5 });
    assert_eq!(
        selection
            .take_text(2, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("界")
    );

    // Starting inside the content leaves the cells alone.
    selection.begin(ContentPosition { row: 0, column: 6 }, 20, 3);
    selection.drag(ContentPosition { row: 0, column: 8 });
    assert_eq!(
        selection
            .take_text(3, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("e\u{301}")
    );
}

#[test]
fn copy_omits_a_language_label_covered_through_its_end() {
    let lines = vec!["  ┌ bash".to_owned(), "  │ echo".to_owned()];
    let copy = vec![
        RowCopy {
            decoration: 4,
            code: None,
            language: true,
            code_continuation: false,
        },
        RowCopy {
            decoration: 4,
            code: Some("echo".to_owned()),
            language: false,
            code_continuation: false,
        },
    ];
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 0, column: 0 }, 20, 1);
    selection.drag(ContentPosition { row: 1, column: 10 });
    assert_eq!(
        selection
            .take_text(1, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("echo")
    );

    // A selection inside the label still copies those cells.
    selection.begin(ContentPosition { row: 0, column: 4 }, 20, 2);
    selection.drag(ContentPosition { row: 0, column: 6 });
    assert_eq!(
        selection
            .take_text(2, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("bas")
    );
}

#[test]
fn a_soft_wrapped_code_line_copies_its_source_once() {
    let lines = vec![
        "  │ let x = collection".to_owned(),
        "  │ ↳.iter().sum()".to_owned(),
        "  │ short".to_owned(),
    ];
    let copy = vec![
        RowCopy {
            decoration: 4,
            code: Some("let x = collection.iter().sum()".to_owned()),
            language: false,
            code_continuation: false,
        },
        RowCopy {
            decoration: 5,
            code: None,
            language: false,
            code_continuation: true,
        },
        RowCopy {
            decoration: 4,
            code: Some("short".to_owned()),
            language: false,
            code_continuation: false,
        },
    ];
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 0, column: 0 }, 20, 1);
    selection.drag(ContentPosition { row: 2, column: 20 });
    assert_eq!(
        selection
            .take_text(1, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("let x = collection.iter().sum()\nshort")
    );

    // Inside one visual row: painted cells only, no hidden tail and no marker.
    selection.begin(ContentPosition { row: 1, column: 5 }, 20, 2);
    selection.drag(ContentPosition { row: 1, column: 9 });
    assert_eq!(
        selection
            .take_text(2, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some(".iter")
    );

    // Across the wrap, but not through the end of the last visual row: the
    // covered cells of one source line, with no newline inserted at the wrap.
    selection.begin(ContentPosition { row: 0, column: 0 }, 20, 3);
    selection.drag(ContentPosition { row: 1, column: 9 });
    assert_eq!(
        selection
            .take_text(3, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("let x = collection.iter")
    );
}

#[test]
fn a_partial_soft_wrap_copies_painted_cells_not_the_source_line() {
    let source = "let x = collection.iter().map(|item| item.value).sum()";
    let lines = vec![
        "  │ let x = collection".to_owned(),
        "  │ ↳.iter().map(|item|".to_owned(),
        "  │ ↳ item.value).sum()".to_owned(),
    ];
    let copy = vec![
        RowCopy {
            decoration: 4,
            code: Some(source.to_owned()),
            language: false,
            code_continuation: false,
        },
        RowCopy {
            decoration: 5,
            code: None,
            language: false,
            code_continuation: true,
        },
        RowCopy {
            decoration: 5,
            code: None,
            language: false,
            code_continuation: true,
        },
    ];
    let mut selection = TranscriptSelection::default();
    selection.begin(ContentPosition { row: 0, column: 0 }, 24, 1);
    selection.drag(ContentPosition { row: 0, column: 22 });
    assert_eq!(
        selection
            .take_text(1, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("let x = collection"),
        "ending on the first visual row copies its cells, not the source"
    );
    selection.begin(ContentPosition { row: 0, column: 0 }, 24, 2);
    selection.drag(ContentPosition { row: 1, column: 9 });
    assert_eq!(
        selection
            .take_text(2, &lines, &copy, &mut |_| None)
            .as_deref(),
        Some("let x = collection.iter"),
        "a selection that stops mid-continuation copies the painted cells"
    );
}
