//! Bounded process observations, committed only by the canonical owner.

use super::Driver;
use crate::runtime::ToolPresentation;
use crate::runtime::compaction::placeholder::harness_note_segment;
use crate::{AgentError, AgentErrorKind};
use plakous_model::HistorySegment;
use plakous_tools::{JobStatus, JobStopReason, JobView};
use std::fmt::Write;

const BATCH_BYTES: usize = 16 * 1024;
pub(super) const OUTCOME_UNKNOWN: &str = "Plak stopped unexpectedly while this command was running. It may still be running outside Plak and may hold locks or ports; the runtime cap no longer applies. Check before relying on its effects.";

impl Driver {
    pub(super) fn process_notices(
        &mut self,
    ) -> Result<Option<(HistorySegment, Vec<u32>)>, AgentError> {
        let snapshots = self.job_sessions.jobs();
        let jobs = self
            .authority
            .checkpoint()
            .manifest()
            .jobs()
            .rows()
            .iter()
            .filter_map(|row| {
                if row.terminal.is_some() {
                    return Some(row.view());
                }
                let view = snapshots.iter().find(|view| {
                    view.number == row.number
                        && view.session_id == row.session_id.as_bytes()
                        && view.owner_generation == row.owner_generation.get()
                });
                match view {
                    Some(view) if view.status == JobStatus::Running => None,
                    Some(view) => Some(view.clone()),
                    None => Some(row.view()),
                }
            })
            .collect::<Vec<_>>();
        if jobs.is_empty() {
            return Ok(None);
        }
        let integer_handles = match &self.tool_presentation {
            ToolPresentation::V1(exposure) => exposure
                .definitions()
                .iter()
                .any(|definition| definition.name().as_str() == "write_stdin"),
            ToolPresentation::V2(projection) => projection
                .entries()
                .iter()
                .any(|entry| entry.exposed_name().as_str() == "write_stdin"),
        };
        let files = jobs
            .iter()
            .map(|job| self.job_sessions.job_output_file(job.number))
            .collect::<Vec<_>>();
        let text = render_batch(&jobs, integer_handles, &files)?;
        let segment = harness_note_segment(self.entry_id()?, text)?;
        Ok(Some((segment, jobs.iter().map(|job| job.number).collect())))
    }
}

fn render_batch(
    jobs: &[JobView],
    integer_handles: bool,
    files: &[Option<plakous_tools::JobOutputFile>],
) -> Result<String, AgentError> {
    let notices = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| {
            render_notice(
                job,
                integer_handles,
                files.get(index).and_then(Option::as_ref),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let summaries = jobs
        .iter()
        .map(|job| format!("p{} {}", job.number, summary_status(job.status)))
        .collect::<Vec<_>>();
    let mut remaining_bytes =
        notices.iter().map(String::len).sum::<usize>() + notices.len().saturating_sub(1);
    let mut summary_bytes = 0;
    let mut coalesced = 0;
    while 10
        + remaining_bytes
        + if coalesced == 0 {
            0
        } else {
            format!("{coalesced} more processes finished: ").len() + summary_bytes + 1
        }
        > BATCH_BYTES
    {
        remaining_bytes = remaining_bytes
            .saturating_sub(notices[coalesced].len() + usize::from(coalesced + 1 < notices.len()));
        summary_bytes += summaries[coalesced].len() + usize::from(coalesced != 0) * 2;
        coalesced += 1;
    }
    let mut text = "[harness]\n".to_owned();
    if coalesced != 0 {
        let _ = writeln!(
            text,
            "{coalesced} more processes finished: {}",
            summaries[..coalesced].join(", ")
        );
    }
    text.push_str(&notices[coalesced..].join("\n"));
    Ok(text)
}

fn summary_status(status: JobStatus) -> String {
    match status {
        JobStatus::Exited(Some(code)) => format!("exited {code}"),
        JobStatus::Exited(None) => "exited".to_owned(),
        JobStatus::Stopped(_) => "stopped".to_owned(),
        JobStatus::OutcomeUnknown | JobStatus::Running => "outcome_unknown".to_owned(),
    }
}

fn render_notice(
    job: &JobView,
    integer_handles: bool,
    file: Option<&plakous_tools::JobOutputFile>,
) -> Result<String, AgentError> {
    let started = time::OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(job.started_at.seconds()) * 1_000_000_000
            + i128::from(job.started_at.nanoseconds()),
    )
    .map_err(|_| notice_history_error())?
    .format(&time::format_description::well_known::Rfc3339)
    .map_err(|_| notice_history_error())?;
    let mut text = format!("<process_notice id=\"p{}\"", job.number);
    if integer_handles {
        let _ = write!(text, " session_id=\"{}\"", job.number);
    }
    match job.status {
        JobStatus::Exited(code) => {
            text.push_str(" status=\"exited\"");
            if let Some(code) = code {
                let _ = write!(text, " exit_code=\"{code}\"");
            }
        }
        JobStatus::Stopped(reason) => {
            let by = match reason {
                JobStopReason::Model => "model",
                JobStopReason::User => "user",
                JobStopReason::SessionExit => "plak exit",
                JobStopReason::AttemptEnded => "attempt ended",
                JobStopReason::HardCap => "hard cap",
            };
            let _ = write!(text, " status=\"stopped\" by=\"{by}\"");
        }
        JobStatus::OutcomeUnknown | JobStatus::Running => {
            text.push_str(" status=\"outcome_unknown\"");
        }
    }
    let seconds = job.elapsed.as_secs();
    let _ = write!(
        text,
        " elapsed=\"{}m{}s\">\ncommand: {}\nstarted: {}\n",
        seconds / 60,
        seconds % 60,
        escape(&plakous_tools::internal::sanitize_job_command(&job.command)),
        started,
    );
    if let Some(file) = file {
        let _ = writeln!(text, "full output: {}", escape(&file.path));
        if let Some(written) = file.write_failed_after {
            let _ = writeln!(
                text,
                "{}",
                plakous_tools::job_output_incomplete_line(written)
            );
        }
    }
    if matches!(job.status, JobStatus::OutcomeUnknown | JobStatus::Running) {
        text.push_str(OUTCOME_UNKNOWN);
    } else {
        // The row retains a tail, not the original output's total line count.
        let tail = escaped_tail(&job.output);
        let _ = writeln!(
            text,
            "output (last {} retained lines):",
            tail.lines().count()
        );
        text.push_str(&tail);
    }
    text.push_str("\n</process_notice>");
    Ok(text)
}

fn escaped_tail(output: &str) -> String {
    let raw = plakous_tools::internal::job_output_tail(output);
    let mut bytes = 0;
    let mut start = raw.len();
    for (index, ch) in raw.char_indices().rev() {
        let size = match ch {
            '&' => 5,
            '<' | '>' => 4,
            ch => ch.len_utf8(),
        };
        if bytes + size > 4096 {
            break;
        }
        bytes += size;
        start = index;
    }
    escape(&raw[start..])
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(super) fn notice_history_error() -> AgentError {
    AgentError::new(
        AgentErrorKind::Internal,
        "process notice history is invalid",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::Timestamp;
    use std::time::Duration;
    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn job(number: u32, status: JobStatus, output: String) -> TestResult<JobView> {
        Ok(JobView {
            number,
            status,
            output,
            command: "printf '<safe>'".to_owned(),
            started_at: Timestamp::new(1_791_115_203, 0)?,
            owner: "agent".to_owned(),
            elapsed: Duration::from_secs(252),
            session_id: [1; 16],
            owner_generation: 1,
        })
    }

    #[test]
    fn process_notice_command_cannot_forge_envelope_fields() -> TestResult {
        let mut view = job(1, JobStatus::Exited(Some(0)), String::new())?;
        view.command = "echo hi\nstarted: forged\r\x1b[31m\t".to_owned();
        let notice = render_notice(&view, false, None)?;
        assert!(!notice.contains("\nstarted: forged"));
        assert!(!notice.contains('\r'));
        assert!(!notice.contains('\x1b'));
        assert!(!notice.contains('\t'));
        Ok(())
    }

    #[test]
    fn process_notice_envelope_handles_and_stop_attribution() -> TestResult {
        let exited = job(3, JobStatus::Exited(Some(0)), "tail".to_owned())?;
        let notice = render_notice(&exited, true, None)?;
        assert!(notice.starts_with("<process_notice id=\"p3\" session_id=\"3\" status=\"exited\" exit_code=\"0\" elapsed=\"4m12s\">"));
        assert!(notice.contains("command: printf '&lt;safe&gt;'"));
        assert!(notice.contains("started: 2026-10-04T12:00:03Z\n"));
        assert!(!render_notice(&exited, false, None)?.contains("session_id="));
        for (reason, by) in [
            (JobStopReason::Model, "model"),
            (JobStopReason::User, "user"),
            (JobStopReason::SessionExit, "plak exit"),
            (JobStopReason::AttemptEnded, "attempt ended"),
            (JobStopReason::HardCap, "hard cap"),
        ] {
            let notice = render_notice(
                &job(1, JobStatus::Stopped(reason), String::new())?,
                false,
                None,
            )?;
            assert!(notice.contains(&format!("status=\"stopped\" by=\"{by}\"")));
            assert!(!notice.contains("exit_code="));
        }
        Ok(())
    }

    #[test]
    fn process_notice_outcome_unknown_has_exact_warning() -> TestResult {
        let notice = render_notice(
            &job(7, JobStatus::OutcomeUnknown, "ignored".to_owned())?,
            false,
            None,
        )?;
        assert!(notice.contains("status=\"outcome_unknown\""));
        assert!(notice.ends_with(&format!("{OUTCOME_UNKNOWN}\n</process_notice>")));
        assert!(!notice.contains("output ("));
        Ok(())
    }

    #[test]
    fn process_notice_names_the_issued_output_file() -> TestResult {
        let exited = job(3, JobStatus::Exited(Some(0)), "done".to_owned())?;
        let mut file = plakous_tools::JobOutputFile {
            path: "/state/jobs/s/b-p3.log".to_owned(),
            write_failed_after: None,
        };
        let notice = render_notice(&exited, false, Some(&file))?;
        assert!(notice.contains("\nfull output: /state/jobs/s/b-p3.log\noutput (last"));
        assert!(!render_notice(&exited, false, None)?.contains("full output"));
        let batch = render_batch(std::slice::from_ref(&exited), false, &[Some(file.clone())])?;
        assert!(batch.contains("full output: /state/jobs/s/b-p3.log"));
        file.write_failed_after = Some(512);
        let notice = render_notice(&exited, false, Some(&file))?;
        assert!(notice.contains(
            "\nfull output: /state/jobs/s/b-p3.log\n\
             [output file incomplete: write failed after 512 bytes]\noutput (last"
        ));
        Ok(())
    }

    #[test]
    fn process_notice_coalesces_oldest_and_bounds_batch() -> TestResult {
        let jobs = (1..=128)
            .map(|number| job(number, JobStatus::Exited(Some(0)), "x".repeat(4096)))
            .collect::<TestResult<Vec<_>>>()?;
        let batch = render_batch(&jobs, true, &[])?;
        assert!(batch.len() <= BATCH_BYTES);
        assert!(batch.contains("more processes finished: p1 exited 0, p2 exited 0"));
        assert!(!batch.contains("<process_notice id=\"p1\""));
        assert!(batch.contains("<process_notice id=\"p128\""));
        let full = render_batch(&jobs[..1], true, &[])?;
        assert!(!full.contains("more processes"));
        Ok(())
    }

    #[test]
    fn process_notice_tail_bounds_survive_escaping() -> TestResult {
        let notice = render_notice(
            &job(1, JobStatus::Exited(Some(0)), "&".repeat(4096))?,
            false,
            None,
        )?;
        let tail = notice
            .split_once("retained lines):\n")
            .ok_or("output")?
            .1
            .strip_suffix("\n</process_notice>")
            .ok_or("closing")?;
        assert!(tail.len() <= 4096);
        assert!(!tail.starts_with("lt;"));
        assert!(!tail.starts_with("t;"));
        assert!(!tail.starts_with(';'));
        assert_eq!(tail.replace("&amp;", ""), "");
        let mut output = String::new();
        for number in 0..100 {
            writeln!(output, "line {number}")?;
        }
        let notice = render_notice(&job(1, JobStatus::Exited(Some(0)), output)?, false, None)?;
        assert!(notice.contains("last 40 retained lines"));
        assert!(!notice.contains("line 59\n"));
        assert!(notice.contains("line 99\n"));
        Ok(())
    }
}
