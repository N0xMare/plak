use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;

use super::*;
use crate::canonical::{
    CanonicalProcessAwaitRequest, CanonicalProcessInputDisposition, CanonicalProcessPollRequest,
    CanonicalProcessStartRequest, CanonicalProcessWriteRequest, CodexWriteArgs,
};
use crate::output::push_output_file;
use crate::projection::{codex_exec_command_schema, write_stdin_schema};
use plakous_model::{
    AttemptId, CallId, IntegrationClass, RunId, ToolCorrelation, ToolEffect, ToolName, TurnId,
};
use plakous_tools::internal::{
    ProcessSessionUpdate, authorization_binding, authorization_grant, bind_effect_dispatch,
    executor, process_session_update, tool_context,
};
use plakous_tools::{
    EffectAuthorizationGrantSource, ExecutionEnvironment, FrozenToolRegistry, ProgressSink,
    ToolExecutor, ToolInvocationRequest, ToolOutcomeKind, ToolPolicyRevision, ToolRegistryBuilder,
    ToolRegistryLimits, register_packs,
};

#[test]
fn a_failed_spool_is_reported_before_the_full_output_line() {
    let mut output = String::from("tail");
    push_output_file(
        &mut output,
        &plakous_tools::JobOutputFile {
            path: String::from("/state/jobs/s/b-p3-0011223344556677.log"),
            write_failed_after: Some(4096),
        },
    );
    assert_eq!(
        output,
        "tail\n[output file incomplete: write failed after 4096 bytes]\n\
         full output: /state/jobs/s/b-p3-0011223344556677.log"
    );
    let mut output = String::new();
    push_output_file(
        &mut output,
        &plakous_tools::JobOutputFile {
            path: String::from("/p.log"),
            write_failed_after: None,
        },
    );
    assert_eq!(output, "full output: /p.log");
}

#[cfg(unix)]
#[tokio::test]
async fn leftover_jobs_raise_leader_status_and_handle_in_every_shell_vocabulary()
-> Result<(), Box<dyn Error>> {
    let command = "sleep 30 & exit 7";
    for (kind, tool, arguments) in [
        (
            ProcessToolProjectionKind::PiBash,
            "bash",
            json!({"command": command}),
        ),
        (
            ProcessToolProjectionKind::CodexUnifiedExec,
            "exec_command",
            json!({"cmd": command, "login": false}),
        ),
        (
            ProcessToolProjectionKind::GrokTerminal,
            "run_terminal_command",
            json!({"command": command, "description": "start service"}),
        ),
        (
            ProcessToolProjectionKind::ClaudeOAuthBash,
            "Bash",
            json!({"command": command}),
        ),
    ] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000606",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?);
        let sessions = environment.joined_process_sessions()?;
        let registry = process_session_registry()?;
        let map = unified_exec_projection(registry.clone(), kind)?;
        let outcome =
            execute_projected_process_tool(&registry, &map, environment, tool, arguments).await?;
        let value: Value = serde_json::from_str(&outcome.joined_text())?;
        assert_eq!(value["exit_code"], 7, "{kind:?}: {value}");
        assert!(
            value["output"]
                .as_str()
                .ok_or("output")?
                .contains("1 process still running as job")
        );
        assert!(value["session_id"].as_u64().is_some() || value["task_id"].as_str().is_some());
        assert_eq!(sessions.running_job_count(), 1);
        sessions.stop_jobs(
            plakous_tools::JobStopReason::Model,
            std::time::Duration::from_millis(200),
        )?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unified_exec_non_tty_has_null_stdin_and_pty_accepts_input() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
            "00000000-0000-0000-0000-000000000603",
        )?),
        workspace.path(),
        ".",
        plakous_tools::ExecutionEnvironmentConfig::default(),
    )?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let tools = ProcessSessionTools::posix_sh()?;
    for command in ["cat", "test -c /dev/stdin"] {
        let result = tools
            .exec_command_with_update(
                &environment,
                &json!({"cmd": command, "login": false, "yield_time_ms": 1000}).to_string(),
                control.clone(),
            )
            .await?;
        assert!(result.joined, "non-tty {command} must finish on EOF");
        assert_eq!(result.projection.exit_code, Some(0));
    }
    let result = tools.exec_command_with_update(
        &environment,
        r#"{"cmd":"read line; printf 'received:%s' \"$line\"","tty":true,"login":false,"yield_time_ms":0}"#,
        control.clone(),
    ).await?;
    let handle = result.projection.session_id.ok_or("missing PTY session")?;
    let result = ProcessSessionTools::write_stdin_with_update(
        &environment,
        &json!({"session_id": handle, "chars": "hello\n", "yield_time_ms": 1000}).to_string(),
        &control,
    )
    .await?;
    assert!(result.projection.output.contains("received:hello"));
    assert!(result.joined);
    Ok(())
}

#[test]
fn process_await_wait_does_not_reserve_a_blocking_worker() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
            "00000000-0000-0000-0000-000000000599",
        )?),
        workspace.path(),
        ".",
        plakous_tools::ExecutionEnvironmentConfig::default(),
    )?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()?;
    let started = runtime.block_on(ProcessSessionTools::posix_sh()?.start_canonical(
        &environment,
        CanonicalProcessStartRequest {
            command: "read line".into(),
            workdir: PathBuf::from("."),
            login: false,
            tty: false,
            keep_stdin_open: true,
            _model_description: None,
            yield_time_ms: 0,
            max_output_bytes: 64,
        },
        ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
    ))?;
    let handle = started.projection.session_id.ok_or("promoted job handle")?;
    let (preempt, receiver) = tokio::sync::watch::channel(plakous_tools::ToolPreempt::None);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let result = runtime.block_on(async {
        let (entered, receiver_entered) = tokio::sync::oneshot::channel();
        let waiting_environment = Arc::clone(&environment);
        let waiting = tokio::spawn(async move {
            let _ = entered.send(());
            execute_process_await(
                waiting_environment,
                json!({"session_ids":[handle],"yield_time_ms":5000,"max_output_bytes":64,"max_output_records":256}),
                ExecutionOperationControl::new(plakous_model::CancellationToken::new()).with_preempt(receiver),
                ProcessOperationKind::Await,
                ProcessHintSource::Default,
            ).await
        });
        receiver_entered.await?;
        let (worker_entered, worker_receiver) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _ = worker_entered.send(());
            let _ = release_rx.recv();
        });
        let available = tokio::time::timeout(std::time::Duration::from_secs(2), worker_receiver).await;
        // Once notification waiting ends, the tail read legitimately needs
        // the blocking pool. Release it before awaiting the final result.
        preempt.send(plakous_tools::ToolPreempt::Steer)?;
        release_tx.send(())?;
        worker.await?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), waiting).await??;
        assert!(available.is_ok(), "process await reserved the only blocking worker while waiting");
        Ok::<_, Box<dyn Error>>(result)
    });
    environment.joined_process_sessions()?.stop_jobs(
        plakous_tools::JobStopReason::SessionExit,
        std::time::Duration::from_millis(200),
    )?;
    let outcome = result??;
    let value: Value = serde_json::from_str(&outcome.joined_text())?;
    assert_eq!(value["tasks"][0]["status"], "running");
    assert_eq!(value["tasks"][0]["task_id"], format!("p{handle}"));
    Ok(())
}
#[test]
fn process_executor_revision_rotates_the_exact_tuple_and_rejects_v2() -> Result<(), Box<dyn Error>>
{
    const PREDECESSOR_CAPABILITY_REVISION: &str = "2";
    const PREDECESSOR_START_ID: &str = "plakous.process-session-start.v2";
    const PREDECESSOR_WRITE_ID: &str = "plakous.process-session-write.v2";
    const PREDECESSOR_POLL_ID: &str = "plakous.process-session-poll.v2";
    const PREDECESSOR_TERMINATE_ID: &str = "plakous.process-session-terminate.v2";
    const PREDECESSOR_EXECUTOR_ID: &str = "plakous.process-session-executor.v2";

    assert_ne!(
        PROCESS_SESSION_CAPABILITY_REVISION,
        PREDECESSOR_CAPABILITY_REVISION
    );
    assert_ne!(PROCESS_SESSION_REGISTERED_TOOL_ID, PREDECESSOR_START_ID);
    assert_ne!(
        PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
        PREDECESSOR_WRITE_ID
    );
    assert_ne!(PROCESS_SESSION_POLL_REGISTERED_TOOL_ID, PREDECESSOR_POLL_ID);
    assert_ne!(
        PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
        PREDECESSOR_TERMINATE_ID
    );
    assert_ne!(PROCESS_SESSION_EXECUTOR_ID, PREDECESSOR_EXECUTOR_ID);

    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    let definition = ToolDefinition::new(
        ToolName::new(PROCESS_SESSION_CAPABILITY_NAME.to_owned())?,
        PREDECESSOR_CAPABILITY_REVISION,
        "Start one canonical joined process session.",
        ToolInputContract::Structured(canonical_process_input_schema(
            CanonicalProcessOperationV2::Start,
        )?),
        ToolOutputContract::Utf8(Utf8TextContractV1::new(1_048_576)?),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let executor: Arc<dyn ToolExecutor> = process;
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
    builder.register(ToolRegistration::new(
        RegisteredToolId::new(PREDECESSOR_START_ID)?,
        ToolExecutorId::new(PREDECESSOR_EXECUTOR_ID)?,
        ToolExecutorBindingIdentity::new(1, &[b"predecessor-process-session-tools"])?,
        definition,
        executor,
    ))?;
    let effective_configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"predecessor-process-configuration"],
    )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.route",
        "test.profile.r1",
        PROCESS_PROJECTION_FAMILY,
        "test-model",
        "test-model",
        effective_configuration,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let error = ProcessToolProjectionKind::CodexUnifiedExec
        .projection_map(
            plakous_tools::JobConfig::default(),
            builder.freeze(),
            target,
        )
        .err()
        .ok_or("predecessor process tuple unexpectedly enrolled")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn await_chatty_terminal_deduplicates_and_reports_consumed_jobs() -> Result<(), Box<dyn Error>>
{
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        JoinedProcessInputDisposition,
    };
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000599")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let started = ProcessSessionTools::posix_sh()?
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: "cat".into(),
                workdir: ".".into(),
                login: false,
                tty: false,
                keep_stdin_open: true,
                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 64,
            },
            control.clone(),
        )
        .await?;
    let handle = started.projection.session_id.ok_or("missing session")?;
    let sessions = environment.joined_process_sessions()?;
    let id = sessions
        .session_for_number(u32::try_from(handle)?)
        .ok_or("missing id")?;
    // Acknowledge each byte before the next write so each is a distinct record,
    // independent of pipe buffering or scheduler timing.
    let mut cursor = JoinedProcessOutputCursor::new(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    for _ in 0..16_385 {
        sessions.write_stdin(id, b"x", JoinedProcessInputDisposition::KeepOpen)?;
        loop {
            let poll = plakous_tools::internal::process_session_tail(&sessions, id, 1, 1)?;
            if poll.next_cursor() != cursor {
                cursor = poll.next_cursor();
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cat did not acknowledge output"
            );
            std::thread::yield_now();
        }
    }
    sessions.write_stdin(id, b"", JoinedProcessInputDisposition::Close)?;
    while sessions.joined_terminal_status(id).is_none() {
        assert!(std::time::Instant::now() < deadline, "cat did not exit");
        std::thread::yield_now();
    }
    let request = CanonicalProcessAwaitRequest {
        session_ids: vec![handle, handle],
        wake: None,
        yield_time_ms: 0,
        max_output_bytes: 64,
        max_output_records: 256,
    };
    let outcome = ProcessSessionTools::await_canonical(&environment, &request, &control).await?;
    let value: Value = serde_json::from_str(&outcome.joined_text())?;
    assert_eq!(value["tasks"][0]["status"], "exited");
    assert_eq!(value["tasks"].as_array().ok_or("tasks")?.len(), 1);
    assert_eq!(value["tasks"][0]["exit_code"], 0);
    plakous_tools::internal::consume_jobs(&sessions, &[u32::try_from(handle)?]);
    let outcome = ProcessSessionTools::await_canonical(&environment, &request, &control).await?;
    let value: Value = serde_json::from_str(&outcome.joined_text())?;
    assert_eq!(value["tasks"][0]["status"], "already_reported");
    let stopped = ProcessSessionTools::terminate_canonical(
        &environment,
        &CanonicalProcessTerminateRequest { session_id: handle },
    )
    .await?;
    assert_eq!(stopped.status, "exited");
    Ok(())
}

#[test]
fn await_schemas_require_exactly_one_nonempty_id_form() -> Result<(), Box<dyn Error>> {
    for ids in [
        json!({}),
        json!({"session_ids":[]}),
        json!({"session_id":1,"session_ids":[1]}),
    ] {
        let mut args = ids;
        if args.get("session_ids") != Some(&json!([])) {
            let error = validate_canonical_poll_shape(&args).expect_err("exclusive id form");
            assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
        }
        args["yield_time_ms"] = json!(0);
        args["max_output_bytes"] = json!(64);
        args["max_output_records"] = json!(256);
        let single = serde_json::from_value::<CanonicalProcessPollRequest>(args.clone())
            .is_ok_and(|request| request.validate().is_ok());
        let multiple = serde_json::from_value::<CanonicalProcessAwaitRequest>(args.clone())
            .is_ok_and(|request| request.validate().is_ok());
        assert!(!single && !multiple, "{args}");
    }
    let grok = ProcessToolProjectionKind::GrokTerminal.tools(plakous_tools::JobConfig::default())?;
    for args in [
        json!({}),
        json!({"task_ids":[]}),
        json!({"task_ids":vec!["p1";129]}),
    ] {
        assert!(
            grok[1].schema().parse_instance(&args.to_string()).is_err(),
            "{args}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle test keeps start, close, poll, terminate, and stale-handle assertions causal"
)]
async fn canonical_process_operations_close_poll_terminate_and_reject_stale_authority()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000564")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("printf ready; read ignored; printf eof"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: true,

                _model_description: None,
                yield_time_ms: 1_000,
                max_output_bytes: 4_096,
            },
            control.clone(),
        )
        .await?;
    let handle = started
        .projection
        .session_id
        .ok_or("expected live handle")?;
    assert_eq!(started.projection.output, "ready");
    assert!(
        ProcessSessionTools::poll_canonical(
            &environment,
            &CanonicalProcessPollRequest {
                session_id: handle,
                cursor: Some(0),
                read_so_far: false,
                wait_for_exit: false,
                yield_time_ms: 0,
                max_output_bytes: 64,
                max_output_records: 4,
            },
            &control,
        )
        .await
        .is_err()
    );
    ProcessSessionTools::write_canonical(
        &environment,
        &CanonicalProcessWriteRequest {
            session_id: handle,
            input: String::new(),
            disposition: CanonicalProcessInputDisposition::Close,
            yield_time_ms: None,
            max_output_bytes: None,
        },
        &control,
    )
    .await?;
    let session = ProcessSessionTools::session(&environment, handle)?;
    let mut changes = session.sessions.job_terminal_changes();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while session
            .sessions
            .joined_terminal_status(session.id)
            .is_none()
        {
            changes.changed().await?;
        }
        Ok::<_, tokio::sync::watch::error::RecvError>(())
    })
    .await??;
    let completed = ProcessSessionTools::poll_canonical(
        &environment,
        &CanonicalProcessPollRequest {
            session_id: handle,
            cursor: Some(started.cursor),
            read_so_far: false,
            wait_for_exit: false,
            yield_time_ms: 5_000,
            max_output_bytes: 4_096,
            max_output_records: 256,
        },
        &control,
    )
    .await?;
    assert_eq!(completed.projection.output, "eof");
    assert!(completed.joined);
    let retained = ProcessSessionTools::terminate_canonical(
        &environment,
        &CanonicalProcessTerminateRequest { session_id: handle },
    )
    .await?;
    assert_eq!(
        retained.projection.exit_code,
        completed.projection.exit_code
    );
    plakous_tools::internal::consume_jobs(
        &environment.joined_process_sessions()?,
        &[u32::try_from(handle)?],
    );
    assert_eq!(
        ProcessSessionTools::terminate_canonical(
            &environment,
            &CanonicalProcessTerminateRequest { session_id: handle },
        )
        .await?
        .projection
        .exit_code,
        completed.projection.exit_code
    );

    let running = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("sleep 30"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 64,
            },
            control.clone(),
        )
        .await?;
    let running_handle = running
        .projection
        .session_id
        .ok_or("expected live handle")?;
    assert!(
        ProcessSessionTools::terminate_canonical(
            &environment,
            &CanonicalProcessTerminateRequest {
                session_id: running_handle
            },
        )
        .await?
        .joined
    );
    assert!(
        ProcessSessionTools::write_canonical(
            &environment,
            &CanonicalProcessWriteRequest {
                session_id: running_handle,
                input: String::from("x"),
                disposition: CanonicalProcessInputDisposition::KeepOpen,
                yield_time_ms: None,
                max_output_bytes: None,
            },
            &control,
        )
        .await
        .is_err()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_process_tool_does_not_repeat_the_runtime_cleanup_attempt()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000565")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("sleep 30"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 0,
                max_output_bytes: 64,
            },
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        )
        .await?;
    assert!(started.projection.session_id.is_some());
    assert!(!sessions.is_drained());

    drop(tools);
    assert!(
        !sessions.is_drained(),
        "executor Drop must not start a second cleanup attempt"
    );

    sessions.join_all()?;
    assert!(sessions.is_drained());
    Ok(())
}

#[test]
fn canonical_process_registration_rejects_shell_identity_drift() -> Result<(), ToolRuntimeError> {
    let drifted = ["/usr/bin/sh", "/bin/bash"]
        .into_iter()
        .find(|path| Path::new(path).is_file() && *path != "/bin/sh")
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "non-canonical shell executable is unavailable",
            )
        })?;
    let shell = JoinedProcessShellProfile::new(drifted, [String::from("-c")])
        .map_err(backend_runtime_error)?;
    let login = JoinedProcessShellProfile::new(drifted, [String::from("-l"), String::from("-c")])
        .map_err(backend_runtime_error)?;
    let error = Arc::new(ProcessSessionTools::new(shell, login))
        .registrations()
        .err()
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "drifted shell unexpectedly registered",
            )
        })?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "full exact provider schema goldens are intentionally reviewed in one causal table"
)]
fn exact_process_projection_schemas_are_v2_bound_goldens() -> Result<(), ToolRuntimeError> {
    let codex =
        ProcessToolProjectionKind::CodexUnifiedExec.tools(plakous_tools::JobConfig::default())?;
    assert_eq!(codex.len(), 2);
    assert_eq!(codex[0].name().as_str(), "exec_command");
    assert_eq!(codex[1].name().as_str(), "write_stdin");
    assert_eq!(
        codex[0].description(),
        text::bound_description(
            PLAKOUS_CODEX_EXEC_COMMAND_DESCRIPTION,
            plakous_tools::JobConfig::default(),
            ProcessToolProjectionKind::CodexUnifiedExec,
            true,
        )
    );
    assert!(
        codex[0]
            .description()
            .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
    );
    assert_eq!(
        codex[1].description(),
        text::bound_description(
            PLAKOUS_TOOL_WRITE_STDIN_DESCRIPTION,
            plakous_tools::JobConfig::default(),
            ProcessToolProjectionKind::CodexUnifiedExec,
            false,
        )
    );
    assert!(
        codex[1]
            .description()
            .contains("Background jobs survive the turn")
    );
    assert_eq!(
        codex[0].schema().document().value().as_ref(),
        &codex_exec_command_schema(plakous_tools::JobConfig::default())
    );
    assert_eq!(
        codex[1].schema().document().value().as_ref(),
        &write_stdin_schema()
    );
    let exact_output = object_schema(
        json!({
            "exit_code": {"type":"integer"},
            "session_id": {"type":"integer","minimum":1},
            "status": {"type":"string","enum":["leftover","timed_out","terminated","cancelled","output_limit","failed","outcome_unknown","cleanup_unresolved"]},
            "output": {"type":"string"}
        }),
        &["output"],
    );
    assert_eq!(
        codex[0].output_schema().document().value().as_ref(),
        &exact_output
    );
    assert_eq!(
        codex[1].output_schema().document().value().as_ref(),
        &exact_output
    );
    assert_eq!(codex[0].schema().document().value()["required"], json!([]));
    assert_eq!(
        codex[0].schema().document().value()["properties"]["yield_time_ms"]["description"],
        format!(
            "Foreground observation hint: defaults to {} ms; clamped to 600000 ms; zero observes for 250 ms first.",
            plakous_tools::JobConfig::default().foreground_seconds() * 1000
        )
    );
    assert_eq!(
        codex[0].schema().document().value()["properties"]["yield_time_ms"]["type"],
        "number"
    );
    assert!(
        codex[0].schema().document().value()["properties"]
            .get("sandbox_permissions")
            .is_some()
    );
    assert_eq!(
        codex[1].schema().document().value()["required"],
        json!(["session_id", "chars"])
    );
    codex[1]
        .schema()
        .parse_instance(r#"{"session_id":1,"chars":"hello\n"}"#)
        .map_err(projection_arguments)?;
    assert!(
        serde_json::from_str::<CodexWriteArgs>(r#"{"session_id":1,"chars":"hello\n"}"#).is_ok()
    );
    assert_eq!(codex[0].output_schema(), codex[1].output_schema());
    assert_eq!(
        codex[0].output_schema().document().value()["required"],
        json!(["output"])
    );
    assert!(
        codex[0]
            .schema()
            .parse_instance(r#"{"cmd":"true"}"#)
            .is_ok()
    );
    assert!(
        codex[0]
            .schema()
            .parse_instance(r#"{"session_id":1}"#)
            .is_ok()
    );
    assert!(
        codex[0]
            .schema()
            .parse_instance(r#"{"cmd":"","session_id":1}"#)
            .is_ok()
    );
    assert!(codex[0].schema().parse_instance(r"{}").is_ok());
    assert!(
        codex[0]
            .schema()
            .parse_instance(
                r#"{"cmd":"true","sandbox_permissions":"require_escalated","justification":"ok","prefix_rule":["git","pull"]}"#
            )
            .is_ok()
    );
    for invalid in [
        r#"{"cmd":"true","shell":"/bin/bash"}"#,
        r#"{"cmd":"true","unknown":1}"#,
    ] {
        assert!(
            codex[0].schema().parse_instance(invalid).is_err(),
            "{invalid}"
        );
    }

    let grok = ProcessToolProjectionKind::GrokTerminal.tools(plakous_tools::JobConfig::default())?;
    assert_eq!(
        grok.iter()
            .map(|tool| tool.name().as_str())
            .collect::<Vec<_>>(),
        [
            "run_terminal_command",
            "get_command_or_subagent_output",
            "kill_command_or_subagent"
        ]
    );
    assert_eq!(grok[0].name().as_str(), "run_terminal_command");
    assert_eq!(
        grok[0].description(),
        text::bound_description(
            PLAKOUS_GROK_RUN_TERMINAL_COMMAND_DESCRIPTION,
            plakous_tools::JobConfig::default(),
            ProcessToolProjectionKind::GrokTerminal,
            true,
        )
    );
    assert!(
        grok[0]
            .description()
            .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
    );
    assert!(
        grok[0].schema().document().value()["properties"]
            .get("background")
            .is_none()
    );
    assert!(
        grok[0].output_schema().document().value()["properties"]
            .get("task_id")
            .is_some()
    );
    assert!(
        grok[0].output_schema().document().value()["properties"]
            .get("session_id")
            .is_none()
    );
    assert_eq!(
        grok[0].schema().document().value()["required"],
        json!(["command", "description"])
    );
    assert!(
        grok[0]
            .schema()
            .parse_instance(r#"{"command":"true","description":"verify","block_until_ms":0}"#)
            .is_ok()
    );
    assert!(
        grok[1]
            .schema()
            .parse_instance(r#"{"task_ids":["p1","2"],"timeout_ms":3600000}"#)
            .is_ok()
    );
    assert!(
        grok[2]
            .schema()
            .parse_instance(r#"{"task_id":"p1"}"#)
            .is_ok()
    );
    assert!(
        grok[0]
            .schema()
            .parse_instance(r#"{"command":"true","description":"verify"}"#)
            .is_ok()
    );
    for invalid in [
        r#"{"command":"true"}"#,
        r#"{"command":"true","description":"verify","timeout":1.5}"#,
        r#"{"command":"true","description":"verify","task_id":1}"#,
        r#"{"command":"true","description":"verify","background":false}"#,
        r#"{"command":"true","description":"verify","background":true}"#,
    ] {
        assert!(
            grok[0].schema().parse_instance(invalid).is_err(),
            "{invalid}"
        );
    }

    let claude =
        ProcessToolProjectionKind::ClaudeOAuthBash.tools(plakous_tools::JobConfig::default())?;
    assert_eq!(claude[0].name().as_str(), "Bash");
    assert_eq!(
        claude[0].description(),
        text::bound_description(
            CLAUDE_OAUTH_BASH_DESCRIPTION,
            plakous_tools::JobConfig::default(),
            ProcessToolProjectionKind::ClaudeOAuthBash,
            true,
        )
    );
    assert!(
        claude[0]
            .description()
            .contains(PLAKOUS_PROCESS_JOB_LIFETIME_CONTRACT)
    );
    assert!(
        claude[0]
            .schema()
            .parse_instance(r#"{"command":"true"}"#)
            .is_ok()
    );
    assert!(
        claude[0]
            .schema()
            .parse_instance(r#"{"command":"true","run_in_background":true}"#)
            .is_ok()
    );
    let names = claude
        .iter()
        .map(|tool| tool.name().as_str())
        .collect::<Vec<_>>();
    // No output tool; background output is
    // reported on completion and a leaf waits through a longer timeout.
    assert_eq!(names, ["Bash", "TaskStop"]);
    let bash = claude[0].description();
    assert!(bash.contains("`timeout` is how long to block for the result"));
    assert!(bash.contains("Wait by giving a long command a timeout long enough to finish"));
    assert!(bash.contains("search with grep, rg or find"));
    assert!(!bash.contains("BashOutput"));
    assert!(!bash.contains("observation hint"));
    assert!(
        claude[1]
            .schema()
            .parse_instance(r#"{"task_id":"3"}"#)
            .is_ok()
    );
    Ok(())
}

#[test]
fn background_preempts_starts_not_existing_job_observations() {
    assert!(ObservationWait::PromotionHint.preempted(plakous_tools::ToolPreempt::Background));
    assert!(!ObservationWait::Hint.preempted(plakous_tools::ToolPreempt::Background));
    assert!(!ObservationWait::Output.preempted(plakous_tools::ToolPreempt::Background));
    assert!(ObservationWait::Hint.preempted(plakous_tools::ToolPreempt::Steer));
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_hint_drains_more_than_one_record_batch_without_waiting()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000601")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/sh", ["-c", "cat"].map(String::from))?,
        ".",
        [],
        true,
        JoinedProcessTerminalMode::Pipes,
    )?;
    let setup = sessions.clone();
    let start_control = control.clone();
    let id =
        tokio::task::spawn_blocking(move || -> Result<_, plakous_tools::ExecutionBackendError> {
            let id = setup.start(request, start_control)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut cursor = JoinedProcessOutputCursor::new(0);
            // An acknowledgement per write guarantees 400 separate unread records.
            for _ in 0..400 {
                setup.write_stdin(id, b"line\n", JoinedProcessInputDisposition::KeepOpen)?;
                loop {
                    let poll = plakous_tools::internal::process_session_tail(&setup, id, 5, 1)?;
                    if poll.next_cursor() != cursor {
                        cursor = poll.next_cursor();
                        break;
                    }
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
            }
            setup.write_stdin(id, b"", JoinedProcessInputDisposition::Close)?;
            while setup.joined_terminal_status(id).is_none() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(
                setup
                    .poll_from(id, JoinedProcessOutputCursor::new(0), 4096, 256)?
                    .more_output()
            );
            Ok(id)
        })
        .await??;
    let before = std::time::Instant::now();
    let result = ProcessSessionTools::observe_bounded_with_update(
        &environment,
        u64::from(JoinedProcessSessions::job_number(id)),
        3000,
        4096,
        256,
        JoinedProcessOutputCursor::new(0),
        true,
        ObservationWait::Hint,
        &control,
    )
    .await?;
    assert!(
        before.elapsed() < std::time::Duration::from_secs(1),
        "pending terminal records waited for the hint deadline"
    );
    assert_eq!(result.projection.output, "line\n".repeat(400));
    assert!(result.joined);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn contended_observation_does_not_block_the_async_worker() -> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000602")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/sh", ["-c", "trap 'echo stopping > stopping; while :; do :; done' TERM; echo ready > ready; while :; do :; done"].map(String::from))?,
        ".", [], false, JoinedProcessTerminalMode::Pipes,
    )?;
    let setup = sessions.clone();
    let start_control = control.clone();
    let id = process_backend(move || setup.start(request, start_control)).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !workspace.path().join("ready").exists() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    plakous_tools::internal::promote(&sessions, id)?;
    let stopper = sessions.clone();
    let stop = tokio::task::spawn_blocking(move || {
        stopper.stop_with_grace(
            id,
            plakous_tools::JobStopReason::Model,
            std::time::Duration::from_millis(600),
        )
    });
    while !workspace.path().join("stopping").exists() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let (entered, receiver) = tokio::sync::oneshot::channel();
    let observation = tokio::spawn(async move {
        let _ = entered.send(());
        ProcessSessionTools::observe_bounded_with_update(
            &environment,
            u64::from(JoinedProcessSessions::job_number(id)),
            3000,
            4096,
            256,
            JoinedProcessOutputCursor::new(0),
            true,
            ObservationWait::Hint,
            &control,
        )
        .await
    });
    receiver.await?;
    let before = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let elapsed = before.elapsed();
    stop.await??;
    let observed = observation.await?;
    assert!(
        elapsed < std::time::Duration::from_millis(300),
        "a contended process poll blocked the only async worker: {elapsed:?}"
    );
    observed?;
    Ok(())
}

#[test]
fn explicit_start_hints_are_not_rejected_before_clamping() -> Result<(), ToolRuntimeError> {
    for (kind, arguments) in [
        (
            ProcessToolProjectionKind::CodexUnifiedExec,
            r#"{"cmd":"true","yield_time_ms":900000}"#,
        ),
        (
            ProcessToolProjectionKind::PiBash,
            r#"{"command":"true","timeout":900}"#,
        ),
        (
            ProcessToolProjectionKind::ClaudeOAuthBash,
            r#"{"command":"true","timeout":900000}"#,
        ),
        (
            ProcessToolProjectionKind::GrokTerminal,
            r#"{"command":"true","description":"verify","block_until_ms":900000}"#,
        ),
    ] {
        assert!(
            kind.tools(plakous_tools::JobConfig::default())?[0]
                .schema()
                .parse_instance(arguments)
                .is_ok(),
            "{kind:?} must accept hints above the observation ceiling"
        );
    }
    Ok(())
}

#[test]
fn process_projection_result_omits_inapplicable_session_fields() -> Result<(), serde_json::Error> {
    assert_eq!(
        serde_json::to_value(ProcessProjectionResult {
            exit_code: Some(0),
            session_id: None,
            output: "done".to_owned(),
        })?,
        json!({"exit_code":0,"output":"done"})
    );
    assert_eq!(
        serde_json::to_value(ProcessProjectionResult {
            exit_code: None,
            session_id: Some(7),
            output: String::new(),
        })?,
        json!({"session_id":7,"output":""})
    );
    Ok(())
}

#[test]
fn process_utf8_decoder_retains_split_scalars_and_replaces_only_invalid_bytes() {
    let mut pending = vec![b'a', 0xe2, 0x82];
    assert_eq!(drain_process_utf8_bounded(&mut pending, false, 8), "a");
    assert_eq!(pending, [0xe2, 0x82]);
    pending.extend_from_slice(&[0xac, 0xff, b'b']);
    assert_eq!(drain_process_utf8_bounded(&mut pending, false, 8), "€�b");
    assert_eq!(pending, [] as [u8; 0]);

    pending.extend_from_slice(&[0xf0, 0x9f]);
    assert_eq!(drain_process_utf8_bounded(&mut pending, true, 8), "�");
    assert_eq!(pending, [] as [u8; 0]);
}

#[test]
fn process_utf8_decoder_makes_exact_progress_for_one_to_four_byte_scalars() {
    for (scalar, width) in [("a", 1_usize), ("¢", 2), ("€", 3), ("😀", 4)] {
        for bound in 1..width {
            let mut pending = scalar.as_bytes().to_vec();
            assert_eq!(drain_process_utf8_bounded(&mut pending, true, bound), "");
            assert_eq!(pending, scalar.as_bytes());
        }
        let mut pending = scalar.as_bytes().to_vec();
        assert_eq!(
            drain_process_utf8_bounded(&mut pending, true, width),
            scalar
        );
        assert_eq!(pending, [] as [u8; 0]);
    }
}

#[test]
fn process_utf8_decoder_retains_invalid_terminal_tails_until_replacement_fits() {
    for invalid in [vec![0xff], vec![0xf0, 0x9f], vec![0xe2, 0x82]] {
        for bound in 1..char::REPLACEMENT_CHARACTER.len_utf8() {
            let mut pending = invalid.clone();
            assert_eq!(drain_process_utf8_bounded(&mut pending, true, bound), "");
            assert_eq!(pending, invalid);
        }
        let mut pending = invalid;
        assert_eq!(
            drain_process_utf8_bounded(&mut pending, true, char::REPLACEMENT_CHARACTER.len_utf8()),
            "�"
        );
        assert_eq!(pending, [] as [u8; 0]);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_utf8_larger_than_observation_bound_returns_without_deadline_spin()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000568")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let started = std::time::Instant::now();
    // A one-byte page cannot hold a three-byte character: the exited
    // process is joined at once and the page counts the bytes it could not
    // show instead of waiting out the yield.
    let first = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("printf '\\342\\202\\254'"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 5_000,
                max_output_bytes: 1,
            },
            control,
        )
        .await?;
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(first.joined);
    assert_eq!(
        first.projection.output,
        output_omitted_marker(3).trim_start()
    );
    assert!(environment.joined_process_sessions()?.is_drained());
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_publishes_its_job_only_in_the_poll_that_completes_it() -> Result<(), Box<dyn Error>>
{
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };
    use std::future::Future as _;

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000572")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let mut start = Box::pin(tools.start_canonical(
        &environment,
        CanonicalProcessStartRequest {
            command: String::from("exec sleep 30"),
            workdir: PathBuf::from("."),
            login: false,
            tty: false,
            keep_stdin_open: false,

            _model_description: None,
            yield_time_ms: 250,
            max_output_bytes: 4_096,
        },
        control,
    ));
    // Cancellation drops a suspended start, and its guard then terminates
    // the process. A job published while the start is suspended would be
    // terminated after its ownership moved to the job table. The start is
    // re-polled only when woken, so the check sees any publication that
    // happened off this task before the start could observe it.
    let completed = std::future::poll_fn(|cx| {
        if sessions.has_jobs() {
            return std::task::Poll::Ready(None);
        }
        start.as_mut().poll(cx).map(Some)
    })
    .await;
    drop(start);
    let running = sessions.running_job_count();
    let started = completed.ok_or("a job was published while its start was suspended")??;
    let handle = started.projection.session_id.ok_or("expected a job")?;
    assert!(started.first_promotion);
    assert_eq!(running, 1);
    ProcessSessionTools::terminate_canonical(
        &environment,
        &CanonicalProcessTerminateRequest { session_id: handle },
    )
    .await?;
    Ok(())
}

/// Drives a start by hand into its reservation, which the test holds
/// there through the observer's room check (run on the blocking pool
/// under the reservation), then either drops the start (a cancelled
/// start) or releases the reservation and polls the start to completion.
/// Neither path depends on when the blocking pool runs.
#[cfg(unix)]
#[allow(
    clippy::too_many_lines,
    reason = "one hand-driven start keeps its setup, reservation and drop or re-poll assertions causal"
)]
fn start_to_its_reservation(repoll: bool) -> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JobOutputRoot,
    };
    use std::future::Future as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::{Duration, Instant, SystemTime};

    #[derive(Default)]
    struct Progress {
        woken: bool,
        reserving: bool,
        released: bool,
    }
    #[derive(Default)]
    struct Signals(Mutex<Progress>, Condvar);
    impl Signals {
        fn progress(&self) -> MutexGuard<'_, Progress> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
        fn update(&self, change: impl FnOnce(&mut Progress)) {
            change(&mut self.progress());
            self.1.notify_all();
        }
    }
    impl Wake for Signals {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.update(|progress| progress.woken = true);
        }
    }
    const SESSION: &str = "00000000-0000-0000-0000-0000000000d0";
    const BRANCH: &str = "00000000-0000-0000-0000-0000000000d1";

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let _entered = runtime.enter();
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = JobOutputRoot::open(&state.path().join("jobs"))?;
    let session_dir = state.path().join("jobs").join(SESSION);
    std::fs::create_dir(&session_dir)?;
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))?;
    let reported = session_dir.join(format!("{BRANCH}-p77-0123456789abcdef.log"));
    std::fs::write(&reported, b"reported long ago")?;
    std::fs::File::open(&reported)?
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 3600))?;
    std::fs::set_permissions(&reported, std::fs::Permissions::from_mode(0o400))?;

    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000d02")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    sessions.bind_job_outputs(
        root,
        plakous_model::SessionId::from_str(SESSION)?,
        plakous_model::BranchId::from_str(BRANCH)?,
    );
    let signals = Arc::new(Signals::default());
    let hold = Arc::clone(&signals);
    // The first room check is the reservation's, before its sweep; it
    // waits until the test releases it. Later checks (the inline
    // promotion's) pass at once.
    sessions.observe_jobs(
        |_| true,
        move || {
            let mut progress = hold.progress();
            if !progress.reserving {
                progress.reserving = true;
                hold.1.notify_all();
                while !progress.released {
                    progress = hold
                        .1
                        .wait(progress)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            true
        },
    );
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let mut start = Box::pin(tools.start_canonical(
        &environment,
        CanonicalProcessStartRequest {
            command: String::from("echo $$ > pid; exec sleep 30"),
            workdir: PathBuf::from("."),
            login: false,
            tty: false,
            keep_stdin_open: false,

            _model_description: None,
            yield_time_ms: 250,
            max_output_bytes: 4_096,
        },
        control,
    ));
    let waker = Waker::from(Arc::clone(&signals));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(20);
    let wait_for = |done: &dyn Fn(&Progress) -> bool| -> Result<(), Box<dyn Error>> {
        let mut progress = signals.progress();
        while !done(&progress) {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or("the start stalled")?;
            progress = signals
                .1
                .wait_timeout(progress, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    };
    // Until the reservation holds, the start cannot complete: a promotable
    // start always reserves first.
    loop {
        signals.progress().woken = false;
        if let Poll::Ready(result) = start.as_mut().poll(&mut cx) {
            return Err(format!(
                "start completed without a reservation: ok={}",
                result.is_ok()
            )
            .into());
        }
        wait_for(&|progress| progress.woken || progress.reserving)?;
        if signals.progress().reserving {
            break;
        }
    }
    let pid = std::fs::read_to_string(workspace.path().join("pid"))?
        .trim()
        .to_owned();
    if repoll {
        signals.update(|progress| progress.released = true);
        // Publication happens only in the poll that completes the start.
        let started = loop {
            assert!(
                !sessions.has_jobs(),
                "a job was published while its start was suspended"
            );
            signals.progress().woken = false;
            if let Poll::Ready(result) = start.as_mut().poll(&mut cx) {
                break result?;
            }
            wait_for(&|progress| progress.woken)?;
        };
        assert!(started.first_promotion);
        assert_eq!(sessions.running_job_count(), 1);
        assert!(!reported.exists(), "the reservation did not sweep");
        let number = u32::try_from(started.projection.session_id.ok_or("expected a job")?)?;
        assert!(
            sessions.job_output_file(number).is_some(),
            "the published job has no file although its reservation fit"
        );
        drop(start);
        runtime.block_on(ProcessSessionTools::terminate_canonical(
            &environment,
            &CanonicalProcessTerminateRequest {
                session_id: u64::from(number),
            },
        ))?;
        return Ok(());
    }
    // Cancellation drops the start while its reservation is held; the
    // reservation then finishes with no one to publish its job.
    drop(start);
    signals.update(|progress| progress.released = true);
    while std::process::Command::new("kill")
        .args(["-0", pid.as_str()])
        .stderr(std::process::Stdio::null())
        .status()?
        .success()
    {
        assert!(
            Instant::now() < deadline,
            "a start dropped in its reservation left its process {pid} running"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    while reported.exists() {
        assert!(
            Instant::now() < deadline,
            "the released reservation never swept"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!sessions.has_jobs() && sessions.jobs().is_empty());
    let leftovers = std::fs::read_dir(&session_dir)?.count();
    assert_eq!(leftovers, 0, "a dropped reservation left a job file behind");
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_start_dropped_after_its_reservation_publishes_nothing_and_terminates()
-> Result<(), Box<dyn Error>> {
    start_to_its_reservation(false)
}

#[cfg(unix)]
#[test]
fn a_start_repolled_after_its_reservation_publishes_inline_with_its_file()
-> Result<(), Box<dyn Error>> {
    start_to_its_reservation(true)
}

#[cfg(unix)]
#[tokio::test]
async fn a_polled_session_is_summarised_to_its_newest_output() -> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000571")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    // About 1.3 MB of output: more than one page, more than the session
    // window. One call shows the start and the end and counts the rest.
    let result = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("seq 1 200000; echo 'FAILED: tail_only_marker'; exit 101"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 20_000,
                max_output_bytes: 4_000,
            },
            control,
        )
        .await?;
    let output = &result.projection.output;
    assert!(result.joined);
    assert_eq!(result.projection.exit_code, Some(101));
    assert!(output.len() < 4_100, "{}", output.len());
    assert!(output.contains(" bytes of output omitted ...]"));
    assert!(
        output.ends_with("200000\nFAILED: tail_only_marker\n"),
        "{output}"
    );
    assert!(environment.joined_process_sessions()?.is_drained());
    Ok(())
}

/// A finished session keeps its handle and stops counting as running.
///
/// These are the two meanings of "active" that the retained set carries,
/// and they sit next to each other here. Reading one as the other is what
/// exhausted the concurrency limit on a sequential program, and later what
/// fenced a turn at cleanup, so the distinction is pinned rather than left
/// to the reader.
#[cfg(unix)]
#[tokio::test]
async fn a_finished_session_keeps_its_handle_and_frees_its_slot() -> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000571")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let tools = ProcessSessionTools::posix_sh()?;
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("sleep 0.3; printf done"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 1,
                max_output_bytes: 4096,
            },
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        )
        .await?;
    assert!(started.projection.session_id.is_some());

    let sessions = environment.joined_process_sessions()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sessions
        .session_for_number(u32::try_from(
            started.projection.session_id.ok_or("handle")?,
        )?)
        .and_then(|id| sessions.joined_terminal_status(id))
        .is_none()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the child never reached a joined terminal status"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Retained: the handle is how the final output is still readable.
    let retained = sessions.jobs();
    assert_eq!(
        retained.len(),
        1,
        "a finished session keeps its handle so its output stays pollable"
    );
    // Not running: the slot it held is free again.
    assert_eq!(
        retained
            .iter()
            .filter(|job| job.status == plakous_tools::JobStatus::Running)
            .count(),
        0,
        "a child that has exited holds no concurrency"
    );

    sessions.join_all()?;
    plakous_tools::internal::consume_jobs(
        &sessions,
        &retained.iter().map(|job| job.number).collect::<Vec<_>>(),
    );
    assert_eq!(sessions.jobs(), [] as [plakous_tools::JobView; 0]);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn forced_backend_retirement_prunes_stale_projection_handles() -> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000569")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let tools = ProcessSessionTools::posix_sh()?;
    let started = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("sleep 30"),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 1,
                max_output_bytes: 1,
            },
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        )
        .await?;
    assert!(started.projection.session_id.is_some());
    let sessions = environment.joined_process_sessions()?;
    sessions.join_all()?;
    plakous_tools::internal::consume_jobs(&sessions, &[1]);
    assert_eq!(sessions.jobs(), [] as [plakous_tools::JobView; 0]);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn codex_output_does_not_turn_a_short_command_into_a_live_session()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000567")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    let result = tools.exec_command_with_update(
        &environment,
        r#"{"cmd":"printf first; sleep 0.05; printf second","login":false,"yield_time_ms":5000}"#,
        ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
    ).await?;
    assert_eq!(result.projection.output, "firstsecond");
    assert_eq!(result.projection.exit_code, Some(0));
    assert_eq!(result.projection.session_id, None);
    assert!(result.joined);
    assert!(sessions.is_drained());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn exited_output_over_one_observation_remains_correlatable_until_joined()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    const OUTPUT_BYTES: usize = PROCESS_OBSERVATION_BYTES + 1_914;
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000568")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let first = tools.exec_command_with_update(
        &environment,
        &format!(
            r#"{{"cmd":"head -c {OUTPUT_BYTES} /dev/zero | tr '\\0' x","login":false,"yield_time_ms":5000}}"#
        ),
        control.clone(),
    ).await?;
    assert_eq!(first.projection.output.len(), PROCESS_OBSERVATION_BYTES);
    assert_eq!(first.projection.exit_code, None);
    assert_eq!(first.status, "running");
    assert!(!first.joined);
    let handle = first
        .projection
        .session_id
        .ok_or("bounded observation lost its continuation handle")?;
    let first_outcome = first.into_outcome()?;
    assert!(matches!(
        process_session_update(&first_outcome),
        Some(ProcessSessionUpdate::Promoted { .. })
    ));

    let mut final_observation = ProcessSessionTools::write_stdin_with_update(
        &environment,
        &format!(r#"{{"session_id":{handle},"chars":""}}"#),
        &control,
    )
    .await?;
    let mut final_bytes = final_observation.projection.output.len();
    for _ in 0..8 {
        if final_observation.joined {
            break;
        }
        final_observation = ProcessSessionTools::observe_with_update(
            &environment,
            handle,
            5_000,
            None,
            JoinedProcessOutputCursor::new(final_observation.cursor),
            true,
            &control,
        )
        .await?;
        final_bytes = final_bytes.saturating_add(final_observation.projection.output.len());
    }
    assert_eq!(final_bytes, OUTPUT_BYTES - PROCESS_OBSERVATION_BYTES);
    assert_eq!(final_observation.projection.session_id, None);
    assert_eq!(final_observation.status, "exited");
    assert!(final_observation.joined);
    let final_outcome = final_observation.into_outcome()?;
    assert!(matches!(
        process_session_update(&final_outcome),
        Some(ProcessSessionUpdate::JobTerminal { .. })
    ));
    assert!(sessions.is_drained());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn join_before_return_keeps_the_head_and_the_tail_of_a_long_log() -> Result<(), Box<dyn Error>>
{
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };
    const PAD: &str = "........................................";

    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000569")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let sessions = environment.joined_process_sessions()?;
    let tools = ProcessSessionTools::posix_sh()?;
    // About 200 KB of log: the command at the top, the failure at the end.
    let result = tools
        .start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: format!(
                    "echo 'running build'; i=0; while [ $i -lt 4000 ]; do \
                 echo \"compiling unit $i {PAD}\"; i=$((i+1)); done; \
                 echo 'error: test tail_only_failure failed'; exit 3"
                ),
                workdir: PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,

                _model_description: None,
                yield_time_ms: 20_000,
                max_output_bytes: PROCESS_OBSERVATION_BYTES,
            },
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        )
        .await?;
    let output = &result.projection.output;
    assert!(output.starts_with("running build\n"), "{output:.200}");
    assert!(output.ends_with("error: test tail_only_failure failed\n"));
    let marker_start = output.find("[... ").ok_or("omitted marker")?;
    let omitted: u64 = output[marker_start + 5..]
        .split(' ')
        .next()
        .ok_or("omitted count")?
        .parse()?;
    let marker = output_omitted_marker(omitted);
    let kept = output.len() - marker.trim_start_matches('\n').len();
    assert!(kept <= PROCESS_OBSERVATION_BYTES, "{kept}");
    // Every produced byte is either shown or counted as omitted.
    let produced = "running build\n".len()
        + (0..4_000)
            .map(|i| format!("compiling unit {i} {PAD}\n").len())
            .sum::<usize>()
        + "error: test tail_only_failure failed\n".len();
    assert_eq!(kept as u64 + omitted, produced as u64);
    assert_eq!(result.projection.exit_code, Some(3));
    assert_eq!(result.projection.session_id, None);
    assert!(result.joined);
    assert!(sessions.is_drained());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn codex_observations_accumulate_one_bound_and_resume_exactly() -> Result<(), Box<dyn Error>>
{
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    const TOTAL_BYTES: usize = 2 * PROCESS_OBSERVATION_BYTES + 17;
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000566")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let tools = ProcessSessionTools::posix_sh()?;
    let control = ExecutionOperationControl::new(plakous_model::CancellationToken::new());
    let first = tools.exec_command_with_update(
        &environment,
        &format!(
            r#"{{"cmd":"stty -echo; head -c {TOTAL_BYTES} /dev/zero | tr '\\0' x; read ignored","login":false,"tty":true}}"#
        ),
        control.clone(),
    ).await?;
    assert_eq!(first.projection.output.len(), PROCESS_OBSERVATION_BYTES);
    assert_eq!(first.cursor, PROCESS_OBSERVATION_BYTES as u64);
    let handle = first
        .projection
        .session_id
        .ok_or("expected retained output")?;

    // Empty writes observe only currently buffered bytes. Wait for the producer
    // through a non-consuming backend cursor before asserting exact page sizes.
    let session = ProcessSessionTools::session(&environment, handle)?;
    let mut buffered_cursor = JoinedProcessOutputCursor::new(first.cursor);
    let buffering_started = std::time::Instant::now();
    while buffered_cursor.get() < TOTAL_BYTES as u64 {
        buffered_cursor = session
            .sessions
            .poll_from(session.id, buffered_cursor, TOTAL_BYTES, 256)?
            .next_cursor();
        assert!(
            buffering_started.elapsed() < std::time::Duration::from_secs(5),
            "producer output did not reach the retained buffer: {} / {TOTAL_BYTES}",
            buffered_cursor.get(),
        );
        std::thread::yield_now();
    }
    assert_eq!(buffered_cursor.get(), TOTAL_BYTES as u64);
    assert_eq!(lock_process_cursor(&session.cursor).get(), first.cursor);

    let second = ProcessSessionTools::write_stdin_with_update(
        &environment,
        &format!(r#"{{"session_id":{handle},"chars":""}}"#),
        &control,
    )
    .await?;
    assert_eq!(second.projection.output.len(), PROCESS_OBSERVATION_BYTES);
    assert_eq!(second.cursor, (2 * PROCESS_OBSERVATION_BYTES) as u64);
    assert_eq!(second.projection.session_id, Some(handle));

    let final_observation = ProcessSessionTools::write_stdin_with_update(
        &environment,
        &format!(r#"{{"session_id":{handle},"chars":""}}"#),
        &control,
    )
    .await?;
    assert_eq!(final_observation.projection.output, "x".repeat(17));
    assert_eq!(final_observation.cursor, TOTAL_BYTES as u64);
    assert!(!final_observation.joined);
    assert_eq!(final_observation.projection.session_id, Some(handle));
    let mut joined = ProcessSessionTools::write_stdin_with_update(
        &environment,
        &format!(r#"{{"session_id":{handle},"chars":"\n"}}"#),
        &control,
    )
    .await?;
    for _ in 0..8 {
        if joined.joined {
            break;
        }
        joined = ProcessSessionTools::observe_with_update(
            &environment,
            handle,
            0,
            None,
            JoinedProcessOutputCursor::new(joined.cursor),
            true,
            &control,
        )
        .await?;
        assert_eq!(joined.projection.output, "");
        assert_eq!(joined.cursor, TOTAL_BYTES as u64);
    }
    assert!(joined.joined);
    assert_eq!(joined.cursor, TOTAL_BYTES as u64);
    assert_eq!(joined.projection.output, "");
    assert_eq!(joined.projection.session_id, None);
    Ok(())
}

async fn execute_process_session_tool(
    registry: &FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    tool: &str,
    arguments: &str,
) -> Result<ToolOutcome, Box<dyn Error>> {
    let ids = (
        RunId::from_str("00000000-0000-0000-0000-000000000580")?,
        TurnId::from_str("00000000-0000-0000-0000-000000000581")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000582")?,
        CallId::from_str("00000000-0000-0000-0000-000000000583")?,
    );
    let invocation = registry.prepare(ToolInvocationRequest::new(
        ids.0,
        ids.1,
        ids.2,
        ids.3,
        ToolName::new(tool)?,
        ToolCorrelation::default(),
        arguments,
    ))?;
    execute_process_invocation(registry, environment, invocation).await
}

async fn execute_process_invocation(
    registry: &FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    mut invocation: ToolInvocation,
) -> Result<ToolOutcome, Box<dyn Error>> {
    use plakous_model::{Clock, FixedClock, Timestamp};

    let ids = (
        invocation.run_id(),
        invocation.turn_id(),
        invocation.attempt_id(),
        invocation.call_id(),
    );
    if registry
        .definition(invocation.identity().name())
        .ok_or("process tool definition missing")?
        .effect()
        == ToolEffect::Mutating
    {
        let intent = Digest::sha256(DigestDomain::Snapshot, 1, &[b"process-session-test-intent"])?;
        let claim = Digest::sha256(DigestDomain::Snapshot, 1, &[b"process-session-test-claim"])?;
        invocation = bind_effect_dispatch(invocation, intent, claim);
    }
    let revision = ToolPolicyRevision::new("process-session-executor-test", 1)?;
    let binding = authorization_binding(&invocation, environment.descriptor().clone(), revision)?;
    let grant = authorization_grant(binding, EffectAuthorizationGrantSource::Policy);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(80, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, ids.3, Arc::clone(&clock))?;
    let context = tool_context(
        ids.0,
        ids.1,
        ids.2,
        plakous_model::CancellationToken::new(),
        None,
        clock,
        environment,
        grant,
        progress,
    );
    let executor = executor(registry, &invocation)?;
    Ok(executor.execute(invocation, context).await?)
}

fn unified_exec_projection(
    registry: FrozenToolRegistry,
    kind: ProcessToolProjectionKind,
) -> Result<ToolProjectionMap, Box<dyn Error>> {
    let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"stdin-projection-test"],
    )?)?)?;
    Ok(kind.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            PROCESS_PROJECTION_FAMILY,
            "test-model",
            "test-model",
            configuration,
            PROCESS_TOOL_PROJECTION_REVISION,
        )?,
    )?)
}

async fn execute_projected_process_tool(
    registry: &FrozenToolRegistry,
    map: &ToolProjectionMap,
    environment: Arc<ExecutionEnvironment>,
    tool: &str,
    arguments: Value,
) -> Result<ToolOutcome, Box<dyn Error>> {
    let prepared = map.prepare_in_environment(
        ToolInvocationRequest::new(
            RunId::from_str("00000000-0000-0000-0000-000000000580")?,
            TurnId::from_str("00000000-0000-0000-0000-000000000581")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000582")?,
            CallId::from_str("00000000-0000-0000-0000-000000000583")?,
            ToolName::new(tool)?,
            ToolCorrelation::default(),
            arguments.to_string(),
        ),
        &environment,
    )?;
    let (invocation, receipt) = prepared.into_parts();
    let outcome = execute_process_invocation(registry, environment, invocation).await?;
    Ok(map.raise(
        &receipt.ok_or("missing process projection receipt")?,
        outcome,
    )?)
}

/// On a node without agent tools Grok's output and kill tools refuse a
/// task id that is not a process id before any effect, without claiming
/// to know what it names.
#[test]
fn grok_process_tools_refuse_non_process_task_ids_as_policy() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
            "00000000-0000-0000-0000-000000000621",
        )?),
        workspace.path(),
        ".",
        plakous_tools::ExecutionEnvironmentConfig::default(),
    )?;
    let map = unified_exec_projection(
        process_session_registry()?,
        ProcessToolProjectionKind::GrokTerminal,
    )?;
    for (tool, arguments) in [
        (
            "get_command_or_subagent_output",
            json!({"task_ids": ["p1", "c3"]}),
        ),
        ("kill_command_or_subagent", json!({"task_id": "c3"})),
    ] {
        let Err(error) = map.prepare_in_environment(
            ToolInvocationRequest::new(
                RunId::from_str("00000000-0000-0000-0000-000000000622")?,
                TurnId::from_str("00000000-0000-0000-0000-000000000623")?,
                AttemptId::from_str("00000000-0000-0000-0000-000000000624")?,
                CallId::from_str("00000000-0000-0000-0000-000000000625")?,
                ToolName::new(tool)?,
                ToolCorrelation::default(),
                arguments.to_string(),
            ),
            &environment,
        ) else {
            return Err(format!("{tool} admitted an agent id").into());
        };
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Policy, "{tool}");
        assert!(error.proven_no_effect(), "{tool}");
        assert_eq!(
            error.message().as_str(),
            "task id `c3` is not a process id; this agent has no agent tools"
        );
    }
    Ok(())
}

/// A stop issued by another layer through [`ProcessSessionTools::stop_job`]
/// shows the model the same bytes the projected stop tool raises.
#[test]
fn stop_job_raises_the_projected_stop_bytes() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
            "00000000-0000-0000-0000-000000000611",
        )?),
        workspace.path(),
        ".",
        plakous_tools::ExecutionEnvironmentConfig::default(),
    )?;
    let registry = process_session_registry()?;
    let map = unified_exec_projection(registry, ProcessToolProjectionKind::GrokTerminal)?;
    let prepared = map.prepare_in_environment(
        ToolInvocationRequest::new(
            RunId::from_str("00000000-0000-0000-0000-000000000612")?,
            TurnId::from_str("00000000-0000-0000-0000-000000000613")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000614")?,
            CallId::from_str("00000000-0000-0000-0000-000000000615")?,
            ToolName::new("kill_command_or_subagent")?,
            ToolCorrelation::default(),
            json!({"task_id": "p1"}).to_string(),
        ),
        &environment,
    )?;
    let (_, receipt) = prepared.into_parts();
    let receipt = receipt.ok_or("missing stop receipt")?;
    for canonical in [
        json!({"status":"terminated","exit_code":null,"session_id":null,"cursor":4,"output":""}),
        json!({"status":"exited","exit_code":0,"session_id":null,"cursor":0,"output":""}),
        json!({"status":"exited","exit_code":3,"session_id":null,"cursor":9,"output":""}),
        json!({"status":"outcome_unknown","exit_code":null,"session_id":null,"cursor":0,"output":""}),
    ] {
        let projected = map.raise(&receipt, ToolOutcome::success_text(canonical.to_string())?)?;
        assert_eq!(
            raised_stop_result(&canonical.to_string())?,
            projected.joined_text(),
            "{canonical}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn production_unified_exec_lowering_gives_non_tty_null_stdin() -> Result<(), Box<dyn Error>> {
    for kind in [ProcessToolProjectionKind::CodexUnifiedExec] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000604",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let map = unified_exec_projection(registry.clone(), kind)?;
        for tty in [None, Some(false)] {
            let mut arguments = json!({"cmd":"cat", "login":false, "yield_time_ms":1000});
            if let Some(tty) = tty {
                arguments["tty"] = json!(tty);
            }
            let outcome = execute_projected_process_tool(
                &registry,
                &map,
                environment.clone(),
                "exec_command",
                arguments,
            )
            .await?;
            assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
            let result: Value = serde_json::from_str(&outcome.joined_text())?;
            assert_eq!(
                result["exit_code"], 0,
                "non-tty cat must finish on EOF: {kind:?}"
            );
            assert!(result["session_id"].is_null());
            assert_eq!(result["output"], "");
        }
    }
    Ok(())
}

/// A process page cut at its output bound, of bytes the presentation
/// escapes, raises on every process presentation inside the per-result
/// model-visible bound.
#[cfg(unix)]
#[tokio::test]
async fn a_maximum_escaped_process_page_raises_on_every_process_presentation()
-> Result<(), Box<dyn Error>> {
    let command = "yes '\"\\' | head -c 200000";
    for (kind, tool, arguments) in [
        (
            ProcessToolProjectionKind::PiBash,
            "bash",
            json!({"command": command}),
        ),
        (
            ProcessToolProjectionKind::CodexUnifiedExec,
            "exec_command",
            json!({"cmd": command, "login": false, "yield_time_ms": 10_000}),
        ),
        (
            ProcessToolProjectionKind::GrokTerminal,
            "run_terminal_command",
            json!({"command": command, "description": "print escaped bytes"}),
        ),
        (
            ProcessToolProjectionKind::ClaudeOAuthBash,
            "Bash",
            json!({"command": command}),
        ),
    ] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000606",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let map = unified_exec_projection(registry.clone(), kind)?;
        let outcome = execute_projected_process_tool(&registry, &map, environment, tool, arguments)
            .await
            .map_err(|error| format!("{kind:?}: {error:?}"))?;
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success, "{kind:?}");
        let raised = outcome.joined_text();
        assert!(
            raised.len() <= SemanticJsonV1::maximum_string_bytes() - 4_096,
            "{kind:?}: {} bytes",
            raised.len()
        );
        let value: Value = serde_json::from_str(&raised)?;
        assert!(value.is_object(), "{kind:?}");
        assert!(raised.contains("\\\"\\\\"), "{kind:?}: output kept");
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn production_unified_exec_closed_stdin_refusal_explains_tty_retry()
-> Result<(), Box<dyn Error>> {
    for kind in [ProcessToolProjectionKind::CodexUnifiedExec] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            plakous_tools::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000605",
            )?),
            workspace.path(),
            ".",
            plakous_tools::ExecutionEnvironmentConfig::default(),
        )?);
        let registry = process_session_registry()?;
        let map = unified_exec_projection(registry.clone(), kind)?;
        let started = execute_projected_process_tool(
            &registry,
            &map,
            environment.clone(),
            "exec_command",
            json!({"cmd":"exec sleep 60", "login":false, "tty":false, "yield_time_ms":0}),
        )
        .await?;
        assert_eq!(started.kind(), ToolOutcomeKind::Success);
        let handle = serde_json::from_str::<Value>(&started.joined_text())?["session_id"]
            .as_u64()
            .ok_or("missing live process handle")?;
        let canonical = execute_process_session_tool(
            &registry,
            environment.clone(),
            PROCESS_SESSION_WRITE_NAME,
            &json!({"session_id":handle, "input":"hello", "disposition":"keep_open"}).to_string(),
        )
        .await?;
        assert_eq!(canonical.kind(), ToolOutcomeKind::Failure);
        assert_eq!(canonical.joined_text(), "joined process stdin is closed");
        for input in ["hello", "\u{3}not-an-interrupt"] {
            let refused = execute_projected_process_tool(
                &registry,
                &map,
                environment.clone(),
                "write_stdin",
                json!({"session_id":handle, "chars":input}),
            )
            .await?;
            assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
            assert_eq!(
                refused.joined_text(),
                "stdin is closed for this session; rerun exec_command with tty=true to keep stdin open"
            );
        }
        let stopped = execute_projected_process_tool(
            &registry,
            &map,
            environment.clone(),
            "write_stdin",
            json!({"session_id":handle, "chars":"\u{3}"}),
        )
        .await?;
        assert_eq!(stopped.kind(), ToolOutcomeKind::Success);
        let polled = execute_projected_process_tool(
            &registry,
            &map,
            environment,
            "write_stdin",
            json!({"session_id":handle, "chars":"", "yield_time_ms":0}),
        )
        .await?;
        assert_eq!(polled.kind(), ToolOutcomeKind::Success);
    }
    Ok(())
}

/// Writing to a session that has already exited and been joined is a
/// definite, model-visible tool failure.
///
/// A live Codex turn sent Ctrl-C to a session whose child had exited. The
/// generic conflict that came back could not be reconciled against the
/// journal, so the turn ended in `OutcomeUnknown` and the agent went into
/// recovery-required. The write reached nothing, so the outcome is
/// knowable and the turn must be able to continue.
#[cfg(unix)]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the exited-session lifecycle is one test: start, write, exit, refusal, continue"
)]
async fn write_to_an_exited_session_is_a_definite_failure_not_an_unknown_outcome()
-> Result<(), Box<dyn Error>> {
    use plakous_model::ToolResultPart;
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000590")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = process_session_registry()?;
    let started = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_CAPABILITY_NAME,
        &serde_json::json!({
            "command": "read line; exit 7",
            "workdir": ".",
            "login": false,
            "tty": false,
            "keep_stdin_open": true,
            "yield_time_ms": 0,
            "max_output_bytes": 4096
        })
        .to_string(),
    )
    .await?;
    assert_eq!(started.kind(), ToolOutcomeKind::Success);
    let [ToolResultPart::Text(text)] = started.parts() else {
        return Err("process start did not return one text part".into());
    };
    let handle = serde_json::from_str::<Value>(text.text())?["session_id"]
        .as_u64()
        .ok_or("process start retained no session")?;

    let write = |input: &str| {
        serde_json::json!({
            "session_id": handle,
            "input": input,
            "disposition": "keep_open"
        })
        .to_string()
    };

    // A write to a still-running session is unchanged: it succeeds, and
    // here it is what makes the child exit.
    let delivered = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_WRITE_NAME,
        &write("\n"),
    )
    .await?;
    assert_eq!(delivered.kind(), ToolOutcomeKind::Success);

    let sessions = environment.joined_process_sessions()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let terminal = sessions
            .session_for_number(u32::try_from(handle)?)
            .and_then(|id| sessions.joined_terminal_status(id));
        if terminal == Some(JoinedProcessStatus::Exited(Some(7))) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child did not reach a joined terminal status"
        );
        tokio::task::yield_now().await;
    }

    // The session is terminal and joined but not retired, which is exactly
    // the live state. The write is refused definitely, and the executor
    // returns an outcome instead of an error the driver would normalize to
    // `OutcomeUnknown`.
    let refused = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_WRITE_NAME,
        &write("x"),
    )
    .await?;
    assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
    let [ToolResultPart::Text(refusal)] = refused.parts() else {
        return Err("refused write did not return one text part".into());
    };
    assert_eq!(
        refusal.text(),
        format!("process session {handle} already exited with status 7; nothing was written")
    );

    // The turn continues: the session is still pollable, and the poll
    // reports the exit the write was refused against.
    let polled = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_POLL_NAME,
        &serde_json::json!({
            "session_id": handle,
            "yield_time_ms": 0,
            "max_output_bytes": 4096,
            "max_output_records": 16
        })
        .to_string(),
    )
    .await?;
    assert_eq!(polled.kind(), ToolOutcomeKind::Success);
    let [ToolResultPart::Text(polled_text)] = polled.parts() else {
        return Err("poll did not return one text part".into());
    };
    assert_eq!(
        serde_json::from_str::<Value>(polled_text.text())?["exit_code"],
        serde_json::json!(7)
    );

    // The promoted snapshot stays readable, but writes still fail definitely.
    let stale = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_WRITE_NAME,
        &write("x"),
    )
    .await?;
    assert_eq!(stale.kind(), ToolOutcomeKind::Failure);
    assert!(sessions.is_drained());
    Ok(())
}

/// A refusal the process executor raises on its way in reaches the model as
/// an ordinary tool failure, whatever the condition was.
///
/// A live Code Mode cell ran one `cat` at a time and had its 67th start
/// refused by the session limit. The refusal arrived as an error rather
/// than an outcome, so the turn was escalated to `OutcomeUnknown` and the
/// model's own JavaScript could not catch it. Nothing was spawned, so
/// there is nothing to reconcile.
#[cfg(unix)]
#[tokio::test]
async fn a_refusal_raised_before_any_spawn_reaches_the_model_as_a_tool_failure()
-> Result<(), Box<dyn Error>> {
    use plakous_model::ToolResultPart;
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        JoinedProcessCommand, MAX_JOINED_PROCESS_SESSIONS,
    };

    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000591")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let sessions = environment.joined_process_sessions()?;
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or("test shell is unavailable")?;
    for _ in 0..MAX_JOINED_PROCESS_SESSIONS {
        sessions.start(
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
            ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
        )?;
    }

    let registry = process_session_registry()?;
    let refused = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_CAPABILITY_NAME,
        &serde_json::json!({
            "command": "true",
            "workdir": ".",
            "login": false,
            "tty": false,
            "keep_stdin_open": false,
            "yield_time_ms": 0,
            "max_output_bytes": 4096
        })
        .to_string(),
    )
    .await?;
    assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
    let [ToolResultPart::Text(text)] = refused.parts() else {
        return Err("the refusal did not return one text part".into());
    };
    assert!(
        text.text()
            .starts_with("running process limit (64) was reached:")
    );
    assert!(text.text().contains("p64"));
    sessions.join_all()?;
    Ok(())
}

/// A sequential program runs as many commands as it likes through the real
/// executor, however many of them leave a page unread.
///
/// This is the live Code Mode shape end to end: each command fills its
/// output page, so the tool layer returns a retained `session_id` without
/// joining, and the finished session keeps its handle. The backend stopped
/// counting those against the running limit; the handle table has to agree,
/// or the 65th command is spawned and killed instead of run.
#[cfg(unix)]
#[tokio::test]
async fn sequential_page_filled_starts_never_exhaust_the_executor_handle_table()
-> Result<(), Box<dyn Error>> {
    use plakous_model::ToolResultPart;
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
        MAX_JOINED_PROCESS_SESSIONS,
    };

    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000592")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = process_session_registry()?;
    // A stdin handshake keeps each start running through its first observation.
    let arguments = serde_json::json!({
        "command": "printf '%0.sx' $(seq 1 256); read line",
        "workdir": ".",
        "login": false,
        "tty": false,
        "keep_stdin_open": true,
        "yield_time_ms": 20,
        "max_output_bytes": 16
    })
    .to_string();

    let mut retained = 0_usize;
    for index in 0..(MAX_JOINED_PROCESS_SESSIONS + 8) {
        let outcome = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            &arguments,
        )
        .await?;
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "start {index} was refused"
        );
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err("process start did not return one text part".into());
        };
        let handle = serde_json::from_str::<Value>(text.text())?["session_id"]
            .as_u64()
            .ok_or("start did not promote its running child")?;
        retained = retained.saturating_add(1);
        let sessions = environment.joined_process_sessions()?;
        let id = sessions
            .session_for_number(u32::try_from(handle)?)
            .ok_or("job handle")?;
        sessions.write_stdin(id, b"finish\n", JoinedProcessInputDisposition::KeepOpen)?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while sessions.joined_terminal_status(id).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
    }
    assert!(
        retained > MAX_JOINED_PROCESS_SESSIONS,
        "the shape under test needs more retained sessions than the limit, got {retained}"
    );
    environment.joined_process_sessions()?.join_all()?;
    Ok(())
}

/// This pack alone: the four canonical process operations, which is all
/// these executor tests dispatch.
fn process_session_registry() -> Result<FrozenToolRegistry, Box<dyn Error>> {
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
    register_packs(
        &[Arc::new(ProcessToolPack::new(
            Arc::new(ProcessSessionTools::posix_sh()?),
            plakous_tools::JobConfig::default(),
        )) as Arc<dyn ToolPack>],
        &mut builder,
    )?;
    Ok(builder.freeze())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn process_session_missing_cwd_is_correctable_before_pipes_or_pty_spawn()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    for tty in [false, true] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000589",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let sessions = environment.joined_process_sessions()?;
        let registry = process_session_registry()?;
        let arguments = |workdir| {
            serde_json::json!({
                "command": "printf executed > process-marker",
                "workdir": workdir,
                "login": false,
                "tty": tty,
                "keep_stdin_open": false,
                "yield_time_ms": 1000,
                "max_output_bytes": 4096,
            })
            .to_string()
        };
        let refused = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            &arguments("missing/workdir"),
        )
        .await?;
        assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
        assert!(sessions.is_drained());
        assert!(!workspace.path().join("process-marker").exists());
        let corrected = execute_process_session_tool(
            &registry,
            Arc::clone(&environment),
            PROCESS_SESSION_CAPABILITY_NAME,
            &arguments("."),
        )
        .await?;
        assert_eq!(corrected.kind(), ToolOutcomeKind::Success);
        assert!(sessions.is_drained());
        assert_eq!(
            std::fs::read(workspace.path().join("process-marker"))?,
            b"executed"
        );
    }
    Ok(())
}

#[tokio::test]
async fn process_session_executor_rejects_unknown_write_handle_as_model_failure()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000584")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = process_session_registry()?;
    let outcome = execute_process_session_tool(
        &registry,
        environment,
        PROCESS_SESSION_WRITE_NAME,
        r#"{"session_id":424242,"input":"x","disposition":"keep_open"}"#,
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(
        outcome.joined_text().contains("unknown or stale"),
        "unexpected failure text: {}",
        outcome.joined_text()
    );
    Ok(())
}

#[tokio::test]
async fn process_session_executor_rejects_malformed_start_arguments_as_model_failure()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000585")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = process_session_registry()?;
    let outcome = execute_process_session_tool(
        &registry,
        environment,
        PROCESS_SESSION_CAPABILITY_NAME,
        r#"{"command":"","workdir":".","login":false,"tty":false,"keep_stdin_open":false,"yield_time_ms":1000,"max_output_bytes":4096}"#,
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(
        outcome
            .joined_text()
            .contains("canonical process start bounds are invalid"),
        "unexpected failure text: {}",
        outcome.joined_text()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_hint_promotes_instead_of_killing() -> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000586")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = process_session_registry()?;
    let outcome = execute_process_session_tool(&registry, Arc::clone(&environment),
        PROCESS_SESSION_CAPABILITY_NAME,
        r#"{"command":"sleep 60","workdir":".","login":false,"tty":false,"keep_stdin_open":false,"yield_time_ms":250,"max_output_bytes":4096}"#,
    ).await?;
    let result: Value = serde_json::from_str(&outcome.joined_text())?;
    environment.joined_process_sessions()?.join_all()?;
    assert_eq!(result["status"], "running");
    assert_eq!(result["session_id"], 1);
    assert_eq!(
        plakous_tools::internal::process_telemetry(&outcome),
        Some(ProcessTelemetry::Promoted {
            operation: ProcessOperationKind::Start,
            hint_source: ProcessHintSource::Default,
            hint_ms: 250,
            preempt: plakous_tools::ToolPreempt::None
        })
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn process_session_executor_rejects_stale_poll_cursor_as_model_failure()
-> Result<(), Box<dyn Error>> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000586")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let registry = process_session_registry()?;
    let started = execute_process_session_tool(
        &registry,
        Arc::clone(&environment),
        PROCESS_SESSION_CAPABILITY_NAME,
        r#"{"command":"printf ready; read ignored; printf eof","workdir":".","login":false,"tty":false,"keep_stdin_open":true,"yield_time_ms":1000,"max_output_bytes":4096}"#,
    )
    .await?;
    assert_eq!(started.kind(), ToolOutcomeKind::Success);
    let started_json: Value = serde_json::from_str(&started.joined_text())?;
    let handle = started_json["session_id"]
        .as_u64()
        .ok_or("expected live process handle")?;
    let outcome = execute_process_session_tool(
        &registry,
        environment,
        PROCESS_SESSION_POLL_NAME,
        &format!(
            r#"{{"session_id":{handle},"cursor":0,"yield_time_ms":0,"max_output_bytes":64,"max_output_records":4}}"#
        ),
    )
    .await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(
        outcome.joined_text().contains("stale or foreign"),
        "unexpected failure text: {}",
        outcome.joined_text()
    );
    Ok(())
}
