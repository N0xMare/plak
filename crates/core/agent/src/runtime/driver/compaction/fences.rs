//! Turn compaction fences and the segment-limit refusal (spec 01 §10.3).

use super::{
    COMMITTED_PRESSURE_FOLD_CAP, CompactionDetail, FAILED_PAID_FOLD_CAP, MANUAL_COMPACTION_REMEDY,
    PAID_FOLD_BACKOFF_ROUNDS, TurnCompactionOutcome, compaction_endpoint_refusal,
    compaction_refusal_reason, failed_paid_fold, has_zero_headroom,
    paid_fold_failed_since_last_round,
};
use crate::input::UserInput;
use crate::runtime::compaction::fold::nested_observation_token_upper_bound;
use crate::runtime::compaction::record::compaction_checkpoint_id;
use crate::runtime::compaction::request::{
    native_compaction_facet_enrolled, native_compaction_scheduling_reserve,
    scheduling_input_headroom, tool_group_fence_occupied_tokens, tool_group_fence_verdict,
};
use crate::runtime::compaction::{CompactOrigin, DurableCompactionIdentity, FenceTrigger};
use crate::runtime::driver::Driver;
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::observation::compaction_session_event_outcome;
use crate::runtime::{ArtifactTransition, OperationId, PromptCommand};
use crate::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason, AgentError,
    AgentErrorKind, AgentEventKind, CompactionEventOutcome, CompactionEventReason,
    CompactionMeasure, CompactionMeasurement, CompactionVerdict, DurableOperationKind,
    DurableOperationState, EventContext, MAX_RUNTIME_RECORDS, MeasuredFit,
    SEGMENT_PRESSURE_SEGMENTS, SEGMENT_REFUSAL_SEGMENTS, TelemetryObservationKind,
};
use plakous_model::{
    ContextHistoryPlan, ContextStatus, ConversationItem, ModelError, ModelErrorKind,
    ModelErrorPhase, ModelOperationRoot, TurnId,
};
use std::time::Duration;

impl Driver {
    async fn measure_scheduling_context(
        &mut self,
        command: &PromptCommand,
    ) -> Option<ContextStatus> {
        match self.measure_live_context_status().await {
            Ok(status) => Some(status),
            Err(error) => {
                let refusal = compaction_refusal_reason(&error);
                self.emit_telemetry(
                    self.telemetry_context.with_turn(command.turn_id),
                    TelemetryObservationKind::CompactionFinished {
                        refusal: Some(refusal),
                        elapsed: Duration::ZERO,
                        outcome: CompactionEventOutcome::Unavailable,
                        reason: CompactionEventReason::Scheduled,
                        tokens_before: None,
                        tokens_after: None,
                        usage: None,
                        measurement: self.compaction_measurement(None),
                    },
                );
                None
            }
        }
    }

    /// A failed paid fold blocks further paid folds during round backoff.
    pub(super) fn paid_fold_admissible(&self, pressure_forced: bool) -> bool {
        !(self.turn_budgets.paid_fold_spent
            || pressure_forced && self.turn_budgets.pressure_fold_spent)
    }

    fn refresh_paid_fold_backoff(&mut self, turn_id: TurnId) {
        if !self.turn_budgets.paid_fold_spent {
            return;
        }
        // The ledger retains every operation since this root's command. Read
        // paid finalization fences, not placeholder commits or provider success:
        // a completed summary can still fail the shrink proof and be billed.
        let checkpoint = self.authority.checkpoint();
        let operations = checkpoint.manifest().ledger().operations();
        let Some(start) = operations.iter().position(|record| {
            matches!(record.kind(), DurableOperationKind::Command { turn_id: Some(root) } if *root == turn_id)
        }) else {
            return;
        };
        let mut failures = 0;
        let mut consecutive_failures = 0;
        let mut rounds = 0;
        for record in &operations[start..] {
            if !matches!(record.state(), DurableOperationState::Finalized(_)) {
                continue;
            }
            match record.kind() {
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(workflow),
                    ..
                } => {
                    let Some(finalization) = record.transitions().last() else {
                        return;
                    };
                    if compaction_checkpoint_id(*workflow, b"success").ok()
                        == Some(finalization.checkpoint_id())
                    {
                        consecutive_failures = 0;
                        rounds = 0;
                    } else if failed_paid_fold(*workflow, record) {
                        failures += 1;
                        consecutive_failures += 1;
                        rounds = 0;
                    }
                }
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(root),
                    ..
                } if *root == turn_id => rounds += 1,
                _ => {}
            }
        }
        if failures >= FAILED_PAID_FOLD_CAP {
            return;
        }
        let backoff = if consecutive_failures == 0 {
            0
        } else {
            PAID_FOLD_BACKOFF_ROUNDS << (consecutive_failures - 1)
        };
        if rounds >= backoff {
            self.turn_budgets.paid_fold_spent = false;
            self.turn_budgets.pressure_fold_spent =
                self.pressure_folds_committed >= COMMITTED_PRESSURE_FOLD_CAP;
        }
    }

    pub(in crate::runtime::driver) fn record_nested_committed_observation(
        &mut self,
        item: &ConversationItem,
    ) {
        self.nested_committed_observation_tokens = self
            .nested_committed_observation_tokens
            .saturating_add(nested_observation_token_upper_bound(item));
    }

    pub(in crate::runtime::driver) async fn promote_tool_exchange_then_maybe_compact(
        &mut self,
        command: &PromptCommand,
        attempt_id: plakous_model::AttemptId,
        batch: Vec<ConversationItem>,
        operations: Vec<OperationId>,
        artifact_transition: ArtifactTransition,
    ) -> Result<(), AgentError> {
        // The tool group's admission prepares the next round, and compaction
        // may dispatch: both need a model inside its admitted window.
        self.reresolve_expired_model(command).await?;
        let capacity_error = self
            .promote_tool_exchange_within_next_round_capacity(
                command,
                attempt_id,
                batch,
                operations,
                artifact_transition,
            )
            .await?;
        let compacted = self
            .maybe_scheduled_compact_after_tool_group(command)
            .await?;
        if let Some(error) = capacity_error {
            if compacted.committed() {
                return Ok(());
            }
            if !self.config.scheduled_compaction_enabled
                || matches!(compacted, TurnCompactionOutcome::Exhausted { .. })
            {
                return Err(self.with_last_compaction_diagnostic(error));
            }
            // An unavailable rung is not exhaustion: ordinary admission retains
            // its typed overflow and the turn's one overflow-recovery backstop.
        }
        if self.observation_budget.capacity_exhausted()
            && self.config.scheduled_compaction_enabled
            && let TurnCompactionOutcome::Exhausted { outcome, refusal } = compacted
            && self
                .measure_scheduling_context(command)
                .await
                .is_some_and(|status| has_zero_headroom(&status))
        {
            let diagnostic = format!(
                "context capacity exhausted: rung=placeholders/replay_omission outcome={outcome:?} refusal={refusal:?}; next-round headroom is 0"
            );
            return Err(self.project_model_error(
                ModelError::new(ModelErrorKind::ContextLimit, diagnostic),
                ModelErrorPhase::Preparation,
            ));
        }
        Ok(())
    }

    /// The ordinary scheduled ladder after a residency candidate deferred;
    /// the caller re-runs residency itself once the ladder settles.
    pub(in crate::runtime::driver) async fn compact_after_deferred_image_residency(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(), AgentError> {
        self.scheduled_ladder_after_tool_group(command)
            .await
            .map(|_| ())
    }

    async fn maybe_scheduled_compact_after_tool_group(
        &mut self,
        command: &PromptCommand,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        self.maybe_image_residency(command, false).await?;
        self.scheduled_ladder_after_tool_group(command).await
    }

    async fn scheduled_ladder_after_tool_group(
        &mut self,
        command: &PromptCommand,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        if !self.config.scheduled_compaction_enabled {
            return Ok(TurnCompactionOutcome::Deferred);
        }
        self.refresh_paid_fold_backoff(command.turn_id);
        let pressure = self.observation_budget.capacity_exhausted();
        if pressure && !self.turn_budgets.pressure_fold_spent {
            return self
                .run_turn_fence_compaction(command, FenceTrigger::CapacityPressure, None)
                .await;
        }
        let Some(status) = self.measure_scheduling_context(command).await else {
            if pressure {
                return self
                    .run_turn_fence_compaction(command, FenceTrigger::CapacityPressure, None)
                    .await;
            }
            return self.compact_for_segment_pressure(command, None, None).await;
        };
        let measurement = Some(self.compaction_measurement(Some(&status)));
        let verdict = tool_group_fence_verdict(
            status.kind(),
            self.anchored_occupied_tokens(&status),
            status.compaction_trigger_tokens(),
            status.uses_unanchored_estimate(),
            native_compaction_scheduling_reserve(&self.provider_configuration),
        );
        if !verdict.is_due() {
            if pressure {
                return self
                    .run_turn_fence_compaction(command, FenceTrigger::CapacityPressure, measurement)
                    .await;
            }
            return self
                .compact_for_segment_pressure(command, None, measurement)
                .await;
        }
        self.run_turn_fence_compaction(command, FenceTrigger::Occupancy, measurement)
            .await
    }

    fn committed_segment_count(&self) -> usize {
        self.authority
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .len()
    }

    /// A paid fold for a committed head of at least
    /// [`SEGMENT_PRESSURE_SEGMENTS`] segments when nothing else triggers one.
    /// It is pressure-forced, so it spends the turn's pressure guard, and like
    /// the occupancy trigger it waits for a model round after a fold that
    /// failed after dispatch: the same source would fail the same way.
    async fn compact_for_segment_pressure(
        &mut self,
        command: &PromptCommand,
        pending_user_input: Option<UserInput>,
        measurement: Option<CompactionMeasurement>,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        if self.committed_segment_count() < SEGMENT_PRESSURE_SEGMENTS
            || !self.paid_fold_admissible(true)
            || self.paid_fold_failed_since_last_round()
        {
            return Ok(TurnCompactionOutcome::Deferred);
        }
        self.run_turn_compaction(
            command,
            FenceTrigger::SegmentPressure,
            pending_user_input,
            measurement,
        )
        .await
    }

    /// Refuses the next round once the committed head holds
    /// [`SEGMENT_REFUSAL_SEGMENTS`] segments. The margin under
    /// [`MAX_RUNTIME_RECORDS`] holds what one round and the refused turn's
    /// terminal append, so the refusal itself always publishes.
    pub(in crate::runtime::driver) fn segment_limit_refusal(
        &mut self,
        turn_id: TurnId,
    ) -> Option<AgentError> {
        if self.committed_segment_count() < SEGMENT_REFUSAL_SEGMENTS {
            return None;
        }
        self.segment_refused_turn = Some(turn_id);
        let segments = self.committed_segment_count();
        let remedy = if self.config.scheduled_compaction_enabled {
            "compact the session or fork from an earlier checkpoint"
        } else {
            MANUAL_COMPACTION_REMEDY
        };
        Some(self.project_model_error(
            ModelError::new(
                ModelErrorKind::ContextLimit,
                format!(
                    "session history holds {segments} segments, at the \
                     {SEGMENT_REFUSAL_SEGMENTS}-segment limit below the \
                     {MAX_RUNTIME_RECORDS}-segment checkpoint bound; {remedy}"
                ),
            ),
            ModelErrorPhase::Preparation,
        ))
    }

    /// Occupancy measured from a current-binding anchor; an unanchored
    /// estimate has none, so it never reaches the occupancy trigger.
    fn anchored_occupied_tokens(&self, status: &ContextStatus) -> Option<u64> {
        if status.wire_input().is_some() {
            self.request_anchor
                .as_ref()
                .filter(|anchor| {
                    anchor.matches_descriptor(self.provider_configuration.effective().descriptor())
                })
                .and_then(|_| status.occupancy_tokens())
                .map(|tokens| tokens.saturating_add(self.nested_committed_observation_tokens))
        } else {
            tool_group_fence_occupied_tokens(
                status.occupancy_estimate_tokens(),
                self.context_anchor,
                self.nested_committed_observation_tokens,
            )
        }
    }

    /// Whether the newest finalized provider call is a fold that failed after
    /// dispatch: the same source would fail the same way, so the occupancy
    /// trigger waits for a model round instead of paying again.
    fn paid_fold_failed_since_last_round(&self) -> bool {
        paid_fold_failed_since_last_round(&self.authority.checkpoint())
    }

    /// Compacts after the provider refused a round's input as over its window.
    ///
    /// The meter did not predict the overflow, so no threshold is consulted:
    /// the provider's refusal is the measurement. Returns whether a compaction
    /// committed, in which case the caller sends the round again as a new
    /// operation on the smaller successor.
    pub(in crate::runtime::driver) async fn compact_after_provider_overflow(
        &mut self,
        command: &PromptCommand,
    ) -> Result<bool, AgentError> {
        if !self.config.scheduled_compaction_enabled {
            return Ok(false);
        }
        self.reresolve_expired_model(command).await?;
        self.refresh_paid_fold_backoff(command.turn_id);
        self.run_turn_fence_compaction(command, FenceTrigger::ProviderOverflow, None)
            .await
            .map(|outcome| outcome.committed())
    }

    pub(in crate::runtime::driver) async fn maybe_compact_before_turn_prompt(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(), AgentError> {
        self.maybe_image_residency(command, true).await?;
        if !self.config.scheduled_compaction_enabled
            || self
                .authority
                .checkpoint()
                .manifest()
                .canonical()
                .history()
                .segments()
                .is_empty()
        {
            return Ok(());
        }
        self.reresolve_expired_model(command).await?;
        let pending_user_input =
            || Some(command.input.clone()).filter(|input| !input.parts().is_empty());
        let Some(status) = self.measure_scheduling_context(command).await else {
            self.compact_for_segment_pressure(command, pending_user_input(), None)
                .await?;
            return Ok(());
        };
        let measurement = Some(self.compaction_measurement(Some(&status)));
        // A session without a host idle fence (a child) is compacted here at the
        // same anchored occupancy trigger. A host fence that already folded left
        // the successor unanchored, and one that failed after dispatch is not
        // repeated before another model round.
        // A seeded genesis has no provider usage anchor yet. Measure it on
        // the destination route instead of sending an above-trigger first turn.
        let unmeasured_seed = std::mem::take(&mut self.seed_compaction_pending);
        // Admission's bound can be tighter than anchored occupancy, or the
        // anchor can belong to an earlier descriptor. Keep the same next-round
        // reserve as the tool-group fence instead of waiting for refusal.
        let verdict = CompactionVerdict::of(CompactionMeasure {
            fit: MeasuredFit::of(status.kind()),
            occupancy: self.anchored_occupied_tokens(&status).or_else(|| {
                unmeasured_seed
                    .then(|| status.unanchored_occupancy_tokens())
                    .flatten()
            }),
            trigger: status.compaction_trigger_tokens(),
            input_headroom: scheduling_input_headroom(
                status.kind(),
                status.uses_unanchored_estimate(),
                native_compaction_scheduling_reserve(&self.provider_configuration),
            ),
        });
        let failed_paid_fold = self.paid_fold_failed_since_last_round();
        let occupancy = verdict.occupancy() && !failed_paid_fold;
        let headroom = verdict.observation_headroom() && !failed_paid_fold;
        if !verdict.overflow() && !occupancy && !headroom {
            self.compact_for_segment_pressure(command, pending_user_input(), measurement)
                .await?;
            return Ok(());
        }
        // Seal against committed intent before the follow-up changes the cut.
        // The pending prompt participates in the retained-tail fit check only.
        if !self
            .run_turn_compaction(
                command,
                FenceTrigger::Occupancy,
                pending_user_input(),
                measurement,
            )
            .await?
            .committed()
            && self.turn_budgets.paid_fold_spent
        {
            // Let ordinary preparation retain the exact typed capacity denial
            // and prompt remedy without repeating a failed paid admission fold.
            // An unpaid refusal may gain a legal cut when new intent commits.
            self.turn_budgets.overflow_recovery_taken = true;
        }
        Ok(())
    }

    async fn run_turn_fence_compaction(
        &mut self,
        command: &PromptCommand,
        trigger: FenceTrigger,
        measurement: Option<CompactionMeasurement>,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        self.run_turn_compaction(command, trigger, None, measurement)
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Turn compaction preserves the pending round and its budget while crossing the scheduled compaction fences"
    )]
    async fn run_turn_compaction(
        &mut self,
        command: &PromptCommand,
        trigger: FenceTrigger,
        pending_user_input: Option<UserInput>,
        measurement: Option<CompactionMeasurement>,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        let checkpoint = self.authority.checkpoint();
        let canonical = checkpoint.manifest().canonical();
        let source = canonical.history().identity();
        if matches!(
            trigger,
            FenceTrigger::Occupancy
                | FenceTrigger::CapacityPressure
                | FenceTrigger::SegmentPressure
        ) && self.last_compaction_successor == Some(source)
        {
            return Ok(TurnCompactionOutcome::Deferred);
        }
        let plan = ContextHistoryPlan::new(&canonical.projection()).map_err(|_| {
            AgentError::new(AgentErrorKind::Internal, "compaction cut planning failed")
        })?;
        if !native_compaction_facet_enrolled(&self.provider_configuration)
            && matches!(
                plan.disposition().kind(),
                plakous_model::ContextPlannerDispositionKind::ProtectedOnly
            )
            && crate::runtime::compaction::placeholder::collect_placeholder_candidates(
                canonical.history(),
            )
            .is_empty()
        {
            self.last_compaction_diagnostic = Some("last compaction: rung=placeholders/replay_omission outcome=NoEligibleRange refusal=None; no consumed tool-result candidates".into());
            return Ok(TurnCompactionOutcome::Exhausted {
                outcome: CompactionEventOutcome::NoEligibleRange,
                refusal: None,
            });
        }
        let topology = self.authority.checkpoint().manifest().topology();
        let uuid = self
            .ids
            .next_uuid(self.clock.now())
            .map_err(|error| AgentError::from_identity(&error))?;
        let request = AgentCompactSessionRequest {
            operation_id: format!("plak:scheduled-compact:{uuid}"),
            session_id: topology.session_id,
            branch_id: topology.branch_id,
            checkpoint_id: topology.checkpoint_id,
            reason: AgentCompactionReason::Scheduled,
            pending_user_input,
            runtime: None,
            fallback_of: None,
        };
        let workflow_id = DurableCompactionIdentity::new(
            &request.operation_id,
            DurableSessionAddress {
                session_id: request.session_id,
                branch_id: request.branch_id,
                checkpoint_id: request.checkpoint_id,
            },
        )?
        .workflow_id;
        let compact_started = std::time::Instant::now();
        let _ = command.shared.events.publish(
            EventContext::turn(command.turn_id),
            AgentEventKind::CompactionStarted {
                workflow_id,
                reason: CompactionEventReason::Scheduled,
            },
        );
        let mut committed_detail = CompactionDetail::default();
        let outcome = Box::pin(self.compact_session_with_detail(
            Some(command),
            request,
            CompactOrigin::TurnFence { trigger },
            &mut committed_detail,
            measurement,
        ))
        .await;
        self.last_compaction_diagnostic = Some(format!(
            "last compaction: rung={} outcome={:?} refusal={:?}{}",
            committed_detail
                .rung
                .unwrap_or("placeholders/replay_omission"),
            compaction_session_event_outcome(&outcome),
            outcome.as_ref().err().map(compaction_refusal_reason),
            committed_detail
                .diagnostic
                .as_deref()
                .or(committed_detail.no_eligible_detail)
                .map_or(String::new(), |detail| format!("; {detail}")),
        ));
        let elapsed_ms = u64::try_from(compact_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // A non-committed terminal that says only *that* it failed cannot be
        // diagnosed from the session log. The classified provider failure, or
        // the endpoint's own refusal, rides the observation.
        let publish_finished =
            |workflow_id: crate::SessionWorkflowId,
             outcome: CompactionEventOutcome,
             summary: Option<crate::control::CompactionSummary>,
             detail: Option<String>,
             feed_seq: Option<crate::feed::FeedSeq>| {
                let _ = command.shared.events.publish(
                    EventContext::turn(command.turn_id).with_feed_seq(feed_seq),
                    AgentEventKind::CompactionFinished {
                        workflow_id,
                        outcome,
                        elapsed_ms,
                        summary,
                        detail: detail.map(Into::into),
                    },
                );
            };
        match outcome {
            Ok(AgentCompactSessionOutcome::Compacted {
                workflow_id: settled_workflow,
                summary,
                feed_seq,
                ..
            }) => {
                if settled_workflow != workflow_id {
                    publish_finished(
                        workflow_id,
                        CompactionEventOutcome::Failed,
                        None,
                        committed_detail.diagnostic.take(),
                        None,
                    );
                    let _ = command.shared.events.publish(
                        EventContext::turn(command.turn_id),
                        AgentEventKind::CompactionStarted {
                            workflow_id: settled_workflow,
                            reason: CompactionEventReason::Scheduled,
                        },
                    );
                }
                publish_finished(
                    settled_workflow,
                    CompactionEventOutcome::Committed,
                    summary,
                    committed_detail.diagnostic,
                    feed_seq,
                );
                Ok(TurnCompactionOutcome::Committed)
            }
            Ok(AgentCompactSessionOutcome::RecoveryRequired { diagnostic, .. }) => {
                publish_finished(
                    workflow_id,
                    CompactionEventOutcome::RecoveryRequired,
                    None,
                    diagnostic.clone(),
                    None,
                );
                Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    diagnostic.unwrap_or_else(|| {
                        "scheduled tool-group compaction remains ambiguous".to_string()
                    }),
                ))
            }
            Ok(AgentCompactSessionOutcome::Failed { diagnostic, .. }) => {
                publish_finished(
                    workflow_id,
                    CompactionEventOutcome::Failed,
                    None,
                    diagnostic,
                    None,
                );
                Ok(if committed_detail.exhausted {
                    TurnCompactionOutcome::Exhausted {
                        outcome: CompactionEventOutcome::Failed,
                        refusal: None,
                    }
                } else {
                    TurnCompactionOutcome::Deferred
                })
            }
            Ok(AgentCompactSessionOutcome::NoEligibleRange) => {
                publish_finished(
                    workflow_id,
                    CompactionEventOutcome::NoEligibleRange,
                    None,
                    committed_detail.no_eligible_detail.map(str::to_owned),
                    None,
                );
                Ok(TurnCompactionOutcome::Exhausted {
                    outcome: CompactionEventOutcome::NoEligibleRange,
                    refusal: None,
                })
            }
            Ok(AgentCompactSessionOutcome::NotNeeded) => {
                publish_finished(
                    workflow_id,
                    CompactionEventOutcome::NotNeeded,
                    None,
                    None,
                    None,
                );
                Ok(TurnCompactionOutcome::Deferred)
            }
            Ok(AgentCompactSessionOutcome::Unavailable) => {
                publish_finished(
                    workflow_id,
                    CompactionEventOutcome::Unavailable,
                    None,
                    Some("compaction preparation or admission was unavailable".to_owned()),
                    None,
                );
                Ok(TurnCompactionOutcome::Deferred)
            }
            Err(error) => {
                publish_finished(
                    workflow_id,
                    CompactionEventOutcome::Unavailable,
                    None,
                    Some(format!(
                        "compaction endpoint refused: {}",
                        compaction_endpoint_refusal(&error)
                    )),
                    None,
                );
                Ok(if committed_detail.exhausted {
                    TurnCompactionOutcome::Exhausted {
                        outcome: CompactionEventOutcome::Unavailable,
                        refusal: Some(compaction_refusal_reason(&error)),
                    }
                } else {
                    TurnCompactionOutcome::Deferred
                })
            }
        }
    }
}
