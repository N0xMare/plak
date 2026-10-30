use super::{
    AgentErrorKind, AgentSessions, AgentSessionsError, AgentSessionsErrorKind,
    CreateSessionOutcome, CreateSessionRequest, DurableRuntimeOpen, DurableSessionAddress,
    DurableSessionRuntime, OpenSessionOutcome, OpenSessionRequest, SessionLocator,
    SessionOperationId, agent_store_origin, runtime_can_restore_phase, runtime_error,
};

impl AgentSessions {
    /// Inspects the exact accepted create receipt without compiling a runtime.
    /// Hosts use the returned genesis locator to read construction facts after
    /// losing locator publication. This never adopts a later head or mutates state.
    ///
    /// # Errors
    /// Returns typed store, integrity or identity failures. Only an absent
    /// deterministic session returns `None`; a retained but stale genesis is
    /// still returned and exact construction reads refuse it.
    pub async fn accepted_create(
        &self,
        operation_id: &SessionOperationId,
    ) -> Result<Option<SessionLocator>, AgentSessionsError> {
        self.runtime
            .accepted_create(operation_id.as_str())
            .await
            .map(|address| address.map(SessionLocator::from_durable))
            .map_err(|error| runtime_error(&error))
    }

    /// Derives the exact genesis locator a create operation will use.
    /// No store, clock or lease is read; this is not evidence of admission.
    ///
    /// # Errors
    /// Returns a typed identity derivation failure.
    pub fn genesis_locator_for(
        operation_id: &SessionOperationId,
    ) -> Result<SessionLocator, AgentSessionsError> {
        DurableSessionRuntime::genesis_address_for(operation_id.as_str())
            .map(SessionLocator::from_durable)
            .map_err(|error| runtime_error(&error))
    }

    /// Atomically creates and attaches one reconstruction-complete session.
    /// Exact retries after release recover the runtime-owned create receipt
    /// before returning the same genesis locator. A still-attached operation
    /// is refused so it cannot create a second private driver.
    ///
    /// # Errors
    ///
    /// Returns typed validation, revalidation, identity, ownership, or backend
    /// failure. Any failed attachment releases its acquired lease.
    pub async fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionOutcome, AgentSessionsError> {
        let CreateSessionRequest {
            operation_id,
            runtime,
            history,
        } = request;
        let mut replay_validation_error = None;
        let named_state = history
            .as_ref()
            .map_or_else(plakous_agent::internal::NamedState::default, |history| {
                history.named_state.clone()
            });
        let history = match history {
            Some(history) => {
                let mut retain_replay = history.retains_replay_for(&runtime);
                if retain_replay
                    && history.validate_replay
                    && let Err(error) =
                        plakous_agent::internal::facade_support::validate_seed_replay(
                            &runtime.builder,
                            history.segments(),
                        )
                        .await
                {
                    retain_replay = false;
                    replay_validation_error = Some(error);
                }
                history.into_canonical(retain_replay)?
            }
            None => plakous_model::CanonicalHistory::default(),
        };
        let _reservation = self.drivers.reserve(&operation_id)?;
        match Box::pin(self.runtime.create_seeded(
            operation_id.as_str(),
            plakous_agent::internal::facade_support::agent_builder_with_named_state(
                runtime.builder,
                named_state,
            ),
            history,
        ))
        .await
        {
            Ok(agent) => {
                let mut session = self.attach(operation_id.clone(), agent).await?;
                session.replay_validation_error = replay_validation_error;
                Ok(CreateSessionOutcome::Created {
                    operation_id,
                    session,
                })
            }
            Err(error) => Err(runtime_error(&error)),
        }
    }

    /// Loads and validates one exact historical head without substituting
    /// current defaults. Working state returns recovery-only authority.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, compatibility, or
    /// backend failure without returning an ordinary runtime.
    pub async fn open(
        &self,
        request: OpenSessionRequest,
    ) -> Result<OpenSessionOutcome, AgentSessionsError> {
        let OpenSessionRequest {
            operation_id,
            locator,
            runtime,
        } = request;
        let _reservation = self.drivers.reserve(&operation_id)?;
        let exact = self.load_exact(locator).await?;
        // Use the runtime's exact closure scope: saved terminal evidence and
        // certified abandonment both recover here without redispatch.
        if let Some(report) = exact.recovery_report()
            && !runtime_can_restore_phase(report.phase())
            && !plakous_agent::internal::checkpoint_closes_on_reopen(&exact.checkpoint)
        {
            return Ok(OpenSessionOutcome::RecoveryRequired {
                operation_id,
                recovery: report,
                retry_after_seconds: None,
            });
        }
        let address = DurableSessionAddress::new(
            locator.session_id,
            locator.branch_id,
            locator.checkpoint_id,
        );
        match Box::pin(
            self.runtime
                .open(operation_id.as_str(), address, runtime.builder),
        )
        .await
        {
            Ok(DurableRuntimeOpen::Opened(agent)) => {
                let session = self.attach(operation_id.clone(), agent).await?;
                Ok(OpenSessionOutcome::Opened {
                    operation_id,
                    session,
                })
            }
            Ok(DurableRuntimeOpen::RestoredWorking { agent, turn }) => {
                let session = self.attach(operation_id.clone(), agent).await?;
                Ok(OpenSessionOutcome::RestoredWorking {
                    operation_id,
                    session,
                    turn,
                })
            }
            Ok(DurableRuntimeOpen::RecoveryRequired {
                retry_after_seconds,
                ..
            }) => {
                let recovery = self
                    .load_exact(locator)
                    .await?
                    .recovery_report()
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "runtime recovery classification changed during exact open",
                        )
                    })?;
                Ok(OpenSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery,
                    retry_after_seconds,
                })
            }
            Err(error) => {
                if error.kind() == AgentErrorKind::Session && agent_store_origin(&error).is_none() {
                    self.load_exact(locator).await?;
                }
                Err(runtime_error(&error).for_session(locator.session_id))
            }
        }
    }
}
