//! Narrow-pane goldens for prose tokens, not fenced code listings.

use ratatui::style::{Modifier, Style};

use super::markdown::{MarkdownTheme, render_markdown};

#[test]
fn long_inline_code_and_urls_wrap_without_losing_cells_or_styles() {
    let theme = MarkdownTheme::default();
    for (source, width, expected, style) in [
        (
            "`abcdefghijklmnop`",
            5,
            vec!["abcde", "fghij", "klmno", "p"],
            theme.inline_code,
        ),
        (
            "`alpha beta gamma`",
            7,
            vec!["alpha", "beta", "gamma"],
            theme.inline_code,
        ),
        (
            "https://example.test/abcdef",
            8,
            vec!["https://", "example.", "test/abc", "def"],
            theme.link,
        ),
        (
            "`界界界界界`",
            5,
            vec!["界界", "界界", "界"],
            theme.inline_code,
        ),
    ] {
        let rendered = render_markdown(source, width, &theme);
        assert_eq!(
            rendered
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            expected,
            "{source}"
        );
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| line.width() <= usize::from(width))
        );
        assert!(
            rendered
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style == style)
        );
    }
}

#[test]
fn a_hard_broken_token_keeps_style_transitions_on_both_sides() {
    let theme = MarkdownTheme {
        strong: Style::default().add_modifier(Modifier::BOLD),
        ..MarkdownTheme::default()
    };
    let rendered = render_markdown("ab**cdefgh**ij", 4, &theme);
    assert_eq!(
        rendered
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["abcd", "efgh", "ij"]
    );
    assert_eq!(rendered.lines[0].spans[0].content, "ab");
    assert_eq!(rendered.lines[0].spans[0].style, theme.text);
    assert_eq!(rendered.lines[0].spans[1].content, "cd");
    assert_eq!(rendered.lines[0].spans[1].style, theme.strong);
    assert_eq!(rendered.lines[1].spans[0].content, "efgh");
    assert_eq!(rendered.lines[1].spans[0].style, theme.strong);
    assert_eq!(rendered.lines[2].spans[0].content, "ij");
    assert_eq!(rendered.lines[2].spans[0].style, theme.text);
}
