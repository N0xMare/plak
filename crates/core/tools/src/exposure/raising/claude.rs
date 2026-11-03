//! Claude Code's result text: background notices and `cat -n` numbered reads.

use super::ReadPageStyle;
use crate::internal::RaisedReadPage;
use crate::{ToolRuntimeError, ToolRuntimeErrorKind};

/// Claude Code `Bash`'s line for a command that keeps running in the background.
pub(super) fn claude_background_notice(
    by_user: bool,
    preempted: bool,
    id: u64,
    hint: u64,
) -> String {
    if by_user {
        format!("Command was manually backgrounded by user with ID: {id}.")
    } else if preempted {
        format!(
            "Command was moved to the background (ID: {id}) so that a message that arrived while it was running can reach you; it was not interrupted."
        )
    } else if hint > 250 {
        let seconds = hint
            .saturating_add(500)
            .checked_div(1000)
            .unwrap_or(1)
            .max(1);
        format!(
            "Command did not complete within its {seconds}s timeout and was moved to the background (ID: {id})."
        )
    } else {
        format!("Command running in background with ID: {id}.")
    }
}

/// Claude's `Read` text: `cat -n` lines, then the clipping and decoding notes,
/// then where to continue when the page is cut short.
pub(super) fn claude_cat_n_from_canonical_read(
    canonical: &str,
) -> Result<(String, RaisedReadPage), ToolRuntimeError> {
    use std::fmt::Write as _;
    let read: crate::WorkspaceFileReadResult =
        serde_json::from_str(canonical).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "workspace read observation is not a neutral read result",
                &error,
            )
        })?;
    let body = &read.content;
    let offset = read.offset;
    let mut numbered = String::new();
    for (index, line) in body.split_inclusive('\n').enumerate() {
        let line_no = offset.saturating_add(index as u64);
        write!(numbered, "{line_no:>6}\t{line}").map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "claude read observation could not be numbered",
                &error,
            )
        })?;
    }
    let lines_bytes = numbered.len();
    let mut notes = Vec::new();
    if read.clipped_lines != 0 {
        notes.push(format!(
            "... {} lines longer than 2000 characters were clipped",
            read.clipped_lines
        ));
    }
    if read.lossy {
        notes.push(String::from("... invalid UTF-8 was replaced with U+FFFD"));
    }
    let continuation = (!read.eof).then(|| continuation(read.next_offset.unwrap_or(0)));
    append_notes(
        &mut numbered,
        notes
            .iter()
            .map(String::as_str)
            .chain(continuation.as_deref()),
    );
    let page = RaisedReadPage::new(
        ReadPageStyle::CatN,
        offset,
        lines_bytes,
        numbered.len(),
        notes,
    );
    Ok((numbered, page))
}

/// A page an observation cut kept only `head` of: the page's notes, a line
/// saying it was cut, then the continuation the description documents.
pub(super) fn cut_cat_n(head: &str, notes: &[String], next_offset: u64) -> String {
    let mut text = head.to_owned();
    let continuation = continuation(next_offset);
    append_notes(
        &mut text,
        notes
            .iter()
            .map(String::as_str)
            .chain([OBSERVATION_CUT_NOTE, continuation.as_str()]),
    );
    text
}

const OBSERVATION_CUT_NOTE: &str = "... page truncated to fit the context budget";

fn continuation(next_offset: u64) -> String {
    format!("... more lines; next_offset={next_offset}")
}

/// Each note on its own line after the numbered lines.
fn append_notes<'a>(text: &mut String, notes: impl IntoIterator<Item = &'a str>) {
    for note in notes {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(note);
    }
}
