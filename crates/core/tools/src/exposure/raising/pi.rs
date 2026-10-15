//! Pi's result text: background notices and plain read text.

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
pub(super) fn pi_text_from_canonical_read(canonical: &str) -> Result<String, ToolRuntimeError> {
    let read: crate::WorkspaceFileReadResult =
        serde_json::from_str(canonical).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "workspace read observation is not a neutral read result",
                &error,
            )
        })?;
    let mut text = read.content;
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
    if let Some(next) = read.next_offset.filter(|_| !read.eof) {
        notes.push(format!(
            "[Showing lines {}-{}. Use offset={next} to continue.]",
            read.offset,
            next.saturating_sub(1)
        ));
    }
    for note in notes {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&note);
    }
    Ok(text)
}
