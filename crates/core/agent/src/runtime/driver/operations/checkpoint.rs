use super::{
    AgentError, AgentEventKind, Arc, BTreeSet, CanonicalHistory, CheckpointId, Driver,
    DurabilityCommitTelemetry, DurableOperationState, EventContext, FamilyArtifact,
    HistorySegmentKind, RuntimeCheckpoint, RuntimeContextState, RuntimeLifecycle,
    RuntimeOperationLedger, StoreAdmission, SuccessorFacts, ledger_needs_retention,
    retain_quiescent_ledger, runtime_manifest_error,
};

impl Driver {
    pub(in crate::runtime::driver) async fn record_job_terminals(
        &mut self,
    ) -> Result<bool, AgentError> {
        let prior = self.authority.checkpoint();
        let manifest = prior.manifest();
        if manifest.lifecycle() != &RuntimeLifecycle::Quiescent {
            return Ok(false);
        }
        let jobs = manifest
            .jobs()
            .with_terminals(&self.job_sessions.jobs())
            .map_err(runtime_manifest_error)?;
        if jobs == *manifest.jobs() && !self.authority.has_pending_line_terminals() {
            return Ok(false);
        }
        let history = manifest.canonical().history().clone();
        let ledger = manifest.ledger().clone();
        let context = manifest.canonical().context().retained_for_exact_resume();
        let family = manifest.family_artifact().cloned();
        let checkpoint_id = self.checkpoint_id()?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            RuntimeLifecycle::Quiescent,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(true)
    }

    /// Recovery publication can restore a full index before this driver runs.
    pub(in crate::runtime::driver) async fn retain_settled_commands(
        &mut self,
    ) -> Result<(), AgentError> {
        let (history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            if manifest.lifecycle() != &RuntimeLifecycle::Quiescent {
                return Ok(());
            }
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let checkpoint_id = self.checkpoint_id()?;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            checkpoint_id,
            RuntimeLifecycle::Quiescent,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "publication receives the closed successor checkpoint groups"
    )]
    pub(in crate::runtime::driver) async fn publish_checkpoint(
        &mut self,
        admission: StoreAdmission,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family_artifact: Option<FamilyArtifact>,
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        Box::pin(self.publish_checkpoint_consuming_jobs(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family_artifact,
            &[],
        ))
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic successor and its consumed observations"
    )]
    pub(super) async fn publish_checkpoint_consuming_jobs(
        &mut self,
        admission: StoreAdmission,
        checkpoint_id: CheckpointId,
        lifecycle: RuntimeLifecycle,
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        context: RuntimeContextState,
        family_artifact: Option<FamilyArtifact>,
        notice_consumptions: &[u32],
    ) -> Result<Arc<RuntimeCheckpoint>, AgentError> {
        // Inside a running Code Cell every store failure is recovery
        // authoritative, so its commits are settlement; each nested call
        // meets the ordinary threshold before it journals anything instead
        // (`refuse_nested_under_store_pressure`).
        let admission =
            if admission == StoreAdmission::Ordinary && self.local_composition.is_in_flight() {
                StoreAdmission::Settlement
            } else {
                admission
            };
        // Both anchors ride every publication, so a recovered finish of a
        // turn's decision commit restores the measure its live terminal had.
        let context = context
            .with_occupancy_anchor(self.context_anchor)
            .and_then(|context| context.with_request_anchor(self.request_anchor.clone()))
            .map_err(runtime_manifest_error)?;
        let retain = lifecycle == RuntimeLifecycle::Quiescent && ledger_needs_retention(&ledger);
        let ledger = if retain {
            // Terminal authority can precede delivery to a still-owned root.
            let owned_turns = self
                .active_root
                .iter()
                .chain(self.roots.iter())
                .map(|root| root.turn_id)
                .collect();
            retain_quiescent_ledger(&ledger, &owned_turns)?
        } else {
            ledger
        };
        let observation = self
            .local_composition
            .as_ref()
            .and_then(|state| state.interpreter_observation().cloned());
        let telemetry = DurabilityCommitTelemetry {
            emitter: Arc::clone(&self.telemetry),
            clock: Arc::clone(&self.clock),
            context: self.telemetry_context_for_turn(
                self.active_root.as_ref().map(|command| command.turn_id),
            ),
        };
        // Keep the owner's large publication future out of the nested driver
        // frame; embedding both frames exceeds the existing test stack budget.
        let carries_job_results = history
            != *self.authority.checkpoint().manifest().canonical().history()
            && history
                .segments()
                .into_iter()
                .last()
                .is_some_and(|segment| segment.kind() == HistorySegmentKind::ToolExchange);
        let mut consumed = notice_consumptions.to_vec();
        if carries_job_results {
            consumed.extend_from_slice(&self.pending_job_consumptions);
        }
        let (jobs, newly_finished, prior) = self.checkpoint_jobs(carries_job_results, &consumed)?;
        // The overlay is eligible only in the checkpoint that first completes
        // its own outer operation, never in an unrelated nested publication.
        let named_state = self
            .pending_named_state
            .as_ref()
            .filter(|(operation, _)| {
                ledger.operation(operation.0).is_some_and(|record| {
                    matches!(record.state(), DurableOperationState::Completed(_))
                })
            })
            .map_or_else(
                || prior.manifest().named_state().clone(),
                |(_, state)| state.clone(),
            );
        let storage_pause = self.active_root.as_ref().map_or_else(
            || crate::runtime::checkpoint::storage_pause::StoragePause::new(self.shutdown.clone()),
            |root| root.shared.storage_pause.clone(),
        );
        let published = Box::pin(storage_pause.scope(self.authority.publish(
            SuccessorFacts {
                checkpoint_id,
                lifecycle,
                history,
                ledger,
                jobs,
                named_state,
                context,
                family: family_artifact,
            },
            observation,
            &telemetry,
            admission,
        )))
        .await?;
        self.settle_checkpoint_publication(
            &published,
            &consumed,
            newly_finished,
            carries_job_results,
            retain,
        );
        Ok(published)
    }

    fn checkpoint_jobs(
        &self,
        carries_job_results: bool,
        consumed: &[u32],
    ) -> Result<(crate::manifest::JobLedger, Vec<u32>, Arc<RuntimeCheckpoint>), AgentError> {
        let jobs = self
            .authority
            .checkpoint()
            .manifest()
            .jobs()
            .updated(
                plakous_tools::internal::next_job_number(&self.job_sessions),
                if carries_job_results {
                    self.pending_job_promotions.clone()
                } else {
                    Vec::new()
                },
                consumed,
            )
            .and_then(|jobs| jobs.with_terminals(&self.job_sessions.jobs()))
            .map_err(runtime_manifest_error)?;
        let prior = self.authority.checkpoint();
        let newly_finished = jobs
            .rows()
            .iter()
            .filter(|row| {
                row.terminal.is_some()
                    && prior
                        .manifest()
                        .jobs()
                        .rows()
                        .iter()
                        .find(|before| before.number == row.number)
                        .is_none_or(|before| before.terminal.is_none())
            })
            .map(|row| row.number)
            .collect::<Vec<_>>();
        Ok((jobs, newly_finished, prior))
    }

    fn settle_checkpoint_publication(
        &mut self,
        published: &RuntimeCheckpoint,
        consumed: &[u32],
        newly_finished: Vec<u32>,
        carries_job_results: bool,
        retain: bool,
    ) {
        if !consumed.is_empty()
            && let Some(host) = &self.job_host
        {
            let terminal = self
                .job_sessions
                .jobs()
                .into_iter()
                .filter(|job| {
                    consumed.contains(&job.number)
                        && job.status != plakous_tools::JobStatus::Running
                })
                .collect::<Vec<_>>();
            host.observe_terminal(self.job_owner, &terminal);
        }
        plakous_tools::internal::consume_jobs(&self.job_sessions, consumed);
        for number in newly_finished {
            let context = self
                .active_root
                .as_ref()
                .map_or_else(EventContext::agent, |root| EventContext::turn(root.turn_id));
            let _ = self
                .events
                .publish(context, AgentEventKind::JobFinished { number });
        }
        if carries_job_results {
            if let Some(root) = &self.active_root {
                for row in &self.pending_job_promotions {
                    let _ = root.shared.events.publish(
                        EventContext::turn(root.turn_id),
                        AgentEventKind::JobPromoted { number: row.number },
                    );
                }
            }
            self.pending_job_promotions.clear();
            self.pending_job_consumptions.clear();
        }
        if retain {
            let kept = published
                .manifest()
                .ledger()
                .commands()
                .into_iter()
                .map(|command| command.command_id().to_owned())
                .collect::<BTreeSet<_>>();
            self.command_ledger
                .retain(|command_id, _| kept.contains(command_id.as_str()));
        }
    }
}
