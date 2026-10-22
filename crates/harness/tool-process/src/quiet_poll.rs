//! Virtual-time observations against a pipe job whose progress we control.
use super::*;
use crate::canonical::{
    CanonicalProcessAwaitRequest, CanonicalProcessInputDisposition, CanonicalProcessPollRequest,
    CanonicalProcessStartRequest, CanonicalProcessWriteRequest,
};
use plakous_model::{CancellationToken, RunId};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, ToolPreempt,
};
use std::str::FromStr;
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn environment(root: &Path) -> TestResult<ExecutionEnvironment> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000772")?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?)
}

fn control() -> ExecutionOperationControl {
    ExecutionOperationControl::new(CancellationToken::new())
}

async fn start(environment: &ExecutionEnvironment, notices: bool) -> TestResult<u64> {
    let result = ProcessSessionTools::posix_sh()?
        .with_config(ProcessPackConfig {
            completion_notices: notices,
        })
        .start_canonical(
            environment,
            CanonicalProcessStartRequest {
                command: "read first; printf progress; read second".into(),
                workdir: ".".into(),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 4096,
            },
            control(),
        )
        .await?;
    result
        .projection
        .session_id
        .ok_or_else(|| "job handle missing".into())
}

async fn poll(
    environment: &ExecutionEnvironment,
    handle: u64,
    hint: u64,
    control: &ExecutionOperationControl,
) -> Result<ProcessExecutionResult, ToolRuntimeError> {
    ProcessSessionTools::write_canonical(
        environment,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: String::new(),
            disposition: CanonicalProcessInputDisposition::KeepOpen,
            yield_time_ms: Some(hint),
            max_output_bytes: Some(4096),
        },
        control,
    )
    .await
}

// Keep virtual time fixed while the real process supervisor and blocking poll settle.
async fn without_advancing<F: Future>(future: F) -> F::Output {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            () = tokio::task::yield_now() => {}
        }
    }
}

fn quiet_line(handle: u64) -> String {
    format!("No new output; process {handle} is still running. Its exit is reported automatically.")
}

#[tokio::test(start_paused = true)]
async fn first_quiet_poll_keeps_hint_and_repeat_uses_900_second_ceiling() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    let before = tokio::time::Instant::now();
    let result = poll(&env, handle, 5000, &control()).await?;
    assert_eq!(before.elapsed(), Duration::from_secs(5));
    assert_eq!(result.projection.output, "");
    let before = tokio::time::Instant::now();
    let result = ProcessSessionTools::poll_canonical(
        &env,
        &CanonicalProcessPollRequest {
            session_id: handle,
            cursor: None,
            read_so_far: false,
            wait_for_exit: false,
            yield_time_ms: 300_000,
            max_output_bytes: 4096,
            max_output_records: 256,
        },
        &control(),
    )
    .await?;
    assert_eq!(before.elapsed(), Duration::from_secs(900));
    assert_eq!(result.projection.session_id, Some(handle));
    assert_eq!(result.projection.output, quiet_line(handle));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn repeat_quiet_poll_waits_past_300_seconds_then_returns_on_exit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    let control = control();
    let began = tokio::time::Instant::now();
    let waiting = poll(&env, handle, 300_000, &control);
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        result = &mut waiting => panic!("repeat returned before exit: {:?}", result?.projection.output),
        () = tokio::time::sleep(Duration::from_secs(301)) => {}
    }
    let session = ProcessSessionTools::session(&env, handle)?;
    session
        .sessions
        .write_stdin(session.id, b"", JoinedProcessInputDisposition::Close)?;
    while session
        .sessions
        .joined_terminal_status(session.id)
        .is_none()
    {
        tokio::task::yield_now().await;
    }
    let result = without_advancing(waiting).await?;
    assert!(
        began.elapsed() <= Duration::from_secs(302),
        "wake waited {:?}",
        began.elapsed()
    );
    assert!(result.joined);
    assert!(result.projection.session_id.is_none());
    assert!(result.projection.exit_code.is_some());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn repeat_quiet_poll_yields_on_steer_and_notice() -> TestResult {
    for reason in [ToolPreempt::Steer, ToolPreempt::Notice(false)] {
        let directory = tempfile::tempdir()?;
        let env = environment(directory.path())?;
        let handle = start(&env, true).await?;
        poll(&env, handle, 5000, &control()).await?;
        let (sender, receiver) = tokio::sync::watch::channel(ToolPreempt::None);
        let control = control().with_preempt(receiver);
        let began = tokio::time::Instant::now();
        let waiting = poll(&env, handle, 300_000, &control);
        tokio::pin!(waiting);
        tokio::select! {
            biased;
            result = &mut waiting => panic!("early return: {:?}", result?.projection.output),
            () = tokio::time::sleep(Duration::from_secs(301)) => {}
        }
        sender.send(reason)?;
        let result = without_advancing(waiting).await?;
        assert!(
            began.elapsed() <= Duration::from_secs(302),
            "wake waited {:?}",
            began.elapsed()
        );
        assert!(!result.joined);
        assert_eq!(result.projection.session_id, Some(handle));
        assert_eq!(result.projection.output, quiet_line(handle));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn repeat_quiet_poll_yields_on_another_own_job_terminal() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    let other = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    let control = control();
    let began = tokio::time::Instant::now();
    let waiting = poll(&env, handle, 300_000, &control);
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        result = &mut waiting => panic!("early return: {:?}", result?.projection.output),
        () = tokio::time::sleep(Duration::from_secs(301)) => {}
    }
    let session = ProcessSessionTools::session(&env, other)?;
    session
        .sessions
        .stop(session.id, plakous_tools::JobStopReason::Model)?;
    let result = without_advancing(waiting).await?;
    assert!(
        began.elapsed() <= Duration::from_secs(302),
        "wake waited {:?}",
        began.elapsed()
    );
    assert_eq!(result.projection.session_id, Some(handle));
    assert_eq!(result.projection.output, quiet_line(handle));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn completion_notices_off_keeps_repeat_poll_window() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, false).await?;
    for _ in 0..3 {
        let before = tokio::time::Instant::now();
        let result = poll(&env, handle, 300_000, &control()).await?;
        assert_eq!(before.elapsed(), Duration::from_secs(300));
        assert_eq!(result.projection.output, "");
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn output_and_nonempty_writes_reset_the_quiet_counter() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    let session = ProcessSessionTools::session(&env, handle)?;
    let mut changes =
        plakous_tools::internal::process_session_changes(&session.sessions, session.id)?;
    session
        .sessions
        .write_stdin(session.id, b"go\n", JoinedProcessInputDisposition::KeepOpen)?;
    loop {
        changes.borrow_and_update();
        if !session
            .sessions
            .poll_from(session.id, JoinedProcessOutputCursor::new(0), 4096, 256)?
            .records()
            .is_empty()
        {
            break;
        }
        changes.changed().await?;
    }
    let result = poll(&env, handle, 300_000, &control()).await?;
    assert_eq!(result.projection.output, "progress");
    let before = tokio::time::Instant::now();
    poll(&env, handle, 5000, &control()).await?;
    assert_eq!(before.elapsed(), Duration::from_secs(5));
    let before = tokio::time::Instant::now();
    let result = ProcessSessionTools::write_canonical(
        &env,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: "partial".into(),
            disposition: CanonicalProcessInputDisposition::KeepOpen,
            yield_time_ms: Some(250),
            max_output_bytes: Some(4096),
        },
        &control(),
    )
    .await?;
    assert!(!result.joined);
    assert_eq!(before.elapsed(), Duration::from_millis(250));
    let before = tokio::time::Instant::now();
    poll(&env, handle, 5000, &control()).await?;
    assert_eq!(before.elapsed(), Duration::from_secs(5));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn bash_foreground_wait_returns_an_honest_partial_on_notice() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let (sender, receiver) = tokio::sync::watch::channel(ToolPreempt::None);
    let began = tokio::time::Instant::now();
    let waiting = tools.start_canonical(
        &env,
        CanonicalProcessStartRequest {
            command: "read line".into(),
            workdir: ".".into(),
            login: false,
            tty: false,
            keep_stdin_open: true,
            _model_description: None,
            yield_time_ms: 600_000,
            max_output_bytes: 4096,
        },
        control().with_preempt(receiver),
    );
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        result = &mut waiting => panic!("foreground returned early: {:?}", result?.projection.output),
        () = tokio::time::sleep(Duration::from_secs(1)) => {}
    }
    sender.send(ToolPreempt::Notice(false))?;
    let result = without_advancing(waiting).await?;
    assert!(
        began.elapsed() < Duration::from_secs(600),
        "notice did not preempt the foreground wait"
    );
    assert!(!result.joined);
    assert!(result.first_promotion);
    assert!(result.projection.session_id.is_some());
    assert_eq!(result.projection.output, "");
    let value: Value = serde_json::from_str(
        &result
            .into_outcome_with_promotion(Some((600_000, ToolPreempt::Notice(false))))?
            .joined_text(),
    )?;
    assert_eq!(value["status"], "running");
    assert_eq!(value["preempted"], true);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_repeat_hold_is_prompt_and_does_not_kill_the_job() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    let control = control();
    let began = tokio::time::Instant::now();
    let waiting = poll(&env, handle, 300_000, &control);
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("repeat returned before cancellation"),
        () = tokio::time::sleep(Duration::from_secs(301)) => {}
    }
    control.cancellation().cancel();
    let error = without_advancing(waiting)
        .await
        .err()
        .ok_or("hold ignored cancellation")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
    assert!(began.elapsed() <= Duration::from_secs(302));
    let session = ProcessSessionTools::session(&env, handle)?;
    assert!(
        session
            .sessions
            .joined_terminal_status(session.id)
            .is_none()
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn preempted_first_poll_does_not_earn_a_repeat_hold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    let (_sender, receiver) = tokio::sync::watch::channel(ToolPreempt::Notice(false));
    poll(&env, handle, 5000, &control().with_preempt(receiver)).await?;
    let began = tokio::time::Instant::now();
    poll(&env, handle, 5000, &control()).await?;
    assert_eq!(began.elapsed(), Duration::from_secs(5));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_inline_foreground_terminal_does_not_wake_a_repeat_hold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    let (sender, receiver) = tokio::sync::watch::channel(ToolPreempt::None);
    let control = control().with_preempt(receiver);
    let waiting = poll(&env, handle, 300_000, &control);
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("repeat returned early"),
        () = tokio::time::sleep(Duration::from_secs(301)) => {}
    }
    let sessions = env.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::shell(tools.shell, "true")?,
        ".",
        Vec::new(),
        false,
        JoinedProcessTerminalMode::Pipes,
    )?;
    let id = plakous_tools::internal::start_process_with_output_limit(
        &sessions,
        request,
        control.clone(),
        4096,
    )?;
    while sessions.joined_terminal_status(id).is_none() {
        tokio::task::yield_now().await;
    }
    sessions.join(id)?;
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("inline completion woke a promoted-job hold"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {}
    }
    let began = tokio::time::Instant::now();
    sender.send(ToolPreempt::Notice(false))?;
    without_advancing(waiting).await?;
    assert!(began.elapsed() <= Duration::from_secs(1));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn revoked_host_notice_availability_disables_holds_for_existing_jobs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    plakous_tools::internal::set_process_notice_delivery_available(
        &env.joined_process_sessions()?,
        false,
    );
    for _ in 0..2 {
        let began = tokio::time::Instant::now();
        poll(&env, handle, 5000, &control()).await?;
        assert_eq!(began.elapsed(), Duration::from_secs(5));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn alternate_observations_and_stdin_close_reset_quiet_state() -> TestResult {
    for tail in [false, true] {
        let directory = tempfile::tempdir()?;
        let env = environment(directory.path())?;
        let handle = start(&env, true).await?;
        poll(&env, handle, 5000, &control()).await?;
        let session = ProcessSessionTools::session(&env, handle)?;
        let mut changes =
            plakous_tools::internal::process_session_changes(&session.sessions, session.id)?;
        session.sessions.write_stdin(
            session.id,
            b"go\n",
            JoinedProcessInputDisposition::KeepOpen,
        )?;
        loop {
            changes.borrow_and_update();
            if !session
                .sessions
                .poll_from(session.id, JoinedProcessOutputCursor::new(0), 4096, 256)?
                .records()
                .is_empty()
            {
                break;
            }
            changes.changed().await?;
        }
        if tail {
            ProcessSessionTools::await_canonical(
                &env,
                &CanonicalProcessAwaitRequest {
                    session_ids: vec![handle],
                    wake: None,
                    yield_time_ms: 0,
                    max_output_bytes: 4096,
                    max_output_records: 256,
                },
                &control(),
            )
            .await?;
            assert_eq!(
                session
                    .quiet_polls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .quiet_polls,
                0
            );
            poll(&env, handle, 5000, &control()).await?;
        } else {
            let cursor = *lock_process_cursor(&session.cursor);
            ProcessSessionTools::observe_bounded_with_update(
                &env,
                handle,
                0,
                4096,
                256,
                cursor,
                true,
                ObservationWait::Hint,
                &control(),
            )
            .await?;
        }
        let began = tokio::time::Instant::now();
        poll(&env, handle, 5000, &control()).await?;
        assert_eq!(began.elapsed(), Duration::from_secs(5));
        ProcessSessionTools::write_canonical(
            &env,
            &CanonicalProcessWriteRequest {
                session_id: handle,
                input: String::new(),
                disposition: CanonicalProcessInputDisposition::Close,
                yield_time_ms: None,
                max_output_bytes: None,
            },
            &control(),
        )
        .await?;
        assert_eq!(
            session
                .quiet_polls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .quiet_polls,
            0
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn canonical_tail_and_exit_awaits_do_not_inherit_a_repeat_hold() -> TestResult {
    for completion_notices in [false, true] {
        let directory = tempfile::tempdir()?;
        let env = environment(directory.path())?;
        let handle = start(&env, completion_notices).await?;
        poll(&env, handle, 5000, &control()).await?;
        for (read_so_far, wait_for_exit, expected) in
            [(false, true, 1000), (true, true, 1000), (true, false, 0)]
        {
            let began = tokio::time::Instant::now();
            let result = ProcessSessionTools::poll_canonical(
                &env,
                &CanonicalProcessPollRequest {
                    session_id: handle,
                    cursor: None,
                    read_so_far,
                    wait_for_exit,
                    yield_time_ms: 1000,
                    max_output_bytes: 4096,
                    max_output_records: 256,
                },
                &control(),
            )
            .await?;
            assert_eq!(began.elapsed(), Duration::from_millis(expected));
            assert_eq!(result.projection.output, "");
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn unread_mail_keeps_later_polls_bounded_and_names_the_end_turn_reason() -> TestResult {
    let directory = tempfile::tempdir()?;
    let env = environment(directory.path())?;
    let handle = start(&env, true).await?;
    poll(&env, handle, 5000, &control()).await?;
    plakous_tools::internal::set_process_unread_mail_pending(&env.joined_process_sessions()?, true);
    for _ in 0..3 {
        let began = tokio::time::Instant::now();
        let result = poll(&env, handle, 5000, &control()).await?;
        assert_eq!(began.elapsed(), Duration::from_secs(5));
        assert_eq!(
            result.projection.output,
            "New messages are waiting; end this turn to receive them."
        );
    }
    let (_sender, receiver) = tokio::sync::watch::channel(ToolPreempt::Notice(true));
    let began = tokio::time::Instant::now();
    let result = poll(&env, handle, 5000, &control().with_preempt(receiver)).await?;
    assert_eq!(began.elapsed(), Duration::ZERO);
    assert_eq!(result.projection.session_id, Some(handle));
    assert_eq!(
        result.projection.output,
        "New messages are waiting; end this turn to receive them."
    );
    Ok(())
}
