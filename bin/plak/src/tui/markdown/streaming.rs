//! Streaming Markdown: the settled prefix of an answer that only grows at
//! its end renders once, and only the text after its last top-level block
//! boundary renders again on every call.

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag};
#[cfg(test)]
use ratatui::text::Text;
use ratatui::text::{Line, Span};

use super::{
    MarkdownTheme, Renderer, RowCopy, code_copy_line, render_markdown_rows, sanitize, wrap_line,
};

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
    source: String,
    scan: BoundaryScan,
    open: Option<OpenFence>,
    fence_prefix: bool,
    visible_rows: usize,
    #[cfg(test)]
    work: usize,
}

impl StreamingMarkdown {
    #[cfg(test)]
    pub(crate) fn work(&self) -> usize {
        self.work
    }

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
    /// Every row, as [`render_markdown`](super::render_markdown) returns them.
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
    doc = "[`render_markdown`](super::render_markdown) for text that only ever grows at its end."
)]
#[cfg_attr(
    not(any(test, feature = "test-support")),
    doc = "`render_markdown` for text that only ever grows at its end."
)]
///
/// The text is cut at its last top-level block boundary (see
/// `BoundaryScan`). The part before it is the settled prefix: it renders
/// once, and when the boundary moves on, only the text between the old and the
/// new boundary renders and is appended. Only the part after the boundary
/// renders on every call. Within a column-zero open fence, completed body
/// rows settle too; only new lines and the unfinished last line are highlighted.
/// `settled` followed by `rest` equals
/// `render_markdown(markdown, width, theme)`.
#[allow(
    clippy::too_many_lines,
    reason = "Streaming markdown tracks open fences, provisional trailing blocks, and completed output in one parser pass."
)]
pub fn render_markdown_streaming<'m>(
    markdown: &str,
    width: u16,
    theme: &MarkdownTheme,
    memo: &'m mut StreamingMarkdown,
) -> StreamedMarkdown<'m> {
    let previous_settled = memo.visible_rows;
    let valid = memo.width == width
        && memo.theme.as_ref() == Some(theme)
        && markdown.starts_with(&memo.source);
    if !valid {
        memo.scan = BoundaryScan::default();
    }
    let mut scan = memo.scan.clone();
    for line in markdown[memo.scan.offset..].split_inclusive('\n') {
        scan.line(line);
        #[cfg(test)]
        {
            memo.work += 1;
        }
        if line.ends_with('\n') {
            memo.scan = scan.clone();
        }
    }
    // A completed closing line seals the cached body without parsing it again.
    if valid
        && !scan.disabled
        && let Some((start, close_start, close_end)) = scan.closed
        && markdown[..close_end].ends_with('\n')
        && memo.open.as_ref().is_some_and(|open| open.start == start)
        && let Some(mut open) = memo.open.take()
    {
        let (_, _, _, visited) = open.render(
            &markdown[..close_start],
            width,
            theme,
            &mut memo.lines,
            &mut memo.copy,
        );
        #[cfg(test)]
        {
            memo.work += visited;
        }
        #[cfg(not(test))]
        let _ = visited;
        memo.lines.truncate(open.complete_rows);
        memo.copy.truncate(open.complete_rows);
        markdown[..close_end].clone_into(&mut memo.prefix);
        memo.fence_prefix = true;
    }
    if (!valid || memo.open.as_ref().map(|open| open.start) != scan.open)
        && let Some(open) = memo.open.take()
    {
        memo.lines.truncate(open.base_rows);
        memo.copy.truncate(open.base_rows);
    }
    if valid {
        memo.source.push_str(&markdown[memo.source.len()..]);
    } else {
        markdown.clone_into(&mut memo.source);
    }
    let boundary =
        scan.boundary
            .unwrap_or(0)
            .max(if valid && memo.fence_prefix && !scan.disabled {
                memo.prefix.len()
            } else {
                0
            });
    let (prefix, suffix) = markdown.split_at(boundary);
    // The memoized prefix is reusable when this text extends it and it still
    // ends on a boundary here: its bytes are the same, so only the line that
    // follows it can differ.
    let extends = memo.width == width
        && memo.theme.as_ref() == Some(theme)
        && prefix.starts_with(memo.prefix.as_str())
        && (memo.fence_prefix
            || memo.prefix.is_empty()
            || markdown[memo.prefix.len()..]
                .lines()
                .next()
                .is_some_and(starts_top_level_block));
    let mut settled_kept = if extends {
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
        memo.fence_prefix = false;
        memo.lines = lines;
        memo.copy = copy;
        prefix.clone_into(&mut memo.prefix);
        memo.width = width;
        memo.theme = Some(*theme);
        0
    };
    if let Some(start) = scan.open
        && let Some((marker, length)) = scan.fence
        && let Some(header_end) = markdown[start..].find('\n').map(|at| start + at + 1)
    {
        if memo.open.is_none() {
            let language = Parser::new(&markdown[start..header_end])
                .find_map(|event| {
                    if let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) = event {
                        Some(info.split_whitespace().next().unwrap_or("").to_owned())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let header = format!("{}\n", char::from(marker).to_string().repeat(length));
            let base_rows = memo.lines.len();
            let (rows, copy) = render_markdown_rows(&markdown[start..header_end], width, theme);
            let gap = !memo.lines.is_empty() && rows.is_empty();
            if !memo.lines.is_empty() && !rows.is_empty() {
                memo.lines.push(Line::default());
                memo.copy.push(RowCopy::default());
            }
            memo.lines.extend(rows);
            memo.copy.extend(copy);
            let complete_rows = memo.lines.len();
            if gap {
                memo.lines.push(Line::default());
                memo.copy.push(RowCopy::default());
            }
            memo.open = Some(OpenFence {
                start,
                header,
                offset: header_end,
                base_rows,
                complete_rows,
                highlight: crate::tui::highlight::LineHighlighter::new(&language),
            });
        } else {
            settled_kept = previous_settled;
        }
        if let Some(open) = memo.open.as_mut() {
            let (rest, rest_copy, visible_rows, visited) =
                open.render(markdown, width, theme, &mut memo.lines, &mut memo.copy);
            #[cfg(test)]
            {
                memo.work += visited;
            }
            #[cfg(not(test))]
            let _ = visited;
            memo.visible_rows = visible_rows;
            return StreamedMarkdown {
                settled: &memo.lines[..visible_rows],
                settled_copy: &memo.copy[..visible_rows],
                settled_kept: settled_kept.min(previous_settled).min(visible_rows),
                rest,
                rest_copy,
            };
        }
    }
    let (mut rest, mut rest_copy) = render_markdown_rows(suffix, width, theme);
    // Every top-level block ends with one blank row, and a render trims only
    // the trailing ones, so the prefix's last block owes the suffix that row.
    if !memo.lines.is_empty() && !rest.is_empty() {
        rest.insert(0, Line::default());
        rest_copy.insert(0, RowCopy::default());
    }
    memo.visible_rows = memo.lines.len();
    StreamedMarkdown {
        settled: &memo.lines,
        settled_copy: &memo.copy,
        settled_kept: settled_kept.min(previous_settled),
        rest,
        rest_copy,
    }
}

/// Resumable search for the last certain top-level block boundary. Only
/// complete lines commit scan state; a partial line is reconsidered next frame.
///
/// That line starts in column 0 with a character that cannot continue a list,
/// a quote, a table or indented code, follows a blank line, and sits outside
/// any fence. A column-zero fence also interrupts a paragraph without a blank.
/// Constructs that can reach back across a blank line end the
/// search: raw HTML blocks and indented fences stop it where they start, and a
/// link reference definition anywhere disables the split, because it changes
/// how earlier text renders.
#[derive(Clone, Debug, Default)]
struct BoundaryScan {
    offset: usize,
    boundary: Option<usize>,
    fence: Option<(u8, usize)>,
    after_blank: bool,
    stopped: bool,
    disabled: bool,
    open: Option<usize>,
    closed: Option<(usize, usize, usize)>,
}

impl BoundaryScan {
    fn line(&mut self, line: &str) {
        let start = self.offset;
        self.offset += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        let indent = text.len() - text.trim_start_matches(' ').len();
        let body = &text[indent..];
        if is_reference_definition(body) || text.contains('\r') {
            self.disabled = true;
            self.boundary = None;
            self.open = None;
        }
        if self.stopped || self.disabled {
            return;
        }
        if let Some((marker, length)) = self.fence {
            if indent <= 3 && closes_fence(body, marker, length) {
                self.closed = self.open.map(|open| (open, start, self.offset));
                self.fence = None;
                self.open = None;
            }
            self.after_blank = false;
            return;
        }
        if body.trim().is_empty() {
            self.after_blank = true;
            return;
        }
        let opens = opens_fence(body);
        if (indent > 0 && (opens.is_some() || indent <= 3 && body.starts_with('<')))
            || (indent == 0 && body.starts_with('<'))
        {
            self.stopped = true;
            return;
        }
        if self.after_blank && indent == 0 && starts_top_level_block(body) {
            self.boundary = Some(start);
        }
        self.after_blank = false;
        self.fence = opens;
        if opens.is_some() && indent == 0 {
            self.open = Some(start);
            // A column-zero fence interrupts a paragraph, even without a blank.
            if start > 0 {
                self.boundary = Some(start);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct OpenFence {
    start: usize,
    header: String,
    offset: usize,
    base_rows: usize,
    complete_rows: usize,
    highlight: crate::tui::highlight::LineHighlighter,
}

impl OpenFence {
    fn render(
        &mut self,
        markdown: &str,
        width: u16,
        theme: &MarkdownTheme,
        lines: &mut Vec<Line<'static>>,
        copy_rows: &mut Vec<RowCopy>,
    ) -> (Vec<Line<'static>>, Vec<RowCopy>, usize, usize) {
        // Cache blank rows too, but expose them only when later content makes
        // them visible. In particular, a partial CR may disappear on the LF.
        let mut visited = 0;
        let mut rest = Vec::new();
        let mut rest_copy = Vec::new();
        let mut visible_rows = self.complete_rows;
        for raw in markdown[self.offset..].split_inclusive('\n') {
            let complete = raw.ends_with('\n');
            let source = raw.strip_suffix('\n').unwrap_or(raw);
            let source = if complete {
                source.strip_suffix('\r').unwrap_or(source)
            } else {
                source
            };
            // Let the parser decide which whitespace-only EOF lines it
            // withholds. Probe only this new line, never the preceding body.
            let content = if source.trim_matches([' ', '\t', '\r']).is_empty() {
                let probe = format!("{}{raw}", self.header);
                Parser::new(&probe).any(|event| {
                    matches!(event, Event::Text(text) if !text.trim_end_matches('\n').is_empty())
                })
            } else {
                true
            };
            // Withheld blanks become empty interior lines when content follows.
            let source = if complete && !content { "" } else { source };
            let clean = sanitize(source);
            let spans = if complete {
                self.highlight.line(&clean, &theme.syntax)
            } else {
                self.highlight.clone().line(&clean, &theme.syntax)
            };
            visited += 1;
            let painted = code_rows(spans, source, width, theme);
            if complete {
                self.offset += raw.len();
                for (row, copy) in painted {
                    lines.push(row);
                    copy_rows.push(copy);
                }
                if content {
                    self.complete_rows = lines.len();
                }
                visible_rows = self.complete_rows;
            } else if content {
                visible_rows = lines.len();
                for (row, copy) in painted {
                    rest.push(row);
                    rest_copy.push(copy);
                }
            }
        }
        (rest, rest_copy, visible_rows, visited)
    }
}

fn code_rows(
    spans: Vec<Span<'static>>,
    source: &str,
    width: u16,
    theme: &MarkdownTheme,
) -> Vec<(Line<'static>, RowCopy)> {
    let mut renderer = Renderer::new(theme, usize::from(width.max(1)));
    renderer.ensure_code_rail();
    renderer.current.code = true;
    renderer.current.soft_wrap = true;
    renderer.current.code_source = Some(code_copy_line(source));
    for span in spans {
        if !span.content.is_empty() {
            renderer.push_clean(span.content.into_owned(), span.style);
        }
    }
    let mut rows = Vec::new();
    wrap_line(&renderer.current, renderer.width, &mut rows);
    rows
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
