use std::str::FromStr;

use super::*;
use crate::ExecutionEnvironmentScope;
use plakous_model::RunId;

type TestError = Box<dyn std::error::Error>;

// Mutex-admission fixtures use this backend cleanup deadline and an
// independent two-second bound; supervisor probes can select a longer deadline.
const CLEANUP_DURATION: Duration = Duration::from_millis(250);

#[test]
fn unjoined_workers_are_recovery_required_not_terminal_failure() {
    assert_eq!(
        joined_process_final_status(JoinedProcessStatus::Failed, true, false, true),
        JoinedProcessStatus::CleanupUnresolved
    );
    assert!(!JoinedProcessStatus::CleanupUnresolved.is_terminal());
    assert_eq!(
        joined_process_final_status(JoinedProcessStatus::Failed, true, true, true),
        JoinedProcessStatus::Failed
    );
    assert!(JoinedProcessStatus::Failed.is_terminal());
}

fn sessions(value: u8) -> Result<(tempfile::TempDir, JoinedProcessSessions), TestError> {
    sessions_with_cleanup(value, CLEANUP_DURATION)
}

fn sessions_with_cleanup(
    value: u8,
    cleanup_duration: Duration,
) -> Result<(tempfile::TempDir, JoinedProcessSessions), TestError> {
    let workspace = tempfile::tempdir()?;
    let limits = ExecutionEnvironmentLimits::new(
        4096,
        4096,
        8192,
        4096,
        Duration::from_secs(30),
        cleanup_duration,
    )?;
    let run_id = RunId::from_str(&format!("00000000-0000-0000-0000-{value:012x}"))?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::new(limits, [], [])?,
    )?;
    Ok((workspace, environment.joined_process_sessions()?))
}

fn sleeping_child(sessions: &JoinedProcessSessions) -> Result<JoinedProcessSessionId, TestError> {
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or("test shell is unavailable")?;
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal(shell, [String::from("-c"), String::from("sleep 30")])?,
        ".",
        [],
        false,
        JoinedProcessTerminalMode::Pipes,
    )?;
    Ok(sessions.start(request, OperationContext::new(CancellationToken::new()))?)
}

fn assert_bounded(started: Instant) {
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cleanup mutex admission exceeded its cleanup deadline"
    );
}

fn join_all_with_bounded_retries(sessions: &JoinedProcessSessions) -> Result<(), TestError> {
    join_until_drained(sessions)
}

fn exiting_child(
    sessions: &JoinedProcessSessions,
    code: u8,
) -> Result<JoinedProcessSessionId, TestError> {
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or("test shell is unavailable")?;
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal(shell, [String::from("-c"), format!("exit {code}")])?,
        ".",
        [],
        true,
        JoinedProcessTerminalMode::Pipes,
    )?;
    Ok(sessions.start(request, OperationContext::new(CancellationToken::new()))?)
}

/// A session that exited and was joined has a knowable disposition, and it
/// keeps that disposition after it is retired. Nothing written to it can
/// have had an effect, which is what makes a refusal definite rather than
/// unknown.
#[test]
fn an_exited_session_reports_its_joined_status_while_active_and_after_retirement()
-> Result<(), TestError> {
    let (_workspace, sessions) = sessions(115)?;
    let id = exiting_child(&sessions, 7)?;
    let started = Instant::now();
    loop {
        let polled = sessions.poll(id, 4096, 16)?;
        if polled.status().is_terminal() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the child did not reach a joined terminal status"
        );
        std::thread::yield_now();
    }
    // Terminal but not retired: the session is still active and its handle
    // still resolves.
    assert_eq!(sessions.active_session_ids(), vec![id]);
    assert_eq!(
        sessions.joined_terminal_status(id),
        Some(JoinedProcessStatus::Exited(Some(7)))
    );
    assert_eq!(
        sessions
            .write_stdin(id, b"x", JoinedProcessInputDisposition::KeepOpen)
            .err()
            .map(|error| error.kind()),
        Some(ExecutionBackendErrorKind::Conflict)
    );

    sessions.terminate(id)?;
    assert!(sessions.is_drained());
    // The tombstone keeps the status, so a late write is still knowable.
    assert_eq!(
        sessions.joined_terminal_status(id),
        Some(JoinedProcessStatus::Exited(Some(7)))
    );
    Ok(())
}

#[test]
fn promoted_short_job_is_retired_without_observer_polling() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(94)?;
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/sh", ["-c", "cat; exit 7"].map(String::from))?,
        ".",
        [],
        true,
        JoinedProcessTerminalMode::Pipes,
    )?;
    let id = sessions.start(request, OperationContext::new(CancellationToken::new()))?;
    sessions.promote(id)?;
    sessions.write_stdin(id, b"", JoinedProcessInputDisposition::Close)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let retired = {
            let state = lock(&sessions.inner.state);
            !state.active.contains_key(&id) && state.finished.contains_key(&id)
        };
        if retired {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unobserved job was not retired before the hang watchdog"
        );
        std::thread::yield_now();
    }
    assert_eq!(sessions.jobs()[0].status, crate::JobStatus::Exited(Some(7)));
    Ok(())
}

#[test]
fn promoted_monitor_exits_on_owner_drop_without_terminal_publication() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(95)?;
    let id = sleeping_child(&sessions)?;
    let session = sessions.session(id)?;
    // Finish the real supervisor, then model one that vanished without publishing.
    {
        let mut owner = lock(&session.owner);
        let supervisor = owner.as_mut().ok_or("missing owner")?;
        supervisor.stop.store(true, Ordering::Release);
        assert!(supervisor.join_until(Instant::now() + Duration::from_secs(5))?);
        owner.take();
    }
    lock(&session.shared).status = JoinedProcessStatus::Running;
    let shared = Arc::clone(&session.shared);
    sessions.promote(id)?;
    drop(session);
    drop(sessions);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Arc::strong_count(&shared) != 1 {
        assert!(
            Instant::now() < deadline,
            "job monitor retained the orphaned status buffer"
        );
        std::thread::yield_now();
    }
    Ok(())
}

#[tokio::test]
async fn process_changes_wake_on_output_and_exit_without_promotion() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(93)?;
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal(
            "/bin/sh",
            [
                "-c".to_owned(),
                "read first; printf done; read second".to_owned(),
            ],
        )?,
        ".",
        [],
        true,
        JoinedProcessTerminalMode::Pipes,
    )?;
    let id = sessions.start(request, OperationContext::new(CancellationToken::new()))?;
    let mut changes = sessions.session_changes(id)?;
    sessions.write_stdin(id, b"go\n", JoinedProcessInputDisposition::KeepOpen)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            changes.borrow_and_update();
            let poll = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 64, 8)?;
            if !poll.records().is_empty() {
                assert_eq!(poll.status(), JoinedProcessStatus::Running);
                break;
            }
            changes.changed().await?;
        }
        sessions.write_stdin(id, b"finish\n", JoinedProcessInputDisposition::KeepOpen)?;
        loop {
            changes.borrow_and_update();
            let poll = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 64, 8)?;
            if poll.status().is_terminal() {
                assert_eq!(poll.status(), JoinedProcessStatus::Exited(Some(0)));
                assert_eq!(
                    poll.records()
                        .iter()
                        .flat_map(|record| record.bytes().iter())
                        .copied()
                        .collect::<Vec<_>>(),
                    b"done"
                );
                break;
            }
            changes.changed().await?;
        }
        Ok::<_, TestError>(())
    })
    .await??;
    sessions.join(id)?;
    Ok(())
}

fn await_terminal(sessions: &JoinedProcessSessions, id: JoinedProcessSessionId) {
    let started = Instant::now();
    while sessions.joined_terminal_status(id).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a child did not reach a joined terminal status"
        );
        std::thread::yield_now();
    }
}

/// A sequential program that starts one command at a time never exhausts
/// the session limit, however many commands it runs.
///
/// The limit bounds concurrent children. A live Code Mode cell looped over
/// 22 files with one `cat` at a time and its 67th start was refused,
/// because every finished session still counted against the limit.
#[test]
fn a_sequential_program_never_exhausts_the_session_limit() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(117)?;
    // Comfortably past the limit, and none of these is ever explicitly
    // retired: this is a model that reads one page and moves on.
    for _ in 0..(MAX_JOINED_PROCESS_SESSIONS + 8) {
        let id = exiting_child(&sessions, 0)?;
        await_terminal(&sessions, id);
    }
    // Finished sessions are remembered so their output stays readable, but
    // only `MAX_JOINED_PROCESS_SESSIONS` of them, plus whichever session
    // the last admission added.
    assert!(
        sessions.active_session_ids().len() <= MAX_JOINED_PROCESS_SESSIONS + 1,
        "finished sessions are retained without bound"
    );
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}

/// The limit still bounds children that are actually running.
#[test]
fn running_children_still_count_against_the_session_limit() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(118)?;
    for _ in 0..MAX_JOINED_PROCESS_SESSIONS {
        sleeping_child(&sessions)?;
    }
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or("test shell is unavailable")?;
    let refused = sessions
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal(
                    shell,
                    [String::from("-c"), String::from("sleep 30")],
                )?,
                ".",
                [],
                false,
                JoinedProcessTerminalMode::Pipes,
            )?,
            OperationContext::new(CancellationToken::new()),
        )
        .err()
        .ok_or("the session limit admitted one child too many")?;
    assert_eq!(refused.kind(), ExecutionBackendErrorKind::ResourceLimit);
    // The refusal happened before any spawn, which is what lets the model
    // see it as an ordinary tool failure.
    assert!(refused.kind().proven_no_effect());
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}

/// Reclaiming finished sessions never takes away output a model can still
/// read: polling a finished session is how its last page is drained.
#[test]
fn a_finished_session_keeps_its_output_readable_across_later_starts() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(119)?;
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or("test shell is unavailable")?;
    let talker = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                shell,
                [String::from("-c"), String::from("printf hello; exit 3")],
            )?,
            ".",
            [],
            true,
            JoinedProcessTerminalMode::Pipes,
        )?,
        OperationContext::new(CancellationToken::new()),
    )?;
    await_terminal(&sessions, talker);
    for _ in 0..8 {
        let id = exiting_child(&sessions, 0)?;
        await_terminal(&sessions, id);
    }
    let polled = sessions.poll(talker, 4096, 16)?;
    assert_eq!(polled.status(), JoinedProcessStatus::Exited(Some(3)));
    let text = polled
        .records()
        .iter()
        .flat_map(|record| record.bytes().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(text, b"hello");
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}

/// A running session has no joined terminal status, so nothing about it is
/// proven and the unknown-outcome path is untouched.
#[test]
fn a_running_session_has_no_joined_terminal_status() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(116)?;
    let id = sleeping_child(&sessions)?;
    assert_eq!(sessions.joined_terminal_status(id), None);
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}

#[test]
fn terminal_status_without_joined_owner_is_not_durable_proof() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions_with_cleanup(120, Duration::from_secs(60))?;
    let id = sleeping_child(&sessions)?;
    let session = sessions.session(id)?;
    // Model the narrow supervisor interval after publishing status and
    // before returning from its thread. The still-running owner must not
    // be certified merely because status has become terminal.
    lock(&session.shared).status = JoinedProcessStatus::Exited(Some(0));
    let started = Instant::now();
    assert!(!sessions.joined_terminal_identities().contains(&id.0));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "proof probe waited for an unfinished supervisor"
    );
    lock(&session.shared).status = JoinedProcessStatus::Running;
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}

#[test]
fn panicked_supervisor_never_becomes_joined_proof_on_a_second_probe() {
    let mut owner = HostProcessOwner {
        stop: Arc::new(AtomicBool::new(false)),
        thread: Some(std::thread::spawn(|| {
            panic!("synthetic supervisor failure")
        })),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while owner
        .thread
        .as_ref()
        .is_some_and(|thread| !thread.is_finished())
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert!(
        owner
            .thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
    );
    for _ in 0..2 {
        assert!(owner.join_until(Instant::now()).is_err());
    }
}

#[test]
fn contended_operation_does_not_stall_joined_proof_probe() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(121)?;
    let id = sleeping_child(&sessions)?;
    let session = sessions.session(id)?;
    let operation = session.enter()?;
    let probe = sessions.clone();
    let (send, receive) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _ = send.send(probe.joined_terminal_identities());
    });
    // A probe that waits on this held lock cannot return until it is released.
    let result = receive.recv_timeout(Duration::from_secs(30));
    drop(operation);
    worker.join().map_err(|_| "joined proof probe panicked")?;
    assert!(
        !result
            .map_err(|_| "joined proof probe blocked on the operation lock")?
            .contains(&id.0),
        "an active child has no joined proof"
    );
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}

#[test]
fn terminate_contention_is_bounded_and_preserves_correlated_owner() -> Result<(), TestError> {
    for value in 91..115 {
        let (_workspace, sessions) = sessions(value)?;
        let id = sleeping_child(&sessions)?;
        let identity_before = sessions.active_session_ids();
        let session = sessions.session(id)?;
        let operation = session.enter()?;

        let started = Instant::now();
        let error = sessions
            .terminate(id)
            .err()
            .ok_or("cleanup unexpectedly joined")?;
        assert_bounded(started);
        assert_eq!(error.kind(), ExecutionBackendErrorKind::CleanupUnresolved);
        assert_eq!(sessions.active_session_ids(), identity_before);
        assert!(!sessions.is_drained());

        drop(operation);
        join_all_with_bounded_retries(&sessions)?;
        assert!(sessions.is_drained());
    }
    Ok(())
}

#[test]
fn join_all_handles_free_children_and_retains_contended_child_for_retry() -> Result<(), TestError> {
    let (_workspace, sessions) = sessions(92)?;
    let free = sleeping_child(&sessions)?;
    let contended = sleeping_child(&sessions)?;
    let session = sessions.session(contended)?;
    let operation = session.enter()?;

    let started = Instant::now();
    let error = sessions
        .join_all()
        .err()
        .ok_or("cleanup unexpectedly joined")?;
    assert_bounded(started);
    assert_eq!(error.kind(), ExecutionBackendErrorKind::CleanupUnresolved);
    assert!(sessions.session(free).is_err());
    assert!(sessions.session(contended).is_ok());
    assert_eq!(sessions.active_session_ids(), vec![contended]);

    drop(operation);
    join_all_with_bounded_retries(&sessions)?;
    assert!(sessions.is_drained());
    Ok(())
}

/// The session limit fits macOS's default soft open-file limit of 256: a
/// session whose wake pipe would leave too little room polls instead, so no
/// start is refused for want of descriptors. It lowers this process's limit,
/// so it runs in the serial lane, one test per process.
#[test]
fn the_session_limit_fits_a_256_descriptor_soft_limit() -> Result<(), TestError> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    struct Restore(Rlimit);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = setrlimit(Resource::Nofile, self.0);
        }
    }

    let original = getrlimit(Resource::Nofile);
    let _restore = Restore(original);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(256),
            maximum: original.maximum,
        },
    )?;
    let (_workspace, sessions) = sessions(120)?;
    for _ in 0..MAX_JOINED_PROCESS_SESSIONS {
        sleeping_child(&sessions)?;
    }
    join_all_with_bounded_retries(&sessions)?;
    Ok(())
}
