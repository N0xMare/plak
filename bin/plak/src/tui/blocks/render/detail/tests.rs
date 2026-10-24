use ratatui::text::Line;

use super::command_lines;
use crate::tui::theme::Theme;

/// A multi-line command is one highlighted unit: a string opened on the
/// first row keeps its colour on the next, and only the first row's
/// program takes the accent.
#[test]
fn a_multi_line_command_highlights_as_one_unit() {
    let theme = Theme::dark();
    let lines = command_lines("cargo test \"a\nb\" &&\n  ls -la", &theme);
    let styled = |line: &Line<'static>, text: &str| {
        line.spans
            .iter()
            .filter(|span| span.content.contains(text))
            .map(|span| span.style)
            .collect::<Vec<_>>()
    };
    assert_eq!(lines.len(), 3);
    assert_eq!(styled(&lines[0], "$"), vec![theme.command_prompt]);
    assert_eq!(styled(&lines[0], "cargo"), vec![theme.command_program]);
    assert_eq!(styled(&lines[1], "b"), vec![theme.command_string]);
    assert!(!lines[2].to_string().contains('$'));
    assert!(
        lines[2]
            .spans
            .iter()
            .all(|span| span.style != theme.command_program),
        "{:?}",
        lines[2]
    );
}
