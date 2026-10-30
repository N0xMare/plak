use super::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, ArtifactPortability,
    DurableRuntimeBranchOutcome, ForkSessionOutcome, ForkSessionRequest, RebindSessionOutcome,
    RebindSessionRequest, RebindToolTransition, SessionBindingReport, SessionLocator,
};

impl AgentSessions {
    /// Creates or recovers one exact quiescent copy-on-write fork.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn fork(
        &self,
        request: ForkSessionRequest,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        let ForkSessionRequest {
            operation_id,
            locator,
            runtime,
        } = request;
        let branched = match runtime {
            None => {
                self.runtime
                    .ordinary_fork(operation_id.as_str(), locator.durable())
                    .await
            }
            Some(runtime) => {
                Box::pin(self.runtime.recompiling_fork(
                    operation_id.as_str(),
                    locator.durable(),
                    runtime.builder,
                ))
                .await
            }
        };
        match branched {
            Ok(DurableRuntimeBranchOutcome::Published(publication)) => {
                let source = SessionLocator::from_durable(publication.source());
                if source != locator {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "runtime fork receipt disagrees with its exact source",
                    ));
                }
                Ok(ForkSessionOutcome::Forked {
                    operation_id,
                    source,
                    target: SessionLocator::from_durable(publication.target()),
                    portability: ArtifactPortability::ClearedUnproved,
                    feed_seq: publication.feed_seq(),
                })
            }
            Ok(DurableRuntimeBranchOutcome::RecoveryRequired { .. }) => {
                let recovery = self
                    .load_exact(locator)
                    .await?
                    .recovery_report()
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "fork recovery classification changed during exact workflow",
                        )
                    })?;
                Ok(ForkSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery,
                })
            }
            Err(error) => Err(self.map_branch_runtime_error(locator, &error).await),
        }
    }

    /// Creates or recovers one explicitly selected, freshly admitted rebind branch.
    ///
    /// # Errors
    ///
    /// Returns typed lookup, stale-locator, integrity, or backend failure.
    pub async fn rebind(
        &self,
        request: RebindSessionRequest,
    ) -> Result<RebindSessionOutcome, AgentSessionsError> {
        let RebindSessionRequest {
            operation_id,
            locator,
            target,
            runtime,
            tool_transition,
        } = request;
        match Box::pin(self.runtime.rebind(
            operation_id.as_str(),
            locator.durable(),
            target.protocol,
            target.route,
            target.model,
            target.selected_provider_configuration,
            target.effective_provider_configuration,
            target.account_binding,
            tool_transition == RebindToolTransition::RecompileForTarget,
            runtime.builder,
        ))
        .await
        {
            Ok(DurableRuntimeBranchOutcome::Published(publication)) => {
                let source = SessionLocator::from_durable(publication.source());
                if source != locator {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "runtime rebind receipt disagrees with its exact source",
                    ));
                }
                let target = SessionLocator::from_durable(publication.target());
                let exact_target = self.load_exact(target).await?;
                let binding = SessionBindingReport::from_checkpoint(&exact_target.checkpoint);
                if binding.identity() != publication.binding().model_binding() {
                    return Err(AgentSessionsError::new(
                        AgentSessionsErrorKind::Integrity,
                        "runtime rebind receipt disagrees with its target manifest",
                    ));
                }
                Ok(RebindSessionOutcome::Rebound {
                    operation_id,
                    source,
                    target,
                    portability: ArtifactPortability::ClearedUnproved,
                    binding: Box::new(binding),
                    feed_seq: publication.feed_seq(),
                })
            }
            Ok(DurableRuntimeBranchOutcome::RecoveryRequired { .. }) => {
                let recovery = self
                    .load_exact(locator)
                    .await?
                    .recovery_report()
                    .ok_or_else(|| {
                        AgentSessionsError::new(
                            AgentSessionsErrorKind::Integrity,
                            "rebind recovery classification changed during exact workflow",
                        )
                    })?;
                Ok(RebindSessionOutcome::RecoveryRequired {
                    operation_id,
                    recovery,
                })
            }
            Err(error) => Err(self.map_branch_runtime_error(locator, &error).await),
        }
    }
}
