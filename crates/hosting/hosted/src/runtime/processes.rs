//! The session's process rows as its faces receive them: published only
//! when they change, and redacted as a tool row is.

use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::views::ProcessView;

use super::Sender;
use crate::surface_protocol::ContentEvent;

impl<P: ProtocolExtension> Sender<P> {
    /// Publishes the session's process rows when they changed since the
    /// last published ones: anything but a running process's elapsed time
    /// within the same second. A session that never had a process publishes
    /// none: no rows is what a face holds before the first. Rows go out
    /// redacted as a tool row's are.
    pub(crate) fn publish_processes(&self, views: Vec<ProcessView>) {
        let views: Vec<_> = views.into_iter().map(redacted_process).collect();
        let mut last = self
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unchanged = match last.as_ref() {
            Some(held) => same_processes(held, &views),
            None => views.is_empty(),
        };
        if unchanged {
            return;
        }
        *last = Some(views.clone());
        drop(last);
        let running = views.iter().filter(|row| row.status.is_live()).count();
        let _ = self.send(ContentEvent::Processes { views, running });
    }
}

/// Whether two process lists show the same: a running process's elapsed
/// time counts only in whole seconds.
fn same_processes(held: &[ProcessView], next: &[ProcessView]) -> bool {
    held.len() == next.len()
        && held.iter().zip(next).all(|(old, new)| {
            old.id == new.id
                && old.command == new.command
                && old.owner == new.owner
                && old.started_at == new.started_at
                && old.status == new.status
                && old.output == new.output
                && old.elapsed_ms / 1000 == new.elapsed_ms / 1000
        })
}

/// A process row as every attachment, a remote one included, receives it
/// (spec 04 §2.10): its command redacted ([`redacted_job_command`]) and
/// bounded as a tool row's subject is, and its output tail redacted as a
/// whole-line tail ([`redacted_job_tail`]).
fn redacted_process(mut view: ProcessView) -> ProcessView {
    view.command = crate::sessions::live_tool_subject(&redacted_job_command(&view.command));
    view.output = redacted_job_tail(&view.output);
    view
}

/// A job's displayed command, redacted as the raw command it shows. The
/// runtime displays a command with each newline and carriage return escaped
/// (`\n`, `\r`) and cut at 1 KiB; escaped, a secret that starts a line
/// (a heredoc's token) follows a letter and misses the redactor's word
/// boundary. The escapes are undone for the redactor and made again after
/// it, which gives back the displayed text wherever nothing was redacted; a
/// private key the cut left without its END is redacted to the end.
pub(super) fn redacted_job_command(command: &str) -> String {
    let raw = command.replace("\\r", "\r").replace("\\n", "\n");
    plakous::tools::preview_redaction::text(&raw)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// The runtime's bound on a job's retained output tail, in bytes.
const TAIL_BYTES: usize = 4096;

/// How far under [`TAIL_BYTES`] a tail cut at that bound can end up, once
/// line endings and a trailing newline are normalized away.
const TAIL_SLACK: usize = 64;

/// A job's retained output tail, redacted for display. A tail at its byte
/// bound starts wherever the bytes ran out, likely inside a line, where a
/// secret's prefix may have been cut off: that partial first line is
/// dropped, so the redactor sees whole lines. A private key whose BEGIN was
/// cut off is redacted from the start of the tail to its END.
pub(super) fn redacted_job_tail(output: &str) -> String {
    let output = if output.len() + TAIL_SLACK >= TAIL_BYTES {
        output.split_once('\n').map_or(output, |(_, rest)| rest)
    } else {
        output
    };
    let mut text = plakous::tools::preview_redaction::text(output);
    // The redactor took every whole key and every key without its END, so
    // an END left is a key's whose BEGIN is gone.
    if let Some(end) = last_key_end(&text) {
        text.replace_range(..end, plakous::tools::preview_redaction::MARKER);
    }
    text
}

/// Where the last `-----END [WORD ]PRIVATE KEY-----` marker in `text` ends.
fn last_key_end(text: &str) -> Option<usize> {
    const KEY: &str = "PRIVATE KEY-----";
    const END: &str = "-----END ";
    let mut search = text.len();
    while let Some(at) = text[..search].rfind(KEY) {
        let before = &text[..at];
        let worded = before
            .strip_suffix(' ')
            .map(|word| word.trim_end_matches(|ch: char| ch.is_ascii_uppercase()));
        if before.ends_with(END) || worded.is_some_and(|rest| rest.ends_with(END)) {
            return Some(at + KEY.len());
        }
        search = at;
    }
    None
}
