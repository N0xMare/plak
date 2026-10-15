//! Claude Code's result text: background notices and `cat -n` numbered reads.

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

pub(super) fn claude_cat_n_from_canonical_read(
    canonical: &str,
) -> Result<String, ToolRuntimeError> {
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
    let eof = read.eof;
    let next_offset = read.next_offset.unwrap_or(0);
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
    for note in notes {
        if !numbered.is_empty() && !numbered.ends_with('\n') {
            numbered.push('\n');
        }
        numbered.push_str(&note);
    }
    if !eof {
        if !numbered.is_empty() && !numbered.ends_with('\n') {
            numbered.push('\n');
        }
        write!(numbered, "... more lines; next_offset={next_offset}").map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "claude read observation could not be numbered",
                &error,
            )
        })?;
    }
    Ok(numbered)
}
