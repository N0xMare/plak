use super::*;
use crate::canonical::{
    AwaitWake, CanonicalProcessAwaitRequest, CanonicalProcessInputDisposition,
    CanonicalProcessStartRequest, CanonicalProcessWriteRequest,
};
use plakous_model::{CancellationToken, RunId};
use plakous_tools::{ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope};
use std::str::FromStr;
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn leftover_jobs_leader_exit_wakes_an_already_held_quiet_poll() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    plakous_tools::internal::set_process_notice_delivery_available(&sessions, true);
    let tools = ProcessSessionTools::posix_sh()?.with_config(ProcessPackConfig {
        completion_notices: true,
    });
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: "read go; sleep 30 & exit 7".to_owned(),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()),
        )
        .await?;
    let handle = started.projection.session_id.ok_or("budget handle")?;
    let bound = ProcessSessionTools::session(&environment, handle)?;
    bound
        .quiet_polls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .quiet_polls = 1;
    let control = ExecutionOperationControl::new(CancellationToken::new());
    let observed = ProcessSessionTools::observe_summarized(
        &environment,
        handle,
        30_000,
        4096,
        JoinedProcessOutputCursor::new(0),
        ObservationWait::Output,
        &control,
    );
    let release = async {
        sessions.write_stdin(
            started.session_id,
            b"go\n",
            JoinedProcessInputDisposition::KeepOpen,
        )
    };
    let (result, released) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(observed, release)
    })
    .await?;
    released?;
    let value: Value = serde_json::from_str(&result?.into_outcome()?.joined_text())?;
    assert_eq!(value["status"], "leftover");
    assert_eq!(value["exit_code"], 7);
    sessions.stop_jobs(
        plakous_tools::JobStopReason::Model,
        Duration::from_millis(200),
    )?;
    Ok(())
}

#[tokio::test]
async fn leftover_jobs_unreaped_leader_pins_the_owned_group() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    let result = start_leftover(&environment, "echo $$ > leader; sleep 30 & exit 0", false).await?;
    let pid = std::fs::read_to_string(directory.path().join("leader"))?
        .trim()
        .parse::<i32>()?;
    let pid = rustix::process::Pid::from_raw(pid).ok_or("leader pid")?;
    let exit = rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )?;
    assert!(
        exit.is_some(),
        "leader was reaped while its group was still owned"
    );
    sessions.stop(result.session_id, plakous_tools::JobStopReason::Model)?;
    assert!(rustix::process::test_kill_process(pid).is_err());
    Ok(())
}

#[tokio::test]
async fn leftover_jobs_disowned_writer_keeps_pipe_and_spools_until_eof() -> TestResult {
    struct FinishOnDrop(PathBuf);
    impl Drop for FinishOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "");
        }
    }
    let directory = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let _finish = FinishOnDrop(directory.path().join("finish"));
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&state.path().join("jobs"))?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let result = start_leftover(
        &environment,
        "(while [ ! -f finish ]; do sleep 0.02; done; printf AFTER_DISOWN) & exit 0",
        false,
    )
    .await?;
    let path = sessions
        .job_output_file(JoinedProcessSessions::job_number(result.session_id))
        .ok_or("spool")?
        .path;
    sessions.disown_leftovers()?;
    std::fs::write(directory.path().join("finish"), "")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !std::fs::read_to_string(&path)?.contains("AFTER_DISOWN") {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn literal_joined_commands_do_not_promote_leftover_shell_children() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    let mut changes = sessions.job_terminal_changes();
    let id = sessions.start(
        JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                "/bin/sh",
                ["-c".to_owned(), "sleep 30 & exit 0".to_owned()],
            )?,
            ".",
            Vec::new(),
            false,
            JoinedProcessTerminalMode::Pipes,
        )?,
        ExecutionOperationControl::new(CancellationToken::new()),
    )?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !sessions
            .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 16)?
            .status()
            .is_terminal()
        {
            changes.changed().await?;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    sessions.join(id)?;
    assert_eq!(sessions.leftover(id), None);
    assert!(!sessions.is_promoted(id));
    Ok(())
}

#[tokio::test]
async fn leftover_jobs_budget_promotion_converts_and_wait_reports_leader_exit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: "read go; sleep 30 & exit 7".to_owned(),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()),
        )
        .await?;
    let id = started.session_id;
    let handle = started.projection.session_id.ok_or("budget handle")?;
    assert_eq!(sessions.leftover(id), None);
    let before = Instant::now();
    let result = ProcessSessionTools::write_canonical(
        &environment,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: "go\n".to_owned(),
            disposition: CanonicalProcessInputDisposition::KeepOpen,
            yield_time_ms: Some(30_000),
            max_output_bytes: Some(4096),
        },
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    assert!(before.elapsed() < Duration::from_secs(5));
    assert!(!result.joined);
    let value: Value = serde_json::from_str(&result.into_outcome()?.joined_text())?;
    assert_eq!(value["status"], "leftover");
    assert_eq!(value["exit_code"], 7);
    let outcome = ProcessSessionTools::await_canonical(
        &environment,
        &await_request(&[handle], 0),
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    assert!(outcome.joined_text().contains("leader exited 7"));
    assert_eq!(sessions.leftover(id), Some((Some(7), 1)));
    sessions.stop_jobs(
        plakous_tools::JobStopReason::Model,
        Duration::from_millis(200),
    )?;
    Ok(())
}

#[tokio::test]
async fn leftover_jobs_pty_output_after_leader_exit_is_platform_specific() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let result = start_leftover(&environment,
        "(while [ ! -f finish ]; do sleep 0.02; done; printf AFTER_LEADER; echo $? > write-status) & exit 0", true).await?;
    let sessions = environment.joined_process_sessions()?;
    let id = result.session_id;
    std::fs::write(directory.path().join("finish"), "")?;
    let mut changes = sessions.job_terminal_changes();
    tokio::time::timeout(Duration::from_secs(5), async {
        while sessions.running_job_count() != 0 {
            changes.changed().await?;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    let status = std::fs::read_to_string(directory.path().join("write-status"))?;
    #[cfg(target_os = "linux")]
    {
        assert_eq!(status.trim(), "0");
        assert!(sessions.jobs()[0].output.contains("AFTER_LEADER"));
    }
    #[cfg(target_os = "macos")]
    {
        assert_ne!(status.trim(), "0");
        assert!(!sessions.jobs()[0].output.contains("AFTER_LEADER"));
    }
    sessions.join(id)?;
    assert!(sessions.joined_terminal_status(id).is_some());
    Ok(())
}

async fn start_leftover(
    environment: &ExecutionEnvironment,
    command: &str,
    tty: bool,
) -> TestResult<ProcessExecutionResult> {
    let tools = ProcessSessionTools::posix_sh()?;
    Ok(tools
        .start_canonical(
            environment,
            CanonicalProcessStartRequest {
                command: command.to_owned(),
                workdir: PathBuf::from("."),
                login: false,
                tty,
                keep_stdin_open: false,
                _model_description: None,
                yield_time_ms: 30_000,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()),
        )
        .await?)
}

#[tokio::test]
async fn leftover_jobs_return_promptly_and_stop_the_live_group() -> TestResult {
    for tty in [false, true] {
        for prefix in ["", "nohup "] {
            let directory = tempfile::tempdir()?;
            let environment = environment(directory.path())?;
            let sessions = environment.joined_process_sessions()?;
            let before = Instant::now();
            let result = start_leftover(
                &environment,
                &format!("{prefix}sleep 30 >/dev/null 2>&1 & echo $! > child; exit 7"),
                tty,
            )
            .await?;
            assert!(before.elapsed() < Duration::from_secs(5));
            let id = result.session_id;
            assert!(result.first_promotion);
            assert_eq!(sessions.leftover(id), Some((Some(7), 1)));
            let value: Value = serde_json::from_str(&result.into_outcome()?.joined_text())?;
            assert_eq!(value["exit_code"], 7);
            assert!(
                value["output"]
                    .as_str()
                    .ok_or("output")?
                    .contains("1 process still running as job")
            );
            let pid = std::fs::read_to_string(directory.path().join("child"))?
                .trim()
                .parse::<i32>()?;
            let pid = rustix::process::Pid::from_raw(pid).ok_or("pid")?;
            assert!(rustix::process::test_kill_process(pid).is_ok());
            assert_eq!(
                sessions.stop(id, plakous_tools::JobStopReason::Model)?,
                JoinedProcessStatus::Terminated
            );
            assert_eq!(
                sessions.jobs()[0].status,
                plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::Model)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn leftover_jobs_report_group_empty_and_keep_spooling() -> TestResult {
    let directory = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&state.path().join("jobs"))?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let result = start_leftover(
        &environment,
        "(while [ ! -f finish ]; do sleep 0.02; done; echo AFTER) & exit 0",
        false,
    )
    .await?;
    let id = result.session_id;
    let number = JoinedProcessSessions::job_number(id);
    let mut changes = sessions.job_terminal_changes();
    std::fs::write(directory.path().join("finish"), "")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while sessions.running_job_count() != 0 {
            changes.changed().await?;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    assert!(sessions.has_unreported_terminal_job());
    assert_eq!(
        sessions.jobs()[0].status,
        plakous_tools::JobStatus::Exited(Some(0))
    );
    assert!(
        std::fs::read_to_string(sessions.job_output_file(number).ok_or("spool")?.path)?
            .contains("AFTER")
    );
    Ok(())
}

#[tokio::test]
async fn leftover_jobs_release_and_attempt_end_stop_but_explicit_disown_does_not() -> TestResult {
    for reason in [
        plakous_tools::JobStopReason::SessionExit,
        plakous_tools::JobStopReason::AttemptEnded,
    ] {
        let directory = tempfile::tempdir()?;
        let environment = environment(directory.path())?;
        let sessions = environment.joined_process_sessions()?;
        let result = start_leftover(&environment, "sleep 30 & exit 0", false).await?;
        sessions.stop_jobs(reason, Duration::from_millis(200))?;
        assert_eq!(
            sessions.jobs()[0].status,
            plakous_tools::JobStatus::Stopped(reason)
        );
        assert!(sessions.joined_terminal_status(result.session_id).is_some());
    }
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    let result = start_leftover(
        &environment,
        "sleep 30 >/dev/null 2>&1 & echo $! > child; exit 0",
        false,
    )
    .await?;
    let pid = std::fs::read_to_string(directory.path().join("child"))?
        .trim()
        .parse::<i32>()?;
    let pid = rustix::process::Pid::from_raw(pid).ok_or("pid")?;
    sessions.disown_leftovers()?;
    sessions.stop_jobs(
        plakous_tools::JobStopReason::SessionExit,
        Duration::from_millis(200),
    )?;
    assert!(rustix::process::test_kill_process(pid).is_ok());
    rustix::process::kill_process(pid, rustix::process::Signal::KILL)?;
    assert_eq!(
        sessions.joined_terminal_status(result.session_id),
        Some(JoinedProcessStatus::OutcomeUnknown)
    );
    Ok(())
}

#[tokio::test]
async fn children_that_exit_before_the_leader_are_not_leftover_jobs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let result = start_leftover(&environment, "true & wait; exit 0", false).await?;
    assert!(result.joined);
    assert!(!result.first_promotion);
    assert_eq!(
        environment.joined_process_sessions()?.jobs(),
        [] as [plakous_tools::JobView; 0]
    );
    Ok(())
}

#[tokio::test]
async fn untruncated_foreground_output_creates_no_output_file() -> TestResult {
    let directory = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    let root = state.path().join("jobs");
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&root)?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let tools = ProcessSessionTools::posix_sh()?;
    let result = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("printf small"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,
                _model_description: None,
                yield_time_ms: 30_000,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()),
        )
        .await?;
    let id = result.session_id;
    let value: Value = serde_json::from_str(&result.into_outcome()?.joined_text())?;
    assert_eq!(value["output"], "small");
    assert!(
        sessions
            .job_output_file(JoinedProcessSessions::job_number(id))
            .is_none()
    );
    assert_eq!(std::fs::read_dir(&root)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn foreground_lossy_utf8_truncation_spools_the_original_bytes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&state.path().join("jobs"))?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let tools = ProcessSessionTools::posix_sh()?;
    for (escape, byte) in [("\\377", 255), ("\\302", 194)] {
        let result = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: format!("head -c 500 /dev/zero | tr '\\000' x; printf '{escape}'"),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,
                    _model_description: None,
                    yield_time_ms: 30_000,
                    max_output_bytes: 502,
                },
                ExecutionOperationControl::new(CancellationToken::new()),
            )
            .await?;
        let value: Value = serde_json::from_str(&result.into_outcome()?.joined_text())?;
        let path = value["output"]
            .as_str()
            .ok_or("output")?
            .lines()
            .find_map(|line| line.strip_prefix("full output: "))
            .ok_or("lossy output path lost")?;
        let mut expected = vec![b'x'; 500];
        expected.push(byte);
        assert_eq!(std::fs::read(path)?, expected);
    }
    Ok(())
}

#[tokio::test]
async fn foreground_truncation_names_complete_output_before_window_eviction() -> TestResult {
    for tty in [false, true] {
        let directory = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let environment = environment(directory.path())?;
        let sessions = environment.joined_process_sessions()?;
        sessions.bind_job_outputs(
            plakous_tools::JobOutputRoot::open(&state.path().join("jobs"))?,
            plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
            plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
        );
        let tools = ProcessSessionTools::posix_sh()?;
        let result = tools
            .start_canonical(
                &environment,
                CanonicalProcessStartRequest {
                    command: String::from(
                        "printf FIRST; head -c 2000000 /dev/zero | tr '\\000' x; printf LAST",
                    ),
                    workdir: PathBuf::from("."),
                    login: false,
                    tty,
                    keep_stdin_open: tty,
                    _model_description: None,
                    yield_time_ms: 30_000,
                    max_output_bytes: 4096,
                },
                ExecutionOperationControl::new(CancellationToken::new()),
            )
            .await?;
        assert!(result.joined && !result.first_promotion);
        let id = result.session_id;
        let value: Value = serde_json::from_str(&result.into_outcome()?.joined_text())?;
        let output = value["output"].as_str().ok_or("output")?;
        let path = output
            .lines()
            .find_map(|line| line.strip_prefix("full output: "))
            .ok_or("foreground output path lost")?;
        let file = std::fs::read_to_string(path)?;
        assert!(file.starts_with("FIRST") && file.ends_with("LAST"));
        assert_eq!(file.len(), 2_000_009);
        assert_eq!(
            sessions
                .job_output_file(JoinedProcessSessions::job_number(id))
                .ok_or("file")?
                .path,
            path
        );
        assert!(!sessions.is_promoted(id));
    }
    Ok(())
}

#[tokio::test]
async fn a_foreign_or_unknown_process_is_not_in_this_session() -> TestResult {
    let first_root = tempfile::tempdir()?;
    let second_root = tempfile::tempdir()?;
    let first = environment(first_root.path())?;
    let second = environment(second_root.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let number = start(&tools, &first).await?;
    {
        let environment = &second;
        let error = ProcessSessionTools::session(environment, number)
            .err()
            .ok_or("foreign id accepted")?;
        assert!(error.to_string().contains("not in this session"));
        assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
    }
    let error = ProcessSessionTools::session(&first, number + 1)
        .err()
        .ok_or("unknown id accepted")?;
    assert!(error.to_string().contains("not in this session"));
    first.joined_process_sessions()?.stop_jobs(
        plakous_tools::JobStopReason::Model,
        Duration::from_millis(250),
    )?;
    Ok(())
}

#[tokio::test]
async fn process_terminal_generation_is_edge_triggered_and_survives_consumption() -> TestResult {
    let root = tempfile::tempdir()?;
    let environment = environment(root.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let number = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    let admission = sessions.job_terminal_generation();
    let id = sessions
        .session_for_number(u32::try_from(number)?)
        .ok_or("job")?;
    sessions.stop(id, plakous_tools::JobStopReason::Model)?;
    let generation = sessions.job_terminal_generation();
    assert!(generation > admission);
    assert_eq!(
        sessions.terminal_jobs_since(admission),
        vec![format!("p{number}")]
    );
    plakous_tools::internal::consume_jobs(&sessions, &[u32::try_from(number)?]);
    assert_eq!(
        sessions.terminal_jobs_since(generation),
        Vec::<String>::new()
    );
    assert_eq!(
        sessions.terminal_jobs_since(admission),
        vec![format!("p{number}")]
    );
    Ok(())
}

fn environment(root: &Path) -> TestResult<ExecutionEnvironment> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000771")?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?)
}

async fn start(tools: &ProcessSessionTools, environment: &ExecutionEnvironment) -> TestResult<u64> {
    let result = tools
        .start_canonical(
            environment,
            CanonicalProcessStartRequest {
                command: String::from("read first; printf progress; read second"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()),
        )
        .await?;
    result
        .projection
        .session_id
        .ok_or_else(|| "job handle".into())
}

#[tokio::test]
async fn empty_poll_returns_on_new_output_without_waiting_full_window() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let handle = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions
        .session_for_number(u32::try_from(handle)?)
        .ok_or("id")?;
    sessions.write_stdin(id, b"first\n", JoinedProcessInputDisposition::KeepOpen)?;
    let before = Instant::now();
    let polled = ProcessSessionTools::write_canonical(
        &environment,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: String::new(),
            disposition: CanonicalProcessInputDisposition::KeepOpen,
            yield_time_ms: Some(5000),
            max_output_bytes: Some(4096),
        },
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    let elapsed = before.elapsed();
    sessions.stop(id, plakous_tools::JobStopReason::Model)?;
    assert_eq!(polled.projection.output, "progress");
    assert!(elapsed < Duration::from_secs(2), "poll waited {elapsed:?}");
    Ok(())
}

#[tokio::test]
async fn control_c_stops_a_pipe_job_and_consumes_as_terminal_result() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let handle = start(&tools, &environment).await?;
    let stopped = ProcessSessionTools::write_canonical(
        &environment,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: String::from("\u{3}"),
            disposition: CanonicalProcessInputDisposition::KeepOpen,
            yield_time_ms: Some(250),
            max_output_bytes: Some(4096),
        },
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    let jobs = environment.joined_process_sessions()?;
    // Cleanup also makes a failure safe to rerun.
    jobs.stop_jobs(
        plakous_tools::JobStopReason::Model,
        Duration::from_millis(200),
    )?;
    assert!(stopped.joined);
    let outcome = stopped.into_outcome()?;
    assert!(matches!(
        plakous_tools::internal::process_session_update(&outcome),
        Some(plakous_tools::internal::ProcessSessionUpdate::JobTerminal { number }) if u64::from(number) == handle
    ));
    assert_eq!(
        jobs.jobs()[0].status,
        plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::Model)
    );
    Ok(())
}

#[test]
fn process_descriptions_explain_session_owned_background_jobs() -> TestResult {
    for kind in [
        ProcessToolProjectionKind::PiBash,
        ProcessToolProjectionKind::CodexUnifiedExec,
        ProcessToolProjectionKind::ClaudeOAuthBash,
        ProcessToolProjectionKind::GrokTerminal,
    ] {
        let descriptions = kind
            .tools(plakous_tools::JobConfig::default())?
            .iter()
            .map(|tool| tool.description().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(descriptions.contains("background"));
        assert!(descriptions.contains("automatically"));
        assert!(!descriptions.contains("terminated when the current root turn ends"));
    }
    Ok(())
}

#[tokio::test]
async fn steer_promotes_a_foreground_process_without_killing_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let (_preempt, receiver) = tokio::sync::watch::channel(plakous_tools::ToolPreempt::Steer);
    let result = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: "read line; printf finished".to_owned(),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 600_000,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()).with_preempt(receiver),
        )
        .await?;
    let handle = result.projection.session_id.ok_or("promoted handle")?;
    let sessions = environment.joined_process_sessions()?;
    assert_eq!(sessions.jobs()[0].status, plakous_tools::JobStatus::Running);
    let completed = ProcessSessionTools::write_canonical(
        &environment,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: "finish\n".to_owned(),
            disposition: CanonicalProcessInputDisposition::KeepOpen,
            yield_time_ms: Some(30_000),
            max_output_bytes: Some(4096),
        },
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    assert!(completed.joined);
    assert_eq!(completed.projection.exit_code, Some(0));
    assert_eq!(completed.projection.output, "finished");
    Ok(())
}

#[tokio::test]
async fn steer_at_full_job_ledger_keeps_foreground_observation() -> TestResult {
    preempt_at_full_job_ledger_keeps_foreground_observation(
        plakous_tools::ToolPreempt::Steer,
        30_000,
    )
    .await
}

#[tokio::test]
async fn background_at_full_job_ledger_keeps_foreground_observation() -> TestResult {
    preempt_at_full_job_ledger_keeps_foreground_observation(
        plakous_tools::ToolPreempt::Background,
        30_000,
    )
    .await
}

#[tokio::test]
async fn background_at_full_job_ledger_outlives_original_hint_without_killing() -> TestResult {
    preempt_at_full_job_ledger_keeps_foreground_observation(
        plakous_tools::ToolPreempt::Background,
        250,
    )
    .await
}

async fn preempt_at_full_job_ledger_keeps_foreground_observation(
    preempt: plakous_tools::ToolPreempt,
    yield_time_ms: u64,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    let started_at = plakous_model::Timestamp::new(10, 0)?;
    let views = (1..=128)
        .map(|number| plakous_tools::JobView {
            number,
            command: "finished fixture".into(),
            started_at,
            owner: "fixture".into(),
            elapsed: Duration::ZERO,
            status: plakous_tools::JobStatus::Exited(Some(0)),
            output: String::new(),
            session_id: [0; 16],
            owner_generation: 1,
        })
        .collect::<Vec<_>>();
    plakous_tools::internal::restore_job_terminals(&sessions, &views);
    plakous_tools::internal::seed_job_counter(&sessions, 129);
    let (_sender, preempt) = tokio::sync::watch::channel(preempt);
    let result = ProcessSessionTools::posix_sh()?
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: "sleep 0.35; printf survived".into(),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,
                _model_description: None,
                yield_time_ms,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()).with_preempt(preempt),
        )
        .await;
    sessions.join_all()?;
    let result = result?;
    assert!(result.joined);
    assert_eq!(result.projection.exit_code, Some(0));
    assert_eq!(result.projection.output, "survived");
    assert_eq!(sessions.jobs().len(), 128);
    Ok(())
}

fn await_request(handles: &[u64], timeout: u64) -> CanonicalProcessAwaitRequest {
    CanonicalProcessAwaitRequest {
        session_ids: handles.to_vec(),
        wake: None,
        yield_time_ms: timeout,
        max_output_bytes: 4096,
        max_output_records: 256,
    }
}

#[tokio::test]
async fn grok_snapshots_read_output_so_far_without_consuming_running_jobs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let handle = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions
        .session_for_number(u32::try_from(handle)?)
        .ok_or("id")?;
    sessions.write_stdin(id, b"first\n", JoinedProcessInputDisposition::KeepOpen)?;
    let control = ExecutionOperationControl::new(CancellationToken::new());
    // Synchronize on output, not on a sleep or an assumed scheduler deadline.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let poll = sessions.poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?;
        if !poll.records().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "no progress");
        std::thread::yield_now();
    }
    for _ in 0..2 {
        let outcome = ProcessSessionTools::await_canonical(
            &environment,
            &await_request(&[handle], 0),
            &control,
        )
        .await?;
        let value: Value = serde_json::from_str(&outcome.joined_text())?;
        assert_eq!(value["tasks"][0]["output"], "progress");
        assert_eq!(value["tasks"][0]["status"], "running");
        assert_eq!(
            plakous_tools::internal::process_session_update(&outcome),
            None
        );
    }
    sessions.stop(id, plakous_tools::JobStopReason::Model)?;
    Ok(())
}

#[tokio::test]
async fn grok_await_waits_for_all_processes_and_reports_only_terminal_ids() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let first = start(&tools, &environment).await?;
    let second = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    for handle in [first, second] {
        let id = sessions
            .session_for_number(u32::try_from(handle)?)
            .ok_or("id")?;
        sessions.write_stdin(id, b"one\ntwo\n", JoinedProcessInputDisposition::KeepOpen)?;
    }
    let outcome = ProcessSessionTools::await_canonical(
        &environment,
        &await_request(&[first, second], 3_600_000),
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    let value: Value = serde_json::from_str(&outcome.joined_text())?;
    for task in value["tasks"].as_array().ok_or("tasks")? {
        assert_eq!(task["status"], "exited");
        assert_eq!(task["exit_code"], 0);
        assert_eq!(task["output"], "progress");
    }
    assert!(
        matches!(plakous_tools::internal::process_session_update(&outcome),
        Some(plakous_tools::internal::ProcessSessionUpdate::JobTerminals { numbers }) if numbers == [u32::try_from(first)?, u32::try_from(second)?])
    );
    // Consumption is deferred to the durable top-level result commit.
    assert_eq!(sessions.jobs().len(), 2);
    Ok(())
}

/// `wake: any` returns at the first terminal, and without ids it watches every
/// own job still listed: one finished job wakes it while the other keeps
/// running and is reported as running.
#[tokio::test]
async fn first_terminal_await_over_every_own_job_returns_when_one_ends() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let first = start(&tools, &environment).await?;
    let second = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions
        .session_for_number(u32::try_from(second)?)
        .ok_or("id")?;
    sessions.write_stdin(id, b"one\ntwo\n", JoinedProcessInputDisposition::KeepOpen)?;
    let mut request = await_request(&[], 3_600_000);
    request.wake = Some(AwaitWake::Any);
    request.validate()?;
    let before = Instant::now();
    let outcome = ProcessSessionTools::await_canonical(
        &environment,
        &request,
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    assert!(before.elapsed() < Duration::from_secs(30));
    let value: Value = serde_json::from_str(&outcome.joined_text())?;
    let tasks = value["tasks"].as_array().ok_or("tasks")?;
    assert_eq!(tasks.len(), 2);
    let status = |handle: u64| {
        tasks
            .iter()
            .find(|task| task["task_id"] == format!("p{handle}"))
            .map(|task| task["status"].clone())
    };
    assert_eq!(status(second), Some(Value::from("exited")));
    assert_eq!(status(first), Some(Value::from("running")));
    assert!(
        matches!(plakous_tools::internal::process_session_update(&outcome),
        Some(plakous_tools::internal::ProcessSessionUpdate::JobTerminals { numbers }) if numbers == [u32::try_from(second)?])
    );
    // Without `wake`, an await still names its processes.
    assert!(await_request(&[], 0).validate().is_err());
    let id = sessions
        .session_for_number(u32::try_from(first)?)
        .ok_or("id")?;
    sessions.stop(id, plakous_tools::JobStopReason::Model)?;
    Ok(())
}

/// A leaf's bare `wait` (no ids, no jobs) is this await. It returns the empty
/// task list at once instead of parking until `yield_time_ms`.
#[tokio::test]
async fn bare_first_terminal_await_with_no_jobs_returns_empty_tasks_at_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    assert_eq!(
        environment.joined_process_sessions()?.jobs(),
        [] as [plakous_tools::JobView; 0]
    );
    let mut request = await_request(&[], 60_000);
    request.wake = Some(AwaitWake::Any);
    request.validate()?;
    let before = Instant::now();
    let outcome = ProcessSessionTools::await_canonical(
        &environment,
        &request,
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    assert!(
        before.elapsed() < Duration::from_secs(2),
        "a bare wait with no jobs parked for {:?}",
        before.elapsed()
    );
    assert_eq!(outcome.joined_text(), r#"{"tasks":[]}"#);
    Ok(())
}

#[tokio::test]
async fn grok_await_steer_returns_without_stopping_or_consuming_running_jobs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let handle = start(&tools, &environment).await?;
    let (_sender, receiver) = tokio::sync::watch::channel(plakous_tools::ToolPreempt::Steer);
    let before = Instant::now();
    let outcome = ProcessSessionTools::await_canonical(
        &environment,
        &await_request(&[handle], 3_600_000),
        &ExecutionOperationControl::new(CancellationToken::new()).with_preempt(receiver),
    )
    .await?;
    assert!(before.elapsed() < Duration::from_secs(2));
    assert_eq!(
        plakous_tools::internal::process_session_update(&outcome),
        None
    );
    let sessions = environment.joined_process_sessions()?;
    assert_eq!(sessions.jobs()[0].status, plakous_tools::JobStatus::Running);
    let id = sessions
        .session_for_number(u32::try_from(handle)?)
        .ok_or("id")?;
    let stopped = ProcessSessionTools::terminate_canonical(
        &environment,
        &CanonicalProcessTerminateRequest { session_id: handle },
    )
    .await?;
    assert!(stopped.joined);
    assert_eq!(
        sessions.jobs()[0].status,
        plakous_tools::JobStatus::Stopped(plakous_tools::JobStopReason::Model)
    );
    let again = ProcessSessionTools::terminate_canonical(
        &environment,
        &CanonicalProcessTerminateRequest { session_id: handle },
    )
    .await?;
    assert_eq!(again.status, stopped.status);
    assert!(sessions.joined_terminal_status(id).is_some());
    Ok(())
}

#[tokio::test]
async fn grok_terminal_snapshot_survives_repeated_reads_and_preserves_natural_exit_on_stop()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let handle = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions
        .session_for_number(u32::try_from(handle)?)
        .ok_or("id")?;
    sessions.write_stdin(id, b"one\ntwo\n", JoinedProcessInputDisposition::KeepOpen)?;
    let control = ExecutionOperationControl::new(CancellationToken::new());
    let awaited = ProcessSessionTools::await_canonical(
        &environment,
        &await_request(&[handle], 5000),
        &control,
    )
    .await?;
    let snapshot =
        ProcessSessionTools::await_canonical(&environment, &await_request(&[handle], 0), &control)
            .await?;
    assert_eq!(awaited.joined_text(), snapshot.joined_text());
    let stopped = ProcessSessionTools::terminate_canonical(
        &environment,
        &CanonicalProcessTerminateRequest { session_id: handle },
    )
    .await?;
    assert_eq!(stopped.status, "exited");
    assert_eq!(stopped.projection.exit_code, Some(0));
    Ok(())
}

#[test]
fn every_process_description_states_default_numeric_job_bounds() -> TestResult {
    for kind in [
        ProcessToolProjectionKind::PiBash,
        ProcessToolProjectionKind::CodexUnifiedExec,
        ProcessToolProjectionKind::ClaudeOAuthBash,
        ProcessToolProjectionKind::GrokTerminal,
    ] {
        for tool in kind.tools(plakous_tools::JobConfig::default())? {
            assert!(
                tool.description().contains("foreground budget: 30 seconds"),
                "{}: {}",
                tool.name(),
                tool.description()
            );
            assert!(tool.description().contains("hard runtime cap: 10 hours"));
            let shell = matches!(
                tool.name().as_str(),
                "bash" | "exec_command" | "run_terminal_command" | "Bash"
            );
            let action = tool.description().contains("Read: exec_command")
                || tool.description().contains("Wait by giving a long command")
                || tool
                    .description()
                    .contains("Wait with wait; stop with stop")
                || tool
                    .description()
                    .contains("Read with get_command_or_subagent_output");
            assert_eq!(action, shell, "{}: {}", tool.name(), tool.description());
            if tool.name().as_str() == "bash" {
                assert!(
                    tool.description()
                        .contains("foreground budget (30 seconds)"),
                    "{}",
                    tool.description()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn every_process_description_states_non_default_numeric_job_bounds() -> TestResult {
    let config = plakous_tools::JobConfig::new(47, 3)?;
    for kind in [
        ProcessToolProjectionKind::PiBash,
        ProcessToolProjectionKind::CodexUnifiedExec,
        ProcessToolProjectionKind::ClaudeOAuthBash,
        ProcessToolProjectionKind::GrokTerminal,
    ] {
        for tool in kind.tools(config)? {
            assert!(
                tool.description()
                    .contains("foreground budget: 47 seconds (47000 ms)")
            );
            assert!(tool.description().contains("hard runtime cap: 3 hours"));
            assert!(
                tool.description()
                    .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
            );
            assert!(!tool.description().contains("default 30000 ms"));
            assert!(!tool.description().contains("10 hours"));
        }
    }
    let codex = ProcessToolProjectionKind::CodexUnifiedExec.tools(config)?;
    assert!(
        codex[0].schema().document().value()["properties"]["yield_time_ms"]["description"]
            .as_str()
            .ok_or("hint description absent")?
            .contains("defaults to 47000 ms")
    );
    Ok(())
}

#[tokio::test]
async fn promotion_and_truncated_pages_name_the_job_output_file() -> TestResult {
    let directory = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        plakous_tools::JobOutputRoot::open(&state.path().join("jobs"))?,
        plakous_model::SessionId::from_str("00000000-0000-0000-0000-0000000000b0")?,
        plakous_model::BranchId::from_str("00000000-0000-0000-0000-0000000000c0")?,
    );
    let tools = ProcessSessionTools::posix_sh()?;
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("read first; printf progress; read second"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(CancellationToken::new()),
        )
        .await?;
    let handle = started.projection.session_id.ok_or("job handle")?;
    let path = sessions
        .job_output_file(u32::try_from(handle)?)
        .ok_or("promotion issued no output file")?
        .path;
    let output = |outcome: &ToolOutcome| -> TestResult<String> {
        let value: Value = serde_json::from_str(&outcome.joined_text())?;
        Ok(value["output"].as_str().ok_or("output")?.to_owned())
    };
    assert_eq!(
        output(&started.into_outcome()?)?,
        format!("full output: {path}")
    );

    let id = sessions
        .session_for_number(u32::try_from(handle)?)
        .ok_or("id")?;
    sessions.write_stdin(id, b"first\n", JoinedProcessInputDisposition::KeepOpen)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while sessions
        .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
        .records()
        .is_empty()
    {
        assert!(Instant::now() < deadline, "no progress");
        std::thread::yield_now();
    }
    let control = ExecutionOperationControl::new(CancellationToken::new());
    let mut request = await_request(&[handle], 0);
    request.max_output_bytes = 4;
    let value: Value = serde_json::from_str(
        &ProcessSessionTools::await_canonical(&environment, &request, &control)
            .await?
            .joined_text(),
    )?;
    let task = value["tasks"][0]["output"].as_str().ok_or("task output")?;
    assert!(task.contains("bytes of output omitted"), "{task}");
    assert!(task.ends_with(&format!("\nfull output: {path}")), "{task}");
    let whole: Value = serde_json::from_str(
        &ProcessSessionTools::await_canonical(&environment, &await_request(&[handle], 0), &control)
            .await?
            .joined_text(),
    )?;
    assert_eq!(whole["tasks"][0]["output"], "progress");

    let poll = |max_output_bytes| CanonicalProcessWriteRequest {
        session_id: handle,
        input: String::new(),
        disposition: CanonicalProcessInputDisposition::KeepOpen,
        yield_time_ms: Some(250),
        max_output_bytes: Some(max_output_bytes),
    };
    sessions.write_stdin(id, b"second\n", JoinedProcessInputDisposition::KeepOpen)?;
    let page = ProcessSessionTools::write_canonical(&environment, &poll(4), &control).await?;
    let text = output(&page.into_outcome()?)?;
    assert!(text.ends_with(&format!("\nfull output: {path}")), "{text}");
    sessions.join_all()?;
    Ok(())
}

/// `wake: "any"` (pi's `wait`) over named ids waits for a still-running named
/// process when another named id was already reported before the call: an
/// already-reported or unknown id is not a fresh terminal.
#[tokio::test]
async fn first_terminal_await_waits_past_an_already_reported_id() -> TestResult {
    let directory = tempfile::tempdir()?;
    let environment = environment(directory.path())?;
    let tools = ProcessSessionTools::posix_sh()?;
    let reported = start(&tools, &environment).await?;
    let running = start(&tools, &environment).await?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions
        .session_for_number(u32::try_from(reported)?)
        .ok_or("id")?;
    sessions.stop(id, plakous_tools::JobStopReason::Model)?;
    // Its completion notice was delivered and committed.
    plakous_tools::internal::consume_jobs(&sessions, &[u32::try_from(reported)?]);
    let mut request = await_request(&[reported, running], 1_500);
    request.wake = Some(AwaitWake::Any);
    request.validate()?;
    let before = Instant::now();
    let outcome = ProcessSessionTools::await_canonical(
        &environment,
        &request,
        &ExecutionOperationControl::new(CancellationToken::new()),
    )
    .await?;
    let elapsed = before.elapsed();
    sessions.join_all()?;
    assert!(
        elapsed >= Duration::from_millis(1_000),
        "returned after {elapsed:?} while p{running} still ran: {}",
        outcome.joined_text()
    );
    Ok(())
}
