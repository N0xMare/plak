use super::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason,
    AgentSessionEndpointError, AgentSessions, AgentSessionsError, AgentSessionsErrorKind,
    CompactReason, CompactSessionOutcome, CompactSessionRequest, ContextQuery,
    ContextStatusRequest, DurableCompactionReceipt, DurableCompactionRecoveryOutcome,
    DurableSessionAddress, SessionBranchKey, SessionContextStatus, SessionContextStatusKind,
    SessionLocator, SessionOperationId, SessionRuntimeBinding, SessionWorkflowUnavailable,
    compaction_pending_user_input, runtime_error, validate_compaction_operation_id,
    validate_compaction_publication,
};

impl AgentSessions {
    /// Returns only descriptor-bound status or a typed refusal; it never
    /// estimates tokens from characters or guesses a model limit.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn context_status(
        &self,
        request: ContextStatusRequest,
    ) -> Result<SessionContextStatus, AgentSessionsError> {
        let pending_user_input = match request.query {
            ContextQuery::Current => None,
            ContextQuery::WithPendingUserInput(input) => Some(input),
        };
        let branch = SessionBranchKey::new(request.locator.session_id, request.locator.branch_id);
        let Some(endpoint) = self.drivers.endpoint(branch) else {
            let exact = self.load_exact(request.locator).await?;
            let recovery = exact.recovery_report();
            return Ok(SessionContextStatus {
                operation_id: request.operation_id,
                locator: request.locator,
                kind: recovery.map_or(
                    SessionContextStatusKind::Unavailable(
                        SessionWorkflowUnavailable::ContextStatus,
                    ),
                    SessionContextStatusKind::RecoveryRequired,
                ),
            });
        };
        let kind = match endpoint
            .context_status(
                request.locator.session_id,
                request.locator.branch_id,
                request.locator.checkpoint_id,
                pending_user_input,
            )
            .await
        {
            Ok(status) => SessionContextStatusKind::Available(Box::new(status)),
            Err(AgentSessionEndpointError::Stale {
                session_id,
                branch_id,
                checkpoint_id,
            }) => {
                return Err(AgentSessionsError::stale(SessionLocator::new(
                    session_id,
                    branch_id,
                    checkpoint_id,
                )));
            }
            Err(AgentSessionEndpointError::InvalidInput) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::InvalidInput,
                    "pending user input is not valid canonical text",
                ));
            }
            Err(
                AgentSessionEndpointError::Backpressure
                | AgentSessionEndpointError::Busy
                | AgentSessionEndpointError::Unavailable
                | AgentSessionEndpointError::OutcomeUnknown
                | AgentSessionEndpointError::Rejected { .. },
            ) => SessionContextStatusKind::Unavailable(SessionWorkflowUnavailable::ContextStatus),
        };
        Ok(SessionContextStatus {
            operation_id: request.operation_id,
            locator: request.locator,
            kind,
        })
    }

    /// Requests driver-owned active-model no-tools compaction, returning an
    /// exact finalized receipt before consulting current branch freshness.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn compact(
        &self,
        request: CompactSessionRequest,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        let CompactSessionRequest {
            operation_id,
            locator,
            reason,
            qualification,
            runtime,
        } = request;
        validate_compaction_operation_id(&operation_id)?;
        let pending_user_input = compaction_pending_user_input(reason, qualification)?;
        match self
            .runtime
            .compaction_receipt(operation_id.as_str(), locator.durable())
            .await
        {
            Ok(DurableCompactionReceipt::Compacted { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(CompactSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                    // A recovered receipt names the checkpoints and nothing
                    // else; the figures live on the successor and this path
                    // does not open it.
                    summary: None,
                    feed_seq: None,
                });
            }
            Ok(DurableCompactionReceipt::Failed { .. }) => {
                return self
                    .finish_failed_compact(operation_id, locator, runtime)
                    .await;
            }
            Ok(DurableCompactionReceipt::Abandoned { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(CompactSessionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::RecoveryRequired(address)) => {
                return Ok(CompactSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                    diagnostic: None,
                });
            }
            Ok(DurableCompactionReceipt::NotFound) => {}
            Err(error) => {
                return Err(self.map_branch_runtime_error(locator, &error).await);
            }
        }

        let exact = self.load_exact(locator).await?;
        if let Some(recovery) = exact.recovery_report() {
            return Ok(CompactSessionOutcome::RecoveryRequired {
                operation_id,
                recovery,
                diagnostic: None,
            });
        }
        let branch = SessionBranchKey::new(locator.session_id, locator.branch_id);
        let Some(endpoint) = self.drivers.endpoint(branch) else {
            return Ok(CompactSessionOutcome::Unavailable {
                operation_id,
                source: locator,
                reason: SessionWorkflowUnavailable::Compaction,
            });
        };
        let reason = match reason {
            CompactReason::Manual => AgentCompactionReason::Manual,
            CompactReason::Scheduled => AgentCompactionReason::Scheduled,
        };
        let outcome = endpoint
            .compact_session(AgentCompactSessionRequest::with_pending_user_input(
                operation_id.as_str().to_owned(),
                locator.session_id,
                locator.branch_id,
                locator.checkpoint_id,
                reason,
                pending_user_input,
                runtime.builder,
            ))
            .await;
        self.map_compact_outcome(operation_id, locator, outcome)
            .await
    }

    async fn finish_failed_compact(
        &self,
        operation_id: SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        Ok(
            match self
                .continue_failed_compaction(&operation_id, locator, runtime)
                .await?
            {
                DurableCompactionRecoveryOutcome::Compacted { source, target } => {
                    let (source, target) =
                        validate_compaction_publication(locator, source, target)?;
                    CompactSessionOutcome::Compacted {
                        operation_id,
                        source,
                        target,
                        summary: None,
                        feed_seq: None,
                    }
                }
                DurableCompactionRecoveryOutcome::Failed { source, target } => {
                    let (source, target) =
                        validate_compaction_publication(locator, source, target)?;
                    CompactSessionOutcome::Failed {
                        operation_id,
                        source,
                        target,
                        diagnostic: None,
                    }
                }
                DurableCompactionRecoveryOutcome::RecoveryRequired(address) => {
                    CompactSessionOutcome::RecoveryRequired {
                        operation_id,
                        recovery: self
                            .load_recovery_required(SessionLocator::from_durable(address))
                            .await?,
                        diagnostic: None,
                    }
                }
                _ => {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "unpaid continuation lost its finalized compaction receipt",
                    ));
                }
            },
        )
    }

    pub(super) async fn continue_failed_compaction(
        &self,
        operation_id: &SessionOperationId,
        locator: SessionLocator,
        runtime: SessionRuntimeBinding,
    ) -> Result<DurableCompactionRecoveryOutcome, AgentSessionsError> {
        let endpoint = self
            .drivers
            .endpoint(SessionBranchKey::new(locator.session_id, locator.branch_id));
        Box::pin(self.runtime.continue_failed_compaction(
            operation_id.as_str(),
            locator.durable(),
            runtime.builder,
            endpoint,
        ))
        .await
        .map_err(|error| runtime_error(&error))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one closed mapping preserves every compaction outcome and endpoint refusal"
    )]
    pub(super) async fn map_compact_outcome(
        &self,
        operation_id: SessionOperationId,
        locator: SessionLocator,
        outcome: Result<AgentCompactSessionOutcome, AgentSessionEndpointError>,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        Ok(match outcome {
            Ok(AgentCompactSessionOutcome::Compacted {
                source,
                target,
                summary,
                feed_seq,
                ..
            }) => {
                let raw_source =
                    DurableSessionAddress::new(locator.session_id, locator.branch_id, source);
                let raw_target =
                    DurableSessionAddress::new(locator.session_id, locator.branch_id, target);
                let source = if source == locator.checkpoint_id {
                    raw_source
                } else {
                    self.runtime
                        .joined_compaction_source(
                            operation_id.as_str(),
                            locator.durable(),
                            raw_source,
                            raw_target,
                        )
                        .await
                        .map_err(|error| runtime_error(&error))?
                };
                let (source, target) =
                    validate_compaction_publication(locator, source, raw_target)?;
                CompactSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                    summary,
                    feed_seq,
                }
            }
            Ok(AgentCompactSessionOutcome::NotNeeded) => CompactSessionOutcome::NotNeeded {
                operation_id,
                source: locator,
            },
            Ok(AgentCompactSessionOutcome::NoEligibleRange) => {
                CompactSessionOutcome::NoEligibleRange {
                    operation_id,
                    source: locator,
                }
            }
            Ok(AgentCompactSessionOutcome::Failed {
                checkpoint,
                diagnostic,
                ..
            }) => CompactSessionOutcome::Failed {
                operation_id,
                source: locator,
                target: SessionLocator::new(locator.session_id, locator.branch_id, checkpoint),
                diagnostic,
            },
            Ok(AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint,
                diagnostic,
            }) => CompactSessionOutcome::RecoveryRequired {
                operation_id,
                recovery: self
                    .load_recovery_required(SessionLocator::new(
                        locator.session_id,
                        locator.branch_id,
                        checkpoint,
                    ))
                    .await?,
                diagnostic,
            },
            Ok(AgentCompactSessionOutcome::Unavailable)
            | Err(
                AgentSessionEndpointError::Backpressure | AgentSessionEndpointError::Unavailable,
            ) => CompactSessionOutcome::Unavailable {
                operation_id,
                source: locator,
                reason: SessionWorkflowUnavailable::Compaction,
            },
            Err(AgentSessionEndpointError::Busy) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::Busy,
                    "compaction cannot start while another compaction is already in progress",
                ));
            }
            Err(AgentSessionEndpointError::OutcomeUnknown) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::OutcomeUnknown,
                    "compaction reply was lost or its admitted outcome is unknown; consult the change feed",
                ));
            }
            Err(AgentSessionEndpointError::Rejected { reason }) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::InvalidInput,
                    reason,
                ));
            }
            Err(AgentSessionEndpointError::Stale {
                session_id,
                branch_id,
                checkpoint_id,
            }) => {
                return Err(AgentSessionsError::stale(SessionLocator::new(
                    session_id,
                    branch_id,
                    checkpoint_id,
                )));
            }
            Err(AgentSessionEndpointError::InvalidInput) => {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::InvalidInput,
                    "compaction runtime or request was rejected for this exact source",
                ));
            }
        })
    }
}
