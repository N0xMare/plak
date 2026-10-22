//! Joined process sessions as a tool executor: results, jobs and observation.

use std::fmt::Write as _;

use super::{
    Arc, CanonicalProcessOperationV2, CanonicalProcessTerminateRequest, ExecutionOperationControl,
    HeadTailPage, JoinedProcessOutputCursor, JoinedProcessSessionId, JoinedProcessSessions,
    JoinedProcessShellProfile, MAX_DRAIN_READS, Mutex, PROCESS_OBSERVATION_BYTES,
    PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_CAPABILITY_REVISION,
    PROCESS_SESSION_EXECUTOR_ID, PROCESS_SESSION_POLL_NAME,
    PROCESS_SESSION_POLL_REGISTERED_TOOL_ID, PROCESS_SESSION_REGISTERED_TOOL_ID,
    PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
    PROCESS_SESSION_WRITE_NAME, PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID, Path, ProcessHintSource,
    ProcessOperationKind, ProcessProjectionResult, ProcessTelemetry, RegisteredToolId,
    ToolConcurrency, ToolContext, ToolDefinition, ToolEffect, ToolExecutor,
    ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInputContract, ToolInvocation,
    ToolName, ToolOutcome, ToolOutputContract, ToolRegistration, ToolRuntimeError,
    ToolRuntimeErrorKind, Utf8TextContractV1, append_output_file, await_process_change,
    backend_runtime_error, canonical_process_input_schema, classify_process_outcome,
    deserialize_semantic_u64, drain_process_utf8_bounded, execute_process_await, json,
    lock_pending_process_utf8, lock_process_cursor, operation_control, process_backend,
    process_observation_labels, process_session_active, process_session_joined,
    process_status_label, projection_arguments, raised_stop_result, status_exit_code,
    validate_canonical_poll_shape,
};

pub(crate) struct ProcessExecutionResult {
    pub(crate) projection: ProcessProjectionResult,
    /// Output bytes the session's window evicted before this page began.
    pub(crate) skipped: u64,
    pub(crate) status: &'static str,
    pub(crate) cursor: u64,
    pub(crate) sessions: JoinedProcessSessions,
    pub(crate) session_id: JoinedProcessSessionId,
    pub(crate) joined: bool,
    pub(crate) first_promotion: bool,
    /// The page omitted output, so the full output file is worth naming.
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ObservationWait {
    PromotionHint,
    Hint,
    Output,
    ExitOrOutput,
}

impl ObservationWait {
    pub(crate) fn preempted(self, preempt: plakous_tools::ToolPreempt) -> bool {
        matches!(
            preempt,
            plakous_tools::ToolPreempt::Steer | plakous_tools::ToolPreempt::Notice(_)
        ) || (self == Self::PromotionHint && preempt == plakous_tools::ToolPreempt::Background)
    }
}

impl ProcessExecutionResult {
    #[cfg(test)]
    pub(crate) fn into_outcome(self) -> Result<ToolOutcome, ToolRuntimeError> {
        self.into_outcome_with_promotion(None)
    }

    pub(crate) fn into_outcome_with_promotion(
        mut self,
        promotion: Option<(u64, plakous_tools::ToolPreempt)>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut output = std::mem::take(&mut self.projection.output);
        if self.first_promotion || self.truncated {
            append_output_file(&mut output, &self.sessions, self.session_id);
        }
        if !self.joined
            && let Some((code, count)) = self.sessions.leftover(self.session_id)
        {
            self.projection.exit_code = code;
            self.status = "leftover";
            let _ = write!(
                output,
                "\n[leader exited {}]\n{count} {} still running as job {}. Session release, attempt end and the hard runtime cap stop this job; use this vocabulary's stop operation to stop it now.",
                code.map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
                if count == 1 { "process" } else { "processes" },
                JoinedProcessSessions::job_number(self.session_id)
            );
        }
        let mut canonical = json!({
            "status": self.status,
            "exit_code": self.projection.exit_code,
            "session_id": self.projection.session_id,
            "cursor": self.cursor,
            "output": output,
        });
        if self.first_promotion
            && self.status != "leftover"
            && let Some((hint, preempted)) = promotion
        {
            canonical["foreground_hint_ms"] = json!(hint);
            canonical["preempted"] = json!(matches!(
                preempted,
                plakous_tools::ToolPreempt::Steer | plakous_tools::ToolPreempt::Notice(_)
            ));
            canonical["backgrounded_by_user"] =
                json!(preempted == plakous_tools::ToolPreempt::Background);
        }
        let outcome =
            ToolOutcome::success_text(serde_json::to_string(&canonical).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "canonical process result could not be encoded",
                    &error,
                )
            })?)?;
        let outcome = if self.first_promotion {
            if let Some((hint_ms, preempt)) = promotion {
                plakous_tools::internal::with_process_telemetry(
                    outcome,
                    ProcessTelemetry::Promoted {
                        operation: ProcessOperationKind::Start,
                        hint_source: ProcessHintSource::Default,
                        hint_ms,
                        preempt,
                    },
                )
            } else {
                outcome
            }
        } else if self.sessions.is_promoted(self.session_id) {
            plakous_tools::internal::with_process_telemetry(
                outcome,
                ProcessTelemetry::Waited {
                    operation: ProcessOperationKind::Poll,
                    yielded: false,
                },
            )
        } else {
            outcome
        };
        Ok(if self.joined {
            process_session_joined(outcome, &self.sessions, self.session_id)
        } else if self.sessions.is_promoted(self.session_id) && !self.first_promotion {
            outcome
        } else {
            process_session_active(outcome, &self.sessions, self.session_id)
        })
    }
}

/// One canonical service implementing both model-specific process views.
///
/// The numeric handle is only a bounded projection of an allocation-local
/// opaque `JoinedProcessSessionId`; it is never an OS PID or durable authority.
pub struct ProcessSessionTools {
    pub(crate) config: super::ProcessPackConfig,
    pub(crate) shell: JoinedProcessShellProfile,
    pub(crate) login_shell: JoinedProcessShellProfile,
}

#[derive(Clone)]
pub(crate) struct BoundProcessSession {
    pub(crate) id: JoinedProcessSessionId,
    pub(crate) sessions: JoinedProcessSessions,
    pub(crate) cursor: Arc<Mutex<JoinedProcessOutputCursor>>,
    pub(crate) pending_utf8: Arc<Mutex<Vec<u8>>>,
    pub(crate) quiet_polls: Arc<Mutex<plakous_tools::internal::ProcessQuietPollState>>,
}

pub(crate) struct ProcessObservationGuard {
    state: Arc<Mutex<plakous_tools::internal::ProcessQuietPollState>>,
    repeat: bool,
}

impl ProcessObservationGuard {
    pub(crate) fn enter(session: &BoundProcessSession, repeat: bool) -> Self {
        let mut state = session
            .quiet_polls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.observers += 1;
        state.repeat_observers += u32::from(repeat);
        Self {
            state: Arc::clone(&session.quiet_polls),
            repeat,
        }
    }
}

impl Drop for ProcessObservationGuard {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.observers -= 1;
        state.repeat_observers -= u32::from(self.repeat);
    }
}

impl BoundProcessSession {
    pub(crate) fn leftover_unobserved(&self) -> bool {
        self.sessions.leftover(self.id).is_some()
            && !self
                .quiet_polls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .leftover_observed
    }
}

fn active_process_result(
    session: BoundProcessSession,
    handle: u64,
    output: String,
    cursor: JoinedProcessOutputCursor,
) -> ProcessExecutionResult {
    ProcessExecutionResult {
        skipped: 0,
        projection: ProcessProjectionResult {
            exit_code: None,
            session_id: Some(handle),
            output,
        },
        sessions: session.sessions,
        session_id: session.id,
        joined: false,
        first_promotion: false,
        truncated: false,
        status: "running",
        cursor: cursor.get(),
    }
}

pub(crate) struct StartedProcessGuard {
    pub(crate) sessions: JoinedProcessSessions,
    pub(crate) session_id: JoinedProcessSessionId,
    pub(crate) armed: bool,
}

impl StartedProcessGuard {
    pub(crate) const fn new(
        sessions: JoinedProcessSessions,
        session_id: JoinedProcessSessionId,
    ) -> Self {
        Self {
            sessions,
            session_id,
            armed: true,
        }
    }

    pub(crate) async fn disarm_or_terminate(&mut self, keep: bool) -> Result<(), ToolRuntimeError> {
        self.armed = false;
        if keep {
            return Ok(());
        }
        let sessions = self.sessions.clone();
        let id = self.session_id;
        process_backend(move || sessions.terminate(id)).await?;
        Ok(())
    }
}

impl Drop for StartedProcessGuard {
    fn drop(&mut self) {
        if self.armed {
            let sessions = self.sessions.clone();
            let id = self.session_id;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                // Cancellation can drop the future before its explicit cleanup.
                runtime.spawn_blocking(move || sessions.terminate(id));
            } else {
                let _ = sessions.terminate(id);
            }
        }
    }
}

impl ToolExecutor for ProcessSessionTools {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let identity = invocation.identity();
            let canonical = match (identity.name().as_str(), identity.tool_id().as_str()) {
                (PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Start)
                }
                (PROCESS_SESSION_WRITE_NAME, PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Write)
                }
                (PROCESS_SESSION_POLL_NAME, PROCESS_SESSION_POLL_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Poll)
                }
                (PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID) => {
                    Some(CanonicalProcessOperationV2::Terminate)
                }
                _ => None,
            }
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "canonical process registration identity is not enrolled",
                )
            })?;
            let mut value = invocation.arguments().value().as_ref().clone();
            let (operation, hint_source) = process_observation_labels(canonical, &mut value);
            if canonical == CanonicalProcessOperationV2::Poll {
                validate_canonical_poll_shape(&value)?;
            }
            context.check_cancelled()?;
            let environment = context.environment().clone();
            let control = operation_control(&context);
            let promotion_hint = (canonical == CanonicalProcessOperationV2::Start).then(|| {
                deserialize_semantic_u64(value["yield_time_ms"].clone())
                    .unwrap_or(30_000)
                    .clamp(250, 600_000)
            });
            let preempt = control.preempt().clone();
            if canonical == CanonicalProcessOperationV2::Poll
                && (value.get("session_ids").is_some() || value.get("wake").is_some())
            {
                return execute_process_await(environment, value, control, operation, hint_source)
                    .await;
            }
            let result = match canonical {
                CanonicalProcessOperationV2::Start => {
                    self.start_canonical(
                        &environment,
                        serde_json::from_value(value).map_err(projection_arguments)?,
                        control,
                    )
                    .await
                }
                CanonicalProcessOperationV2::Write => {
                    Self::write_canonical(
                        &environment,
                        &serde_json::from_value(value).map_err(projection_arguments)?,
                        &control,
                    )
                    .await
                }
                CanonicalProcessOperationV2::Poll => {
                    Self::poll_canonical(
                        &environment,
                        &serde_json::from_value(value).map_err(projection_arguments)?,
                        &control,
                    )
                    .await
                }
                CanonicalProcessOperationV2::Terminate => {
                    Self::terminate_canonical(
                        &environment,
                        &serde_json::from_value(value).map_err(projection_arguments)?,
                    )
                    .await
                }
            };
            match result {
                Ok(process_result) => {
                    let yielded = !process_result.joined
                        && matches!(
                            *preempt.borrow(),
                            plakous_tools::ToolPreempt::Steer
                                | plakous_tools::ToolPreempt::Notice(_)
                        );
                    process_result
                        .into_outcome_with_promotion(
                            promotion_hint.map(|hint| (hint, *preempt.borrow())),
                        )
                        .map(|outcome| {
                            classify_process_outcome(outcome, operation, hint_source, yielded)
                        })
                }
                Err(error) if error.proven_no_effect() => {
                    // One rule, not a list of conditions. Whatever the process
                    // executor refuses on its way in — a malformed argument, an
                    // unknown or exited `session_id`, a `workdir` outside the
                    // workspace, a session limit, a full operation queue, a mode
                    // it does not support — never started a process and never
                    // delivered a byte, so the outcome is known. A started
                    // dispatch that returned `Err` would be normalized to
                    // `OutcomeUnknown` and force recovery, and the model's own
                    // program could not catch it. Refusals it can act on must
                    // reach it as ordinary tool failures instead.
                    //
                    // The disposition travels with the error from the authority
                    // that raised it, so a condition nobody has hit yet is
                    // classified by where it was raised rather than by a list
                    // that the next condition will outgrow.
                    ToolOutcome::failure_text(error.message().as_str())
                }
                Err(error) => Err(error),
            }
        })
    }
}

impl ProcessSessionTools {
    /// Constructs the sole fixed shell profile enrolled by the canonical
    /// process-session registration: `/bin/sh -c`, with `/bin/sh -l -c` for
    /// explicit login-shell requests.
    ///
    /// # Errors
    ///
    /// Returns an invariant error only if the fixed absolute profiles cannot
    /// be represented by the host-process contract.
    pub fn posix_sh() -> Result<Self, ToolRuntimeError> {
        let shell = JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")])
            .map_err(backend_runtime_error)?;
        let login_shell =
            JoinedProcessShellProfile::new("/bin/sh", [String::from("-l"), String::from("-c")])
                .map_err(backend_runtime_error)?;
        Ok(Self::new(shell, login_shell))
    }

    /// Binds the canonical service to exact non-login and login shell profiles.
    #[must_use]
    pub fn new(shell: JoinedProcessShellProfile, login_shell: JoinedProcessShellProfile) -> Self {
        Self {
            shell,
            login_shell,
            config: super::ProcessPackConfig::default(),
        }
    }

    /// Configures host-backed completion observation.
    #[must_use]
    pub fn with_config(mut self, config: super::ProcessPackConfig) -> Self {
        self.config = config;
        self
    }

    /// Snapshots or awaits the caller's own jobs by number, exactly as the
    /// multi-id output tool does: one shared deadline (`timeout_ms` clamped
    /// to one hour, zero for a snapshot), per-task rows with `not_found` and
    /// `already_reported` tombstones, a steer returning early, and terminal
    /// rows consuming their completion notices at the caller's commit.
    ///
    /// For a capability that covers the output tool's slot on behalf of
    /// another layer; the text is the model-visible `{ "tasks": [...] }`.
    ///
    /// # Errors
    ///
    /// Returns the process executor's own error for a call whose outcome is
    /// not known; a refusal before any effect is a failure outcome.
    pub async fn await_jobs(
        context: &ToolContext,
        numbers: &[u64],
        timeout_ms: u64,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut value = json!({
            "session_ids": numbers,
            "yield_time_ms": timeout_ms.min(3_600_000),
            "max_output_bytes": PROCESS_OBSERVATION_BYTES,
            "max_output_records": 256,
        });
        let (operation, hint_source) =
            process_observation_labels(CanonicalProcessOperationV2::Poll, &mut value);
        context.check_cancelled()?;
        execute_process_await(
            context.environment().clone(),
            value,
            operation_control(context),
            operation,
            hint_source,
        )
        .await
    }

    /// Awaits the caller's own jobs exactly as pi's `wait` does on a node
    /// without agent tools: the first terminal among `numbers`, or among every
    /// own job still listed when `numbers` is empty (`wake: "any"`), under one
    /// deadline (`timeout_ms` clamped to one hour, zero for a snapshot). The
    /// text is the model-visible `{ "tasks": [...] }`, byte-identical to the
    /// process-only presentation's.
    ///
    /// # Errors
    ///
    /// Returns the process executor's own error for a call whose outcome is
    /// not known; a refusal before any effect is a failure outcome.
    pub async fn await_first_job(
        context: &ToolContext,
        numbers: &[u64],
        timeout_ms: u64,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut value = json!({
            "wake": "any",
            "yield_time_ms": timeout_ms.min(3_600_000),
            "max_output_bytes": PROCESS_OBSERVATION_BYTES,
            "max_output_records": 256,
        });
        if !numbers.is_empty() {
            value["session_ids"] = json!(numbers);
        }
        let (operation, hint_source) =
            process_observation_labels(CanonicalProcessOperationV2::Poll, &mut value);
        context.check_cancelled()?;
        execute_process_await(
            context.environment().clone(),
            value,
            operation_control(context),
            operation,
            hint_source,
        )
        .await
    }

    /// Stops one of the caller's own jobs by number, exactly as the stop
    /// tool does, and returns its model-visible stop result:
    /// `{ exit_code?, status?, output }`, `status` only when the process did
    /// not end by its own exit.
    ///
    /// # Errors
    ///
    /// Returns the process executor's own error for a stop whose outcome is
    /// not known; a refusal before any effect is a failure outcome.
    pub async fn stop_job(
        context: &ToolContext,
        number: u64,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut value = json!({"session_id": number});
        let (operation, hint_source) =
            process_observation_labels(CanonicalProcessOperationV2::Terminate, &mut value);
        context.check_cancelled()?;
        let environment = context.environment().clone();
        let request = CanonicalProcessTerminateRequest { session_id: number };
        let result = match Self::terminate_canonical(&environment, &request).await {
            Ok(result) => result,
            Err(error) if error.proven_no_effect() => {
                return ToolOutcome::failure_text(error.message().as_str());
            }
            Err(error) => return Err(error),
        };
        let sessions = result.sessions.clone();
        let session_id = result.session_id;
        let canonical = classify_process_outcome(
            result.into_outcome_with_promotion(None)?,
            operation,
            hint_source,
            false,
        );
        let stopped = ToolOutcome::success_text(raised_stop_result(&canonical.joined_text())?)?;
        let stopped = match plakous_tools::internal::process_telemetry(&canonical) {
            Some(telemetry) => plakous_tools::internal::with_process_telemetry(stopped, telemetry),
            None => stopped,
        };
        Ok(process_session_joined(stopped, &sessions, session_id))
    }

    /// Creates the sole canonical capability registration backed by this shared service.
    ///
    /// # Errors
    ///
    /// Returns an invalid-definition error if the locked registration cannot be constructed.
    #[allow(
        clippy::too_many_lines,
        reason = "the four exact registrations and one shared binding are reviewed together"
    )]
    pub fn registrations(self: Arc<Self>) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        if self.shell.executable() != Path::new("/bin/sh")
            || self.shell.arguments() != ["-c"]
            || self.login_shell.executable() != Path::new("/bin/sh")
            || self.login_shell.arguments() != ["-l", "-c"]
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "canonical process-session executor requires the fixed /bin/sh profiles",
            ));
        }
        let shell = self
            .shell
            .identity()
            .binding_identity()
            .digest()
            .identity_bytes();
        let login_shell = self
            .login_shell
            .identity()
            .binding_identity()
            .digest()
            .identity_bytes();
        let executor_binding_identity = ToolExecutorBindingIdentity::new(
            1,
            &[
                b"process-session-leftover-jobs",
                b"non-login-shell",
                &shell,
                b"login-shell",
                &login_shell,
            ],
        )?;
        let executor: Arc<dyn ToolExecutor> = self;
        [
            (
                CanonicalProcessOperationV2::Start,
                PROCESS_SESSION_CAPABILITY_NAME,
                PROCESS_SESSION_REGISTERED_TOOL_ID,
                "Start one canonical joined process session.",
                ToolEffect::Mutating,
            ),
            (
                CanonicalProcessOperationV2::Write,
                PROCESS_SESSION_WRITE_NAME,
                PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
                "Write bounded input to one canonical joined process session.",
                ToolEffect::Mutating,
            ),
            (
                CanonicalProcessOperationV2::Poll,
                PROCESS_SESSION_POLL_NAME,
                PROCESS_SESSION_POLL_REGISTERED_TOOL_ID,
                "Read bounded output from one exact process-session cursor.",
                ToolEffect::ReadOnly,
            ),
            (
                CanonicalProcessOperationV2::Terminate,
                PROCESS_SESSION_TERMINATE_NAME,
                PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
                "Terminate and join one canonical process session.",
                ToolEffect::Mutating,
            ),
        ]
        .into_iter()
        .map(|(operation, name, tool_id, description, effect)| {
            let definition = ToolDefinition::new(
                ToolName::new(name.to_owned()).map_err(|error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "canonical process operation name is invalid",
                        &error,
                    )
                })?,
                PROCESS_SESSION_CAPABILITY_REVISION,
                description,
                ToolInputContract::Structured(canonical_process_input_schema(operation)?),
                ToolOutputContract::Utf8(Utf8TextContractV1::new(1_048_576).map_err(|error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "canonical process output contract is invalid",
                        &error,
                    )
                })?),
                ToolConcurrency::Serial,
                effect,
            )
            .map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "canonical process operation definition is invalid",
                    &error,
                )
            })?;
            Ok(ToolRegistration::new(
                RegisteredToolId::new(tool_id)?,
                ToolExecutorId::new(PROCESS_SESSION_EXECUTOR_ID)?,
                executor_binding_identity,
                definition,
                Arc::clone(&executor),
            )
            .nested_eligible())
        })
        .collect()
    }

    pub(crate) fn reported_canonical(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
    ) -> Option<ProcessExecutionResult> {
        let sessions = environment.existing_joined_process_sessions()?;
        let number = u32::try_from(handle).ok()?;
        let (id, status) = plakous_tools::internal::reported_process_session(&sessions, number)?;
        Some(ProcessExecutionResult {
            skipped: 0,
            projection: ProcessProjectionResult {
                exit_code: status_exit_code(status),
                session_id: None,
                output: String::new(),
            },
            status: process_status_label(status),
            cursor: 0,
            sessions,
            session_id: id,
            joined: true,
            first_promotion: false,
            truncated: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn codex_shell(
        &self,
        requested: Option<&str>,
        login: bool,
    ) -> Result<JoinedProcessShellProfile, ToolRuntimeError> {
        let profile = if login {
            &self.login_shell
        } else {
            &self.shell
        };
        if requested.is_some_and(|requested| requested != profile.executable().to_string_lossy()) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "requested shell is not the enrolled shell profile",
            ));
        }
        Ok(profile.clone())
    }

    pub(crate) fn session(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
    ) -> Result<BoundProcessSession, ToolRuntimeError> {
        let unknown = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "process session handle is unknown or stale: not in this session",
            )
        };
        let sessions = environment
            .existing_joined_process_sessions()
            .ok_or_else(unknown)?;
        let number = u32::try_from(handle).map_err(|_| unknown())?;
        let id = sessions.session_for_number(number).ok_or_else(unknown)?;
        let (cursor, pending_utf8, quiet_polls) =
            plakous_tools::internal::process_session_projection_state(&sessions, id)
                .map_err(backend_runtime_error)?;
        Ok(BoundProcessSession {
            id,
            sessions,
            cursor,
            pending_utf8,
            quiet_polls,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the closed process observation axes remain explicit at the single call boundary"
    )]
    #[cfg(test)]
    pub(crate) async fn observe_with_update(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
        yield_time_ms: u64,
        max_output_tokens: Option<u64>,
        cursor: JoinedProcessOutputCursor,
        may_join: bool,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let max_bytes = max_output_tokens
            .and_then(|tokens| tokens.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(PROCESS_OBSERVATION_BYTES)
            .clamp(1, PROCESS_OBSERVATION_BYTES);
        Self::observe_bounded_with_update(
            environment,
            handle,
            yield_time_ms,
            max_bytes,
            256,
            cursor,
            may_join,
            ObservationWait::Hint,
            control,
        )
        .await
    }

    /// One model-facing observation of a running session: everything it
    /// produced during the yield, shown as head and tail when it exceeds
    /// `max_bytes`, with the cursor advanced to the end. A chatty process is
    /// then summarised per call instead of paged 40 KB at a time, which a
    /// model polling a long test run could never catch up with.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one observation coordinates output draining, quiet holds and wake attribution"
    )]
    pub(crate) async fn observe_summarized(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
        yield_time_ms: u64,
        max_bytes: usize,
        cursor: JoinedProcessOutputCursor,
        wait: ObservationWait,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let session = Self::session(environment, handle)?;
        let starting_cursor = cursor.get();
        let repeat = wait == ObservationWait::Output && {
            let state = session
                .quiet_polls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.completion_notices
                && state.quiet_polls >= 1
                && plakous_tools::internal::process_notice_delivery_available(&session.sessions)
                && !plakous_tools::internal::process_unread_mail_pending(&session.sessions)
        };
        let _observer = ProcessObservationGuard::enter(&session, repeat);
        let wait = if repeat {
            ObservationWait::ExitOrOutput
        } else {
            wait
        };
        let yield_time_ms = if repeat { 900_000 } else { yield_time_ms };
        let mut terminals = session.sessions.job_terminal_changes();
        let generation = session.sessions.job_terminal_generation();
        let another_job_terminal = || {
            session
                .sessions
                .terminal_jobs_since(generation)
                .iter()
                .any(|id| id != &format!("p{handle}"))
        };
        terminals.borrow_and_update();
        let mut other_terminal = false;
        let deadline = tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(yield_time_ms))
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "yield timeout overflow",
                )
            })?;
        let mut page = HeadTailPage::new(max_bytes);
        let mut cursor = cursor;
        let mut drained_after_deadline = 0_usize;
        let mut changes =
            plakous_tools::internal::process_session_changes(&session.sessions, session.id)
                .map_err(backend_runtime_error)?;
        let mut preempt = control.preempt().clone();
        loop {
            changes.borrow_and_update();
            let remaining = u64::try_from(
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX);
            let result = Self::observe_bounded_with_update(
                environment,
                handle,
                if repeat { 0 } else { remaining },
                PROCESS_OBSERVATION_BYTES,
                256,
                cursor,
                true,
                wait,
                control,
            )
            .await?;
            page.skip(result.skipped);
            let empty = result.projection.output.is_empty();
            // A full chunk means more output is already waiting: drain the
            // backlog (bounded by the session window) before returning.
            let full = result.projection.output.len() + 4 >= PROCESS_OBSERVATION_BYTES;
            page.push(&result.projection.output);
            if tokio::time::Instant::now() >= deadline {
                drained_after_deadline += 1;
            }
            if result.joined
                || session.leftover_unobserved()
                || (matches!(
                    wait,
                    ObservationWait::Output | ObservationWait::ExitOrOutput
                ) && !empty
                    && !full)
                || (repeat && (other_terminal || another_job_terminal()))
                || wait.preempted(*control.preempt().borrow())
                || (tokio::time::Instant::now() >= deadline
                    && (!full || drained_after_deadline > MAX_DRAIN_READS))
            {
                let truncated = page.omitted != 0;
                let mut output = page.finish();
                let leftover = session.sessions.leftover(session.id).is_some();
                let mut state = session
                    .quiet_polls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if leftover && !state.leftover_observed {
                    state.leftover_observed = true;
                    state.quiet_polls = 0;
                } else if result.joined
                    || !output.is_empty()
                    || state.output_cursor != result.cursor
                    || result.cursor != starting_cursor
                {
                    state.quiet_polls = 0;
                } else if matches!(
                    wait,
                    ObservationWait::Output | ObservationWait::ExitOrOutput
                ) {
                    if tokio::time::Instant::now() >= deadline
                        || other_terminal
                        || another_job_terminal()
                    {
                        state.quiet_polls = state.quiet_polls.saturating_add(1);
                    }
                    if repeat {
                        output = format!(
                            "No new output; process {handle} is still running. Its exit is reported automatically."
                        );
                    }
                }
                if let Some(reason) = control.preempt().borrow().notice_reason().or_else(|| {
                    (!result.joined
                        && plakous_tools::internal::process_unread_mail_pending(&session.sessions))
                    .then_some("New messages are waiting; end this turn to receive them.")
                }) {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(reason);
                }
                return Ok(ProcessExecutionResult {
                    projection: ProcessProjectionResult {
                        output,
                        ..result.projection
                    },
                    skipped: 0,
                    truncated,
                    ..result
                });
            }
            cursor = JoinedProcessOutputCursor::new(result.cursor);
            if empty {
                tokio::select! {
                    () = await_process_change(&mut changes, &mut preempt, deadline, control) => {}
                    _ = terminals.changed(), if repeat => { other_terminal = another_job_terminal(); }
                }
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "one observer owns output cursor, joined proof and cancellation through the bounded poll"
    )]
    pub(crate) async fn observe_bounded_with_update(
        environment: &plakous_tools::ExecutionEnvironment,
        handle: u64,
        yield_time_ms: u64,
        max_bytes: usize,
        max_records: usize,
        cursor: JoinedProcessOutputCursor,
        may_join: bool,
        wait: ObservationWait,
        control: &ExecutionOperationControl,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        let session = Self::session(environment, handle)?;
        let _observer = ProcessObservationGuard::enter(&session, false);
        let mut changes =
            plakous_tools::internal::process_session_changes(&session.sessions, session.id)
                .map_err(backend_runtime_error)?;
        let mut preempt = control.preempt().clone();
        let deadline = tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(yield_time_ms))
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "yield timeout overflow",
                )
            })?;
        let mut output = String::new();
        let mut skipped = 0_u64;
        let mut cursor = cursor;
        loop {
            changes.borrow_and_update();
            let remaining = max_bytes.saturating_sub(output.len());
            debug_assert_ne!(remaining, 0);
            let backend = session.sessions.clone();
            let id = session.id;
            let poll =
                process_backend(move || backend.poll_from(id, cursor, remaining, max_records))
                    .await?;
            let backend_terminal = poll.status().is_terminal() && !poll.more_output();
            skipped = skipped.saturating_add(poll.skipped_bytes());
            let (terminal, buffered_without_text_progress) = {
                let mut pending_utf8 = lock_pending_process_utf8(&session.pending_utf8);
                let pending_before = pending_utf8.len();
                let output_before = output.len();
                for record in poll.records() {
                    pending_utf8.extend_from_slice(record.bytes());
                }
                output.push_str(&drain_process_utf8_bounded(
                    &mut pending_utf8,
                    backend_terminal,
                    remaining,
                ));
                let terminal = backend_terminal && pending_utf8.is_empty();
                let text_progress = output.len() != output_before;
                let buffered_without_text_progress = !text_progress
                    && !pending_utf8.is_empty()
                    && (pending_utf8.len() != pending_before || backend_terminal);
                (terminal, buffered_without_text_progress)
            };
            cursor = poll.next_cursor();
            *lock_process_cursor(&session.cursor) = cursor;
            {
                let mut state = session
                    .quiet_polls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.output_cursor != cursor.get() {
                    state.quiet_polls = 0;
                    state.output_cursor = cursor.get();
                }
            }
            if may_join && terminal {
                let backend = session.sessions.clone();
                let status = process_backend(move || backend.join(id)).await?;
                session
                    .quiet_polls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .joined = true;
                return Ok(ProcessExecutionResult {
                    skipped,
                    projection: ProcessProjectionResult {
                        exit_code: status_exit_code(status),
                        session_id: None,
                        output,
                    },
                    sessions: session.sessions,
                    session_id: session.id,
                    joined: true,
                    first_promotion: false,
                    truncated: false,
                    status: process_status_label(status),
                    cursor: cursor.get(),
                });
            }
            control.check_cancelled().map_err(backend_runtime_error)?;
            if session.leftover_unobserved()
                || wait.preempted(*control.preempt().borrow())
                || (matches!(
                    wait,
                    ObservationWait::Output | ObservationWait::ExitOrOutput
                ) && !output.is_empty())
                || output.len() == max_bytes
                || buffered_without_text_progress
                || !may_join
                || (!poll.status().is_terminal() && tokio::time::Instant::now() >= deadline)
            {
                let mut active = active_process_result(session, handle, output, cursor);
                active.skipped = skipped;
                return Ok(active);
            }
            if poll.more_output() {
                continue;
            }
            await_process_change(&mut changes, &mut preempt, deadline, control).await;
        }
    }
}
