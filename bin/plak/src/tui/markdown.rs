//! Compact, presentation-only Markdown rendering for terminal transcript text.
//!
//! The renderer deliberately owns no theme selection, link activation, or
//! transcript state. Callers provide semantic styles and a cell width, then
//! receive inert Ratatui text suitable for scrollback or a paragraph widget.
//! Its child `streaming` renders text that only grows at its end, settling
//! the prefix before its last top-level block boundary.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
#[cfg(any(test, feature = "test-support"))]
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

mod streaming;

pub use streaming::{StreamingMarkdown, render_markdown_streaming};

#[cfg_attr(
    any(test, feature = "test-support"),
    doc = "Semantic styles consumed by [`render_markdown`]."
)]
#[cfg_attr(
    not(any(test, feature = "test-support")),
    doc = "Semantic styles consumed by the markdown renderer."
)]
///
/// Defaults use terminal modifiers only, so applications can patch in colors
/// from any light or dark theme without the renderer assuming a palette.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarkdownTheme {
    /// Ordinary prose.
    pub text: Style,
    /// Heading levels H1 through H6.
    pub headings: [Style; 6],
    /// Strong emphasis.
    pub strong: Style,
    /// Emphasis and the conservative superscript/subscript fallback.
    pub emphasis: Style,
    /// Struck-through content.
    pub strikethrough: Style,
    /// Inline code spans and inline/display math fallbacks.
    pub inline_code: Style,
    /// Fenced and indented code content.
    pub code_block: Style,
    /// A fenced code block's language label.
    pub code_language: Style,
    /// Syntax roles for fenced and indented code bodies.
    pub syntax: crate::tui::highlight::HighlightStyles,
    /// Markdown labels and bare HTTP/mail links.
    pub link: Style,
    /// Path-like tokens in prose.
    pub path: Style,
    /// Block-quote rails.
    pub quote_marker: Style,
    /// List bullets, ordinals, task markers, and compact table separators.
    pub list_marker: Style,
    /// Horizontal rules.
    pub rule: Style,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        Self {
            text: Style::default(),
            headings: [
                bold.add_modifier(Modifier::UNDERLINED),
                bold,
                bold,
                bold,
                bold,
                bold.add_modifier(Modifier::DIM),
            ],
            strong: bold,
            emphasis: Style::default().add_modifier(Modifier::ITALIC),
            strikethrough: Style::default().add_modifier(Modifier::CROSSED_OUT),
            inline_code: Style::default().add_modifier(Modifier::DIM),
            code_block: Style::default(),
            code_language: Style::default().add_modifier(Modifier::DIM),
            syntax: crate::tui::highlight::HighlightStyles::default(),
            link: Style::default().add_modifier(Modifier::UNDERLINED),
            path: Style::default().add_modifier(Modifier::UNDERLINED),
            quote_marker: Style::default().add_modifier(Modifier::DIM),
            list_marker: Style::default().add_modifier(Modifier::DIM),
            rule: Style::default().add_modifier(Modifier::DIM),
        }
    }
}

#[derive(Clone, Debug)]
struct StyledText {
    text: String,
    style: Style,
}

#[derive(Clone, Debug, Default)]
struct LogicalLine {
    prefix: Vec<StyledText>,
    continuation: Vec<StyledText>,
    body: Vec<StyledText>,
    /// Whitespace inside the body is content, not a wrap opportunity.
    code: bool,
    /// The line is a fenced/indented code row: it is clipped at the width
    /// rather than reflowed, so a code listing keeps its own line breaks
    /// (§5.6).
    no_reflow: bool,
    /// Leading cells of `prefix` that are a quote rail or a code rail.
    /// List markers and table rails are not decoration.
    prefix_decoration: usize,
    /// Leading cells of `continuation` that are a quote rail or a code rail.
    continuation_decoration: usize,
    /// The `┌ lang` row of a fenced block. Copy omits it when the selection
    /// covers the row through its end.
    language: bool,
    /// Unclipped fenced or indented code body, without the rail. `None` for
    /// every other row, including the language label.
    code_source: Option<String>,
}

/// What copy does with one rendered markdown row, before the transcript's
/// two-space block indent is added.
///
/// `decoration` is the width of a leading quote rail or code rail. List
/// markers and table rails are content and stay at zero.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RowCopy {
    pub(crate) decoration: u16,
    /// Unclipped code body. Copy uses it when the selection runs through the
    /// row's end; a clipped listing then pastes the whole source line.
    pub(crate) code: Option<String>,
    /// Language-label row. A selection that covers it through its end omits it.
    pub(crate) language: bool,
}

#[derive(Clone, Copy, Debug)]
enum ListKind {
    Bullet,
    Ordered(u64),
}

/// Left rail drawn down a fenced or indented code block, so a listing reads as
/// one raised object instead of as prose that happens to be monospaced.
const CODE_RAIL: &str = "\u{2502} ";
/// Opening corner of a fenced block that named its language.
const CODE_RAIL_HEAD: &str = "\u{250c} ";

/// Cells consumed by the ` │ ` separator drawn between two columns.
const TABLE_SEPARATOR_CELLS: usize = 3;
/// Cells consumed by the two outer rails and their adjacent spaces: `│ ` and ` │`.
const TABLE_EDGE_CELLS: usize = 4;
/// A column squeezed below this many cells wraps nearly every word onto its
/// own row; the table reads better as labelled records instead.
const MIN_WRAPPED_TABLE_COLUMN_CELLS: usize = 12;
/// A record value narrower than this goes on its own line under its label.
const MIN_READABLE_TABLE_COLUMN_CELLS: usize = 6;

#[derive(Debug, Default)]
struct Table {
    alignments: Vec<Alignment>,
    header: Vec<Vec<StyledText>>,
    body: Vec<Vec<Vec<StyledText>>>,
    row: Vec<Vec<StyledText>>,
    cell: Vec<StyledText>,
    in_header: bool,
}

struct CodeFence {
    language: String,
    source: String,
}

struct Renderer<'a> {
    theme: &'a MarkdownTheme,
    width: usize,
    lines: Vec<LogicalLine>,
    current: LogicalLine,
    style: Style,
    style_stack: Vec<Style>,
    lists: Vec<ListKind>,
    item_marker: Option<String>,
    item_marker_used: bool,
    quote_depth: usize,
    code: Option<CodeFence>,
    table: Option<Table>,
    /// True while the open paragraph's own line breaks are the content
    /// (§4 "Composer editing"). Set from the paragraph source on
    /// `Tag::Paragraph`; false everywhere else, so a soft break in a heading
    /// or a table cell stays a space.
    keep_paragraph_line_breaks: bool,
}

/// Render GFM-like Markdown to width-bounded Ratatui text.
///
/// Headings, nested ordered/unordered lists, block quotes, emphasis,
/// strikethrough, inline code, links, task markers, fenced/indented code, and
/// GFM tables are supported. Tables are laid out with measured columns and the
/// alignment their delimiter row declares; an over-wide table truncates per column
/// rather than wrapping. Unknown HTML is shown as inert text. Control
/// characters are replaced so model output cannot inject terminal escape
/// sequences.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn render_markdown(markdown: &str, width: u16, theme: &MarkdownTheme) -> Text<'static> {
    Text::from(render_markdown_rows(markdown, width, theme).0)
}

#[cfg_attr(
    any(test, feature = "test-support"),
    doc = "[`render_markdown`] plus the per-row copy metadata the transcript layout carries through to selection."
)]
#[cfg_attr(
    not(any(test, feature = "test-support")),
    doc = "Rendered rows plus the per-row copy metadata the transcript layout carries through to selection."
)]
pub(crate) fn render_markdown_rows(
    markdown: &str,
    width: u16,
    theme: &MarkdownTheme,
) -> (Vec<Line<'static>>, Vec<RowCopy>) {
    Renderer::new(theme, usize::from(width.max(1))).render_rows(markdown)
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a MarkdownTheme, width: usize) -> Self {
        Self {
            theme,
            width,
            lines: Vec::new(),
            current: LogicalLine::default(),
            style: theme.text,
            style_stack: Vec::new(),
            lists: Vec::new(),
            item_marker: None,
            item_marker_used: false,
            quote_depth: 0,
            code: None,
            table: None,
            keep_paragraph_line_breaks: false,
        }
    }

    fn render_rows(mut self, markdown: &str) -> (Vec<Line<'static>>, Vec<RowCopy>) {
        let options = Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TABLES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_GFM;
        // Offsets, not just events: deciding whether a paragraph's soft
        // breaks are prose wrapping or the reply's own structure needs the
        // paragraph's source.
        for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
            if matches!(event, Event::Start(Tag::Paragraph)) {
                self.keep_paragraph_line_breaks =
                    markdown.get(range).is_some_and(paragraph_keeps_line_breaks);
            }
            self.event(event);
        }
        if self.code.is_some() {
            self.emit_code_body();
        }
        self.flush();
        while self.lines.last().is_some_and(|line| line.body.is_empty()) {
            self.lines.pop();
        }
        let mut rows = Vec::new();
        for line in &self.lines {
            wrap_line(line, self.width, &mut rows);
        }
        let mut lines = Vec::with_capacity(rows.len());
        let mut copy = Vec::with_capacity(rows.len());
        for (line, meta) in rows {
            lines.push(line);
            copy.push(meta);
        }
        (lines, copy)
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if let Some(fence) = self.code.as_mut() {
                    fence.source.push_str(&text);
                } else {
                    self.push_detected(&text);
                }
            }
            Event::Code(text) | Event::InlineMath(text) => {
                self.push_styled(&text, self.style.patch(self.theme.inline_code));
            }
            Event::DisplayMath(text) => {
                self.flush();
                self.push_styled(&text, self.style.patch(self.theme.inline_code));
                self.flush();
            }
            Event::SoftBreak => {
                if self.keep_paragraph_line_breaks {
                    self.flush();
                } else {
                    self.push_styled(" ", self.style);
                }
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                let count = self.width.min(32);
                self.push_styled(&"─".repeat(count), self.theme.rule);
                self.flush();
                self.blank();
            }
            Event::TaskListMarker(checked) => {
                self.push_styled(
                    if checked { "[x] " } else { "[ ] " },
                    self.theme.list_marker,
                );
            }
            Event::FootnoteReference(text) => self.push_styled(&format!("[{text}]"), self.style),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The markdown tag dispatcher updates nesting and style state for each supported parser tag before text is emitted."
    )]
    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush();
                self.push_style(self.theme.headings[heading_index(level)]);
            }
            Tag::Emphasis | Tag::Superscript | Tag::Subscript => {
                self.push_style(self.theme.emphasis);
            }
            Tag::Strong => self.push_style(self.theme.strong),
            Tag::Strikethrough => self.push_style(self.theme.strikethrough),
            Tag::Link { .. } => self.push_style(self.theme.link),
            Tag::Image { dest_url, .. } => {
                self.push_styled("[image: ", self.style.patch(self.theme.link));
                self.push_style(if looks_like_path(&dest_url) {
                    self.theme.path
                } else {
                    self.theme.link
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::List(start) => {
                self.flush();
                self.lists
                    .push(start.map_or(ListKind::Bullet, ListKind::Ordered));
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(ListKind::Ordered(next)) => {
                        let marker = format!("{next}. ");
                        *next = next.saturating_add(1);
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.item_marker = Some(marker);
                self.item_marker_used = false;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeFence {
                    language: language.clone(),
                    source: String::new(),
                });
                // The fence's info string is chrome, not a body line —
                // a ```text fence used to print the bare word `text` as
                // content. It rides the block's opening corner instead.
                if !language.is_empty() {
                    self.current.prefix.push(StyledText {
                        text: CODE_RAIL_HEAD.to_owned(),
                        style: self.theme.quote_marker,
                    });
                    self.current.continuation.push(StyledText {
                        text: CODE_RAIL.to_owned(),
                        style: self.theme.quote_marker,
                    });
                    self.current.prefix_decoration = CODE_RAIL_HEAD.width();
                    self.current.continuation_decoration = CODE_RAIL.width();
                    self.current.language = true;
                    self.push_clean(language, self.theme.code_language);
                    self.current.code = true;
                    self.current.no_reflow = true;
                    self.flush();
                }
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.push_styled(&format!("[{label}] "), self.theme.list_marker);
            }
            Tag::Table(alignments) => {
                self.flush();
                self.table = Some(Table {
                    alignments,
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.clear();
                }
            }
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.keep_paragraph_line_breaks = false;
                if self.item_marker.is_none() {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush();
                self.blank();
            }
            TagEnd::Emphasis
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link => {
                self.pop_style();
            }
            TagEnd::Image => {
                self.pop_style();
                self.push_styled("]", self.style.patch(self.theme.link));
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.blank();
            }
            TagEnd::Item => {
                self.flush();
                self.item_marker = None;
                self.item_marker_used = false;
            }
            TagEnd::CodeBlock => {
                self.emit_code_body();
                self.current = LogicalLine::default();
                self.blank();
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut()
                    && !table.in_header
                {
                    let row = std::mem::take(&mut table.row);
                    table.body.push(row);
                }
            }
            // The head has no `TableRow`: its cells land directly in the row.
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.header = std::mem::take(&mut table.row);
                    table.in_header = false;
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(&table);
                }
                self.blank();
            }
            TagEnd::FootnoteDefinition => {
                self.flush();
                self.blank();
            }
            TagEnd::HtmlBlock
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_style(&mut self, extra: Style) {
        self.style_stack.push(self.style);
        self.style = self.style.patch(extra);
    }

    fn pop_style(&mut self) {
        self.style = self.style_stack.pop().unwrap_or(self.theme.text);
    }

    fn push_detected(&mut self, text: &str) {
        for segment in split_preserving_whitespace(text) {
            let clean = sanitize(segment).replace(['\n', '\r'], " ");
            let trimmed = clean.trim_matches(|ch: char| {
                ch.is_ascii_whitespace()
                    || matches!(ch, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';')
            });
            let style = if looks_like_url(trimmed) {
                self.style.patch(self.theme.link)
            } else if looks_like_path(trimmed) {
                self.style.patch(self.theme.path)
            } else {
                self.style
            };
            self.push_clean(clean, style);
        }
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        self.push_clean(sanitize(text), style);
    }

    fn push_clean(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(table) = self.table.as_mut() {
            if let Some(last) = table.cell.last_mut()
                && last.style == style
            {
                last.text.push_str(&text);
            } else {
                table.cell.push(StyledText { text, style });
            }
            return;
        }
        self.ensure_prefix();
        if let Some(last) = self.current.body.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
        } else {
            self.current.body.push(StyledText { text, style });
        }
    }

    fn emit_code_body(&mut self) {
        let Some(fence) = self.code.take() else {
            return;
        };
        let language = if fence.language.is_empty() {
            None
        } else {
            Some(fence.language.as_str())
        };
        // Paint from the display sanitizer (a tab is a space). Copy keeps the
        // fence line: tabs stay, a CR before LF is dropped, other controls go.
        let source = sanitize(&fence.source);
        let highlighted = crate::tui::highlight::lines(&source, language, &self.theme.syntax);
        let sources = code_source_lines(&fence.source);
        for (spans, source_line) in highlighted.into_iter().zip(sources) {
            self.ensure_code_rail();
            self.current.code = true;
            self.current.no_reflow = true;
            self.current.code_source = Some(code_copy_line(source_line));
            for span in spans {
                if !span.content.is_empty() {
                    self.push_clean(span.content.into_owned(), span.style);
                }
            }
            self.lines.push(std::mem::take(&mut self.current));
        }
    }

    fn ensure_code_rail(&mut self) {
        if !self.current.prefix.is_empty() {
            return;
        }
        let quote = "\u{2502} ".repeat(self.quote_depth);
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        let rail = format!("{quote}{indent}{CODE_RAIL}");
        let width = rail.width();
        self.current.prefix_decoration = width;
        self.current.continuation_decoration = width;
        self.current.prefix.push(StyledText {
            text: rail.clone(),
            style: self.theme.quote_marker,
        });
        self.current.continuation.push(StyledText {
            text: rail,
            style: self.theme.quote_marker,
        });
    }

    fn ensure_prefix(&mut self) {
        if !self.current.prefix.is_empty() || !self.current.body.is_empty() {
            return;
        }
        let quote = "│ ".repeat(self.quote_depth);
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        let quote_width = quote.width();
        self.current.prefix_decoration = quote_width;
        self.current.continuation_decoration = quote_width;
        if !quote.is_empty() {
            self.current.prefix.push(StyledText {
                text: quote.clone(),
                style: self.theme.quote_marker,
            });
            self.current.continuation.push(StyledText {
                text: quote,
                style: self.theme.quote_marker,
            });
        }
        if !indent.is_empty() {
            self.current.prefix.push(StyledText {
                text: indent.clone(),
                style: self.theme.list_marker,
            });
            self.current.continuation.push(StyledText {
                text: indent,
                style: self.theme.list_marker,
            });
        }
        if let Some(marker) = self.item_marker.as_ref() {
            let visible = if self.item_marker_used {
                " ".repeat(marker.width())
            } else {
                self.item_marker_used = true;
                marker.clone()
            };
            self.current.prefix.push(StyledText {
                text: visible,
                style: self.theme.list_marker,
            });
            self.current.continuation.push(StyledText {
                text: " ".repeat(marker.width()),
                style: self.theme.list_marker,
            });
        }
    }

    /// Lay a parsed GFM table out with measured, bounded columns.
    ///
    /// Columns are measured from their widest cell; when the table would
    /// exceed the content width the widest columns give up cells first and
    /// their cells wrap inside the column. A table that would squeeze some
    /// column below a readable width becomes labelled records.
    fn emit_table(&mut self, table: &Table) {
        let columns = table
            .header
            .len()
            .max(table.body.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let natural = table_natural_widths(table, columns);
        let widths = self.table_widths(&natural);
        let squeezed_unreadably = natural
            .iter()
            .zip(&widths)
            .any(|(natural, fitted)| fitted < natural && *fitted < MIN_WRAPPED_TABLE_COLUMN_CELLS);
        let separators = TABLE_SEPARATOR_CELLS
            .saturating_mul(columns - 1)
            .saturating_add(TABLE_EDGE_CELLS);
        let overflows = widths.iter().sum::<usize>().saturating_add(separators) > self.width;
        if squeezed_unreadably || overflows {
            self.emit_stacked_table(table, columns);
            return;
        }
        self.lines.push(self.table_rule(&widths, '┌', '┬', '┐'));
        if !table.header.is_empty() {
            let header = self.table_row(&table.header, &widths, &table.alignments);
            self.lines.extend(header);
            if !table.body.is_empty() {
                self.lines.push(self.table_rule(&widths, '├', '┼', '┤'));
            }
        }
        for (index, row) in table.body.iter().enumerate() {
            if index > 0 {
                self.lines.push(self.table_rule(&widths, '├', '┼', '┤'));
            }
            let lines = self.table_row(row, &widths, &table.alignments);
            self.lines.extend(lines);
        }
        self.lines.push(self.table_rule(&widths, '└', '┴', '┘'));
    }

    /// Narrow tables become labelled records. Unlike one-cell-wide grids this
    /// keeps both the field name and value readable, while ordinary wrapping
    /// still bounds every emitted line to the pane.
    fn emit_stacked_table(&mut self, table: &Table, columns: usize) {
        if table.body.is_empty() {
            for heading in &table.header {
                self.lines.push(LogicalLine {
                    body: heading.clone(),
                    ..LogicalLine::default()
                });
            }
            return;
        }
        for (row_index, row) in table.body.iter().enumerate() {
            if row_index > 0 {
                self.lines.push(LogicalLine {
                    body: vec![StyledText {
                        text: "─".repeat(self.width),
                        style: self.theme.list_marker,
                    }],
                    no_reflow: true,
                    ..LogicalLine::default()
                });
            }
            for index in 0..columns {
                let Some(cell) = row.get(index).filter(|cell| !cell.is_empty()) else {
                    continue;
                };
                let label = table.header.get(index).filter(|label| !label.is_empty());
                let mut prefix = label.cloned().unwrap_or_default();
                if label.is_some() {
                    prefix.push(StyledText {
                        text: ": ".to_owned(),
                        style: self.theme.list_marker,
                    });
                }
                let mut indent: usize = prefix.iter().map(|part| part.text.width()).sum();
                if indent.saturating_add(MIN_READABLE_TABLE_COLUMN_CELLS) > self.width {
                    if let Some(last) = prefix.last_mut() {
                        last.text = last.text.trim_end().to_owned();
                    }
                    self.lines.push(LogicalLine {
                        body: std::mem::take(&mut prefix),
                        ..LogicalLine::default()
                    });
                    indent = 2;
                    prefix.push(StyledText {
                        text: "  ".to_owned(),
                        style: self.theme.text,
                    });
                }
                self.lines.push(LogicalLine {
                    prefix,
                    continuation: vec![StyledText {
                        text: " ".repeat(indent.min(self.width.saturating_sub(1))),
                        style: self.theme.text,
                    }],
                    body: cell.clone(),
                    code: false,
                    no_reflow: false,
                    ..LogicalLine::default()
                });
            }
        }
    }

    fn table_widths(&self, natural: &[usize]) -> Vec<usize> {
        let columns = natural.len();
        let separators = TABLE_SEPARATOR_CELLS
            .saturating_mul(columns.saturating_sub(1))
            .saturating_add(TABLE_EDGE_CELLS);
        let available = self.width.saturating_sub(separators).max(columns);
        fit_column_widths(natural, available)
    }

    /// One logical table row as one or more pane lines: every cell wraps
    /// within its own column, so content is never cut and a row never wraps
    /// at the pipe.
    fn table_row(
        &self,
        row: &[Vec<StyledText>],
        widths: &[usize],
        alignments: &[Alignment],
    ) -> Vec<LogicalLine> {
        let empty: Vec<StyledText> = Vec::new();
        let wrapped: Vec<Vec<CellLine>> = widths
            .iter()
            .enumerate()
            .map(|(index, width)| wrap_cell(row.get(index).unwrap_or(&empty), *width))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        (0..height)
            .map(|line| {
                let mut body = vec![StyledText {
                    text: "│ ".to_owned(),
                    style: self.theme.list_marker,
                }];
                for (index, width) in widths.iter().enumerate() {
                    if index > 0 {
                        body.push(StyledText {
                            text: " │ ".to_owned(),
                            style: self.theme.list_marker,
                        });
                    }
                    let alignment = alignments.get(index).copied().unwrap_or(Alignment::None);
                    let cell = wrapped
                        .get(index)
                        .and_then(|cell| cell.get(line))
                        .cloned()
                        .unwrap_or_default();
                    body.extend(self.pad_cell(cell, *width, alignment));
                }
                body.push(StyledText {
                    text: " │".to_owned(),
                    style: self.theme.list_marker,
                });
                LogicalLine {
                    prefix: Vec::new(),
                    continuation: Vec::new(),
                    body,
                    code: true,
                    // Tables are already width-fitted. Reflow splits on the spaces
                    // around `│` and slams columns onto the next row.
                    no_reflow: true,
                    ..LogicalLine::default()
                }
            })
            .collect()
    }

    fn table_rule(&self, widths: &[usize], left: char, join: char, right: char) -> LogicalLine {
        let middle = widths
            .iter()
            .map(|width| "─".repeat(width + 2))
            .collect::<Vec<_>>()
            .join(&join.to_string());
        let text = format!("{left}{middle}{right}");
        LogicalLine {
            prefix: Vec::new(),
            continuation: Vec::new(),
            body: vec![StyledText {
                text,
                style: self.theme.list_marker,
            }],
            code: true,
            no_reflow: true,
            ..LogicalLine::default()
        }
    }

    fn pad_cell(&self, cell: CellLine, width: usize, alignment: Alignment) -> Vec<StyledText> {
        let pad = width.saturating_sub(cell.used);
        let (left, right) = match alignment {
            Alignment::Right => (pad, 0),
            Alignment::Center => (pad / 2, pad - pad / 2),
            Alignment::Left | Alignment::None => (0, pad),
        };
        let mut fitted = Vec::new();
        if left > 0 {
            fitted.push(StyledText {
                text: " ".repeat(left),
                style: self.theme.text,
            });
        }
        fitted.extend(cell.parts);
        if right > 0 {
            fitted.push(StyledText {
                text: " ".repeat(right),
                style: self.theme.text,
            });
        }
        fitted
    }

    fn flush(&mut self) {
        if self.current.body.is_empty() {
            return;
        }
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|line| !line.body.is_empty()) {
            self.lines.push(LogicalLine::default());
        }
    }
}

/// Longest line a paragraph may hold and still be read as line-per-item.
///
/// Prose is wrapped at a column, and no model wraps below this. A paragraph
/// whose every line is shorter than this was not produced by wrapping, so its
/// line breaks are the content.
const LINE_PER_ITEM_MAX_CHARS: usize = 48;

/// Whether a paragraph's single newlines are structure rather than wrapping.
///
/// `CommonMark` folds a soft break to a space, which is right for flowed prose
/// and wrong for the line-per-item replies a coding harness gets constantly —
/// file lists, checklists, step plans, `count from 1 to 50, each number on its
/// own line`. Wrapping is what this distinguishes: a wrapped paragraph has
/// every line but the last close to its wrap column, so a paragraph of two or
/// more lines that are all short, or that all open with a bullet or checkbox
/// `CommonMark` did not turn into a list, keeps its breaks.
fn paragraph_keeps_line_breaks(source: &str) -> bool {
    let lines: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() < 2 {
        return false;
    }
    lines
        .iter()
        .all(|line| line.chars().count() <= LINE_PER_ITEM_MAX_CHARS)
        || lines.iter().all(|line| opens_an_item(line))
}

/// A bullet or checkbox that `CommonMark` left as plain text (`-`, `*`, `+` and
/// `1.` are already lists, so they never reach here).
fn opens_an_item(line: &str) -> bool {
    line.starts_with([
        '\u{2022}', '\u{2023}', '\u{25aa}', '\u{25e6}', '\u{2013}', '\u{2014}',
    ]) || line.starts_with("[ ] ")
        || line.starts_with("[x] ")
        || line.starts_with("[X] ")
}

fn heading_index(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 0,
        HeadingLevel::H2 => 1,
        HeadingLevel::H3 => 2,
        HeadingLevel::H4 => 3,
        HeadingLevel::H5 => 4,
        HeadingLevel::H6 => 5,
    }
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\t' | '\r' => ' ',
            '\n' => ch,
            ch if ch.is_control() => '�',
            ch => ch,
        })
        .collect()
}

/// One fence line for [`RowCopy`]: keep tabs, drop the CR of a CRLF ending,
/// and replace every other control. The painted row still uses [`sanitize`].
fn code_copy_line(line: &str) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    line.chars()
        .map(|ch| match ch {
            '\t' => ch,
            ch if ch.is_control() => '�',
            ch => ch,
        })
        .collect()
}

fn split_preserving_whitespace(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut whitespace = text.chars().next().is_some_and(char::is_whitespace);
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() != whitespace {
            result.push(&text[start..index]);
            start = index;
            whitespace = ch.is_whitespace();
        }
    }
    if start < text.len() {
        result.push(&text[start..]);
    }
    result
}

fn looks_like_url(text: &str) -> bool {
    text.starts_with("https://") || text.starts_with("http://") || text.starts_with("mailto:")
}

fn looks_like_path(text: &str) -> bool {
    if text.is_empty() || looks_like_url(text) {
        return false;
    }
    text.starts_with('/')
        || text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with("~/")
        || (text.contains('/') && !text.contains("//") && !text.chars().any(char::is_whitespace))
}

fn cells_u16(cells: usize) -> u16 {
    u16::try_from(cells).unwrap_or(u16::MAX)
}

/// Source lines of a fenced or indented body, in the same order
/// [`crate::tui::highlight::lines`] paints them. A trailing newline the fence
/// itself adds is not a row; a blank line inside the body is.
fn code_source_lines(source: &str) -> Vec<&str> {
    let trimmed = source.trim_end_matches('\n');
    if trimmed.is_empty() {
        return if source.is_empty() {
            Vec::new()
        } else {
            vec![""]
        };
    }
    let Some((head, last)) = trimmed.rsplit_once('\n') else {
        return vec![trimmed];
    };
    let mut lines: Vec<&str> = head.split('\n').collect();
    lines.push(last);
    lines
}

fn copy_row(decoration: usize, code: Option<String>, language: bool) -> RowCopy {
    RowCopy {
        decoration: cells_u16(decoration),
        code,
        language,
    }
}

fn wrap_line(line: &LogicalLine, width: usize, output: &mut Vec<(Line<'static>, RowCopy)>) {
    // A code listing owns its own line breaks. Reflowing it at the pane width
    // re-writes the program the model actually produced, so an over-wide row
    // is clipped at the rail instead.
    if line.no_reflow {
        let prefix = fit_spans(styled_spans(&line.prefix), width);
        let prefix_cells = spans_width(&prefix);
        let mut spans = prefix;
        let used = prefix_cells;
        spans.extend(fit_spans(
            styled_spans(&line.body),
            width.saturating_sub(used),
        ));
        output.push((
            Line::from(spans),
            copy_row(
                line.prefix_decoration.min(prefix_cells),
                line.code_source.clone(),
                line.language,
            ),
        ));
        return;
    }
    let indent_width = width.saturating_sub(1);
    let mut spans = fit_spans(styled_spans(&line.prefix), indent_width);
    let mut used = spans_width(&spans);
    let mut decoration = line.prefix_decoration.min(used);
    let continuation = fit_spans(styled_spans(&line.continuation), indent_width);
    let continuation_width = spans_width(&continuation);
    let continuation_decoration = line.continuation_decoration.min(continuation_width);
    let mut pending_space = None;
    let mut word: Vec<(&str, Style, usize)> = Vec::new();
    let mut place_word = |word: &mut Vec<(&str, Style, usize)>,
                          pending_space: &mut Option<Style>| {
        if word.is_empty() {
            return;
        }
        let word_width = word
            .iter()
            .fold(0usize, |total, (_, _, cells)| total.saturating_add(*cells));
        let separator = usize::from(pending_space.is_some() && used > continuation_width);
        if used > continuation_width
            && used.saturating_add(separator).saturating_add(word_width) > width
        {
            output.push((
                Line::from(std::mem::take(&mut spans)),
                copy_row(decoration, None, false),
            ));
            spans.clone_from(&continuation);
            used = continuation_width;
            decoration = continuation_decoration;
            *pending_space = None;
        } else if let Some(style) = pending_space.take()
            && used > continuation_width
            && used < width
        {
            spans.push(Span::styled(" ".to_owned(), style));
            used += 1;
        }
        for (grapheme, style, cells) in word.drain(..) {
            if used > continuation_width && used.saturating_add(cells) > width {
                output.push((
                    Line::from(std::mem::take(&mut spans)),
                    copy_row(decoration, None, false),
                ));
                spans.clone_from(&continuation);
                used = continuation_width;
                decoration = continuation_decoration;
            }
            if let Some(span) = spans.last_mut().filter(|span| span.style == style) {
                span.content.to_mut().push_str(grapheme);
            } else {
                spans.push(Span::styled(grapheme.to_owned(), style));
            }
            used = used.saturating_add(cells);
        }
    };

    for part in &line.body {
        for grapheme in part.text.graphemes(true) {
            if grapheme.chars().all(char::is_whitespace) {
                place_word(&mut word, &mut pending_space);
                if line.code {
                    word.push((grapheme, part.style, grapheme.width().max(1)));
                    place_word(&mut word, &mut pending_space);
                } else {
                    pending_space = Some(part.style);
                }
            } else {
                word.push((grapheme, part.style, grapheme.width().max(1)));
            }
        }
    }
    place_word(&mut word, &mut pending_space);
    output.push((Line::from(spans), copy_row(decoration, None, false)));
}

fn styled_spans(parts: &[StyledText]) -> Vec<Span<'static>> {
    parts
        .iter()
        .map(|part| Span::styled(part.text.clone(), part.style))
        .collect()
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut fitted = Vec::new();
    let mut remaining = width;
    for span in spans {
        let (head, _) = split_at_width(span.content.as_ref(), remaining);
        if !head.is_empty() {
            remaining = remaining.saturating_sub(head.width());
            fitted.push(Span::styled(head.to_owned(), span.style));
        }
        if remaining == 0 {
            break;
        }
    }
    fitted
}

/// Each column's widest cell, at least one cell.
fn table_natural_widths(table: &Table, columns: usize) -> Vec<usize> {
    let mut widths = vec![1usize; columns];
    for row in std::iter::once(&table.header).chain(table.body.iter()) {
        for (index, cell) in row.iter().enumerate().take(columns) {
            let cells = cell.iter().fold(0usize, |total, part| {
                // Measured as `wrap_cell` places them: every grapheme takes at
                // least one cell.
                let width: usize = part
                    .text
                    .graphemes(true)
                    .map(|grapheme| grapheme.width().max(1))
                    .sum();
                total.saturating_add(width)
            });
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cells);
            }
        }
    }
    widths
}

/// Column widths that fit `available` cells. Columns narrower than an even
/// share keep their natural width; the rest split what remains, so a short
/// column is never squeezed to make room for a long one.
fn fit_column_widths(natural: &[usize], available: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= available {
        return natural.to_vec();
    }
    let mut widths = vec![0usize; natural.len()];
    let mut open: Vec<usize> = (0..natural.len()).collect();
    let mut remaining = available;
    loop {
        let share = remaining / open.len().max(1);
        let (fits, wide): (Vec<usize>, Vec<usize>) =
            open.iter().partition(|&&index| natural[index] <= share);
        if fits.is_empty() {
            let mut extra = remaining.saturating_sub(share.saturating_mul(wide.len()));
            for index in wide {
                widths[index] = share.max(1) + usize::from(extra > 0);
                extra = extra.saturating_sub(1);
            }
            return widths;
        }
        for index in fits {
            widths[index] = natural[index];
            remaining = remaining.saturating_sub(natural[index]);
        }
        open = wide;
        if open.is_empty() {
            return widths;
        }
    }
}

/// One wrapped line of a table cell and the terminal cells it occupies.
#[derive(Clone, Debug, Default)]
struct CellLine {
    parts: Vec<StyledText>,
    used: usize,
}

impl CellLine {
    fn push(&mut self, grapheme: &str, style: Style, cells: usize) {
        if let Some(last) = self.parts.last_mut().filter(|part| part.style == style) {
            last.text.push_str(grapheme);
        } else {
            self.parts.push(StyledText {
                text: grapheme.to_owned(),
                style,
            });
        }
        self.used = self.used.saturating_add(cells);
    }
}

/// Word-wrap a styled cell to `width` cells; a word longer than the column
/// splits on grapheme boundaries.
fn wrap_cell(cell: &[StyledText], width: usize) -> Vec<CellLine> {
    let width = width.max(1);
    let mut lines = vec![CellLine::default()];
    let mut word: Vec<(&str, Style, usize)> = Vec::new();
    let mut pending_space: Option<Style> = None;
    let place = |word: &mut Vec<(&str, Style, usize)>,
                 pending_space: &mut Option<Style>,
                 lines: &mut Vec<CellLine>| {
        if word.is_empty() {
            return;
        }
        let word_width: usize = word.iter().map(|(_, _, cells)| cells).sum();
        let used = lines.last().map_or(0, |line| line.used);
        let space = pending_space.take().filter(|_| used > 0);
        if used > 0 && used + usize::from(space.is_some()) + word_width > width {
            lines.push(CellLine::default());
        } else if let (Some(style), Some(line)) = (space, lines.last_mut()) {
            line.push(" ", style, 1);
        }
        for (grapheme, style, cells) in word.drain(..) {
            if lines
                .last()
                .is_some_and(|line| line.used > 0 && line.used + cells > width)
            {
                lines.push(CellLine::default());
            }
            if let Some(line) = lines.last_mut() {
                line.push(grapheme, style, cells);
            }
        }
    };
    for part in cell {
        for grapheme in part.text.graphemes(true) {
            if grapheme.chars().all(char::is_whitespace) {
                place(&mut word, &mut pending_space, &mut lines);
                pending_space = Some(part.style);
            } else {
                word.push((grapheme, part.style, grapheme.width().max(1)));
            }
        }
    }
    place(&mut word, &mut pending_space, &mut lines);
    lines
}

fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    if width == 0 {
        return ("", text);
    }
    let mut used = 0;
    let mut end = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = grapheme.width().max(1);
        if used + grapheme_width > width {
            break;
        }
        used += grapheme_width;
        end = index + grapheme.len();
    }
    text.split_at(end)
}

#[cfg(test)]
mod tests;
