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
fn rendered_batches_are_recognized_as_process_notices() -> TestResult {
    let single = render_batch(
        &[job(2, JobStatus::Exited(Some(1)), "tail".to_owned())?],
        false,
        &[None],
    )?;
    assert!(is_process_notice_batch(&single));
    let jobs = (1..=40)
        .map(|number| job(number, JobStatus::Exited(Some(0)), "x".repeat(4_000)))
        .collect::<TestResult<Vec<_>>>()?;
    let files = jobs.iter().map(|_| None).collect::<Vec<_>>();
    let coalesced = render_batch(&jobs, false, &files)?;
    assert!(coalesced.contains(" more processes finished: "));
    assert!(is_process_notice_batch(&coalesced));
    assert!(!is_process_notice_batch(
        "[harness] This turn has used its entire model-operation budget"
    ));
    assert!(!is_process_notice_batch(
        "[plak: earlier work in this turn was summarized]"
    ));
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
