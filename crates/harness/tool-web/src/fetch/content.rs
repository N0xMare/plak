//! What one fetched response becomes in the tool result.

use std::fmt::Write as _;
use std::rc::Rc;

use plakous_model::{ImageBlock, MediaSource, TextBlock, ToolResultPart};
use plakous_tools::{
    MAX_STANDARD_IMAGE_BYTES, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind,
};

use super::{Fetched, MAX_INLINE_FETCH_TEXT_BYTES};
use crate::render::UNTRUSTED_LINE_TEMPLATE;

/// Body bytes an HTTP error result carries.
pub(crate) const ERROR_BODY_BYTES: usize = 8 * 1024;
/// Head and tail an oversized text keeps inline around the spill line.
const SPILL_EDGE_BYTES: usize = 16 * 1024;
/// Deepest HTML element nesting converted to Markdown.
pub(crate) const MAX_HTML_DEPTH: usize = 512;
/// Elements dropped, with their content, before conversion.
const SKIPPED_TAGS: [&str; 5] = ["script", "style", "noscript", "svg", "iframe"];

#[derive(Debug, Eq, PartialEq)]
enum Kind {
    Html,
    Text,
    Image(&'static str),
    Binary,
}

/// The tool result of one settled response. An HTTP error, a refused
/// charset or binary type, and unconvertible HTML are tool errors.
pub(crate) fn render(
    fetched: &Fetched,
    spill: &mut dyn FnMut(&[u8]) -> Option<String>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let mut head = header(fetched);
    if fetched.status.as_u16() >= 400 {
        let (text, _) = decode(&fetched.body, fetched.truncated);
        head.push('\n');
        head.push_str(if text.is_empty() {
            "(empty body)"
        } else {
            &text
        });
        if fetched.truncated {
            let _ = write!(head, "\n[the body continues past {ERROR_BODY_BYTES} bytes]");
        }
        return ToolOutcome::failure_text(head);
    }
    let essence = fetched.content_type.as_deref().map(essence);
    let kind = classify(essence.as_deref(), &fetched.body);
    if matches!(kind, Kind::Html | Kind::Text)
        && let Some(charset) = fetched.content_type.as_deref().and_then(charset)
        && !matches!(charset.as_str(), "utf-8" | "utf8" | "us-ascii" | "ascii")
    {
        return ToolOutcome::failure_text(format!(
            "fetch_url refused {}: charset {charset} is not UTF-8",
            essence.as_deref().unwrap_or("the response")
        ));
    }
    match kind {
        Kind::Html | Kind::Text => {
            let (decoded, lossy) = decode(&fetched.body, fetched.truncated);
            let text = if kind == Kind::Html {
                match html_to_markdown(&decoded) {
                    Ok(text) => text,
                    Err(message) => return ToolOutcome::failure_text(message),
                }
            } else {
                decoded
            };
            if lossy {
                head.push_str("note: invalid UTF-8 was replaced with U+FFFD\n");
            }
            if fetched.truncated {
                let _ = writeln!(
                    head,
                    "note: the body was cut at the {}-byte cap",
                    fetched.max_bytes
                );
            }
            head.push('\n');
            head.push_str(&bounded(text, spill));
            ToolOutcome::success_text(head)
        }
        Kind::Image(media_type) => image(fetched, head, media_type),
        Kind::Binary => ToolOutcome::failure_text(format!(
            "fetch_url returned binary content ({}, {}); use a shell download if you need the file",
            essence.as_deref().unwrap_or("no content type"),
            size(fetched)
        )),
    }
}

fn header(fetched: &Fetched) -> String {
    let mut head = UNTRUSTED_LINE_TEMPLATE.replace("{tool}", "fetch_url");
    let _ = write!(head, "\nurl: {}\n", fetched.requested);
    if fetched.final_url != fetched.requested {
        let _ = writeln!(head, "final url: {}", fetched.final_url);
    }
    let _ = writeln!(head, "status: {}", fetched.status.as_u16());
    let _ = writeln!(
        head,
        "content type: {}",
        fetched.content_type.as_deref().unwrap_or("(none)")
    );
    head
}

fn size(fetched: &Fetched) -> String {
    if fetched.truncated {
        format!("more than {} bytes", fetched.body.len())
    } else {
        format!("{} bytes", fetched.body.len())
    }
}

/// The lowercase media type without parameters.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    })
}

fn classify(essence: Option<&str>, body: &[u8]) -> Kind {
    let Some(essence) = essence else {
        // No declared type: an image by signature, text when it is UTF-8.
        if let Ok(header) = plakous_tool_files::inspect_image_header(body)
            && let Some(media_type) = still_image(header.media_type)
        {
            return Kind::Image(media_type);
        }
        return if std::str::from_utf8(body).is_ok() {
            Kind::Text
        } else {
            Kind::Binary
        };
    };
    match essence {
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "application/json" | "application/xml" => Kind::Text,
        _ if essence.starts_with("text/")
            || essence.ends_with("+json")
            || essence.ends_with("+xml") =>
        {
            Kind::Text
        }
        _ => still_image(essence).map_or(Kind::Binary, Kind::Image),
    }
}

fn still_image(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn image(
    fetched: &Fetched,
    mut head: String,
    declared: &'static str,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let limit = usize::try_from(MAX_STANDARD_IMAGE_BYTES).unwrap_or(usize::MAX);
    if fetched.truncated || fetched.body.len() > limit {
        return ToolOutcome::failure_text(format!(
            "fetch_url returned an image too large to show ({declared}, {}; the limit is \
             {limit} bytes); use a shell download if you need the file",
            size(fetched)
        ));
    }
    let header = match plakous_tool_files::inspect_image_header(&fetched.body) {
        Ok(header) if header.media_type == declared => header,
        _ => {
            return ToolOutcome::failure_text(format!(
                "fetch_url returned content declared {declared} that is not a valid {declared} image"
            ));
        }
    };
    let dimensions = header.dimensions.map_or_else(
        || String::from("unknown"),
        |(width, height)| format!("{width}x{height}"),
    );
    let _ = write!(
        head,
        "image: media_type={declared} bytes={} dimensions={dimensions}",
        fetched.body.len()
    );
    let content = |error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Execution,
            "fetch_url image could not be returned",
            &error,
        )
    };
    ToolOutcome::success(vec![
        ToolResultPart::Text(TextBlock::new(head, None).map_err(content)?),
        ToolResultPart::Image(
            ImageBlock::new(
                declared,
                MediaSource::bytes(fetched.body.clone()).map_err(content)?,
            )
            .map_err(content)?,
        ),
    ])
}

/// UTF-8 text and whether invalid bytes were replaced. A body cut at the cap
/// drops a trailing incomplete character instead of replacing it.
fn decode(body: &[u8], truncated: bool) -> (String, bool) {
    let body = match std::str::from_utf8(body) {
        Ok(text) => return (text.to_owned(), false),
        Err(error) if truncated && error.error_len().is_none() => &body[..error.valid_up_to()],
        Err(_) => body,
    };
    match String::from_utf8(body.to_vec()) {
        Ok(text) => (text, false),
        Err(_) => (String::from_utf8_lossy(body).into_owned(), true),
    }
}

/// Markdown of one HTML document with scripts, styles and embedded frames
/// dropped.
///
/// # Errors
///
/// Refuses a document nested deeper than [`MAX_HTML_DEPTH`] elements, since
/// the conversion walk recurses per level.
pub(crate) fn html_to_markdown(html: &str) -> Result<String, String> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .build();
    let tree = converter
        .html_to_tree(html)
        .map_err(|_| String::from("fetch_url could not parse the HTML"))?;
    if depth(&tree) > MAX_HTML_DEPTH {
        return Err(format!(
            "fetch_url refused the HTML: it nests deeper than {MAX_HTML_DEPTH} elements"
        ));
    }
    Ok(converter.tree_to_markdown(&tree))
}

/// The deepest nesting under `root`, walked without recursion. Template
/// contents are not walked by the conversion either.
fn depth(root: &Rc<htmd::Node>) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(Rc::clone(root), 0_usize)];
    while let Some((node, level)) = pending.pop() {
        deepest = deepest.max(level);
        if level > MAX_HTML_DEPTH {
            return level;
        }
        for child in node.children.borrow().iter() {
            pending.push((Rc::clone(child), level + 1));
        }
    }
    deepest
}

/// `text` within [`MAX_INLINE_FETCH_TEXT_BYTES`]: an oversized text keeps
/// its head and tail around the line naming the spilled file.
pub(crate) fn bounded(text: String, spill: &mut dyn FnMut(&[u8]) -> Option<String>) -> String {
    if text.is_empty() {
        return String::from("(empty body)");
    }
    if text.len() <= MAX_INLINE_FETCH_TEXT_BYTES {
        return text;
    }
    let head = floor_boundary(&text, SPILL_EDGE_BYTES);
    let tail = ceil_boundary(&text, text.len() - SPILL_EDGE_BYTES);
    let location = match spill(text.as_bytes()) {
        Some(path) => format!("full output: {path}"),
        None => String::from("[the full output could not be retained]"),
    };
    format!(
        "{}\n[... {} bytes omitted ...]\n{}\n{location}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
