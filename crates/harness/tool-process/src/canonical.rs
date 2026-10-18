//! The four canonical process operations and their wire requests.

use super::{
    Arc, BoundProcessSession, CanonicalProcessOperationV2, Deserialize, ExecutionOperationControl,
    HeadTailPage, JoinedProcessCommand, JoinedProcessInputDisposition, JoinedProcessOutputCursor,
    JoinedProcessSessionId, JoinedProcessSessions, JoinedProcessStartRequest,
    JoinedProcessTerminalMode, ObservationWait, PROCESS_OBSERVATION_BYTES, PathBuf,
    ProcessExecutionResult, ProcessHintSource, ProcessOperationKind, ProcessProjectionResult,
    ProcessSessionTools, ProcessTelemetry, Serialize, StartedProcessGuard, ToolOutcome,
    ToolRuntimeError, ToolRuntimeErrorKind, Value, append_output_file, backend_runtime_error,
    deserialize_optional_semantic_u64, deserialize_semantic_u64, deserialize_semantic_usize,
    exited_session_refusal, json, lock_process_cursor, process_status_label, projection_arguments,
    status_exit_code,
};
#[cfg(test)]
use super::{ProcessToolProjectionKind, ProjectedProcessTool};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
// This is the closed wire DTO for four independent canonical process
// capabilities, not one state machine encoded as flags.
#[allow(
    clippy::struct_excessive_bools,
    reason = "closed wire DTO carries independent process capabilities"
)]
pub(crate) struct CanonicalProcessStartRequest {
    pub(crate) command: String,
    pub(crate) workdir: PathBuf,
    pub(crate) login: bool,
    pub(crate) tty: bool,
    pub(crate) keep_stdin_open: bool,
    #[serde(default, rename = "model_description")]
    pub(crate) _model_description: Option<String>,
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    pub(crate) yield_time_ms: u64,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    pub(crate) max_output_bytes: usize,
}

impl CanonicalProcessStartRequest {
    pub(crate) fn validate(&self) -> Result<(), ToolRuntimeError> {
        if self.command.is_empty()
            || self.command.len() > 1_048_576
            || !(1..=PROCESS_OBSERVATION_BYTES).contains(&self.max_output_bytes)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process start bounds are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CanonicalProcessInputDisposition {
    KeepOpen,
    Close,
}

impl From<CanonicalProcessInputDisposition> for JoinedProcessInputDisposition {
    fn from(value: CanonicalProcessInputDisposition) -> Self {
        match value {
            CanonicalProcessInputDisposition::KeepOpen => Self::KeepOpen,
            CanonicalProcessInputDisposition::Close => Self::Close,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalProcessWriteRequest {
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    pub(crate) session_id: u64,
    pub(crate) input: String,
    pub(crate) disposition: CanonicalProcessInputDisposition,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    pub(crate) yield_time_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    pub(crate) max_output_bytes: Option<u64>,
}

impl CanonicalProcessWriteRequest {
    pub(crate) fn validate(&self) -> Result<(), ToolRuntimeError> {
        if self.session_id == 0
            || self.input.len() > 262_144
            || self.yield_time_ms.is_some_and(|yield_time_ms| {
                yield_time_ms
                    > if self.input.is_empty() {
                        300_000
                    } else {
                        30_000
                    }
            })
            || self
                .max_output_bytes
                .is_some_and(|bytes| !(1..=PROCESS_OBSERVATION_BYTES as u64).contains(&bytes))
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process write bounds are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalProcessPollRequest {
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    pub(crate) session_id: u64,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    pub(crate) cursor: Option<u64>,
    #[serde(default)]
    pub(crate) read_so_far: bool,
    #[serde(default)]
    pub(crate) wait_for_exit: bool,
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    pub(crate) yield_time_ms: u64,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    pub(crate) max_output_bytes: usize,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    pub(crate) max_output_records: usize,
}

impl CanonicalProcessPollRequest {
    pub(crate) fn validate(&self) -> Result<(), ToolRuntimeError> {
        if self.session_id == 0
            || self.yield_time_ms
                > if self.wait_for_exit {
                    3_600_000
                } else {
                    300_000
                }
            || !(1..=PROCESS_OBSERVATION_BYTES).contains(&self.max_output_bytes)
            || !(1..=256).contains(&self.max_output_records)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process poll bounds are invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalProcessTerminateRequest {
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    pub(crate) session_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalProcessAwaitRequest {
    #[serde(default, deserialize_with = "deserialize_session_ids")]
    pub(crate) session_ids: Vec<u64>,
    /// `any`: return at the first terminal; without `session_ids`, over every
    /// own job still listed. Absent: await every named process.
    #[serde(default)]
    pub(crate) wake: Option<AwaitWake>,
    #[serde(deserialize_with = "deserialize_semantic_u64")]
    pub(crate) yield_time_ms: u64,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    pub(crate) max_output_bytes: usize,
    #[serde(deserialize_with = "deserialize_semantic_usize")]
    pub(crate) max_output_records: usize,
}

/// When a multi-process await returns.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AwaitWake {
    /// At the first terminal among the awaited processes.
    Any,
}

pub(crate) fn validate_canonical_poll_shape(value: &Value) -> Result<(), ToolRuntimeError> {
    if value.get("wake").is_some() && value.get("session_id").is_none() {
        return Ok(());
    }
    if value.get("session_id").is_some() == value.get("session_ids").is_some() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "canonical process poll requires exactly one of session_id or session_ids",
        ));
    }
    Ok(())
}

fn deserialize_session_ids<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<u64>, D::Error> {
    Vec::<Value>::deserialize(deserializer)?
        .into_iter()
        .map(|value| deserialize_semantic_u64(value).map_err(serde::de::Error::custom))
        .collect()
}

impl CanonicalProcessAwaitRequest {
    pub(crate) fn validate(&self) -> Result<(), ToolRuntimeError> {
        if (self.session_ids.is_empty() && self.wake.is_none())
            || self.session_ids.len() > 128
            || self.session_ids.contains(&0)
            || self.yield_time_ms > 3_600_000
            || !(1..=PROCESS_OBSERVATION_BYTES).contains(&self.max_output_bytes)
            || !(1..=256).contains(&self.max_output_records)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "canonical process await bounds are invalid",
            ));
        }
        Ok(())
    }
}

pub(crate) fn process_observation_labels(
    canonical: CanonicalProcessOperationV2,
    value: &mut Value,
) -> (ProcessOperationKind, ProcessHintSource) {
    let hint_source = match value
        .as_object_mut()
        .and_then(|value| value.remove("hint_source"))
        .as_ref()
        .and_then(Value::as_str)
    {
        Some("model_hint") => ProcessHintSource::ModelHint,
        Some("background") => ProcessHintSource::Background,
        _ => ProcessHintSource::Default,
    };
    let operation = match canonical {
        CanonicalProcessOperationV2::Start => ProcessOperationKind::Start,
        CanonicalProcessOperationV2::Write => ProcessOperationKind::WriteWait,
        CanonicalProcessOperationV2::Poll
            if (value.get("session_ids").is_some() || value.get("wake").is_some())
                && value
                    .get("yield_time_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    != 0 =>
        {
            ProcessOperationKind::Await
        }
        _ => ProcessOperationKind::Poll,
    };
    (operation, hint_source)
}

pub(crate) fn classify_process_outcome(
    outcome: ToolOutcome,
    operation: ProcessOperationKind,
    hint_source: ProcessHintSource,
    yielded: bool,
) -> ToolOutcome {
    match plakous_tools::internal::process_telemetry(&outcome) {
        Some(ProcessTelemetry::Promoted {
            hint_ms, preempt, ..
        }) => plakous_tools::internal::with_process_telemetry(
            outcome,
            ProcessTelemetry::Promoted {
                operation,
                hint_source: if preempt == plakous_tools::ToolPreempt::Background {
                    ProcessHintSource::Background
                } else {
                    hint_source
                },
                hint_ms,
                preempt,
            },
        ),
        Some(ProcessTelemetry::Waited {
            yielded: previous, ..
        }) => plakous_tools::internal::with_process_telemetry(
            outcome,
            ProcessTelemetry::Waited {
                operation,
                yielded: previous || yielded,
            },
        ),
        None => outcome,
    }
}

/// Reserves the job's output file on the blocking pool (the reservation walks
/// the jobs root), then promotes inline. Publication is never followed by an
/// await, so cancellation, which drops a suspended start and terminates its
/// process, can never reach a job whose ownership has moved to the job table.
/// The outer error is only a lost worker; the inner one is the promotion's.
async fn promote_job(
    sessions: &JoinedProcessSessions,
    id: JoinedProcessSessionId,
) -> Result<Result<(), plakous_tools::ExecutionBackendError>, ToolRuntimeError> {
    let reserving = sessions.clone();
    let reservation =
        process_backend(move || Ok(plakous_tools::internal::reserve_job_output(&reserving, id)))
            .await?;
    Ok(plakous_tools::internal::promote_reserved(
        sessions,
        id,
        reservation,
    ))
}

pub(crate) async fn process_backend<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, plakous_tools::ExecutionBackendError> + Send + 'static,
) -> Result<T, ToolRuntimeError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "process backend worker did not complete",
                &error,
            )
        })?
        .map_err(backend_runtime_error)
}

pub(crate) async fn await_process_change(
    changes: &mut tokio::sync::watch::Receiver<u64>,
    preempt: &mut tokio::sync::watch::Receiver<plakous_tools::ToolPreempt>,
    deadline: std::time::Instant,
    control: &ExecutionOperationControl,
) {
    // Some callers deliberately have no preempt sender. A closed watch must
    // not turn a long observation into a ready-future busy loop.
    let preempt_open = preempt.has_changed().is_ok();
    tokio::select! {
        _ = changes.changed() => {}
        _ = preempt.changed(), if preempt_open => {}
        () = control.cancellation().cancelled() => {}
        () = tokio::time::sleep_until(deadline.into()) => {}
    }
}

/// The stop tool's raised result for one canonical terminate result: the
/// same bytes the projection's process raising writes for a stop.
pub(crate) fn raised_stop_result(canonical: &str) -> Result<String, ToolRuntimeError> {
    #[derive(Serialize)]
    struct Stopped<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<&'a str>,
        output: &'a str,
    }
    let value: Value = serde_json::from_str(canonical).map_err(projection_arguments)?;
    let status = value["status"].as_str().unwrap_or_default();
    serde_json::to_string(&Stopped {
        exit_code: value["exit_code"].as_i64(),
        session_id: value["session_id"].as_u64(),
        status: matches!(
            status,
            "timed_out"
                | "terminated"
                | "cancelled"
                | "output_limit"
                | "failed"
                | "outcome_unknown"
                | "cleanup_unresolved"
        )
        .then_some(status),
        output: value["output"].as_str().unwrap_or_default(),
    })
    .map_err(projection_arguments)
}

pub(crate) async fn execute_process_await(
    environment: Arc<plakous_tools::ExecutionEnvironment>,
    value: Value,
    control: ExecutionOperationControl,
    operation: ProcessOperationKind,
    hint_source: ProcessHintSource,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let request = serde_json::from_value(value).map_err(projection_arguments)?;
    let result = ProcessSessionTools::await_canonical(&environment, &request, &control).await;
    match result {
        Err(error) if error.proven_no_effect() => {
            ToolOutcome::failure_text(error.message().as_str())
        }
        result => {
            result.map(|outcome| classify_process_outcome(outcome, operation, hint_source, false))
        }
    }
}

impl ProcessSessionTools {
    #[cfg(test)]
    pub(crate) async fn exec_command_with_update(
        &self,
        environment: &plakous_tools::ExecutionEnvironment,
        raw: &str,
        control: ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let tool = projection_tool(ProcessToolProjectionKind::CodexUnifiedExec, "exec_command")?;
        tool.schema
            .parse_instance(raw)
            .map_err(projection_arguments)?;
        let args: CodexExecArgs = serde_json::from_str(raw).map_err(projection_arguments)?;
        let profile = self.codex_shell(args.shell.as_deref(), args.login.unwrap_or(true))?;
        let terminal_mode = if args.tty.unwrap_or(false) {
            JoinedProcessTerminalMode::UnixPty
        } else {
            JoinedProcessTerminalMode::Pipes
        };
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(profile, args.cmd).map_err(backend_runtime_error)?,
            args.workdir.unwrap_or_else(|| PathBuf::from(".")),
            Vec::new(),
            args.tty.unwrap_or(false),
            terminal_mode,
        )
        .map_err(backend_runtime_error)?;
        let sessions = environment
            .joined_process_sessions()
            .map_err(backend_runtime_error)?;
        let observation_control = control.clone();
        let start_sessions = sessions.clone();
        let max_bytes = args
            .max_output_tokens
            .and_then(non_negative_f64_to_u64)
            .and_then(|tokens| tokens.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(PROCESS_OBSERVATION_BYTES)
            .clamp(1, PROCESS_OBSERVATION_BYTES);
        let id = process_backend(move || {
            plakous_tools::internal::start_process_with_output_limit(
                &start_sessions,
                request,
                control,
                max_bytes,
            )
        })
        .await?;
        let handle = u64::from(JoinedProcessSessions::job_number(id));
        let mut guard = StartedProcessGuard::new(sessions.clone(), id);
        let result = async {
            let yield_time_ms = args
                .yield_time_ms
                .and_then(non_negative_f64_to_u64)
                .unwrap_or(10_000);
            let max_output_tokens = args.max_output_tokens.and_then(non_negative_f64_to_u64);
            let mut result = Self::observe_with_update(
                environment,
                handle,
                yield_time_ms,
                max_output_tokens,
                JoinedProcessOutputCursor::new(0),
                true,
                &observation_control,
            )
            .await;
            if result
                .as_ref()
                .is_ok_and(|result| result.projection.session_id.is_some())
            {
                plakous_tools::internal::promote(&sessions, id).map_err(backend_runtime_error)?;
                if let Ok(result) = &mut result {
                    result.first_promotion = true;
                }
            }
            result
        }
        .await;
        guard.disarm_or_terminate(result.is_ok()).await?;
        result
    }

    #[cfg(test)]
    pub(crate) async fn write_stdin_with_update(
        environment: &plakous_tools::ExecutionEnvironment,
        raw: &str,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let tool = projection_tool(ProcessToolProjectionKind::CodexUnifiedExec, "write_stdin")?;
        tool.schema
            .parse_instance(raw)
            .map_err(projection_arguments)?;
        let args: CodexWriteArgs = serde_json::from_str(raw).map_err(projection_arguments)?;
        let session = Self::session(environment, args.session_id)?;
        if let Some(chars) = args.chars.as_deref().filter(|chars| !chars.is_empty()) {
            let backend = session.sessions.clone();
            let id = session.id;
            let chars = chars.to_owned();
            process_backend(move || {
                backend.write_stdin(
                    id,
                    chars.as_bytes(),
                    JoinedProcessInputDisposition::KeepOpen,
                )
            })
            .await?;
        }
        let cursor = *lock_process_cursor(&session.cursor);
        Self::observe_with_update(
            environment,
            args.session_id,
            args.yield_time_ms.unwrap_or(5_000),
            args.max_output_tokens,
            cursor,
            !args.chars.as_deref().unwrap_or("").is_empty(),
            control,
        )
        .await
    }

    pub(crate) async fn start_canonical(
        &self,
        environment: &plakous_tools::ExecutionEnvironment,
        request: CanonicalProcessStartRequest,
        control: ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        request.validate()?;
        let yield_time_ms = request.yield_time_ms.clamp(250, 600_000);
        let max_output_bytes = request.max_output_bytes;
        let profile = if request.login {
            &self.login_shell
        } else {
            &self.shell
        };
        let sessions = environment
            .joined_process_sessions()
            .map_err(backend_runtime_error)?;
        let observation_control = control.clone();
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::shell(profile.clone(), request.command)
                .map_err(backend_runtime_error)?,
            request.workdir,
            Vec::new(),
            request.keep_stdin_open,
            if request.tty {
                JoinedProcessTerminalMode::UnixPty
            } else {
                JoinedProcessTerminalMode::Pipes
            },
        )
        .map_err(backend_runtime_error)?;
        let start_sessions = sessions.clone();
        let id = process_backend(move || {
            plakous_tools::internal::start_process_with_output_limit(
                &start_sessions,
                request,
                control
                    .with_preempt(tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1),
                max_output_bytes,
            )
        })
        .await?;
        let handle = u64::from(JoinedProcessSessions::job_number(id));
        let mut guard = StartedProcessGuard::new(sessions.clone(), id);
        let result = async {
            let observed_at = std::time::Instant::now();
            let mut result = Self::observe_summarized(
                environment,
                handle,
                yield_time_ms,
                max_output_bytes,
                JoinedProcessOutputCursor::new(0),
                ObservationWait::PromotionHint,
                &observation_control,
            )
            .await;
            if result
                .as_ref()
                .is_ok_and(|result| result.projection.session_id.is_some())
            {
                let promotion = promote_job(&sessions, id).await?;
                if promotion.as_ref().is_err_and(|error| {
                    error.kind() == plakous_tools::ExecutionBackendErrorKind::ResourceLimit
                }) && *observation_control.preempt().borrow() != plakous_tools::ToolPreempt::None
                {
                    // A full notice ledger cannot turn observation preemption into cancellation.
                    let remaining = std::time::Duration::from_millis(yield_time_ms)
                        .saturating_sub(observed_at.elapsed());
                    let foreground = observation_control.clone().with_preempt(
                        tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1,
                    );
                    result = Self::continue_foreground_until_promotable(
                        environment,
                        &sessions,
                        id,
                        u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                        max_output_bytes,
                        &foreground,
                    )
                    .await;
                } else {
                    promotion.map_err(backend_runtime_error)?;
                    if let Ok(result) = &mut result {
                        result.first_promotion = true;
                    }
                }
            }
            result
        }
        .await;
        guard.disarm_or_terminate(result.is_ok()).await?;
        result
    }

    pub(crate) async fn continue_foreground_until_promotable(
        environment: &plakous_tools::ExecutionEnvironment,
        sessions: &JoinedProcessSessions,
        id: JoinedProcessSessionId,
        mut window: u64,
        max_output_bytes: usize,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let handle = u64::from(JoinedProcessSessions::job_number(id));
        loop {
            let mut result = Self::observe_summarized(
                environment,
                handle,
                window,
                max_output_bytes,
                JoinedProcessOutputCursor::new(0),
                ObservationWait::Hint,
                control,
            )
            .await?;
            if result.projection.session_id.is_none() {
                return Ok(result);
            }
            match promote_job(sessions, id).await? {
                Ok(()) => {
                    result.first_promotion = true;
                    return Ok(result);
                }
                Err(error)
                    if error.kind() == plakous_tools::ExecutionBackendErrorKind::ResourceLimit =>
                {
                    window = 250;
                }
                Err(error) => return Err(backend_runtime_error(error)),
            }
        }
    }

    pub(crate) async fn write_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessWriteRequest,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        request.validate()?;
        let session = Self::session(environment, request.session_id)?;
        if request.input == "\u{3}" {
            return Self::terminate_canonical(
                environment,
                &CanonicalProcessTerminateRequest {
                    session_id: request.session_id,
                },
            )
            .await;
        }
        if let Some(status) = session.sessions.joined_terminal_status(session.id)
            && !request.input.is_empty()
        {
            return Err(exited_session_refusal(request.session_id, status));
        }
        // Empty input with `keep_open` only polls; `close` still closes.
        if !request.input.is_empty()
            || matches!(request.disposition, CanonicalProcessInputDisposition::Close)
        {
            let sessions = session.sessions.clone();
            let id = session.id;
            let input = request.input.clone();
            let disposition = request.disposition.into();
            process_backend(move || sessions.write_stdin(id, input.as_bytes(), disposition))
                .await?;
        }
        // Reference `write_stdin` answers with the output that follows the
        // write, so the model does not need a second call to see it.
        if let Some(yield_time_ms) = request.yield_time_ms {
            let cursor = *lock_process_cursor(&session.cursor);
            return Self::observe_summarized(
                environment,
                request.session_id,
                yield_time_ms,
                request
                    .max_output_bytes
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .unwrap_or(PROCESS_OBSERVATION_BYTES),
                cursor,
                if request.input.is_empty() {
                    ObservationWait::Output
                } else {
                    ObservationWait::Hint
                },
                control,
            )
            .await;
        }
        Ok(ProcessExecutionResult {
            skipped: 0,
            projection: ProcessProjectionResult {
                exit_code: None,
                session_id: Some(request.session_id),
                output: String::new(),
            },
            status: "running",
            cursor: lock_process_cursor(&session.cursor).get(),
            sessions: session.sessions,
            session_id: session.id,
            joined: false,
            first_promotion: false,
            truncated: false,
        })
    }

    pub(crate) async fn poll_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessPollRequest,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        request.validate()?;
        if let Some(reported) = Self::reported_canonical(environment, request.session_id) {
            return Ok(reported);
        }
        let session = Self::session(environment, request.session_id)?;
        if request.read_so_far {
            let outcome = Self::await_canonical(
                environment,
                &CanonicalProcessAwaitRequest {
                    session_ids: vec![request.session_id],
                    wake: None,
                    yield_time_ms: if request.wait_for_exit {
                        request.yield_time_ms
                    } else {
                        0
                    },
                    max_output_bytes: request.max_output_bytes,
                    max_output_records: request.max_output_records,
                },
                control,
            )
            .await?;
            let value: Value =
                serde_json::from_str(&outcome.joined_text()).map_err(projection_arguments)?;
            let task = &value["tasks"][0];
            let terminal = task["status"] != "running";
            let status = session.sessions.joined_terminal_status(session.id);
            return Ok(ProcessExecutionResult {
                projection: ProcessProjectionResult {
                    exit_code: status.and_then(status_exit_code),
                    session_id: if terminal {
                        None
                    } else {
                        Some(request.session_id)
                    },
                    output: task["output"].as_str().unwrap_or_default().to_owned(),
                },
                skipped: 0,
                status: status.map_or("running", process_status_label),
                cursor: lock_process_cursor(&session.cursor).get(),
                sessions: session.sessions,
                session_id: session.id,
                joined: terminal,
                first_promotion: false,
                truncated: false,
            });
        }
        let stored = lock_process_cursor(&session.cursor).get();
        if let Some(cursor) = request.cursor
            && cursor != stored
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "process poll cursor is stale or foreign",
            ));
        }
        Self::observe_summarized(
            environment,
            request.session_id,
            request.yield_time_ms,
            request.max_output_bytes,
            JoinedProcessOutputCursor::new(stored),
            ObservationWait::Output,
            control,
        )
        .await
    }

    pub(crate) async fn terminate_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessTerminateRequest,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        if let Some(reported) = Self::reported_canonical(environment, request.session_id) {
            return Ok(reported);
        }
        let session = Self::session(environment, request.session_id)?;
        let cursor = lock_process_cursor(&session.cursor).get();
        let sessions = session.sessions.clone();
        let id = session.id;
        let status =
            process_backend(move || sessions.stop(id, plakous_tools::JobStopReason::Model)).await?;
        Ok(ProcessExecutionResult {
            skipped: 0,
            projection: ProcessProjectionResult {
                exit_code: status_exit_code(status),
                session_id: None,
                output: String::new(),
            },
            status: process_status_label(status),
            cursor,
            sessions: session.sessions,
            session_id: session.id,
            joined: true,
            first_promotion: false,
            truncated: false,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one multi-session wait keeps terminal reporting and notice attribution together"
    )]
    pub(crate) async fn await_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        request: &CanonicalProcessAwaitRequest,
        control: &ExecutionOperationControl,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        request.validate()?;
        let mut handles = Vec::new();
        for handle in &request.session_ids {
            if !handles.contains(handle) {
                handles.push(*handle);
            }
        }
        if handles.is_empty() {
            // A bare first-terminal await watches every own job still listed.
            handles = environment
                .joined_process_sessions()
                .map_err(backend_runtime_error)?
                .jobs()
                .into_iter()
                .map(|job| u64::from(job.number))
                .collect();
        }
        let sessions = handles
            .into_iter()
            .map(|handle| (handle, Self::session(environment, handle)))
            .collect::<Vec<_>>();
        let mut changes = environment
            .joined_process_sessions()
            .map_err(backend_runtime_error)?
            .job_terminal_changes();
        let mut preempt = control.preempt().clone();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(request.yield_time_ms);
        loop {
            changes.borrow_and_update();
            control.check_cancelled().map_err(backend_runtime_error)?;
            let terminal =
                |(_, session): &(u64, Result<BoundProcessSession, ToolRuntimeError>)| match session
                {
                    Ok(session) => session
                        .sessions
                        .joined_terminal_status(session.id)
                        .is_some(),
                    Err(_) => true,
                };
            let settled = match request.wake {
                // An already-reported or unknown id is no fresh terminal: only
                // a listed job that is or turns terminal wakes the await.
                // It settles at once when every listed id is already reported
                // or unknown, or nothing is listed (a bare wait with no jobs:
                // `all` on an empty session list).
                Some(AwaitWake::Any) => {
                    sessions.iter().all(|(_, session)| session.is_err())
                        || sessions.iter().any(|(_, session)| {
                            session.as_ref().is_ok_and(|session| {
                                session
                                    .sessions
                                    .joined_terminal_status(session.id)
                                    .is_some()
                            })
                        })
                }
                None => sessions.iter().all(terminal),
            };
            if settled
                || std::time::Instant::now() >= deadline
                || *control.preempt().borrow() == plakous_tools::ToolPreempt::Steer
            {
                break;
            }
            await_process_change(&mut changes, &mut preempt, deadline, control).await;
        }
        let page_budget = (request.max_output_bytes / sessions.len().max(1)).max(1);
        let any_promoted = sessions.iter().any(|(_, session)| {
            session
                .as_ref()
                .is_ok_and(|session| session.sessions.is_promoted(session.id))
        });
        let mut tasks = Vec::new();
        let mut terminals = Vec::new();
        for (handle, session) in sessions {
            let Ok(session) = session else {
                let reported = Self::reported_canonical(environment, handle);
                let mut task = json!({"task_id":format!("p{handle}"), "status":if reported.is_some() {"already_reported"} else {"not_found"}, "output":""});
                if let Some(code) = reported.and_then(|result| result.projection.exit_code) {
                    task["exit_code"] = json!(code);
                }
                tasks.push(task);
                continue;
            };
            let backend = session.sessions.clone();
            let id = session.id;
            let max_records = request.max_output_records;
            let poll = process_backend(move || {
                plakous_tools::internal::process_session_tail(
                    &backend,
                    id,
                    page_budget,
                    max_records,
                )
            })
            .await?;
            let status = poll.status();
            let mut bytes = Vec::new();
            for record in poll.records() {
                bytes.extend_from_slice(record.bytes());
            }
            let mut page = HeadTailPage::new(page_budget);
            page.skip(poll.skipped_bytes());
            page.push(&String::from_utf8_lossy(&bytes));
            if status.is_terminal() {
                if session.sessions.active_session_ids().contains(&session.id) {
                    let backend = session.sessions.clone();
                    process_backend(move || backend.join(id)).await?;
                }
                if session.sessions.is_promoted(session.id) {
                    terminals.push(u32::try_from(handle).map_err(projection_arguments)?);
                }
            }
            let truncated = page.omitted != 0;
            let mut output = page.finish();
            if truncated {
                append_output_file(&mut output, &session.sessions, session.id);
            }
            let mut task = json!({"task_id":format!("p{handle}"), "status":process_status_label(status), "output":output});
            if let Some(code) = status_exit_code(status) {
                task["exit_code"] = json!(code);
            }
            tasks.push(task);
        }
        let yielded = tasks.iter().any(|task| task["status"] == "running")
            && *control.preempt().borrow() == plakous_tools::ToolPreempt::Steer;
        let outcome = ToolOutcome::success_text(
            serde_json::to_string(&json!({"tasks":tasks})).map_err(projection_arguments)?,
        )?;
        let outcome = if any_promoted {
            plakous_tools::internal::with_process_telemetry(
                outcome,
                ProcessTelemetry::Waited {
                    operation: ProcessOperationKind::Await,
                    yielded,
                },
            )
        } else {
            outcome
        };
        Ok(plakous_tools::internal::process_job_terminals(
            outcome, terminals,
        ))
    }
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code, reason = "Codex wire compat fields are decoded and ignored")]
struct CodexExecArgs {
    cmd: String,
    workdir: Option<PathBuf>,
    shell: Option<String>,
    login: Option<bool>,
    tty: Option<bool>,
    yield_time_ms: Option<f64>,
    max_output_tokens: Option<f64>,
    session_id: Option<u64>,
    sandbox_permissions: Option<String>,
    justification: Option<String>,
    prefix_rule: Option<Vec<String>>,
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexWriteArgs {
    pub(crate) session_id: u64,
    pub(crate) chars: Option<String>,
    pub(crate) yield_time_ms: Option<u64>,
    pub(crate) max_output_tokens: Option<u64>,
}

#[cfg(test)]
fn non_negative_f64_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "u64::MAX fits exactly in f64 mantissa"
    )]
    let max = u64::MAX as f64;
    if value > max {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "whole non-negative values within u64 range"
    )]
    Some(value as u64)
}

#[cfg(test)]
fn projection_tool(
    projection: ProcessToolProjectionKind,
    name: &str,
) -> Result<ProjectedProcessTool, ToolRuntimeError> {
    projection
        .tools(plakous_tools::JobConfig::default())?
        .into_iter()
        .find(|tool| tool.name().as_str() == name)
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidDefinition,
                "process projection tool is absent",
            )
        })
}
