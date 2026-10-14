//! Compact, presentation-only Markdown rendering for terminal transcript text.
//!
//! The renderer deliberately owns no theme selection, link activation, or
//! transcript state. Callers provide semantic styles and a cell width, then
//! receive inert Ratatui text suitable for scrollback or a paragraph widget.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
#[cfg(any(test, feature = "test-support"))]
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

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
#[derive(Clone, Debug, Default)]
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

/// The rendered rows of a streaming answer's settled prefix.
///
/// See [`render_markdown_streaming`]. Holds nothing but a pure function's
/// last result, so any mismatch simply renders again.
#[derive(Clone, Debug, Default)]
pub struct StreamingMarkdown {
    prefix: String,
    width: u16,
    theme: Option<MarkdownTheme>,
    lines: Vec<Line<'static>>,
    copy: Vec<RowCopy>,
}

impl StreamingMarkdown {
    /// Bytes of settled prefix the last call reused or rendered.
    #[cfg(test)]
    pub(crate) fn settled_bytes(&self) -> usize {
        self.prefix.len()
    }
}

/// One [`render_markdown_streaming`] result.
#[derive(Debug)]
pub struct StreamedMarkdown<'m> {
    /// Rows of the settled prefix.
    pub settled: &'m [Line<'static>],
    /// Copy metadata parallel to [`Self::settled`].
    pub settled_copy: &'m [RowCopy],
    /// How many leading `settled` rows are exactly the previous call's.
    pub settled_kept: usize,
    /// The blank row the settled prefix owes the rest, when both have rows,
    /// then the rows of the text after the prefix.
    pub rest: Vec<Line<'static>>,
    /// Copy metadata parallel to [`Self::rest`].
    pub rest_copy: Vec<RowCopy>,
}

impl StreamedMarkdown<'_> {
    /// Every row, as [`render_markdown`] returns them.
    #[cfg(test)]
    #[must_use]
    pub fn into_text(self) -> Text<'static> {
        let mut lines = self.settled.to_vec();
        lines.extend(self.rest);
        Text::from(lines)
    }
}

#[cfg_attr(
    any(test, feature = "test-support"),
    doc = "[`render_markdown`] for text that only ever grows at its end."
)]
#[cfg_attr(
    not(any(test, feature = "test-support")),
    doc = "`render_markdown` for text that only ever grows at its end."
)]
///
/// The text is cut at its last top-level block boundary (see
/// [`settled_boundary`]). The part before it is the settled prefix: it renders
/// once, and when the boundary moves on, only the text between the old and the
/// new boundary renders and is appended. Only the part after the boundary
/// renders on every call. `settled` followed by `rest` equals
/// `render_markdown(markdown, width, theme)`.
pub fn render_markdown_streaming<'m>(
    markdown: &str,
    width: u16,
    theme: &MarkdownTheme,
    memo: &'m mut StreamingMarkdown,
) -> StreamedMarkdown<'m> {
    let boundary = settled_boundary(markdown).unwrap_or(0);
    let (prefix, suffix) = markdown.split_at(boundary);
    // The memoized prefix is reusable when this text extends it and it still
    // ends on a boundary here: its bytes are the same, so only the line that
    // follows it can differ.
    let extends = memo.width == width
        && memo.theme.as_ref() == Some(theme)
        && prefix.starts_with(memo.prefix.as_str())
        && (memo.prefix.is_empty()
            || markdown[memo.prefix.len()..]
                .lines()
                .next()
                .is_some_and(starts_top_level_block));
    let settled_kept = if extends {
        let kept = memo.lines.len();
        let delta = &prefix[memo.prefix.len()..];
        if !delta.is_empty() {
            let (rows, copy) = render_markdown_rows(delta, width, theme);
            if !memo.lines.is_empty() && !rows.is_empty() {
                memo.lines.push(Line::default());
                memo.copy.push(RowCopy::default());
            }
            memo.lines.extend(rows);
            memo.copy.extend(copy);
            memo.prefix.push_str(delta);
        }
        kept
    } else {
        let (lines, copy) = if prefix.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            render_markdown_rows(prefix, width, theme)
        };
        memo.lines = lines;
        memo.copy = copy;
        prefix.clone_into(&mut memo.prefix);
        memo.width = width;
        memo.theme = Some(*theme);
        0
    };
    let (mut rest, mut rest_copy) = render_markdown_rows(suffix, width, theme);
    // Every top-level block ends with one blank row, and a render trims only
    // the trailing ones, so the prefix's last block owes the suffix that row.
    if !memo.lines.is_empty() && !rest.is_empty() {
        rest.insert(0, Line::default());
        rest_copy.insert(0, RowCopy::default());
    }
    StreamedMarkdown {
        settled: &memo.lines,
        settled_copy: &memo.copy,
        settled_kept,
        rest,
        rest_copy,
    }
}

/// The byte offset of the last line that is certain to open a new top-level
/// block whatever follows it, so the text before it renders the same alone as
/// it does in the whole.
///
/// That line starts in column 0 with a character that cannot continue a list,
/// a quote, a table or indented code, follows a blank line, and sits outside
/// any fence. Constructs that can reach back across a blank line end the
/// search: raw HTML blocks and indented fences stop it where they start, and a
/// link reference definition anywhere disables the split, because it changes
/// how earlier text renders.
fn settled_boundary(markdown: &str) -> Option<usize> {
    let mut boundary = None;
    let mut fence: Option<(u8, usize)> = None;
    let mut after_blank = false;
    let mut offset = 0;
    for line in markdown.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        let indent = text.len() - text.trim_start_matches(' ').len();
        let body = &text[indent..];
        if is_reference_definition(body) {
            return None;
        }
        if let Some((marker, length)) = fence {
            if indent <= 3 && closes_fence(body, marker, length) {
                fence = None;
            }
            after_blank = false;
            continue;
        }
        if body.trim().is_empty() {
            after_blank = true;
            continue;
        }
        let opens = opens_fence(body);
        if (indent > 0 && (opens.is_some() || indent <= 3 && body.starts_with('<')))
            || (indent == 0 && body.starts_with('<'))
        {
            break;
        }
        if after_blank && indent == 0 && starts_top_level_block(body) {
            boundary = Some(start);
        }
        after_blank = false;
        fence = opens;
    }
    // The prefix must end in the blank line before the boundary, never be the
    // whole text.
    boundary.filter(|at| *at > 0)
}

fn starts_top_level_block(line: &str) -> bool {
    line.chars().next().is_some_and(|first| {
        !first.is_whitespace()
            && !first.is_ascii_digit()
            && !matches!(first, '-' | '+' | '*' | '>' | '|' | '<' | '[' | ':' | '=')
    })
}

fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let marker = *line.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = line.bytes().take_while(|byte| *byte == marker).count();
    if length < 3 || (marker == b'`' && line[length..].contains('`')) {
        return None;
    }
    Some((marker, length))
}

fn closes_fence(line: &str, marker: u8, length: usize) -> bool {
    let run = line.bytes().take_while(|byte| *byte == marker).count();
    run >= length && line[run..].trim().is_empty()
}

fn is_reference_definition(line: &str) -> bool {
    line.starts_with('[')
        && line
            .find("]:")
            .is_some_and(|close| close > 1 && !line[1..close].contains(']'))
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
mod tests {
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

    /// A code listing owns its own line breaks: reflowing it at the pane width
    /// re-writes the program the model produced. An over-wide row is clipped
    /// at the rail instead, and the row count matches the source's.
    #[test]
    fn a_code_block_is_clipped_at_the_width_and_never_reflowed() {
        let long = "let x = collection.iter().map(|item| item.value).sum::<u64>();";
        let source = format!("```\n{long}\nshort\n```");
        let lines = plain(&render_markdown(&source, 24, &MarkdownTheme::default()));
        let code = lines
            .iter()
            .filter(|line| line.starts_with('│'))
            .collect::<Vec<_>>();
        assert_eq!(code.len(), 2, "one painted row per source row: {lines:?}");
        assert!(code[0].starts_with("│ let x = collection.i"), "{code:?}");
        assert_eq!(code[1], "│ short");
        assert!(lines.iter().all(|line| line.width() <= 24), "{lines:?}");
    }

    /// Copy metadata is not inferred from the painted cells: a code rail and a
    /// quote rail are decoration, a clipped code row still names its source,
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
plak logout claude --local
    plak logout codex --local
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
        assert_eq!(
            copy[claude].code.as_deref(),
            Some("plak logout claude --local")
        );
        let codex = at("plak logout codex");
        assert_eq!(
            copy[codex].code.as_deref(),
            Some("    plak logout codex --local")
        );

        let clipped_line = "let x = collection.iter().map(|item| item.value).sum::<u64>();";
        let (clipped, clipped_copy) = render_markdown_rows(
            &format!("```\n{clipped_line}\nshort\n```"),
            24,
            &MarkdownTheme::default(),
        );
        let wide = clipped
            .iter()
            .position(|line| line.to_string().starts_with("│ let"))
            .expect("clipped code row");
        assert!(clipped[wide].to_string().width() <= 24);
        assert!(!clipped[wide].to_string().contains(clipped_line));
        assert_eq!(clipped_copy[wide].code.as_deref(), Some(clipped_line));
        assert!(!clipped_copy[wide].language);
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
}
