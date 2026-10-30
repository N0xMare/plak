use super::{
    AbandonCodeCellOutcome, AbandonCodeCellRequest, AbandonCompactionOutcome,
    AbandonCompactionRequest, AgentSessions, AgentSessionsError, AgentSessionsErrorKind,
    DurableCodeCellAbandonmentOutcome, DurableCodeCellAbandonmentRequest, DurableCompactionReceipt,
    DurableCompactionRecoveryOutcome, DurableEffectEvidenceRecoveryOutcome,
    DurableEffectEvidenceRecoveryRequest, RecoverEffectEvidenceOutcome,
    RecoverEffectEvidenceRequest, RecoverSessionOutcome, RecoverSessionRequest, RuntimeLifecycle,
    RuntimeWorkingRoot, SessionLocator, SessionOperationId, SessionWorkflowUnavailable,
    runtime_error, validate_compaction_operation_id, validate_compaction_publication,
    validate_compaction_source,
};
#[cfg(feature = "sessions-sqlite")]
use {
    super::{RecoveryRequiredReport, store_error, types},
    plakous_store::SqliteSessionStore,
    std::path::Path,
};

impl AgentSessions {
    /// Inspects one exact SQLite branch head without opening a mutable runtime,
    /// acquiring a lease, changing a boot domain, or calling a provider or tool.
    /// A stale locator returns the current head in the typed error; callers may
    /// inspect that head separately but must not adopt it as a session command.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, schema, or storage failure.
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_recovery_sqlite(
        path: impl AsRef<Path>,
        locator: SessionLocator,
    ) -> Result<Option<RecoveryRequiredReport>, AgentSessionsError> {
        let (head, metadata) =
            SqliteSessionStore::inspect_branch_metadata(path, locator.session_ref())
                .map_err(store_error)?;
        let current = SessionLocator::new(
            head.session().session_id(),
            head.session().branch_id(),
            head.checkpoint().checkpoint_id(),
        );
        if current != locator {
            return Err(AgentSessionsError::stale(current));
        }
        let RuntimeLifecycle::Working {
            recovery_phase,
            obligations,
            ..
        } = metadata.lifecycle
        else {
            return Ok(None);
        };
        let obligations = obligations
            .into_iter()
            .map(|obligation| {
                metadata
                    .requests
                    .iter()
                    .find(|(id, _)| *id == obligation.operation_id())
                    .map(|(_, identity)| types::obligation_report(obligation, *identity))
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "working checkpoint lost its exact recovery obligations",
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(RecoveryRequiredReport {
            locator,
            phase: types::recovery_phase_report(recovery_phase),
            obligations,
        }))
    }

    /// Reconciles only the exact compaction operation bound to this identity.
    /// A finalized receipt wins before branch freshness, and recovery never
    /// issues a second provider call.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn recover(
        &self,
        request: RecoverSessionRequest,
    ) -> Result<RecoverSessionOutcome, AgentSessionsError> {
        let RecoverSessionRequest {
            operation_id,
            locator,
            runtime,
        } = request;
        validate_compaction_operation_id(&operation_id)?;
        let recovery_head = match self
            .runtime
            .compaction_receipt(operation_id.as_str(), locator.durable())
            .await
        {
            Ok(DurableCompactionReceipt::Compacted { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(RecoverSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::Failed { .. }) => {
                let recovered = self
                    .continue_failed_compaction(&operation_id, locator, runtime)
                    .await?;
                return self
                    .present_compaction_recovery(operation_id, locator, recovered)
                    .await;
            }
            Ok(DurableCompactionReceipt::Abandoned { source, target }) => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                return Ok(RecoverSessionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                });
            }
            Ok(DurableCompactionReceipt::NotFound) => None,
            Ok(DurableCompactionReceipt::RecoveryRequired(address)) => Some(address),
            Err(error) => {
                return Err(self.map_branch_runtime_error(locator, &error).await);
            }
        };

        if let Some(address) = recovery_head {
            if address.session_id() != locator.session_id
                || address.branch_id() != locator.branch_id
            {
                return Err(AgentSessionsError::new(
                    AgentSessionsErrorKind::Integrity,
                    "compaction recovery receipt crossed its source branch",
                ));
            }
            self.load_recovery_required(SessionLocator::from_durable(address))
                .await?;
        } else {
            let exact = self.load_exact(locator).await?;
            if matches!(
                exact.checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::Turn { .. },
                    ..
                }
            ) {
                let recovery = exact.recovery_report().ok_or_else(|| {
                    AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "ordinary working recovery lost its exact obligations",
                    )
                })?;
                return Ok(RecoverSessionOutcome::Unavailable {
                    operation_id,
                    recovery,
                    reason: SessionWorkflowUnavailable::WorkingRecovery,
                });
            }
        }

        let recovered = match Box::pin(self.runtime.recover_compaction(
            operation_id.as_str(),
            locator.durable(),
            runtime.builder,
        ))
        .await
        {
            Ok(recovered) => recovered,
            Err(error) => {
                let error_locator = recovery_head.map_or(locator, SessionLocator::from_durable);
                return Err(self.map_branch_runtime_error(error_locator, &error).await);
            }
        };
        self.present_compaction_recovery(operation_id, locator, recovered)
            .await
    }

    async fn present_compaction_recovery(
        &self,
        operation_id: SessionOperationId,
        locator: SessionLocator,
        recovered: DurableCompactionRecoveryOutcome,
    ) -> Result<RecoverSessionOutcome, AgentSessionsError> {
        Ok(match recovered {
            DurableCompactionRecoveryOutcome::NotRequired(source) => {
                RecoverSessionOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableCompactionRecoveryOutcome::Compacted { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverSessionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Failed { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverSessionOutcome::Failed {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Abandoned { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverSessionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::RecoveryRequired(address) => {
                RecoverSessionOutcome::Unavailable {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                    reason: SessionWorkflowUnavailable::WorkingRecovery,
                }
            }
        })
    }

    /// Deliberately terminalizes one exact ambiguous compaction without
    /// provider lookup or redispatch. The returned target preserves the source
    /// semantics and may admit a later, differently identified operation.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, identity, ownership, integrity, or backend failure.
    pub async fn abandon_compaction(
        &self,
        request: AbandonCompactionRequest,
    ) -> Result<AbandonCompactionOutcome, AgentSessionsError> {
        let AbandonCompactionRequest {
            operation_id,
            locator,
        } = request;
        let outcome = self
            .runtime
            .abandon_compaction(operation_id.as_str(), locator.durable())
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(match outcome {
            DurableCompactionRecoveryOutcome::NotRequired(source) => {
                AbandonCompactionOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableCompactionRecoveryOutcome::Compacted { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCompactionOutcome::Compacted {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Abandoned { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCompactionOutcome::Abandoned {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::Failed { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCompactionOutcome::Failed {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCompactionRecoveryOutcome::RecoveryRequired(address) => {
                AbandonCompactionOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                }
            }
        })
    }

    /// Finalizes one exact recovery-only Code Cell without evaluating source,
    /// redispatching a nested child, or emitting another root turn result.
    /// Exact repetition returns the first committed target.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, identity, ownership, integrity, or backend failure.
    pub async fn abandon_code_cell(
        &self,
        request: AbandonCodeCellRequest,
    ) -> Result<AbandonCodeCellOutcome, AgentSessionsError> {
        let AbandonCodeCellRequest {
            operation_id,
            locator,
        } = request;
        let request =
            DurableCodeCellAbandonmentRequest::new(operation_id.as_str(), locator.durable())
                .map_err(|error| runtime_error(&error))?;
        let outcome = self
            .runtime
            .abandon_code_cell(request)
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(match outcome {
            DurableCodeCellAbandonmentOutcome::NotRequired(source) => {
                AbandonCodeCellOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableCodeCellAbandonmentOutcome::Finalized { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                AbandonCodeCellOutcome::Finalized {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableCodeCellAbandonmentOutcome::RecoveryRequired(address) => {
                AbandonCodeCellOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                }
            }
        })
    }

    /// Finalizes one exact Working effect from executor-owned terminal evidence.
    ///
    /// The private driver validates the original claim/dispatch evidence and
    /// executor identity, then completes that same effect without executing the
    /// tool. Identical evidence returns the first committed target. Conflicting
    /// evidence leaves the caller turn [`RecoverEffectEvidenceOutcome::RecoveryRequired`]
    /// and never dispatches under a new identity.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, identity, ownership, integrity, or backend failure.
    pub async fn recover_effect_evidence(
        &self,
        request: RecoverEffectEvidenceRequest,
    ) -> Result<RecoverEffectEvidenceOutcome, AgentSessionsError> {
        let RecoverEffectEvidenceRequest {
            operation_id,
            locator,
            recovery_operation,
            evidence,
            executor,
            outcome,
        } = request;
        let request = DurableEffectEvidenceRecoveryRequest::new(
            operation_id.as_str(),
            locator.durable(),
            recovery_operation,
            evidence.claim_digest,
            evidence.was_dispatched(),
            executor,
            outcome,
        )
        .map_err(|error| runtime_error(&error))?;
        let outcome = self
            .runtime
            .recover_effect_evidence(request)
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(match outcome {
            DurableEffectEvidenceRecoveryOutcome::NotRequired(source) => {
                RecoverEffectEvidenceOutcome::NotRequired {
                    operation_id,
                    source: validate_compaction_source(locator, source)?,
                }
            }
            DurableEffectEvidenceRecoveryOutcome::Finalized { source, target } => {
                let (source, target) = validate_compaction_publication(locator, source, target)?;
                RecoverEffectEvidenceOutcome::Finalized {
                    operation_id,
                    source,
                    target,
                }
            }
            DurableEffectEvidenceRecoveryOutcome::RecoveryRequired(address) => {
                RecoverEffectEvidenceOutcome::RecoveryRequired {
                    operation_id,
                    recovery: self
                        .load_recovery_required(SessionLocator::from_durable(address))
                        .await?,
                }
            }
        })
    }
}
