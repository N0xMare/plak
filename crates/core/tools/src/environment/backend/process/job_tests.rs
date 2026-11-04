use super::*;
use crate::{ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JobStatus, JobStopReason};
use plakous_model::{CancellationToken, RunId};
use std::str::FromStr;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn explicit_leftover_disown_is_released_and_reopen_is_read_only() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = sessions(directory.path())?;
    let id = service.start(
        request(
            "(n=0; while [ ! -f finish-leftover ] && [ -d \"$PWD\" ] && [ $((n += 1)) -le 60000 ]; do sleep 0.01; done) &",
            false,
        )?,
        OperationContext::new(CancellationToken::new()),
    )?;
    service.promote(id)?;
    // A liveness bound only: generous, so load never trips it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while service.leftover(id).is_none() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    if service.leftover(id).is_none() {
        fs::write(directory.path().join("finish-leftover"), b"finish")?;
        service.join_all()?;
        return Err("shell leader never yielded a leftover group".into());
    }
    let disowned = service.disown_leftovers();
    // The deliberately relinquished process must still get its fixture-owned
    // completion signal even if a handoff assertion fails.
    fs::write(directory.path().join("finish-leftover"), b"finish")?;
    disowned?;
    let views = service.jobs();
    assert_eq!(views[0].status, JobStatus::Released);
    service.join_all()?;
    assert_eq!(service.jobs()[0].status, JobStatus::Released);
    let restored = sessions(directory.path())?;
    restored.restore_job_terminals(&views);
    assert_eq!(restored.jobs()[0].status, JobStatus::Released);
    let restored_id = restored
        .session_for_number(views[0].number)
        .ok_or("snapshot")?;
    assert_eq!(
        restored
            .poll_from(restored_id, JoinedProcessOutputCursor::new(0), 1024, 20)?
            .status(),
        JoinedProcessStatus::Released
    );
    assert!(
        restored
            .write_stdin(
                restored_id,
                b"no authority",
                JoinedProcessInputDisposition::KeepOpen
            )
            .is_err()
    );
    restored.stop(restored_id, JobStopReason::Model)?;
    assert_eq!(restored.jobs()[0].status, JobStatus::Released);
    restored.consume_jobs(&[views[0].number]);
    assert_eq!(restored.jobs(), [] as [crate::JobView; 0]);
    Ok(())
}

fn sessions(root: &Path) -> TestResult<JoinedProcessSessions> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000691")?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?
    .joined_process_sessions()?)
}

fn join_until_drained(sessions: &JoinedProcessSessions) -> TestResult {
    super::join_until_drained(sessions)
}

fn request(command: &str, stdin: bool) -> TestResult<JoinedProcessStartRequest> {
    Ok(JoinedProcessStartRequest::new(
        JoinedProcessCommand::shell(
            super::super::JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()])?,
            command,
        )?,
        ".",
        Vec::new(),
        stdin,
        JoinedProcessTerminalMode::Pipes,
    )?)
}

#[test]
fn review_release_records_session_exit_stopper() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let id = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.promote(id)?;
    sessions.join_all()?;
    assert_eq!(
        sessions.jobs()[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    Ok(())
}

#[test]
fn review_cancel_between_observation_and_promotion_refuses_handle() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let cancellation = CancellationToken::new();
    let id = sessions.start(
        request("read line", true)?,
        OperationContext::new(cancellation.clone()),
    )?;
    assert_eq!(
        sessions
            .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
            .status(),
        JoinedProcessStatus::Running
    );
    cancellation.cancel();
    let error = sessions
        .promote(id)
        .err()
        .ok_or("cancelled foreground was promoted")?;
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Cancelled);
    assert!(!sessions.is_promoted(id));
    sessions.join_all()?;
    Ok(())
}

#[test]
fn review_running_snapshot_counts_toward_cap_and_is_consumable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let source = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.promote(source)?;
    let mut views = sessions.jobs();
    let template = views[0].clone();
    for number in 2..=128 {
        let mut view = template.clone();
        view.number = number;
        views.push(view);
    }
    let restored = self::sessions(directory.path())?;
    restored.seed_job_counter(129);
    restored.restore_job_terminals(&views);
    assert_eq!(restored.jobs().len(), 128);
    assert_eq!(restored.jobs()[0].status, JobStatus::OutcomeUnknown);
    let id = restored
        .session_for_number(1)
        .ok_or("running snapshot missing")?;
    assert_eq!(
        restored
            .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
            .status(),
        JoinedProcessStatus::OutcomeUnknown
    );
    assert!(
        restored
            .write_stdin(
                id,
                b"no live input",
                JoinedProcessInputDisposition::KeepOpen
            )
            .is_err()
    );
    assert_eq!(
        restored.stop(id, JobStopReason::Model)?,
        JoinedProcessStatus::OutcomeUnknown
    );
    let next = restored.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    assert!(restored.promote(next).is_err());
    restored.consume_jobs(&[1]);
    assert!(restored.session_for_number(1).is_none());
    restored.promote(next)?;
    restored.join_all()?;
    sessions.join_all()?;
    Ok(())
}

#[test]
fn review_consumption_removes_active_terminal_and_missing_stopper_is_unknown() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let id = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    // Hold the operation so the monitor cannot retire this terminal first.
    let session = sessions.session(id)?;
    let operation = session.enter()?;
    sessions.promote(id)?;
    lock(&session.shared).status = JoinedProcessStatus::Terminated;
    assert_eq!(sessions.jobs()[0].status, JobStatus::OutcomeUnknown);
    sessions.consume_jobs(&[1]);
    assert_eq!(sessions.jobs().len(), 1, "unjoined owner must remain owned");
    assert!(sessions.session_for_number(1).is_some());
    drop(operation);
    lock(&session.owner)
        .as_mut()
        .ok_or("owner")?
        .stop
        .store(true, Ordering::Release);
    session.join_finished_owner(JoinedProcessStatus::Terminated)?;
    Ok(())
}

/// A stopped job whose notice was consumed before its owner joined stays
/// listed with its terminal status, but it is no longer unreported.
#[test]
fn a_consumed_terminal_job_awaiting_its_join_is_not_unreported() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let id = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    let session = sessions.session(id)?;
    let operation = session.enter()?;
    sessions.promote(id)?;
    assert!(!sessions.has_unreported_terminal_job(), "a running job");
    lock(&session.shared).status = JoinedProcessStatus::Terminated;
    assert!(sessions.has_unreported_terminal_job());
    sessions.consume_jobs(&[1]);
    assert_eq!(sessions.jobs().len(), 1, "unjoined owner must remain owned");
    assert_ne!(sessions.jobs()[0].status, JobStatus::Running);
    assert!(!sessions.has_unreported_terminal_job());
    drop(operation);
    lock(&session.owner)
        .as_mut()
        .ok_or("owner")?
        .stop
        .store(true, Ordering::Release);
    session.join_finished_owner(JoinedProcessStatus::Terminated)?;
    Ok(())
}

#[test]
fn consume_running_job_keeps_the_owner_joinable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let id = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.promote(id)?;
    sessions.consume_jobs(&[1]);
    assert!(sessions.session_for_number(1).is_some());
    sessions.stop(id, JobStopReason::Model)?;
    assert!(sessions.joined_terminal_status(id).is_some());
    Ok(())
}

#[test]
fn tty_job_tail_normalizes_line_endings_and_all_controls() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let mut start = request("read line; printf 'one\\n two\\177\\302\\205\\n'", true)?;
    start.terminal_mode = JoinedProcessTerminalMode::UnixPty;
    let id = sessions.start(start, OperationContext::new(CancellationToken::new()))?;
    sessions.promote(id)?;
    sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
    // A liveness bound only: generous, so load never trips it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while sessions.joined_terminal_status(id).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let output = sessions.jobs()[0].output.clone();
    sessions.join_all()?;
    assert!(output.ends_with("one\n two\u{fffd}\u{fffd}"), "{output:?}");
    assert!(!output.contains('\r'));
    Ok(())
}

#[test]
fn review_job_tail_sanitizes_controls_and_is_bounded() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let id = sessions.start(
        request("read line; printf 'a\\000b\\001c\\r\\tend\\n'", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.promote(id)?;
    sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while sessions.joined_terminal_status(id).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(sessions.jobs()[0].output, "a\u{fffd}b\u{fffd}c\tend");
    sessions.join_all()?;
    Ok(())
}

#[test]
fn promotion_detaches_cancel_and_per_id_join_retains_terminal_snapshot() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    sessions.seed_job_counter(7);
    let cancellation = CancellationToken::new();
    let id = sessions.start(
        request("read line; printf 'tail'; exit 7", true)?,
        OperationContext::new(cancellation.clone()),
    )?;
    assert_eq!(JoinedProcessSessions::job_number(id), 7);
    sessions.promote(id)?;
    cancellation.cancel();
    sessions.write_stdin(id, b"finish\n", JoinedProcessInputDisposition::KeepOpen)?;
    // A liveness bound only: generous, so load never trips it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while sessions.joined_terminal_status(id).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(sessions.join(id)?, JoinedProcessStatus::Exited(Some(7)));
    assert!(!sessions.active_session_ids().contains(&id));
    let snapshot = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?;
    assert_eq!(snapshot.status(), JoinedProcessStatus::Exited(Some(7)));
    assert_eq!(
        snapshot
            .records()
            .iter()
            .flat_map(|record| record.bytes().iter())
            .copied()
            .collect::<Vec<_>>(),
        b"tail"
    );
    assert_eq!(sessions.jobs()[0].status, JobStatus::Exited(Some(7)));
    assert_eq!(sessions.jobs()[0].output, "tail");
    assert_eq!(sessions.next_job_number(), 8);
    sessions.consume_jobs(&[7]);
    assert_eq!(sessions.jobs(), [] as [crate::jobs::JobView; 0]);
    assert!(
        sessions
            .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)
            .is_err()
    );
    Ok(())
}

#[test]
fn foreground_retention_does_not_count_as_unconsumed_job_rows() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    for promoted in [true, false] {
        for _ in 0..64 {
            let id = sessions.start(
                request(if promoted { "read line" } else { "true" }, promoted)?,
                OperationContext::new(CancellationToken::new()),
            )?;
            if promoted {
                sessions.promote(id)?;
                sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while sessions.joined_terminal_status(id).is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            if promoted {
                sessions.join(id)?;
            }
        }
    }
    assert_eq!(sessions.jobs().len(), 64);
    let id = sessions.start(
        request("true", false)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.terminate(id)?;
    sessions.consume_jobs(&(1..=64).collect::<Vec<_>>());
    sessions.join_all()?;
    Ok(())
}

#[test]
fn promotion_refuses_the_129th_unconsumed_row() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    for _ in 0..127 {
        let id = sessions.start(
            request("read line", true)?,
            OperationContext::new(CancellationToken::new()),
        )?;
        sessions.promote(id)?;
        sessions.write_stdin(id, b"done\n", JoinedProcessInputDisposition::KeepOpen)?;
        // A liveness bound only: generous, so load never trips it.
        let deadline = Instant::now() + Duration::from_secs(30);
        while sessions.joined_terminal_status(id).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        sessions.join(id)?;
    }
    let first = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    let second = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.promote(first)?;
    let refused = sessions
        .promote(second)
        .err()
        .ok_or("129th promotion was admitted")?;
    assert!(!refused.proven_no_effect());
    assert_eq!(sessions.jobs().len(), 128);
    let foreground = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    assert!(sessions.promote(foreground).is_err());
    sessions.join_all()?;
    Ok(())
}

#[test]
fn sixty_fifth_running_start_is_refused_before_spawn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    for _ in 0..64 {
        sessions.start(
            request("read line", true)?,
            OperationContext::new(CancellationToken::new()),
        )?;
    }
    let refused = sessions.start(
        request("printf bad > marker", false)?,
        OperationContext::new(CancellationToken::new()),
    );
    join_until_drained(&sessions)?;
    let error = refused.err().ok_or("65th start was admitted")?;
    assert!(error.to_string().contains("p64"), "{error}");
    assert!(!directory.path().join("marker").exists());
    Ok(())
}

#[test]
fn host_capacity_refuses_the_129th_start_across_services_before_spawn() -> TestResult {
    let directory = tempfile::tempdir()?;
    let services = (0..3)
        .map(|_| sessions(directory.path()))
        .collect::<TestResult<Vec<_>>>()?;
    let capacity = JoinedProcessHostCapacity::default();
    for service in &services {
        service.bind_host_capacity(capacity.clone());
    }
    for service in &services[..2] {
        for _ in 0..64 {
            service.start(
                request("read line", true)?,
                OperationContext::new(CancellationToken::new()),
            )?;
        }
    }
    let refused = services[2].start(
        request("printf bad > marker", false)?,
        OperationContext::new(CancellationToken::new()),
    );
    for service in &services {
        join_until_drained(service)?;
    }
    assert!(refused.is_err());
    assert!(!directory.path().join("marker").exists());
    let id = services[2].start(
        request("true", false)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    services[2].terminate(id)?;
    Ok(())
}

#[test]
fn explicit_stop_snapshot_keeps_attribution() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sessions = sessions(directory.path())?;
    let id = sessions.start(
        request("read line", true)?,
        OperationContext::new(CancellationToken::new()),
    )?;
    sessions.promote(id)?;
    sessions.stop_with_grace(id, JobStopReason::User, Duration::from_millis(200))?;
    assert_eq!(
        sessions.jobs()[0].status,
        JobStatus::Stopped(JobStopReason::User)
    );
    let restored = self::sessions(directory.path())?;
    restored.seed_job_counter(sessions.next_job_number());
    restored.restore_job_terminals(&sessions.jobs());
    restored.restore_job_terminals(&sessions.jobs());
    assert_eq!(restored.jobs().len(), 1);
    let restored_id = restored.session_for_number(1).ok_or("restored handle")?;
    assert_ne!(restored_id, id);
    assert_eq!(
        restored.stop(restored_id, JobStopReason::Model)?,
        JoinedProcessStatus::Terminated
    );
    assert_eq!(
        restored.jobs()[0].status,
        JobStatus::Stopped(JobStopReason::User)
    );
    restored.consume_jobs(&[1]);
    assert!(restored.session_for_number(1).is_none());
    let mut capped = sessions.jobs()[0].clone();
    capped.status = JobStatus::Stopped(JobStopReason::HardCap);
    let restored_cap = self::sessions(directory.path())?;
    restored_cap.seed_job_counter(sessions.next_job_number());
    restored_cap.restore_job_terminals(&[capped]);
    let cap_id = restored_cap.session_for_number(1).ok_or("restored cap")?;
    assert_eq!(
        restored_cap.stop(cap_id, JobStopReason::User)?,
        JoinedProcessStatus::TimedOut
    );
    assert_eq!(
        restored_cap.jobs()[0].status,
        JobStatus::Stopped(JobStopReason::HardCap)
    );
    Ok(())
}
