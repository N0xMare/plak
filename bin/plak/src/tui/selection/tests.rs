use super::*;
use ratatui::style::Color;
use ratatui::{buffer::Buffer, layout::Rect};
use unicode_width::UnicodeWidthChar as _;

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
            &[],
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

fn selection_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Blue)
}

fn paint_chars(buffer: &mut Buffer, y: u16, line: &str) {
    for (column, ch) in line.chars().enumerate() {
        assert_eq!(
            ch.width(),
            Some(1),
            "test glyph {ch:?} is wider than one cell"
        );
        let column = u16::try_from(column).unwrap();
        buffer[(column, y)].set_symbol(&ch.to_string());
    }
}

/// Glyphs wearing the selection style, one string per row in `from..=to`.
fn highlighted_rows(
    buffer: &Buffer,
    width: u16,
    style: Style,
    from: usize,
    to: usize,
) -> Vec<String> {
    (from..=to)
        .map(|row| {
            let y = u16::try_from(row).unwrap();
            let mut text = String::new();
            for x in 0..width {
                let cell = &buffer[(x, y)];
                if Some(cell.fg) == style.fg && Some(cell.bg) == style.bg {
                    text.push_str(cell.symbol());
                }
            }
            text
        })
        .collect()
}

fn row(decoration: u16, code: Option<&str>, language: bool, continuation: bool) -> RowCopy {
    RowCopy {
        decoration,
        code: code.map(str::to_owned),
        language,
        code_continuation: continuation,
    }
}

/// Render `lines` and return `(highlighted rows, copied text)`.
fn paint_selection(
    lines: &[&str],
    copy: &[RowCopy],
    start: ContentPosition,
    end: ContentPosition,
) -> (Vec<String>, String) {
    const WIDTH: u16 = 48;
    let height = u16::try_from(lines.len()).unwrap();
    let area = Rect::new(0, 0, WIDTH, height);
    let mut buffer = Buffer::empty(area);
    for (y, line) in lines.iter().enumerate() {
        paint_chars(&mut buffer, u16::try_from(y).unwrap(), line);
    }
    let owned: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    let style = selection_style();
    let mut selection = TranscriptSelection::default();
    selection.begin(start, usize::from(WIDTH), 1);
    assert!(selection.drag(end));
    selection.render(&mut buffer, area, 0, &owned, copy, style);
    let from = start.row.min(end.row);
    let to = start.row.max(end.row);
    let highlighted = highlighted_rows(&buffer, WIDTH, style, from, to);
    let copied = selection
        .take_text(1, &owned, copy, &mut |_| None)
        .unwrap_or_default();
    (highlighted, copied)
}

fn at(row: usize, column: usize) -> ContentPosition {
    ContentPosition { row, column }
}

#[test]
fn highlight_matches_copy_for_every_non_copyable_prefix() {
    // Past the plain row, so a bug that paints to the drag column shows up.
    let edge = 40;
    let prose = paint_selection(
        &["  Ready."],
        &[row(2, None, false, false)],
        at(0, 0),
        at(0, edge),
    );
    assert_eq!(prose.0, vec!["Ready."]);
    assert_eq!(prose.1, "Ready.");

    let quote = paint_selection(
        &["  │ a quoted sentence"],
        &[row(4, None, false, false)],
        at(0, 0),
        at(0, edge),
    );
    assert_eq!(quote.0, vec!["a quoted sentence"]);
    assert_eq!(quote.1, "a quoted sentence");

    let list = paint_selection(
        &["  • alpha item"],
        &[row(2, None, false, false)],
        at(0, 0),
        at(0, edge),
    );
    assert_eq!(list.0, vec!["• alpha item"]);
    assert_eq!(list.1, "• alpha item");

    let table = paint_selection(
        &["  │ plakous/ │ facade"],
        &[row(2, None, false, false)],
        at(0, 0),
        at(0, edge),
    );
    assert_eq!(table.0, vec!["│ plakous/ │ facade"]);
    assert_eq!(table.1, "│ plakous/ │ facade");

    let output = paint_selection(
        &["    │ output ▸"],
        &[row(6, None, false, false)],
        at(0, 0),
        at(0, edge),
    );
    assert_eq!(output.0, vec!["output ▸"]);
    assert_eq!(output.1, "output ▸");

    // A detail or diff rail is decoration. The marker stays.
    let diff = paint_selection(
        &["    │ -two"],
        &[row(6, None, false, false)],
        at(0, 0),
        at(0, edge),
    );
    assert_eq!(diff.0, vec!["-two"]);
    assert_eq!(diff.1, "-two");
}

#[test]
fn highlight_matches_a_partial_selection_inside_a_rail() {
    let line = &["  │ echo"];
    let copy = &[row(4, Some("echo"), false, false)];
    // Starts on the indent and ends inside the word. The rail is not copied.
    let (highlighted, copied) = paint_selection(line, copy, at(0, 1), at(0, 5));
    assert_eq!(highlighted, vec!["ec"]);
    assert_eq!(copied, "ec");

    // Ends inside the rail: nothing is copied, so nothing is highlighted.
    let (highlighted, copied) = paint_selection(line, copy, at(0, 0), at(0, 2));
    assert_eq!(highlighted, vec![""]);
    assert_eq!(copied, "");

    // Starts in the content. Those cells stay highlighted exactly as before.
    let (highlighted, copied) = paint_selection(line, copy, at(0, 4), at(0, 6));
    assert_eq!(highlighted, vec!["ech"]);
    assert_eq!(copied, "ech");
}

#[test]
fn highlight_omits_a_language_label_and_a_code_rail() {
    let lines = &["  ┌ bash", "  │ echo"];
    let copy = &[
        row(4, None, true, false),
        row(4, Some("echo"), false, false),
    ];
    let (highlighted, copied) = paint_selection(lines, copy, at(0, 0), at(1, 40));
    assert_eq!(highlighted, vec!["", "echo"]);
    assert_eq!(copied, "echo");

    // Inside the label, starting on the rail: the label cells are copied.
    let (highlighted, copied) = paint_selection(lines, copy, at(0, 1), at(0, 6));
    assert_eq!(highlighted, vec!["bas"]);
    assert_eq!(copied, "bas");

    let (highlighted, copied) = paint_selection(
        &["  │ echo hi"],
        &[row(4, Some("echo hi"), false, false)],
        at(0, 0),
        at(0, 40),
    );
    assert_eq!(highlighted, vec!["echo hi"]);
    assert_eq!(copied, "echo hi");
}

#[test]
fn highlight_matches_a_soft_wrapped_code_line_without_the_marker() {
    let lines = &["  │ let x = collection", "  │ ↳.iter().sum()", "  │ short"];
    let copy = &[
        row(4, Some("let x = collection.iter().sum()"), false, false),
        row(5, None, false, true),
        row(4, Some("short"), false, false),
    ];
    let (highlighted, copied) = paint_selection(lines, copy, at(0, 0), at(2, 40));
    assert_eq!(
        highlighted,
        vec!["let x = collection", ".iter().sum()", "short"]
    );
    assert_eq!(copied, "let x = collection.iter().sum()\nshort");
    assert!(!highlighted[1].contains('↳'));

    let (partial, partial_copy) = paint_selection(lines, copy, at(0, 0), at(1, 9));
    assert_eq!(partial, vec!["let x = collection", ".iter"]);
    assert_eq!(partial_copy, "let x = collection.iter");
    assert!(!partial.join("").contains('│'));
}

#[test]
fn highlight_skips_prompt_chrome_past_the_plain_row() {
    const WIDTH: u16 = 48;
    let line = "❯ ship it";
    let area = Rect::new(0, 0, WIDTH, 1);
    let mut buffer = Buffer::empty(area);
    paint_chars(&mut buffer, 0, line);
    for (offset, ch) in "14:02".chars().enumerate() {
        buffer[(30 + u16::try_from(offset).unwrap(), 0)].set_symbol(&ch.to_string());
    }
    for (offset, ch) in " steer".chars().enumerate() {
        buffer[(36 + u16::try_from(offset).unwrap(), 0)].set_symbol(&ch.to_string());
    }
    let style = selection_style();
    let owned = vec![line.to_owned()];
    let copy = [row(0, None, false, false)];
    let mut selection = TranscriptSelection::default();
    selection.begin(at(0, 0), usize::from(WIDTH), 1);
    assert!(selection.drag(at(0, 47)));
    selection.render(&mut buffer, area, 0, &owned, &copy, style);
    let highlighted = highlighted_rows(&buffer, WIDTH, style, 0, 0);
    let copied = selection
        .take_text(1, &owned, &copy, &mut |_| None)
        .unwrap_or_default();
    assert_eq!(highlighted, vec![line]);
    assert_eq!(copied, line);
    assert_ne!(Some(buffer[(30u16, 0)].bg), style.bg);
    assert_ne!(Some(buffer[(36u16, 0)].bg), style.bg);
}

#[test]
fn highlight_keeps_a_blank_line_between_copied_prose() {
    let lines = &["  one", "", "  two"];
    let copy = &[
        row(2, None, false, false),
        row(0, None, false, false),
        row(2, None, false, false),
    ];
    let (highlighted, copied) = paint_selection(lines, copy, at(0, 0), at(2, 40));
    assert_eq!(highlighted, vec!["one", "", "two"]);
    assert_eq!(copied, "one\n\ntwo");
}
