//! Process status labels and bounded output paging.

use super::{
    JoinedProcessOutputCursor, JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStatus,
    Mutex, ToolRuntimeError, ToolRuntimeErrorKind, output_omitted_marker, text,
};

/// Refuses an operation on a session that has already exited and been joined.
///
/// A joined terminal status is proof the group is gone and reaped, so this
/// write reached nothing and mutated nothing. That makes the outcome known,
/// and the model is told exactly what it found rather than being handed an
/// unreconcilable journal gap. A session that is terminating but not yet
/// joined has no such proof and stays on the unknown-outcome path.
pub(crate) fn exited_session_refusal(
    session_id: u64,
    status: JoinedProcessStatus,
) -> ToolRuntimeError {
    let disposition = match status {
        JoinedProcessStatus::Exited(Some(code)) => format!("status {code}"),
        JoinedProcessStatus::Exited(None) => String::from("no status"),
        JoinedProcessStatus::OutcomeUnknown => String::from("an unknown outcome"),
        JoinedProcessStatus::Terminated => String::from("an explicit termination"),
        JoinedProcessStatus::Cancelled => String::from("a cancellation"),
        JoinedProcessStatus::TimedOut => String::from("a timeout"),
        JoinedProcessStatus::OutputLimit => String::from("an output limit"),
        // `Failed`, and nothing else: `Running` and `CleanupUnresolved` are
        // not joined terminal statuses and never reach here.
        _ => String::from("a failure"),
    };
    ToolRuntimeError::new(
        // The same no-effect family as a stale or unknown `session_id`: the
        // model supplied an identity that can no longer be written to, and it
        // can correct that itself.
        ToolRuntimeErrorKind::InvalidArguments,
        format!(
            "process session {session_id} already exited with {disposition}; nothing was written"
        ),
    )
}

pub(crate) fn status_exit_code(status: JoinedProcessStatus) -> Option<i32> {
    match status {
        JoinedProcessStatus::Exited(code) => code,
        _ => None,
    }
}

pub(crate) const fn process_status_label(status: JoinedProcessStatus) -> &'static str {
    match status {
        JoinedProcessStatus::Running => "running",
        JoinedProcessStatus::Exited(_) => "exited",
        JoinedProcessStatus::Terminated => "terminated",
        JoinedProcessStatus::Cancelled => "cancelled",
        JoinedProcessStatus::TimedOut => "timed_out",
        JoinedProcessStatus::OutputLimit => "output_limit",
        JoinedProcessStatus::Failed => "failed",
        JoinedProcessStatus::CleanupUnresolved => "cleanup_unresolved",
        // `JoinedProcessStatus` is `#[non_exhaustive]` outside its crate; a
        // status this pack has not enrolled has no exact outcome evidence.
        _ => "outcome_unknown",
    }
}

pub(crate) fn lock_process_cursor(
    cursor: &Mutex<JoinedProcessOutputCursor>,
) -> std::sync::MutexGuard<'_, JoinedProcessOutputCursor> {
    cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn lock_pending_process_utf8(
    pending: &Mutex<Vec<u8>>,
) -> std::sync::MutexGuard<'_, Vec<u8>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn drain_process_utf8_bounded(
    pending: &mut Vec<u8>,
    terminal: bool,
    max_bytes: usize,
) -> String {
    let mut output = String::new();
    while output.len() < max_bytes {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                let available = max_bytes.saturating_sub(output.len()).min(text.len());
                let retained = utf8_string_prefix_len(text, available);
                output.push_str(&text[..retained]);
                pending.drain(..retained);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid != 0 {
                    let available = max_bytes.saturating_sub(output.len()).min(valid);
                    let text = std::str::from_utf8(&pending[..valid]).unwrap_or_default();
                    let retained = utf8_string_prefix_len(text, available);
                    output.push_str(&text[..retained]);
                    pending.drain(..retained);
                    if retained != valid {
                        break;
                    }
                }
                match error.error_len() {
                    Some(invalid)
                        if max_bytes.saturating_sub(output.len())
                            >= char::REPLACEMENT_CHARACTER.len_utf8() =>
                    {
                        output.push(char::REPLACEMENT_CHARACTER);
                        pending.drain(..invalid);
                    }
                    None if terminal
                        && max_bytes.saturating_sub(output.len())
                            >= char::REPLACEMENT_CHARACTER.len_utf8() =>
                    {
                        output.push(char::REPLACEMENT_CHARACTER);
                        pending.clear();
                        break;
                    }
                    _ => break,
                }
            }
        }
    }
    output
}

/// Reads a summarised observation may make after its yield to drain a backlog:
/// more than one whole session window takes at one chunk each.
pub(crate) const MAX_DRAIN_READS: usize = 64;

/// One foreground page: the first 30% and the last 70% of its byte budget,
/// with the omitted byte count between them. A failure is usually at the end
/// of a long log, and the command that produced it at the start.
pub(crate) struct HeadTailPage {
    pub(crate) head: String,
    pub(crate) tail: String,
    pub(crate) head_budget: usize,
    pub(crate) tail_budget: usize,
    pub(crate) omitted: u64,
}

impl HeadTailPage {
    pub(crate) fn new(budget: usize) -> Self {
        let head_budget = budget * 3 / 10;
        Self {
            head: String::new(),
            tail: String::new(),
            head_budget,
            tail_budget: budget - head_budget,
            omitted: 0,
        }
    }

    /// Adds bytes the window evicted before they could be shown.
    pub(crate) fn skip(&mut self, bytes: u64) {
        self.omitted = self.omitted.saturating_add(bytes);
    }

    pub(crate) fn push(&mut self, mut text: &str) {
        let room = self.head_budget.saturating_sub(self.head.len());
        if room != 0 {
            let taken = utf8_string_prefix_len(text, room);
            self.head.push_str(&text[..taken]);
            text = &text[taken..];
        }
        self.tail.push_str(text);
        if self.tail.len() > self.tail_budget {
            let mut cut = self.tail.len() - self.tail_budget;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.omitted += cut as u64;
            self.tail.drain(..cut);
        }
    }

    pub(crate) fn finish(mut self) -> String {
        if self.omitted == 0 {
            self.head.push_str(&self.tail);
            return self.head;
        }
        // Cut on line boundaries when one is near, so neither side shows a
        // half line.
        if let Some(line_end) = self
            .head
            .rfind('\n')
            .filter(|end| self.head.len() - end <= 256)
        {
            self.omitted += (self.head.len() - line_end - 1) as u64;
            self.head.truncate(line_end + 1);
        }
        if let Some(line_start) = self.tail.find('\n').filter(|start| *start < 256) {
            self.omitted += (line_start + 1) as u64;
            self.tail.drain(..=line_start);
        }
        let marker = output_omitted_marker(self.omitted);
        let marker = marker.trim_start_matches('\n');
        if !self.head.ends_with('\n') && !self.head.is_empty() {
            self.head.push('\n');
        }
        self.head.push_str(marker);
        self.head.push_str(&self.tail);
        self.head
    }
}

/// Names the job's output file, when it has one, as the last line of `output`
/// (after the incomplete-file line when spooling to it failed).
pub(crate) fn append_output_file(
    output: &mut String,
    sessions: &JoinedProcessSessions,
    id: JoinedProcessSessionId,
) {
    if let Some(file) = sessions.job_output_file(JoinedProcessSessions::job_number(id)) {
        push_output_file(output, &file);
    }
}

pub(crate) fn push_output_file(output: &mut String, file: &plakous_tools::JobOutputFile) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if let Some(written) = file.write_failed_after {
        output.push_str(&plakous_tools::job_output_incomplete_line(written));
        output.push('\n');
    }
    output.push_str(text::PROCESS_OUTPUT_FILE);
    output.push_str(&file.path);
}

fn utf8_string_prefix_len(text: &str, maximum: usize) -> usize {
    let mut retained = maximum.min(text.len());
    while !text.is_char_boundary(retained) {
        retained = retained.saturating_sub(1);
    }
    retained
}
