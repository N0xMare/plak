//! Pi's result text: background notices and plain read text.

use super::ReadPageStyle;
use crate::internal::RaisedReadPage;
use crate::{ToolRuntimeError, ToolRuntimeErrorKind};

/// Pi `bash`'s line for a command that keeps running in the background.
pub(super) fn pi_background_notice(by_user: bool, preempted: bool, id: u64, hint: u64) -> String {
    if by_user {
        format!("Command was moved to the background by the user (id {id}).")
    } else if preempted {
        format!(
            "Command was moved to the background (id {id}) so that a message that arrived while it was running can reach you; it was not interrupted."
        )
    } else if hint > 250 {
        let seconds = (hint.saturating_add(500) / 1000).max(1);
        format!(
            "Command did not finish within its {seconds}s timeout and keeps running in the background (id {id})."
        )
    } else {
        format!("Command running in the background (id {id}).")
    }
}

/// Pi's `read` text: the selected lines as they are in the file, then the
/// clipping and decoding notes, then where to continue when the page is cut.
pub(super) fn pi_text_from_canonical_read(
    canonical: &str,
) -> Result<(String, RaisedReadPage), ToolRuntimeError> {
    let read: crate::WorkspaceFileReadResult =
        serde_json::from_str(canonical).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "workspace read observation is not a neutral read result",
                &error,
            )
        })?;
    let mut text = read.content;
    let lines_bytes = text.len();
    let mut notes = Vec::new();
    if read.clipped_lines != 0 {
        notes.push(format!(
            "[{} lines longer than 2000 characters were clipped]",
            read.clipped_lines
        ));
    }
    if read.lossy {
        notes.push(String::from("[invalid UTF-8 was replaced with U+FFFD]"));
    }
    let continuation = read
        .next_offset
        .filter(|_| !read.eof)
        .map(|next| showing_lines(read.offset, next));
    append_notes(
        &mut text,
        notes
            .iter()
            .map(String::as_str)
            .chain(continuation.as_deref()),
    );
    let page = RaisedReadPage::new(
        ReadPageStyle::Pi,
        read.offset,
        lines_bytes,
        text.len(),
        notes,
    );
    Ok((text, page))
}

/// A page an observation cut kept only `head` of: the page's notes, then the
/// continuation line for the lines shown.
pub(super) fn cut_pi(head: &str, notes: &[String], offset: u64, shown: u64) -> String {
    let next = offset.saturating_add(shown);
    let continuation = if shown == 0 {
        format!("[Use offset={next} to continue.]")
    } else {
        showing_lines(offset, next)
    };
    let mut text = head.to_owned();
    append_notes(
        &mut text,
        notes
            .iter()
            .map(String::as_str)
            .chain([continuation.as_str()]),
    );
    text
}

fn showing_lines(first: u64, next: u64) -> String {
    format!(
        "[Showing lines {first}-{}. Use offset={next} to continue.]",
        next.saturating_sub(1)
    )
}

/// Each note after one blank line.
fn append_notes<'a>(text: &mut String, notes: impl IntoIterator<Item = &'a str>) {
    for note in notes {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(note);
    }
}
