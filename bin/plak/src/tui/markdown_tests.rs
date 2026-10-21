//! Focused contract tests for the staged terminal Markdown module.

use crate::tui::markdown::{
    MarkdownTheme, StreamingMarkdown, render_markdown, render_markdown_rows,
    render_markdown_streaming,
};

#[test]
fn public_shape_renders_bounded_safe_text() {
    let rendered = render_markdown(
        "## Result\n\n- `cargo test`\n- [source](./src/main.rs)\n\n```rs\nlet wide = \"界界界\";\n```",
        12,
        &MarkdownTheme::default(),
    );
    assert!(rendered.lines.iter().all(|line| line.width() <= 12));
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line.to_string() == "Result")
    );
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line.to_string().contains("cargo"))
    );
}

/// Renders to plain rows with trailing blanks trimmed, for the goldens below.
fn rows(markdown: &str, width: u16) -> String {
    let text = render_markdown(markdown, width, &MarkdownTheme::default());
    let mut out: Vec<String> = text
        .lines
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect();
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out.join("\n")
}

/// A reply written one item per line was reflowed into a run-on
/// paragraph. Goldens taken from the live drive
/// (`32a-before-ctrl-c.txt`, `13-interrupt-after-esc.txt`).
#[test]
fn line_per_item_replies_keep_their_own_line_breaks() {
    // "count slowly from 1 to 50, each number on its own line" — the reply is
    // one CommonMark paragraph of soft breaks, and used to render as
    // `1 2 3 4 5 6 7 8 9 10 …` wrapped across two rows.
    let counted = (1..=10)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(rows(&counted, 100), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10");

    // A file list.
    let files = "src/main.rs\nsrc/ui.rs\nsrc/markdown.rs\nCargo.toml";
    assert_eq!(rows(files, 100), files);

    // A checklist CommonMark does not turn into a list.
    let checklist = "[x] read the spec\n[ ] write the fix\n[ ] land the regression test";
    assert_eq!(rows(checklist, 100), checklist);

    // A step plan whose steps are long enough to exceed the short-line rule,
    // but which every line opens as an item.
    let plan = concat!(
        "\u{2022} first inspect the composer input path and every caller it has today\n",
        "\u{2022} then replace the blanket rejection with the explicit blocklist\n",
        "\u{2022} finally pin the shaping sequences with a regression test of their own"
    );
    assert_eq!(rows(plan, 100), plan);
}

/// The other half of the rule: a true paragraph still flows.
#[test]
fn flowed_prose_still_folds_soft_breaks_to_spaces() {
    // Model output wrapped at a column is one paragraph, not four items.
    let wrapped = concat!(
        "I could not read README.md because it does not appear to exist in the\n",
        "current working directory. The session shell reported an empty result\n",
        "for every command, so the workspace looks empty from where I am sitting."
    );
    assert_eq!(
        rows(wrapped, 260),
        "I could not read README.md because it does not appear to exist in the current \
working directory. The session shell reported an empty result for every command, so \
the workspace looks empty from where I am sitting."
    );

    // A single line is never line-per-item.
    assert_eq!(rows("just one sentence.", 100), "just one sentence.");

    // The rule is scoped to the open paragraph: a heading is its own block and
    // the paragraph after it is judged on its own (one line, so it flows).
    assert_eq!(rows("# alpha\nbeta", 100), "alpha\n\nbeta");
}

/// Hard breaks and real lists are untouched by the paragraph rule.
#[test]
fn hard_breaks_and_lists_survive_the_paragraph_rule() {
    // Two trailing spaces are a hard break: a break in flowed prose too.
    let hard = concat!(
        "This first line is long enough that nothing about it looks like an item,  \n",
        "and this second line is just as long and would otherwise be folded in."
    );
    assert_eq!(
        rows(hard, 200),
        "This first line is long enough that nothing about it looks like an item,\n\
and this second line is just as long and would otherwise be folded in."
    );

    // Real list items keep one row each and keep their markers.
    assert_eq!(
        rows("- alpha\n- beta\n- gamma", 100),
        "• alpha\n• beta\n• gamma"
    );
    assert_eq!(rows("1. alpha\n2. beta", 100), "1. alpha\n2. beta");

    // A tight list item carries no paragraph at all, so its lazy
    // continuation lines still fold — the CommonMark reading, and the one the
    // paragraph rule deliberately does not reach.
    assert_eq!(rows("- alpha\n  beta\n  gamma", 100), "• alpha beta gamma");
}

/// Documents that stress every construct a settled-prefix split could get
/// wrong: blocks that continue across blank lines, fences with blank lines
/// inside, and definitions that reach back to earlier text.
const STREAMED_DOCUMENTS: &[&str] = &[
    "First paragraph with **bold** text.\n\nSecond paragraph.\n\n## Heading\n\nThird one\nwith a soft break.\n",
    "Intro:\n\n- one\n- two\n\n- loose three\n\n  continued item\n\nAfter the list.\n\n1. first\n2. second\n\n3. third\n\nDone.",
    "Here is the code:\n\n```rust\nfn main() {\n\n    println!(\"hi\");\n}\n```\n\nAnd more prose.\n\n~~~\ntilde fence\n\n~~~\n\nEnd.",
    "Unclosed fence follows.\n\n```python\ndef f():\n\n    return 1\n\nprint(f())\n",
    "> quoted\n> more\n\n> second quote\n\nplain\n\n    indented code\n\n    still code\n\nback to text",
    "| a | b |\n| --- | ---: |\n| 1 | 2 |\n\nafter table\n\ntext | with pipe\n--- | ---\nx | y\n\nend",
    "Setext\n======\n\n---\n\n***\n\n*emphasis first*\n\n+ plus list\n\nend",
    "Uses a [ref] link.\n\nMore text.\n\n[ref]: https://example.com\n\nTail.",
    "Footnote[^1] here.\n\nText.\n\n[^1]: the note\n",
    "<div>\n\nhtml block\n\n</div>\n\nafter html\n\nmore",
    "- item\n\n  ```\n  code\n\nnot in the list\n  ```\n\ntrailing",
    "   ```\nindented fence\n\nstill fenced\n   ```\n\nafter",
    "src/main.rs\nsrc/lib.rs\n\nCargo.toml\nREADME.md\n\nline one\nline two",
    "```javascript\r\nconst x = 1;\r\n\r\n\tcall(x);\r\n```\r\n",
    "~~~\n\nhello\n\n~~~\nTail",
    "```rust\n\n\n",
    "Code follows:\n```rust\nlet x = 1;\n```\nDone",
    "- list item\n```rust\nlet x = 1;\n```",
    "```r&#117;st\nlet x = 1;\n```",
    "```rust\nlet x = 1;\rlet y = 2;\n```",
    "crlf one\r\n\r\ncrlf two\r\n\r\n- crlf list\r\n",
    "Unicode 界界界 wide text that wraps around the narrow width several times over.\n\nNext 界.\n",
];

fn plain_lines(text: &ratatui::text::Text<'_>) -> Vec<ratatui::text::Line<'static>> {
    text.lines
        .iter()
        .map(|line| {
            ratatui::text::Line::from(
                line.spans
                    .iter()
                    .map(|span| ratatui::text::Span::styled(span.content.to_string(), span.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Streaming reuses the settled prefix's rows. At every length a stream
/// can reach, the result must equal rendering the whole text again.
#[test]
fn streaming_render_equals_a_full_render_at_every_prefix() {
    let theme = MarkdownTheme::default();
    for document in STREAMED_DOCUMENTS {
        for width in [12_u16, 40, 100] {
            let mut memo = StreamingMarkdown::default();
            let mut previous = Vec::new();
            for end in (0..=document.len()).filter(|end| document.is_char_boundary(*end)) {
                let prefix = &document[..end];
                let streamed = render_markdown_streaming(prefix, width, &theme, &mut memo);
                assert!(streamed.settled_kept <= previous.len());
                assert_eq!(
                    &streamed.settled[..streamed.settled_kept],
                    &previous[..streamed.settled_kept]
                );
                previous = streamed.settled.to_vec();
                let streamed = streamed.into_text();
                crate::tui::highlight::clear_memo();
                let full = render_markdown(prefix, width, &theme);
                assert_eq!(
                    plain_lines(&streamed),
                    plain_lines(&full),
                    "width {width}, prefix {prefix:?}"
                );
            }
        }
    }
}

#[test]
fn a_streamed_answer_settles_its_earlier_blocks() {
    let theme = MarkdownTheme::default();
    let mut memo = StreamingMarkdown::default();
    let mut text = String::new();
    let mut previous = 0;
    for paragraph in 0..50 {
        text.push_str(&format!(
            "Paragraph {paragraph} explains one more step.\n\nNext"
        ));
        let streamed = render_markdown_streaming(&text, 80, &theme, &mut memo);
        assert_eq!(
            streamed.settled_kept, previous,
            "a moving boundary appends to the settled rows it already has"
        );
        previous = streamed.settled.len();
        text.truncate(text.len() - "Next".len());
    }
    assert_eq!(
        memo.settled_bytes(),
        text.len(),
        "every finished paragraph settled"
    );

    let mut memo = StreamingMarkdown::default();
    let linked = "See [x].\n\nMore.\n\n[x]: https://example.com\n\nTail";
    let _ = render_markdown_streaming(linked, 80, &theme, &mut memo);
    assert_eq!(
        memo.settled_bytes(),
        0,
        "a reference definition disables the split"
    );
}

#[test]
fn fence_streaming_matches_fresh_rows_and_copy_after_every_edit() {
    let documents = [
        "```rust\n/* comment\ncontinued */\n\n\nlet x = 1;\n```\n\nDone.",
        "~~~\n\n\nhello\n\n~~~\nTail",
        "Before.\n\n```\n\n\ntext\n\n```\n\nAfter.",
        "```javascript\r\nconst x = 1;\r\n\r\n\tcall(x);\r\n```\r\n",
        "```\n\n\n",
        "```rust\n\n\n",
        "```rust\nlet x = 1;\n",
        "```rust\nx\n \n\t\nq\n   \n```",
        "> ```rust\n> fn main() {}\n> ```",
    ];
    let mut memo = StreamingMarkdown::default();
    for document in documents {
        let mut colored = MarkdownTheme::default();
        colored.syntax.keyword = ratatui::style::Style::default().fg(ratatui::style::Color::Red);
        for width in [8, 40] {
            for theme in [MarkdownTheme::default(), colored] {
                for end in (0..=document.len()).filter(|at| document.is_char_boundary(*at)) {
                    let source = &document[..end];
                    let streamed = render_markdown_streaming(source, width, &theme, &mut memo);
                    let mut copy = streamed.settled_copy.to_vec();
                    copy.extend(streamed.rest_copy.clone());
                    crate::tui::highlight::clear_memo();
                    let (rows, full_copy) = render_markdown_rows(source, width, &theme);
                    assert_eq!(
                        streamed.into_text().lines,
                        rows,
                        "{source:?}, width {width}"
                    );
                    assert_eq!(copy, full_copy, "copy: {source:?}");
                }
                // Resize/retheme the same completed source, then restore it.
                for (changed_width, changed_theme) in [(width + 1, colored), (width, theme)] {
                    let streamed = render_markdown_streaming(
                        document,
                        changed_width,
                        &changed_theme,
                        &mut memo,
                    )
                    .into_text();
                    assert_eq!(
                        streamed,
                        render_markdown(document, changed_width, &changed_theme)
                    );
                }
            }
        }
    }
    // Replacing an already complete language line must invalidate both caches.
    for source in [
        "```rust\nlet x = 1;\n",
        "```python\nlet x = 1;\n",
        "```python\nlet x = 1;\n```",
    ] {
        let theme = MarkdownTheme::default();
        let streamed = render_markdown_streaming(source, 40, &theme, &mut memo).into_text();
        assert_eq!(streamed, render_markdown(source, 40, &theme));
    }
}

#[test]
fn open_fence_work_only_visits_new_lines() {
    let theme = MarkdownTheme::default();
    let mut memo = StreamingMarkdown::default();
    let mut source = String::from("```rust\n");
    for index in 0..2000 {
        source.push_str("let x = ");
        let before = memo.work();
        let streamed = render_markdown_streaming(&source, 80, &theme, &mut memo);
        assert!(streamed.rest.len() <= 1);
        source.push_str("1;\n");
        let streamed = render_markdown_streaming(&source, 80, &theme, &mut memo);
        assert_eq!(streamed.settled.len(), index + 2);
        let work = memo.work() - before;
        assert!(work <= 5, "frame {index}: visited {work} lines");
    }
    assert_eq!(
        memo.settled_bytes(),
        0,
        "the source prefix is still inside a fence"
    );
    source.push_str("```\n");
    let before = memo.work();
    let streamed = render_markdown_streaming(&source, 80, &theme, &mut memo).into_text();
    assert!(memo.work() - before <= 2, "closing seals the cached rows");
    assert_eq!(streamed, render_markdown(&source, 80, &theme));
}

#[test]
fn blank_crlf_fence_rows_are_cached_even_when_the_lf_hides_them() {
    let theme = MarkdownTheme::default();
    let mut memo = StreamingMarkdown::default();
    let mut source = String::from("```rust\r\n");
    for _ in 0..2000 {
        let before = memo.work();
        source.push('\r');
        let streamed = render_markdown_streaming(&source, 80, &theme, &mut memo);
        assert!(streamed.rest.len() <= 1);
        source.push('\n');
        let streamed = render_markdown_streaming(&source, 80, &theme, &mut memo);
        assert_eq!(streamed.settled.len(), 1);
        assert_eq!(streamed.rest.len(), 0);
        assert!(memo.work() - before <= 5);
    }
    source.push_str("let x = 1;\r\n```\r\n");
    let streamed = render_markdown_streaming(&source, 80, &theme, &mut memo).into_text();
    assert_eq!(streamed, render_markdown(&source, 80, &theme));
}
