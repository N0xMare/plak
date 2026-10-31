use super::*;

fn plain(text: &Text<'_>) -> Vec<String> {
    text.lines.iter().map(Line::to_string).collect()
}

#[test]
fn renders_structure_and_bounds_terminal_width() {
    let source = "# Heading\n\n1. **bold** and *soft* `code`\n2. https://example.test/a\n\n> quoted words that wrap";
    let rendered = render_markdown(source, 16, &MarkdownTheme::default());
    let lines = plain(&rendered);
    assert!(lines.iter().any(|line| line == "Heading"));
    assert!(lines.iter().any(|line| line.starts_with("1. bold")));
    assert!(lines.iter().any(|line| line.starts_with("│ quoted")));
    assert!(lines.iter().all(|line| line.width() <= 16));
}

/// A ```text fence printed the bare word `text` as a body
/// line, a ```rust fence dropped the blank lines between items, and
/// neither had a frame. A fenced block now reads as one raised object:
/// its info string rides the opening corner, every row sits on the rail,
/// blank rows survive, and nothing is reflowed.
#[test]
fn a_fenced_code_block_renders_as_a_code_block() {
    let source = "before\n\n```rust\npub fn gcd(a: u64) -> u64 {\n\n    a\n}\n```\n\nafter";
    let lines = plain(&render_markdown(source, 60, &MarkdownTheme::default()));
    let code = lines
        .iter()
        .skip_while(|line| !line.starts_with('┌'))
        .take_while(|line| line.starts_with('┌') || line.starts_with('│'))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        code,
        vec![
            "┌ rust".to_owned(),
            "│ pub fn gcd(a: u64) -> u64 {".to_owned(),
            // A blank line inside the fence is content, not a gap to drop.
            "│ ".to_owned(),
            "│     a".to_owned(),
            "│ }".to_owned(),
        ],
        "{lines:?}"
    );
    // The prose either side is untouched.
    assert!(lines.contains(&"before".to_owned()), "{lines:?}");
    assert!(lines.contains(&"after".to_owned()), "{lines:?}");
    // The language tag never appears as a body line of its own.
    assert!(!lines.contains(&"rust".to_owned()), "{lines:?}");
}

#[test]
fn a_javascript_fence_highlights_keywords_separately() {
    let rendered = render_markdown(
        "```javascript\nconst greeting = \"hello\";\n```",
        60,
        &MarkdownTheme::default(),
    );
    let line = rendered
        .lines
        .iter()
        .find(|line| line.to_string().contains("const"));
    assert!(
        line.is_some_and(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "const" && span.style.add_modifier.contains(Modifier::BOLD)
        })),
        "javascript keywords should be their own bold span: {rendered:?}"
    );
}

/// A code listing keeps its source line breaks and soft-wraps an over-wide
/// row. Continuation rows repeat the rail and a marker; prose is not joined
/// onto the listing, and nothing exceeds the pane.
#[test]
fn a_code_block_soft_wraps_over_wide_rows_and_keeps_source_breaks() {
    let long = "let x = collection.iter().map(|item| item.value).sum::<u64>();";
    let source = format!("```\n{long}\nshort\n```");
    let lines = plain(&render_markdown(&source, 24, &MarkdownTheme::default()));
    let code = lines
        .iter()
        .filter(|line| line.starts_with('\u{2502}'))
        .cloned()
        .collect::<Vec<_>>();
    assert!(code.len() > 2, "the long source row wraps: {lines:?}");
    assert!(
        code[0].starts_with("\u{2502} let x = collection"),
        "{code:?}"
    );
    assert!(
        code[1].starts_with("\u{2502} \u{21b3}"),
        "continuation marker: {code:?}"
    );
    assert_eq!(code.last().map(String::as_str), Some("\u{2502} short"));
    assert!(
        !code
            .iter()
            .any(|line| line.contains("short") && line.contains("let")),
        "{code:?}"
    );
    assert!(lines.iter().all(|line| line.width() <= 24), "{lines:?}");
}

/// A continuation budget under two cells cannot hold a wide grapheme beside
/// the marker, and a grapheme wider than that budget cannot sit on a later
/// row. The fence clips instead of painting past the pane, and the row
/// estimate matches that clip.
#[test]
fn a_narrow_fence_clips_wide_graphemes_to_the_pane() {
    let line = "a\u{754c}b\u{754c}c";
    let source = format!("```\n{line}\n```");
    for width in [3usize, 4, 5] {
        let rows = plain(&render_markdown(
            &source,
            u16::try_from(width).unwrap(),
            &MarkdownTheme::default(),
        ));
        assert!(
            rows.iter().all(|row| row.width() <= width),
            "width {width}: {rows:?}"
        );
    }
    let painted = plain(&render_markdown(&source, 4_u16, &MarkdownTheme::default()));
    let body = painted
        .iter()
        .filter(|row| row.starts_with('\u{2502}'))
        .count();
    assert_eq!(
        crate::tui::blocks::code_line_rows_for_test(line, 4),
        body,
        "estimate must match the painted rows of the clipped line: {painted:?}"
    );
    // U+17D8 is three cells. At pane width 5 the continuation budget is two,
    // so the grapheme does not fit a later row. Clipping keeps the pane;
    // wrapping would paint it past the rail.
    let wider = "a\u{17D8}";
    let wider_source = format!("```\n{wider}\n```");
    let wider_rows = plain(&render_markdown(
        &wider_source,
        5,
        &MarkdownTheme::default(),
    ));
    assert!(
        wider_rows.iter().all(|row| row.width() <= 5),
        "width 5: {wider_rows:?}"
    );
    let wider_body = wider_rows
        .iter()
        .filter(|row| row.starts_with('\u{2502}'))
        .count();
    assert_eq!(
        crate::tui::blocks::code_line_rows_for_test(wider, 5),
        wider_body,
        "estimate must match the clipped three-cell grapheme: {wider_rows:?}"
    );
}

/// Copy metadata is not inferred from the painted cells: a code rail and a
/// quote rail are decoration, a wrapped code row still names its source,
/// and list markers and table rails are content.
#[test]
fn copy_rows_name_code_and_quote_decoration_and_leave_lists_and_tables() {
    let source = "\
Ready.

> a quoted sentence

- alpha item

| Path | Role |
| --- | --- |
| plakous/ | facade |

```bash
plak logout claude
    plak logout codex
```
";
    let (lines, copy) = render_markdown_rows(source, 60, &MarkdownTheme::default());
    assert_eq!(lines.len(), copy.len());
    let text = |index: usize| lines[index].to_string();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|line| line.to_string().contains(needle))
            .unwrap_or_else(|| panic!("{needle} missing: {lines:?}"))
    };

    let prose = at("Ready.");
    assert_eq!(copy[prose].decoration, 0);
    assert!(copy[prose].code.is_none());

    let quote = at("quoted sentence");
    assert!(text(quote).starts_with("│ "), "{:?}", text(quote));
    assert_eq!(copy[quote].decoration, 2);
    assert!(copy[quote].code.is_none());

    let item = at("alpha item");
    assert!(text(item).starts_with("• "), "{:?}", text(item));
    assert_eq!(copy[item].decoration, 0);
    assert!(copy[item].code.is_none());

    let table = at("plakous/");
    assert!(text(table).starts_with('│'), "{:?}", text(table));
    assert_eq!(copy[table].decoration, 0);
    assert!(copy[table].code.is_none(), "a table rail is not a code row");

    let lang = at("bash");
    assert!(text(lang).starts_with("┌ bash"), "{:?}", text(lang));
    assert!(copy[lang].language);
    assert!(copy[lang].code.is_none());
    assert_eq!(copy[lang].decoration, 2);

    let claude = at("plak logout claude");
    assert!(
        text(claude).starts_with("│ plak logout claude"),
        "{:?}",
        text(claude)
    );
    assert_eq!(copy[claude].decoration, 2);
    assert_eq!(copy[claude].code.as_deref(), Some("plak logout claude"));
    let codex = at("plak logout codex");
    assert_eq!(copy[codex].code.as_deref(), Some("    plak logout codex"));

    let wrapped_line = "let x = collection.iter().map(|item| item.value).sum::<u64>();";
    let (wrapped, wrapped_copy) = render_markdown_rows(
        &format!("```\n{wrapped_line}\nshort\n```"),
        24,
        &MarkdownTheme::default(),
    );
    let wide = wrapped
        .iter()
        .position(|line| line.to_string().starts_with("│ let"))
        .expect("wrapped code row");
    assert!(wrapped[wide].to_string().width() <= 24);
    assert!(!wrapped[wide].to_string().contains(wrapped_line));
    assert_eq!(wrapped_copy[wide].code.as_deref(), Some(wrapped_line));
    assert!(!wrapped_copy[wide].language);
    assert!(!wrapped_copy[wide].code_continuation);
    let continuations: Vec<_> = wrapped_copy
        .iter()
        .enumerate()
        .filter(|(_, row)| row.code_continuation)
        .collect();
    assert!(!continuations.is_empty(), "{wrapped:?}");
    assert!(continuations.iter().all(|(_, row)| row.code.is_none()));
    assert!(
        continuations
            .iter()
            .all(|(index, _)| wrapped[*index].to_string().contains('\u{21b3}'))
    );
}

#[test]
fn code_copy_keeps_tabs_and_drops_a_cr_before_lf() {
    assert_eq!(code_copy_line("\tcat <<-EOF\r"), "\tcat <<-EOF");
    assert_eq!(code_copy_line("a\rb"), "a\u{FFFD}b");
    assert_eq!(code_copy_line("\t\u{1b}x"), "\t\u{FFFD}x");

    let source = "```make\nall:\n\techo hi\r\n\tcat <<-EOF\n\t\tbody\n\tEOF\n```";
    let (lines, copy) = render_markdown_rows(source, 80, &MarkdownTheme::default());
    let code: Vec<&str> = copy.iter().filter_map(|row| row.code.as_deref()).collect();
    assert_eq!(
        code,
        ["all:", "\techo hi", "\tcat <<-EOF", "\t\tbody", "\tEOF"]
    );
    assert!(
        lines.iter().all(|line| !line.to_string().contains('\t')),
        "painted cells keep spaces: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains("echo hi"))
    );
}

#[test]
fn unfinished_fence_and_controls_are_safe() {
    let rendered = render_markdown(
        "```rust\nlet x = \"\u{1b}[31m\";",
        10,
        &MarkdownTheme::default(),
    );
    let lines = plain(&rendered);
    // The fence's info string rides the block's opening corner
    // instead of printing as a body line.
    assert_eq!(lines.first().map(String::as_str), Some("┌ rust"));
    assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
    assert!(lines.iter().all(|line| line.width() <= 10));
}

#[test]
fn wide_unicode_and_long_tokens_split_on_grapheme_boundaries() {
    let rendered = render_markdown("界界界 abcdef", 4, &MarkdownTheme::default());
    assert_eq!(plain(&rendered), ["界界", "界", "abcd", "ef"]);

    let narrow = render_markdown("界", 1, &MarkdownTheme::default());
    assert_eq!(plain(&narrow), ["界"]);
}

#[test]
fn styled_unicode_word_wraps_as_one_token_across_style_spans() {
    let theme = MarkdownTheme {
        strong: Style::default().add_modifier(Modifier::BOLD),
        ..MarkdownTheme::default()
    };
    let rendered = render_markdown("x ab**e\u{301}界**cd", 6, &theme);
    assert_eq!(plain(&rendered), ["x", "abe\u{301}界c", "d"]);
    assert_eq!(rendered.lines[1].spans.len(), 3);
    assert_eq!(rendered.lines[1].spans[0].content, "ab");
    assert_eq!(rendered.lines[1].spans[1].content, "e\u{301}界");
    assert_eq!(rendered.lines[1].spans[2].content, "c");
    assert!(
        rendered.lines[1].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn wrapped_table_rows_have_muted_grid_and_record_rules() {
    let source = "| Name | Value |\n| --- | --- |\n| first | wrapped words across lines |\n| second | another wrapped row |\n";
    let theme = MarkdownTheme::default();
    let lines = render_markdown(source, 26, &theme);
    let text = plain(&lines);
    assert!(text[0].starts_with('┌'), "{text:?}");
    assert!(text.last().unwrap().starts_with('└'), "{text:?}");
    assert_eq!(text.iter().filter(|line| line.starts_with('├')).count(), 2);
    for line in lines
        .iter()
        .filter(|line| matches!(line.to_string().chars().next(), Some('┌' | '├' | '└')))
    {
        assert!(
            line.spans
                .iter()
                .all(|span| span.style == theme.list_marker)
        );
    }
    assert!(text.iter().all(|line| line.width() == text[0].width()));
    assert!(
        text.iter()
            .filter(|line| line.contains("wrapped"))
            .all(|line| line.starts_with('│') && line.ends_with('│'))
    );
    let rows = text
        .iter()
        .filter(|line| line.starts_with('│'))
        .collect::<Vec<_>>();
    assert!(rows.len() > 3, "both body rows wrap: {text:?}");
    let records = plain(&render_markdown(source, 12, &theme));
    assert!(
        records.iter().any(|line| line == &"─".repeat(12)),
        "{records:?}"
    );
}

#[test]
fn gfm_table_renders_measured_columns_and_a_header_rule() {
    let source = concat!(
        "| Path | Role | Lines |\n",
        "| --- | --- | --- |\n",
        "| plakous/ | facade | 39k |\n",
        "| agent/ | loop | 120k |\n",
    );
    let rendered = render_markdown(source, 40, &MarkdownTheme::default());
    assert_eq!(
        plain(&rendered),
        [
            "┌──────────┬────────┬───────┐",
            "│ Path     │ Role   │ Lines │",
            "├──────────┼────────┼───────┤",
            "│ plakous/ │ facade │ 39k   │",
            "├──────────┼────────┼───────┤",
            "│ agent/   │ loop   │ 120k  │",
            "└──────────┴────────┴───────┘",
        ]
    );
}

#[test]
fn table_column_alignment_follows_the_delimiter_row() {
    let source = concat!(
        "| left | mid | right |\n",
        "| :--- | :-: | ----: |\n",
        "| a | b | c |\n",
    );
    let rendered = render_markdown(source, 40, &MarkdownTheme::default());
    assert_eq!(
        plain(&rendered),
        [
            "┌──────┬─────┬───────┐",
            "│ left │ mid │ right │",
            "├──────┼─────┼───────┤",
            "│ a    │  b  │     c │",
            "└──────┴─────┴───────┘",
        ]
    );
}

#[test]
fn table_columns_measure_unicode_cells_by_display_cells() {
    let source = concat!("| key | value |\n", "| --- | --- |\n", "| 界界 | ok |\n");
    let rendered = render_markdown(source, 40, &MarkdownTheme::default());
    let lines = plain(&rendered);
    assert!(lines.iter().all(|line| line.width() == 16), "{lines:?}");
    assert_eq!(lines[3], "│ 界界 │ ok    │");
}

#[test]
fn over_wide_tables_wrap_inside_each_cell_instead_of_truncating() {
    let source = concat!(
        "| first | second |\n",
        "| --- | --- |\n",
        "| aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb |\n",
    );
    let rendered = render_markdown(source, 40, &MarkdownTheme::default());
    let lines = plain(&rendered);
    assert!(lines.iter().all(|line| line.width() <= 40), "{lines:?}");
    assert!(!lines.iter().any(|line| line.contains('…')), "{lines:?}");
    let body = &lines[3..lines.len() - 1];
    assert!(
        body.len() > 1,
        "a long cell wraps onto more rows: {lines:?}"
    );
    assert!(body.iter().all(|line| line.contains('│')), "{lines:?}");
    let cells: Vec<(String, String)> = body
        .iter()
        .map(|line| {
            let (left, right) = line.trim_matches('│').split_once('│').unwrap_or_default();
            (left.trim().to_owned(), right.trim().to_owned())
        })
        .collect();
    let first: String = cells.iter().map(|(left, _)| left.as_str()).collect();
    let second: String = cells.iter().map(|(_, right)| right.as_str()).collect();
    assert_eq!(first, "a".repeat(30));
    assert_eq!(second, "b".repeat(30));
}

#[test]
fn styled_cells_wrap_by_word_and_keep_every_row_the_same_width() {
    let source = concat!(
        "| Vertical | Process tools today | Plan |\n",
        "| --- | --- | --- |\n",
        "| Codex | `exec_command` / `write_stdin`. Every call returns within 30 s. | Keep the contract. |\n",
        "| Grok | `run_terminal_command`, foreground only, **up to 30 minutes**. | Refresh the pack. |\n",
    );
    let lines = plain(&render_markdown(source, 60, &MarkdownTheme::default()));
    let width = lines[0].width();
    assert!(width <= 60, "{lines:?}");
    assert!(
        lines.iter().all(|line| line.width() == width),
        "mixed-style cells must not over-pad a row: {lines:?}"
    );
    assert!(!lines.iter().any(|line| line.contains('…')), "{lines:?}");
    let text = lines.join("\n");
    for word in [
        "exec_command",
        "write_stdin.",
        "minutes.",
        "Refresh",
        "contract.",
    ] {
        assert!(text.contains(word), "{word} lost: {lines:?}");
    }
}

#[test]
fn narrow_columns_keep_their_natural_width_when_others_wrap() {
    assert_eq!(fit_column_widths(&[5, 40, 40], 50), vec![5, 23, 22]);
    assert_eq!(fit_column_widths(&[5, 10], 50), vec![5, 10]);
    assert_eq!(fit_column_widths(&[30, 30], 20), vec![10, 10]);
}

#[test]
fn a_wide_four_column_table_uses_readable_records() {
    let source = concat!(
        "| Account | Auth | Models | Composition |\n",
        "| --- | --- | --- | --- |\n",
        "| grok / SuperGrok | OAuth, paste code | grok-4.6 (primary campaign) | direct |\n",
        "| codex | OAuth, loopback callback | gpt-6-astra, gpt-5.6-sol | native-ptc |\n",
    );
    let rendered = render_markdown(source, 48, &MarkdownTheme::default());
    let lines = plain(&rendered);
    assert!(
        lines.iter().all(|line| line.width() <= 48),
        "over-wide: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "Account: grok / SuperGrok"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "Models: gpt-6-astra, gpt-5.6-sol"),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains('…')),
        "readable fallback must not truncate cells: {lines:?}"
    );
}

#[test]
fn narrow_asymmetric_table_falls_back_to_bounded_labelled_records() {
    let source = concat!(
        "| Name | Description | State |\n",
        "| --- | --- | --- |\n",
        "| alpha | a very long explanation that must remain readable | ready |\n",
        "| beta | short |\n",
    );
    let lines = plain(&render_markdown(source, 18, &MarkdownTheme::default()));
    assert!(lines.iter().all(|line| line.width() <= 18), "{lines:?}");
    assert!(lines.iter().any(|line| line == "Name: alpha"), "{lines:?}");
    assert!(
        lines.iter().any(|line| line == "Description:")
            && lines.iter().any(|line| line.contains("a very long")),
        "the long cell must wrap rather than collapse to an ellipsis: {lines:?}"
    );
    assert!(lines.iter().any(|line| line == "Name: beta"), "{lines:?}");
    assert!(
        !lines.iter().any(|line| line == "State:"),
        "a missing asymmetric cell must not create an empty field: {lines:?}"
    );
}

#[test]
fn narrow_header_only_table_keeps_its_headings() {
    let source = "| Name | Description | State |\n| --- | --- | --- |\n";
    let lines = plain(&render_markdown(source, 18, &MarkdownTheme::default()));
    assert!(lines.iter().any(|line| line.contains("Name")), "{lines:?}");
    assert!(lines.iter().any(|line| line.contains("State")), "{lines:?}");
}

#[test]
fn a_table_whose_separators_alone_overflow_becomes_records() {
    let source = concat!(
        "| a | b | c | d | e | f | g | h | i | j |\n",
        "| - | - | - | - | - | - | - | - | - | - |\n",
        "| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 0 |\n",
    );
    let lines = plain(&render_markdown(source, 30, &MarkdownTheme::default()));
    assert!(lines.iter().all(|line| line.width() <= 30), "{lines:?}");
    assert!(lines.iter().any(|line| line == "a: 1"), "{lines:?}");
    assert!(lines.iter().any(|line| line == "j: 0"), "{lines:?}");
}

#[test]
fn url_and_path_receive_semantic_styles() {
    let theme = MarkdownTheme {
        link: Style::default().add_modifier(Modifier::REVERSED),
        path: Style::default().add_modifier(Modifier::RAPID_BLINK),
        ..MarkdownTheme::default()
    };
    let rendered = render_markdown("https://example.test ./src/main.rs", 80, &theme);
    let spans = &rendered.lines[0].spans;
    assert!(
        spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::REVERSED))
    );
    assert!(
        spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::RAPID_BLINK))
    );
}
