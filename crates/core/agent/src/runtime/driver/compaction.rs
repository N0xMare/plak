//! Driver-owned compaction dispatch, controls and durable publication.

use super::commands::ordinary_capacity_refusal;
use super::operations::transition_admission;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::compaction::fold::{
    build_compaction_success_fold, compact_diagnostic, nested_observation_token_upper_bound,
};
use crate::runtime::compaction::live::{CompactionStreamError, LiveCompaction};
use crate::runtime::compaction::record::{
    compaction_attempt_id, compaction_checkpoint_id, compaction_operation_component,
};
use crate::runtime::compaction::request::{
    compact_runtime_endpoint_error, compact_runtime_endpoint_rejected, context_status_audit,
    exact_compaction_runtime, harness_compaction_input, harness_compaction_input_at_depth,
    harness_compaction_prefix, harness_compaction_reduction_count,
    native_compaction_dispatch_prefix, native_compaction_facet_enrolled,
    portable_compaction_history, should_schedule_mid_turn_compaction_with_trigger,
    tool_group_fence_occupied_tokens, unmeasured_native_compact_audit,
};
use crate::runtime::compaction::{
    CompactOrigin, CompactionCompletionError, DurableCompactionIdentity,
    DurableCompactionRequestRecord, ExactCompactionRuntime,
};
use crate::runtime::context::{context_status_projection, pending_user_segment};
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::driver::model_cleanup::{
    abandoned_compaction_error, agent_model_timeout, ambiguous_compaction_error,
    join_cancelled_control_start, join_cancelled_control_stream,
};
use crate::runtime::driver::{Driver, ModelStartAdmission};
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::observation::{
    committed_compaction_summary, compaction_event_reason, compaction_session_event_outcome,
    compaction_telemetry_tokens,
};
use crate::runtime::operation_record::{
    compaction_success_lifecycle, operation_result_component, working_lifecycle,
    working_lifecycle_for_root,
};
use crate::runtime::terminal::cancelled_error;
use crate::runtime::{ArtifactTransition, OperationId, PromptCommand};
use crate::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason, AgentError,
    AgentErrorKind, AgentEventKind, AgentSessionEndpointError, CheckpointId,
    CompactionEventOutcome, CompactionEventReason, ContextEvaluationAudit, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition, EventContext,
    RuntimeComponentKind, RuntimeLifecycle, StoreAdmission, TelemetryObservationKind,
};
use futures_util::StreamExt;
use plakous_model::{
    Actor, Clock, CompiledProviderConfiguration, ContentBlock, ContextHistoryPlan, ContextStatus,
    ContextStatusKind, ConversationItem, Digest, DigestDomain, HistorySegmentKind, LanguageModel,
    ModelCacheAffinity, ModelError, ModelErrorKind, ModelErrorPhase, ModelEvent,
    ModelOperationRoot, ModelPreparationContext, ModelPreparationError, ModelRequest,
    ModelResponse, ModelStreamItem, PreparedCallRecord, Provenance, ProviderCallRecoveryLocator,
    ProviderCallRecoveryLocatorStage, RequestMetadata, SemanticCompactionCut, TextBlock, Timestamp,
    TurnId, Usage, ValidationMode,
};
use std::sync::Arc;
use std::time::Duration;

const PAID_FOLD_BACKOFF_ROUNDS: usize = 2;
const FAILED_PAID_FOLD_CAP: usize = 3;
/// Committed pressure-forced paid folds one root turn may pay for. Recurring
/// oversized output re-arms pressure after every commit; past this cap a
/// pressure fence uses only the free placeholder rung for the rest of the turn.
const COMMITTED_PRESSURE_FOLD_CAP: usize = 3;

#[derive(Default)]
struct CompactionDetail {
    diagnostic: Option<String>,
    exhausted: bool,
    rung: Option<&'static str>,
}

enum TurnCompactionOutcome {
    Committed,
    Deferred,
    Exhausted {
        outcome: CompactionEventOutcome,
        refusal: Option<crate::CompactionRefusalReason>,
    },
}

impl TurnCompactionOutcome {
    fn committed(&self) -> bool {
        matches!(self, Self::Committed)
    }
}

fn has_zero_headroom(status: &ContextStatus) -> bool {
    use plakous_model::ContextHeadroom;
    match status.kind() {
        ContextStatusKind::WouldOverflow(_) | ContextStatusKind::ProtectedOverflow(_) => true,
        ContextStatusKind::Fits {
            headroom:
                ContextHeadroom::Shared(tokens)
                | ContextHeadroom::ProviderReservedInput { input: tokens }
                | ContextHeadroom::Separate { input: tokens, .. }
                | ContextHeadroom::SharedWithOutputCap { shared: tokens, .. },
        } => tokens.value() == 0,
        _ => false,
    }
}

/// Paid input needs a bound; an offline successor has ordinary next-round admission.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum CompactionFitBasis {
    ActiveHead,
    PaidFoldInput,
    OfflineSuccessor,
}

/// Projects one compaction provider failure.
///
/// Compaction publishes nothing until it holds a complete result, so a provider
/// failure that names its own definite outcome leaves the canonical history
/// exactly as it was: no record was written, nothing was replaced, and the
/// session continues on the history it already had. That is
/// `AgentCompactSessionOutcome::Failed`, which the driver already knows how to
/// carry. Forcing every provider failure to an unknown outcome fenced a whole
/// session over a transient provider condition — a deadline, a rate limit — and
/// cost the operator the conversation.
///
/// A failure the provider itself could not determine still keeps the
/// exact-recovery boundary, and `AgentError::from_model` is the one reader of
/// that: it escalates `OutcomeUnknown` and a hosted-effect retry risk, and
/// classifies everything else definitely. Paid recovery needs no separate check
/// here, because a model error may only carry it when its kind is already
/// `OutcomeUnknown` — `validate_paid_recovery_semantics` refuses any other
/// combination with `PaidRecoveryRequiresOutcomeUnknown`.
fn compaction_provider_error(error: ModelError) -> AgentError {
    AgentError::from_model(error)
}

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
                    },
                );
                None
            }
        }
    }

    fn abandoned_compaction_timeout(&self) -> AgentError {
        agent_model_timeout(
            &self.model_error_target,
            "compaction was abandoned when its deadline elapsed after dispatch; it was not \
             applied and may have been billed",
        )
    }

    /// A failed paid fold blocks further paid folds during round backoff.
    fn paid_fold_admissible(&self, pressure_forced: bool) -> bool {
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
                    } else if compaction_checkpoint_id(*workflow, b"failed").ok()
                        == Some(finalization.checkpoint_id())
                        && record.transitions().iter().any(|transition| {
                            matches!(transition.state(), DurableOperationState::Dispatched)
                        })
                        && !record.transitions().iter().any(|transition| {
                            matches!(
                                transition.state(),
                                DurableOperationState::ProvenNotDispatched(_)
                            )
                        })
                    {
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

    pub(super) fn record_nested_committed_observation(&mut self, item: &ConversationItem) {
        self.nested_committed_observation_tokens = self
            .nested_committed_observation_tokens
            .saturating_add(nested_observation_token_upper_bound(item));
    }

    pub(super) async fn promote_tool_exchange_then_maybe_compact(
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

    async fn maybe_scheduled_compact_after_tool_group(
        &mut self,
        command: &PromptCommand,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        if !self.config.scheduled_compaction_enabled {
            return Ok(TurnCompactionOutcome::Deferred);
        }
        self.refresh_paid_fold_backoff(command.turn_id);
        let pressure = self.observation_budget.capacity_exhausted();
        if pressure && !self.turn_budgets.pressure_fold_spent {
            return self.run_turn_fence_compaction(command, true).await;
        }
        let Some(status) = self.measure_scheduling_context(command).await else {
            if pressure {
                return self.run_turn_fence_compaction(command, true).await;
            }
            return Ok(TurnCompactionOutcome::Deferred);
        };
        let occupied = if status.wire_input().is_some() {
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
        };
        let trigger = status.compaction_trigger_tokens();
        if !should_schedule_mid_turn_compaction_with_trigger(status.kind(), occupied, trigger) {
            if pressure {
                return self.run_turn_fence_compaction(command, true).await;
            }
            return Ok(TurnCompactionOutcome::Deferred);
        }
        self.run_turn_fence_compaction(command, false).await
    }

    /// Compacts after the provider refused a round's input as over its window.
    ///
    /// The meter did not predict the overflow, so no threshold is consulted:
    /// the provider's refusal is the measurement. Returns whether a compaction
    /// committed, in which case the caller sends the round again as a new
    /// operation on the smaller successor.
    pub(super) async fn compact_after_provider_overflow(
        &mut self,
        command: &PromptCommand,
    ) -> Result<bool, AgentError> {
        if !self.config.scheduled_compaction_enabled {
            return Ok(false);
        }
        self.reresolve_expired_model(command).await?;
        self.refresh_paid_fold_backoff(command.turn_id);
        self.run_turn_fence_compaction(command, false)
            .await
            .map(|outcome| outcome.committed())
    }

    pub(super) async fn maybe_compact_before_turn_prompt(
        &mut self,
        command: &PromptCommand,
    ) -> Result<(), AgentError> {
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
        let Some(status) = self.measure_scheduling_context(command).await else {
            return Ok(());
        };
        if !matches!(
            status.kind(),
            ContextStatusKind::WouldOverflow(_) | ContextStatusKind::ProtectedOverflow(_)
        ) {
            return Ok(());
        }
        // Seal against committed intent before the follow-up changes the cut.
        // The pending prompt participates in the retained-tail fit check only.
        if !self
            .run_turn_compaction(command, false, Some(command.text.clone()))
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
        pressure_forced: bool,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        self.run_turn_compaction(command, pressure_forced, None)
            .await
    }

    async fn run_turn_compaction(
        &mut self,
        command: &PromptCommand,
        pressure_forced: bool,
        pending_user_input: Option<String>,
    ) -> Result<TurnCompactionOutcome, AgentError> {
        let checkpoint = self.authority.checkpoint();
        let canonical = checkpoint.manifest().canonical();
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
            self.last_compaction_diagnostic = Some("last compaction: rung=placeholders/replay_omission outcome=NoEligibleRange refusal=None".into());
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
            CompactOrigin::TurnFence { pressure_forced },
            &mut committed_detail,
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
                .as_ref()
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
                    Some("no strictly shrinking offline successor fits".to_owned()),
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

    #[allow(
        clippy::too_many_lines,
        reason = "the one driver-owned compaction sequence keeps every paid and publication fence visible"
    )]
    pub(super) async fn compact_session(
        &mut self,
        active: Option<&PromptCommand>,
        request: AgentCompactSessionRequest,
        origin: CompactOrigin,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        self.compact_session_with_detail(active, request, origin, &mut CompactionDetail::default())
            .await
    }

    async fn compact_session_with_detail(
        &mut self,
        active: Option<&PromptCommand>,
        request: AgentCompactSessionRequest,
        origin: CompactOrigin,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let mut request = request;
        let source = DurableSessionAddress::new(
            request.session_id,
            request.branch_id,
            request.checkpoint_id,
        );
        let identity = DurableCompactionIdentity::new(&request.operation_id, source)
            .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
        let checkpoint = self.authority.checkpoint();
        let topology = checkpoint.manifest().topology();
        if (
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
        ) != (request.session_id, request.branch_id, request.checkpoint_id)
        {
            return Err(AgentSessionEndpointError::Stale {
                session_id: topology.session_id,
                branch_id: topology.branch_id,
                checkpoint_id: topology.checkpoint_id,
            });
        }
        if self.compaction_in_flight {
            return Err(AgentSessionEndpointError::Busy);
        }
        let working_empty = checkpoint
            .manifest()
            .canonical()
            .working()
            .segments()
            .is_empty();
        let busy = match origin {
            CompactOrigin::Endpoint => {
                active.is_some()
                    || !matches!(
                        checkpoint.manifest().lifecycle(),
                        RuntimeLifecycle::Quiescent
                    )
            }
            CompactOrigin::TurnFence { .. } => {
                active.is_none()
                    || !matches!(
                        checkpoint.manifest().lifecycle(),
                        RuntimeLifecycle::Working { .. }
                    )
                    || !working_empty
            }
        };
        if busy {
            return Err(AgentSessionEndpointError::Busy);
        }
        let runtime = match (origin, request.runtime.take()) {
            (CompactOrigin::TurnFence { .. }, None) => ExactCompactionRuntime {
                model: Arc::clone(&self.model),
                provider_configuration: Arc::clone(&self.provider_configuration),
                clock: Arc::clone(&self.clock),
                model_operation_timeout: self.config.model_operation_timeout,
            },
            (CompactOrigin::Endpoint, Some(builder)) => {
                exact_compaction_runtime(builder, &checkpoint, self.run_id)
                    .await
                    .map_err(|error| compact_runtime_endpoint_error(&error))?
            }
            _ => return Err(AgentSessionEndpointError::InvalidInput),
        };
        self.compaction_in_flight = true;
        let compaction_reason = compaction_event_reason(request.reason);
        let compaction_turn_id = active.map(|command| command.turn_id);
        let compaction_context = self.telemetry_context_for_turn(compaction_turn_id);
        self.emit_telemetry(
            compaction_context,
            TelemetryObservationKind::CompactionStarted {
                reason: compaction_reason,
            },
        );
        let compaction_started = std::time::Instant::now();
        let mut measured_source_tokens = None;
        let outcome = Box::pin(self.compact_session_after_runtime(
            active,
            request,
            origin,
            source,
            identity,
            runtime,
            &mut measured_source_tokens,
            committed_detail,
        ))
        .await;
        self.compaction_in_flight = false;
        let compaction_elapsed = compaction_started.elapsed();
        if matches!(outcome, Ok(AgentCompactSessionOutcome::Compacted { .. })) {
            // A successor is a different request prefix: nothing the provider
            // counted before it measures it.
            self.nested_committed_observation_tokens = 0;
        }
        let (tokens_before, tokens_after, compaction_usage) =
            compaction_telemetry_tokens(&self.authority, &outcome);
        self.emit_telemetry(
            compaction_context,
            TelemetryObservationKind::CompactionFinished {
                refusal: outcome.as_ref().err().map(compaction_refusal_reason),
                elapsed: compaction_elapsed,
                outcome: compaction_session_event_outcome(&outcome),
                reason: compaction_reason,
                tokens_before: tokens_before.or(measured_source_tokens),
                tokens_after,
                usage: compaction_usage,
            },
        );
        if let Some(command) = active {
            command
                .shared
                .record_compaction_telemetry(compaction_elapsed);
        }
        outcome
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the one driver-owned compaction sequence keeps every paid and publication fence visible"
    )]
    async fn compact_session_after_runtime(
        &mut self,
        active: Option<&PromptCommand>,
        request: AgentCompactSessionRequest,
        origin: CompactOrigin,
        source: DurableSessionAddress,
        identity: DurableCompactionIdentity,
        runtime: ExactCompactionRuntime,
        measured_source_tokens: &mut Option<u64>,
        committed_detail: &mut CompactionDetail,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let _model_root_scope = Arc::clone(&runtime.model)
            .begin_operation_root(ModelOperationRoot::SessionWorkflow(identity.workflow_id))
            .map_err(ModelError::from)
            .map_err(|error| self.compact_preparation_endpoint_error(error))?;
        let mut native_allowed = true;
        loop {
            let checkpoint = self.authority.checkpoint();
            let source_cache_affinity = checkpoint
                .manifest()
                .canonical()
                .context()
                .affinity()
                .clone();
            let source_projection = checkpoint.manifest().canonical().projection();
            let source_request = ModelRequest::from_parts(
                checkpoint.manifest().canonical().stable_prefix().clone(),
                source_projection.clone(),
                RequestMetadata::for_session_workflow(
                    self.run_id,
                    identity.workflow_id,
                    compaction_attempt_id(
                        identity.workflow_id,
                        if native_allowed {
                            b"source-measurement"
                        } else {
                            b"summary-source-measurement"
                        },
                    )
                    .map_err(|error| compact_runtime_endpoint_error(&error))?,
                    runtime.clock.now(),
                ),
            )
            .map_err(compact_runtime_endpoint_rejected)?;
            let qualification_request = request
                .pending_user_input
                .as_deref()
                .map(|pending| {
                    let notice = self
                        .process_notices()
                        .map_err(|_| AgentSessionEndpointError::Unavailable)?
                        .map(|(segment, _)| segment);
                    let projection = context_status_projection(&checkpoint, Some(pending), notice)?;
                    ModelRequest::from_parts(
                        checkpoint.manifest().canonical().stable_prefix().clone(),
                        projection,
                        RequestMetadata::for_session_workflow(
                            self.run_id,
                            identity.workflow_id,
                            compaction_attempt_id(
                                identity.workflow_id,
                                if native_allowed {
                                    b"qualification-measurement"
                                } else {
                                    b"summary-qualification-measurement"
                                },
                            )
                            .map_err(|error| compact_runtime_endpoint_error(&error))?,
                            runtime.clock.now(),
                        ),
                    )
                    .map_err(compact_runtime_endpoint_rejected)
                })
                .transpose()?;
            let native =
                native_allowed && native_compaction_facet_enrolled(&runtime.provider_configuration);
            committed_detail.rung = Some(if native {
                "native facet"
            } else {
                "harness summary"
            });
            let scheduled_pending_qualified = if request.reason == AgentCompactionReason::Scheduled
                && let Some(qualification_request) = qualification_request
            {
                let qualification_status = self
                    .measure_compaction_projection(
                        &runtime,
                        qualification_request,
                        CompactionFitBasis::ActiveHead,
                        None,
                        runtime.clock.now(),
                    )
                    .await
                    .map_err(|error| compact_runtime_endpoint_error(&error))?
                    .0;
                !matches!(qualification_status.kind(), ContextStatusKind::Fits { .. })
            } else {
                false
            };
            let measured = self
                .measure_compaction_projection(
                    &runtime,
                    source_request,
                    CompactionFitBasis::ActiveHead,
                    None,
                    runtime.clock.now(),
                )
                .await;
            let (before, occupancy_before, source_fits) = match measured {
                Ok((source_status, before)) => (
                    before,
                    source_status.unanchored_occupancy_tokens(),
                    matches!(source_status.kind(), ContextStatusKind::Fits { .. }),
                ),
                Err(error)
                    if request.reason == AgentCompactionReason::Scheduled
                        && request.pending_user_input.is_some()
                        && !scheduled_pending_qualified =>
                {
                    // Skip-if-fits is idle-only: a pending user that still Fits must
                    // not start a paid compact when source measurement fails.
                    // Fence (b) has no pending user; measurement failure must not
                    // become Unavailable (WouldOverflow / mid-turn source).
                    return Err(compact_runtime_endpoint_error(&error));
                }
                Err(_) if native || origin.is_turn_fence() => (
                    unmeasured_native_compact_audit(source_projection.identity())
                        .map_err(|error| compact_runtime_endpoint_error(&error))?,
                    None,
                    true,
                ),
                Err(error) => {
                    return Err(compact_runtime_endpoint_rejected(format!(
                        "compaction source measurement failed: {}",
                        compact_diagnostic(&error)
                    )));
                }
            };
            *measured_source_tokens = Some(before.input_tokens());
            let source_harness_plan = checkpoint
                .manifest()
                .canonical()
                .stable_prefix()
                .resolved_harness_plan_identity()
                .ok_or(AgentSessionEndpointError::Rejected {
                    reason: "compaction source omitted its harness plan identity".into(),
                })?;
            if let CompactOrigin::TurnFence { pressure_forced } = origin
                && !self.paid_fold_admissible(pressure_forced)
            {
                return Box::pin(self.replace_protected_tool_results(
                    active,
                    origin,
                    &runtime,
                    identity,
                    &request.operation_id,
                    source,
                    source_projection.identity(),
                    source_harness_plan,
                    before,
                    occupancy_before,
                    false,
                ))
                .await;
            }
            let (mut compaction_request, cut_identity, record_version, cut) = if native {
                let prefix = native_compaction_dispatch_prefix(
                    checkpoint.manifest().canonical().stable_prefix(),
                )
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
                (
                    ModelRequest::from_parts(
                        prefix,
                        source_projection.clone(),
                        RequestMetadata::for_session_workflow(
                            self.run_id,
                            identity.workflow_id,
                            identity.attempt_id,
                            runtime.clock.now(),
                        ),
                    )
                    .map_err(compact_runtime_endpoint_rejected)?,
                    source_projection.identity(),
                    3_u32,
                    None,
                )
            } else {
                let plan = ContextHistoryPlan::new(&source_projection)
                    .map_err(compact_runtime_endpoint_rejected)?;
                let cut = match SemanticCompactionCut::seal(&source_projection, &plan) {
                    Ok(cut) => cut,
                    Err(plakous_model::SemanticCompactionCutError::NoEligibleSelectedPrefix) => {
                        // Ladder rung three. The native facet is not enrolled on
                        // this route and protection leaves no older intent group
                        // to fold, which is the single-turn shape where the whole
                        // session is the latest intent and its causal tail. Replace
                        // the oldest completed tool-result bodies instead of
                        // refusing; `NoEligibleRange` still comes back when even
                        // that cannot fit, keeping the existing refusal.
                        return Box::pin(self.replace_protected_tool_results(
                            active,
                            origin,
                            &runtime,
                            identity,
                            &request.operation_id,
                            source,
                            source_projection.identity(),
                            source_harness_plan,
                            before,
                            occupancy_before,
                            false,
                        ))
                        .await;
                    }
                    Err(error) => return Err(compact_runtime_endpoint_rejected(error)),
                };
                // A source that fits always has a tail that fits. When it does not,
                // a retained tail that cannot fit on its own means no summary can
                // make a fitting successor, so paying for one is waste: the free
                // rung runs instead, exactly as when nothing older is foldable.
                // At the idle fence the successor must also hold the pending
                // prompt, so a pending prompt that does not fit beside the source
                // runs the gate with that prompt after the tail.
                let pending_segment = if scheduled_pending_qualified {
                    request
                        .pending_user_input
                        .as_deref()
                        .map(|text| pending_user_segment(&checkpoint, text))
                        .transpose()?
                } else {
                    None
                };
                if (!source_fits || pending_segment.is_some())
                    && self
                        .retained_tail_cannot_fit(
                            &runtime,
                            checkpoint.manifest().canonical().stable_prefix(),
                            &cut,
                            pending_segment,
                            identity,
                            source_cache_affinity.clone(),
                        )
                        .await
                {
                    return Box::pin(self.replace_protected_tool_results(
                        active,
                        origin,
                        &runtime,
                        identity,
                        &request.operation_id,
                        source,
                        source_projection.identity(),
                        source_harness_plan,
                        before,
                        occupancy_before,
                        false,
                    ))
                    .await;
                }
                let prefix = harness_compaction_prefix(
                    checkpoint.manifest().canonical().stable_prefix(),
                    runtime.provider_configuration.selected_identity(),
                    runtime.provider_configuration.effective_identity(),
                )
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
                let input = harness_compaction_input(&cut, identity.workflow_id)
                    .map_err(|error| compact_runtime_endpoint_error(&error))?;
                (
                    ModelRequest::from_parts(
                        prefix,
                        input,
                        RequestMetadata::for_session_workflow(
                            self.run_id,
                            identity.workflow_id,
                            identity.attempt_id,
                            runtime.clock.now(),
                        ),
                    )
                    .map_err(compact_runtime_endpoint_rejected)?,
                    cut.identity(),
                    2_u32,
                    Some(cut),
                )
            };
            if native && !source_fits {
                let probe = ModelRequest::from_parts(
                    compaction_request.stable_prefix().clone(),
                    compaction_request.input().clone(),
                    RequestMetadata::for_session_workflow(
                        self.run_id,
                        identity.workflow_id,
                        compaction_attempt_id(identity.workflow_id, b"native-fit")
                            .map_err(|error| compact_runtime_endpoint_error(&error))?,
                        runtime.clock.now(),
                    ),
                )
                .map_err(compact_runtime_endpoint_rejected)?;
                if self
                    .measure_compaction_projection(
                        &runtime,
                        probe,
                        CompactionFitBasis::PaidFoldInput,
                        Some(source_cache_affinity.clone()),
                        runtime.clock.now(),
                    )
                    .await
                    .is_ok_and(|(status, _)| {
                        matches!(
                            status.kind(),
                            ContextStatusKind::WouldOverflow(_)
                                | ContextStatusKind::ProtectedOverflow(_)
                        )
                    })
                {
                    committed_detail.diagnostic = Some(
                    "native facet preparation refused for capacity; trying the sealed harness cut"
                        .into(),
                );
                    native_allowed = false;
                    continue;
                }
            }
            if let Some(cut) = cut
                && harness_compaction_reduction_count(&cut) > 0
            {
                // Measure each candidate with its own offline attempt identity.
                // Only the selected request is prepared with the provider attempt
                // below, persisted, and dispatched. A refusal during a fit probe
                // must never consume or reuse that dispatch capability.
                for depth in 0..=harness_compaction_reduction_count(&cut) {
                    let input =
                        harness_compaction_input_at_depth(&cut, identity.workflow_id, depth)
                            .map_err(|error| compact_runtime_endpoint_error(&error))?;
                    let candidate = ModelRequest::from_parts(
                        compaction_request.stable_prefix().clone(),
                        input,
                        compaction_request.metadata(),
                    )
                    .map_err(compact_runtime_endpoint_rejected)?;
                    let probe = ModelRequest::from_parts(
                        candidate.stable_prefix().clone(),
                        candidate.input().clone(),
                        RequestMetadata::for_session_workflow(
                            self.run_id,
                            identity.workflow_id,
                            compaction_attempt_id(
                                identity.workflow_id,
                                format!("summary-fit-{depth}").as_bytes(),
                            )
                            .map_err(|error| compact_runtime_endpoint_error(&error))?,
                            candidate.metadata().created_at(),
                        ),
                    )
                    .map_err(compact_runtime_endpoint_rejected)?;
                    let measurement = self
                        .measure_compaction_projection(
                            &runtime,
                            probe,
                            CompactionFitBasis::PaidFoldInput,
                            Some(source_cache_affinity.clone()),
                            runtime.clock.now(),
                        )
                        .await;
                    match measurement {
                        Ok((status, _)) => {
                            compaction_request = candidate;
                            if !matches!(
                                status.kind(),
                                ContextStatusKind::WouldOverflow(_)
                                    | ContextStatusKind::ProtectedOverflow(_)
                            ) {
                                break;
                            }
                        }
                        // Unrelated preparation failures do not authorize further
                        // elision. The unchanged preparation/fallback path below
                        // reports the actual failure with no paid dispatch.
                        Err(_) => break,
                    }
                }
            }
            let descriptor = runtime
                .provider_configuration
                .effective()
                .descriptor()
                .clone();
            let mut validated = descriptor
                .validate_request(compaction_request.clone(), ValidationMode::Strict)
                .map_err(ModelError::from)
                .map_err(|error| self.compact_preparation_endpoint_error(error))?;
            descriptor
                .preflight_context_admission_requirement(
                    validated.request().context_admission_requirement(),
                )
                .map_err(ModelPreparationError::from)
                .map_err(ModelError::from)
                .map_err(|error| self.compact_preparation_endpoint_error(error))?;
            let prepared_at = runtime.clock.now();
            let authority = Arc::clone(&runtime.model)
                .revalidate_runtime_authority(prepared_at)
                .await
                .map_err(ModelPreparationError::from)
                .map_err(ModelError::from)
                .map_err(|error| self.compact_preparation_endpoint_error(error))?;
            validate_configuration_runtime_authority(
                &runtime.provider_configuration,
                &descriptor,
                &authority,
                prepared_at,
            )
            .map_err(ModelError::from)
            .map_err(|error| self.compact_preparation_endpoint_error(error))?;
            // Paid dispatch uses the source branch's cache affinity; only the
            // successor checkpoint rotates to `target_affinity_entropy`.
            let preparation = ModelPreparationContext::new(authority, prepared_at)
                .with_required_input_bound()
                .with_request_anchor(self.request_anchor.clone())
                .with_cache_affinity(source_cache_affinity);
            let portable_preparation = preparation.clone();
            let preparation = self.with_completed_history_prefix(&validated, preparation);
            // A compaction carries cache affinity and closed-history proof. Its
            // input is the sealed cut, a strict subset of the live turn's active family
            // artifact covers, so attaching that artifact asks the route to reuse
            // state that does not match this request's canonical coverage. On the
            // Responses wire `from_active_artifact` refuses that outright on a
            // coverage mismatch, which is how the live Grok and Codex growth turns
            // of 2026-09-18 lost every compaction they attempted; all five
            // Responses routes now prepare fresh on that condition.
            //
            // The previous-response continuation is a second, separate attachment.
            // Only the two Anthropic Messages routes read it, so it was never what
            // Grok or Codex hit, but a compaction must not continue the live turn's
            // response there either, for the same coverage reason. Neither is
            // attached here, and `the_prepared_compaction_attaches_no_artifact_and_no_continuation`
            // pins that so a future caller cannot re-attach either one.
            let mut prepared =
                Arc::clone(&runtime.model).prepare_call(validated.clone(), preparation.clone());
            let native_protected_history = native
                && prepared.as_ref().is_err_and(|error| {
                    error.rejection_kind()
                        == Some(plakous_model::ModelPreparationErrorKind::ProtectedHistory)
                });
            if native_protected_history {
                let history = portable_compaction_history(compaction_request.input().history())
                    .map_err(|error| compact_runtime_endpoint_error(&error))?;
                compaction_request = ModelRequest::from_parts(
                    compaction_request.stable_prefix().clone(),
                    plakous_model::ConversationView::from_shared(
                        history,
                        plakous_model::WorkingProjection::default(),
                    ),
                    compaction_request.metadata(),
                )
                .map_err(compact_runtime_endpoint_rejected)?;
                validated = descriptor
                    .validate_request(compaction_request.clone(), ValidationMode::Strict)
                    .map_err(ModelError::from)
                    .map_err(|error| self.compact_preparation_endpoint_error(error))?;
                // The failed lowering created no dispatch capability. Retry fresh
                // once; only this selected call can ever be persisted and paid.
                prepared = Arc::clone(&runtime.model)
                    .prepare_call(validated.clone(), portable_preparation);
            }
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error)
                    if native
                        && ModelError::from(error.clone()).kind()
                            == ModelErrorKind::ContextLimit =>
                {
                    let error = self
                        .project_model_error(ModelError::from(error), ModelErrorPhase::Preparation);
                    committed_detail.diagnostic = Some(format!(
                        "native facet preparation refused for capacity: {}",
                        compact_diagnostic(&error)
                    ));
                    // Preparation created no capability or durable operation. Keep
                    // this workflow's sole dispatch attempt and try its sealed cut.
                    native_allowed = false;
                    continue;
                }
                // The compaction's own request could not be prepared. Nothing has
                // been dispatched and nothing is owed, so this is not a terminal
                // outcome for the ladder: it is one rung being unavailable. Falling
                // to placeholders here is what stops the next round reaching the
                // provider over the window. The live Grok growth case did exactly
                // that — two compactions unavailable because an active Responses
                // artifact no longer matched the request's canonical coverage,
                // then a provider capacity denial, with no placeholder record anywhere.
                Err(error) => {
                    let error = self
                        .project_model_error(ModelError::from(error), ModelErrorPhase::Preparation);
                    let rung = if native {
                        "native facet"
                    } else {
                        "harness summary"
                    };
                    let preparation_refusal =
                        format!("{rung} preparation failed: {}", compact_diagnostic(&error));
                    committed_detail.diagnostic = Some(preparation_refusal.clone());
                    return Box::pin(self.replace_protected_tool_results(
                    active,
                origin,
                    &runtime,
                    identity,
                    &request.operation_id,
                    source,
                    source_projection.identity(),
                    source_harness_plan,
                    before,
                    occupancy_before,
                    native_protected_history,
                ))
                .await
                .and_then(|outcome| match outcome {
                    // Placeholders could not act either; report the original
                    // preparation failure rather than inventing a range
                    // outcome for a rung that never got to run.
                    AgentCompactSessionOutcome::NoEligibleRange => {
                        committed_detail.exhausted = true;
                        Err(compact_runtime_endpoint_rejected(format!(
                            "{preparation_refusal}; no fitting offline successor (tool-result placeholders or replay omission)"
                        )))
                    }
                    outcome => Ok(outcome),
                });
                }
            };
            let prepared_record = prepared.record().clone();
            let entropy_left = self
                .ids
                .next_uuid(self.clock.now())
                .map_err(compact_runtime_endpoint_rejected)?;
            let entropy_right = self
                .ids
                .next_uuid(self.clock.now())
                .map_err(compact_runtime_endpoint_rejected)?;
            let target_affinity_entropy = *Digest::sha256(
                DigestDomain::ProviderOperation,
                3,
                &[
                    b"plakous-agent/session-compaction-affinity-v1",
                    identity.workflow_id.as_bytes(),
                    entropy_left.as_bytes(),
                    entropy_right.as_bytes(),
                ],
            )
            .map_err(compact_runtime_endpoint_rejected)?
            .bytes();
            let record = DurableCompactionRequestRecord {
                version: record_version,
                workflow_id: identity.workflow_id,
                turn_fence: origin.is_turn_fence(),
                operation_id: request.operation_id.clone(),
                source_session: source.session_id,
                source_branch: source.branch_id,
                source_checkpoint: source.checkpoint_id,
                source_history: source_projection.history().identity(),
                cut: cut_identity,
                request: compaction_request,
                provider_configuration: runtime.provider_configuration.as_ref().clone(),
                source_harness_plan,
                before_measurement: before.measurement_identity(),
                before_status: before.status_identity(),
                before_input_tokens: before.input_tokens(),
                target_affinity_entropy,
            };
            let (compaction_operation, provider_operation) = self
                .prepare_compaction_operations(identity, &record, &prepared_record)
                .await
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
            let locator_refusal = match prepared_record.recovery_locator() {
                None => None,
                Some(prepared_locator) => match self
                    .persist_compaction_locator(
                        identity,
                        provider_operation,
                        identity.prepared_locator_checkpoint,
                        prepared_locator,
                        ProviderCallRecoveryLocatorStage::Prepared,
                    )
                    .await
                {
                    Ok(()) => None,
                    // An ordinary refusal leaves both operations `Prepared`, so
                    // the failure below settles them as proven not dispatched.
                    Err(error) if ordinary_capacity_refusal(&error).is_some() => Some(error),
                    Err(_) => {
                        return Ok(AgentCompactSessionOutcome::RecoveryRequired {
                            checkpoint: self.authority.checkpoint().id(),
                            diagnostic: None,
                        });
                    }
                },
            };
            let mut live = LiveCompaction::prepared(
                origin,
                active.cloned(),
                identity,
                (compaction_operation, provider_operation),
                record,
                prepared,
                validated,
                runtime,
            );
            let result = if let Some(error) = locator_refusal {
                Err(error)
            } else {
                if matches!(
                    origin,
                    CompactOrigin::TurnFence {
                        pressure_forced: true
                    }
                ) {
                    self.turn_budgets.pressure_fold_spent = true;
                }
                Box::pin(self.run_compaction_call(&mut live)).await
            };
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    // A fold that did not commit may still have been billed. The
                    // next rounds use the free rung with escalating backoff and
                    // a per-turn failure cap. A successful fold re-arms pressure
                    // for later newly consumed work.
                    let failed = self
                        .terminalize_compaction_failure(
                            identity,
                            compaction_operation,
                            provider_operation,
                            live.source(),
                            live.record(),
                            &error,
                            live.origin(),
                            live.turn_id(),
                            None,
                            Some((live.prepared_record().identity(), identity.attempt_id)),
                        )
                        .await;
                    if origin.is_turn_fence() {
                        let head = self.authority.checkpoint();
                        let operations = head.manifest().ledger().operations();
                        let billed = operations
                            .iter()
                            .find(|record| record.operation_id() == provider_operation.0)
                            .is_some_and(|record| {
                                record.transitions().iter().any(|transition| {
                                    matches!(transition.state(), DurableOperationState::Dispatched)
                                }) && !record.transitions().iter().any(|transition| {
                                    matches!(
                                        transition.state(),
                                        DurableOperationState::ProvenNotDispatched(_)
                                    )
                                })
                            });
                        self.turn_budgets.paid_fold_spent |= billed;
                        if !billed {
                            self.turn_budgets.pressure_fold_spent = false;
                        }
                    }
                    // A definite paid-fold failure mid-turn changed nothing, so the
                    // free rung still has the whole source to work on, and the
                    // turn goes on under the window instead of over it. It is a
                    // new workflow: the failed fold keeps its own record. A
                    // cancelled fold is not retried in any form, and an idle
                    // `/compact` reports its failure to the operator who asked.
                    if !origin.is_turn_fence()
                        || !matches!(failed, AgentCompactSessionOutcome::Failed { .. })
                        || error.kind() == AgentErrorKind::Cancelled
                    {
                        return Ok(failed);
                    }
                    let head = self.authority.checkpoint();
                    let topology = head.manifest().topology();
                    let current = DurableSessionAddress::new(
                        topology.session_id,
                        topology.branch_id,
                        topology.checkpoint_id,
                    );
                    let fallback_operation = format!("{}:placeholders", request.operation_id);
                    let Ok(fallback) = DurableCompactionIdentity::new(&fallback_operation, current)
                    else {
                        return Ok(failed);
                    };
                    return match Box::pin(self.replace_protected_tool_results(
                        active,
                        origin,
                        live.runtime(),
                        fallback,
                        &fallback_operation,
                        current,
                        source_projection.identity(),
                        source_harness_plan,
                        before,
                        occupancy_before,
                        native_protected_history,
                    ))
                    .await
                    {
                        Ok(placeholders @ AgentCompactSessionOutcome::Compacted { .. }) => {
                            if let AgentCompactSessionOutcome::Failed { diagnostic, .. } = &failed {
                                committed_detail.diagnostic.clone_from(diagnostic);
                            }
                            Ok(placeholders)
                        }
                        Ok(AgentCompactSessionOutcome::NoEligibleRange) => {
                            committed_detail.exhausted = true;
                            Ok(failed)
                        }
                        _ => Ok(failed),
                    };
                }
            };
            let Ok(completed) = compaction_operation_component(&response) else {
                // Paid compact already completed. Encoding or byte-budget
                // failure retains its unresolved fence; it never permits replay.
                return Ok(AgentCompactSessionOutcome::RecoveryRequired {
                    checkpoint: self.authority.checkpoint().id(),
                    diagnostic: None,
                });
            };
            if self
                .transition_compaction_operations(
                    identity,
                    compaction_operation,
                    provider_operation,
                    identity.completed_checkpoint,
                    DurableOperationState::Completed(completed),
                )
                .await
                .is_err()
            {
                return Ok(AgentCompactSessionOutcome::RecoveryRequired {
                    checkpoint: self.authority.checkpoint().id(),
                    diagnostic: None,
                });
            }
            return match self
                .publish_compaction_success(
                    identity,
                    compaction_operation,
                    provider_operation,
                    live.record(),
                    live.prepared_record(),
                    &response,
                    Arc::clone(&live.runtime().model),
                    &live.runtime().provider_configuration,
                    live.runtime().clock.as_ref(),
                    live.runtime().model_operation_timeout,
                    live.origin(),
                    live.turn_id(),
                )
                .await
            {
                Ok((target, occupancy_after)) => {
                    if matches!(
                        origin,
                        CompactOrigin::TurnFence {
                            pressure_forced: true
                        }
                    ) {
                        self.pressure_folds_committed += 1;
                    }
                    if origin.is_turn_fence() {
                        self.turn_budgets.pressure_fold_spent =
                            self.pressure_folds_committed >= COMMITTED_PRESSURE_FOLD_CAP;
                    }
                    Ok(AgentCompactSessionOutcome::Compacted {
                        workflow_id: identity.workflow_id,
                        source: live.source().checkpoint_id,
                        target,
                        summary: committed_compaction_summary(&self.authority).map(|summary| {
                            summary.with_occupancy(occupancy_before, occupancy_after)
                        }),
                        feed_seq: self.authority.feed_seq_at(target),
                    })
                }
                Err(
                    CompactionCompletionError::RecoveryRequired(error)
                    | CompactionCompletionError::Semantic(error),
                ) => {
                    if origin.is_turn_fence() {
                        self.turn_budgets.paid_fold_spent = true;
                    }
                    let failed = self
                        .terminalize_compaction_failure(
                            identity,
                            compaction_operation,
                            provider_operation,
                            live.source(),
                            live.record(),
                            &error,
                            live.origin(),
                            live.turn_id(),
                            response.usage(),
                            None,
                        )
                        .await;
                    if !origin.is_turn_fence()
                        || !matches!(failed, AgentCompactSessionOutcome::Failed { .. })
                    {
                        return Ok(failed);
                    }
                    let head = self.authority.checkpoint();
                    let topology = head.manifest().topology();
                    let current = DurableSessionAddress::new(
                        topology.session_id,
                        topology.branch_id,
                        topology.checkpoint_id,
                    );
                    let fallback_operation = format!("{}:placeholders", request.operation_id);
                    let Ok(fallback) = DurableCompactionIdentity::new(&fallback_operation, current)
                    else {
                        return Ok(failed);
                    };
                    match Box::pin(self.replace_protected_tool_results(
                        active,
                        origin,
                        live.runtime(),
                        fallback,
                        &fallback_operation,
                        current,
                        source_projection.identity(),
                        source_harness_plan,
                        before,
                        occupancy_before,
                        native_protected_history,
                    ))
                    .await
                    {
                        Ok(placeholders @ AgentCompactSessionOutcome::Compacted { .. }) => {
                            if let AgentCompactSessionOutcome::Failed { diagnostic, .. } = &failed {
                                committed_detail.diagnostic.clone_from(diagnostic);
                            }
                            Ok(placeholders)
                        }
                        Ok(AgentCompactSessionOutcome::NoEligibleRange) => {
                            committed_detail.exhausted = true;
                            Ok(failed)
                        }
                        _ => Ok(failed),
                    }
                }
            };
        }
    }

    /// Whether the cut's retained tail alone is definitely over the window.
    ///
    /// Measured like a successor (fresh, without the live request anchor). A
    /// measurement that cannot be made answers `false`, leaving the paid fold
    /// and its own successor check to decide.
    async fn retained_tail_cannot_fit(
        &self,
        runtime: &ExactCompactionRuntime,
        stable_prefix: &plakous_model::StableRequestPrefix,
        cut: &SemanticCompactionCut,
        pending: Option<plakous_model::HistorySegment>,
        identity: DurableCompactionIdentity,
        affinity: ModelCacheAffinity,
    ) -> bool {
        let mut segments = cut.retained_segments().to_vec();
        segments.extend(pending);
        let Ok(tail) = plakous_model::CanonicalHistory::new(segments) else {
            return false;
        };
        let Ok(attempt) = compaction_attempt_id(identity.workflow_id, b"retained-tail-fit") else {
            return false;
        };
        let Ok(request) = ModelRequest::from_parts(
            stable_prefix.clone(),
            plakous_model::ConversationView::from_shared(
                tail,
                plakous_model::WorkingProjection::default(),
            ),
            RequestMetadata::for_session_workflow(
                self.run_id,
                identity.workflow_id,
                attempt,
                runtime.clock.now(),
            ),
        ) else {
            return false;
        };
        self.measure_compaction_projection(
            runtime,
            request,
            CompactionFitBasis::OfflineSuccessor,
            Some(affinity),
            runtime.clock.now(),
        )
        .await
        .is_ok_and(|(status, _)| !matches!(status.kind(), ContextStatusKind::Fits { .. }))
    }

    pub(super) async fn measure_compaction_projection(
        &self,
        runtime: &ExactCompactionRuntime,
        request: ModelRequest,
        basis: CompactionFitBasis,
        explicit_affinity: Option<ModelCacheAffinity>,
        prepared_at: Timestamp,
    ) -> Result<(ContextStatus, ContextEvaluationAudit), AgentError> {
        self.measure_compaction_projection_with_wire_digest(
            runtime,
            request,
            basis,
            explicit_affinity,
            prepared_at,
        )
        .await
        .map(|(status, audit, _)| (status, audit))
    }

    pub(super) async fn measure_compaction_projection_with_wire_digest(
        &self,
        runtime: &ExactCompactionRuntime,
        request: ModelRequest,
        basis: CompactionFitBasis,
        explicit_affinity: Option<ModelCacheAffinity>,
        prepared_at: Timestamp,
    ) -> Result<(ContextStatus, ContextEvaluationAudit, Option<Digest>), AgentError> {
        let descriptor = runtime
            .provider_configuration
            .effective()
            .descriptor()
            .clone();
        let plan = descriptor
            .validate_request(request, ValidationMode::Strict)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        descriptor
            .preflight_context_admission_requirement(plan.request().context_admission_requirement())
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        let authority = Arc::clone(&runtime.model)
            .revalidate_runtime_authority(prepared_at)
            .await
            .map_err(ModelPreparationError::from)
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        validate_configuration_runtime_authority(
            &runtime.provider_configuration,
            &descriptor,
            &authority,
            prepared_at,
        )
        .map_err(ModelError::from)
        .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        let mut context = self.with_completed_history_prefix(
            &plan,
            ModelPreparationContext::new(authority, prepared_at).with_request_anchor(
                if basis == CompactionFitBasis::OfflineSuccessor {
                    None
                } else {
                    self.request_anchor.clone()
                },
            ),
        );
        if basis == CompactionFitBasis::ActiveHead {
            context = self.with_previous_response_metadata(&plan, context)?;
            let (artifact, affinity) = self.active_model_preparation_state()?;
            context = context.with_cache_affinity(affinity);
            if let Some(artifact) = artifact {
                context = context.with_active_family_artifact(artifact);
            }
        } else {
            if basis == CompactionFitBasis::PaidFoldInput {
                context = context.with_required_input_bound();
            }
            if let Some(affinity) = explicit_affinity {
                context = context.with_cache_affinity(affinity);
            }
        }
        let mut wire_digest = None;
        let status = match Arc::clone(&runtime.model).prepare_call(plan.clone(), context) {
            Ok(prepared) => {
                let record = prepared.record();
                wire_digest = Some(record.projection().wire_digest());
                descriptor
                    .context_status(&plan, record.projection(), prepared_at)
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Model,
                            "prepared context status could not be reconstructed",
                        )
                    })?
                    .with_input_parts(record.projection().token_projection().input_parts())
            }
            Err(error) => error.context_status().cloned().ok_or_else(|| {
                self.project_model_error(error.into(), ModelErrorPhase::Preparation)
            })?,
        };
        let audit = context_status_audit(&status)?;
        Ok((status, audit, wire_digest))
    }

    async fn prepare_compaction_operations(
        &mut self,
        identity: DurableCompactionIdentity,
        request_record: &DurableCompactionRequestRecord,
        prepared_record: &PreparedCallRecord,
    ) -> Result<(OperationId, OperationId), AgentError> {
        let compaction_operation = self.take_operation_id()?;
        let provider_operation = self.take_operation_id()?;
        let compaction_request = compaction_operation_component(request_record)?;
        let provider_request = compaction_operation_component(prepared_record)?;
        let (history, mut ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        for record in [
            DurableOperationRecord::new(
                compaction_operation.0,
                DurableOperationKind::Compaction {
                    workflow_id: identity.workflow_id,
                },
                compaction_request,
                vec![DurableOperationTransition::new(
                    identity.prepared_checkpoint,
                    DurableOperationState::Prepared,
                )],
            ),
            DurableOperationRecord::new(
                provider_operation.0,
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(identity.workflow_id),
                    attempt_id: identity.attempt_id,
                },
                provider_request,
                vec![DurableOperationTransition::new(
                    identity.prepared_checkpoint,
                    DurableOperationState::Prepared,
                )],
            ),
        ] {
            ledger = ledger
                .append_operation(record.map_err(runtime_manifest_error)?)
                .map_err(runtime_manifest_error)?;
        }
        let lifecycle = working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?;
        self.publish_checkpoint(
            StoreAdmission::Ordinary,
            identity.prepared_checkpoint,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok((compaction_operation, provider_operation))
    }

    async fn persist_compaction_locator(
        &mut self,
        identity: DurableCompactionIdentity,
        provider_operation: OperationId,
        checkpoint_id: CheckpointId,
        locator: &ProviderCallRecoveryLocator,
        expected_stage: ProviderCallRecoveryLocatorStage,
    ) -> Result<(), AgentError> {
        if locator.availability_stage() != expected_stage
            || locator.operation_root() != ModelOperationRoot::SessionWorkflow(identity.workflow_id)
        {
            return Err(AgentError::new(
                AgentErrorKind::Model,
                "provider recovery locator belongs to another compaction stage or root",
            ));
        }
        let (admission, history, ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let provider = manifest
                .ledger()
                .operation(provider_operation.0)
                .ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "compaction provider operation disappeared",
                    )
                })?;
            let prepared: PreparedCallRecord = serde_json::from_slice(provider.request().bytes())
                .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "prepared compaction provider call is not reconstructive",
                )
            })?;
            prepared
                .recovery_request_with_locator(locator.clone())
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Model,
                        "provider recovery locator does not match the prepared compaction call",
                    )
                })?;
            let evidence = runtime_component(
                RuntimeComponentKind::Operation,
                serde_json::to_vec(locator).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "provider recovery locator could not be encoded",
                    )
                })?,
            )?;
            let target = DurableOperationState::LocatorBound(evidence);
            (
                transition_admission(provider.state(), &target),
                manifest.canonical().history().clone(),
                manifest
                    .ledger()
                    .transition_operation(
                        provider_operation.0,
                        DurableOperationTransition::new(checkpoint_id, target),
                    )
                    .map_err(runtime_manifest_error)?,
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        let lifecycle = working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(())
    }

    async fn transition_compaction_operations(
        &mut self,
        identity: DurableCompactionIdentity,
        compaction_operation: OperationId,
        provider_operation: OperationId,
        checkpoint_id: CheckpointId,
        state: DurableOperationState,
    ) -> Result<(), AgentError> {
        let mut admission = StoreAdmission::Ordinary;
        for operation in [compaction_operation, provider_operation] {
            if transition_admission(&self.operation_state(operation)?, &state)
                != StoreAdmission::Ordinary
            {
                admission = StoreAdmission::Settlement;
            }
        }
        let (history, mut ledger, context, family) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.canonical().history().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
                manifest.family_artifact().cloned(),
            )
        };
        for operation in [compaction_operation, provider_operation] {
            ledger = ledger
                .transition_operation(
                    operation.0,
                    DurableOperationTransition::new(checkpoint_id, state.clone()),
                )
                .map_err(runtime_manifest_error)?;
        }
        let lifecycle = working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?;
        self.publish_checkpoint(
            admission,
            checkpoint_id,
            lifecycle,
            history,
            ledger,
            context,
            family,
        )
        .await?;
        Ok(())
    }

    async fn run_compaction_call(
        &mut self,
        live: &mut LiveCompaction,
    ) -> Result<ModelResponse, AgentError> {
        let (deadline_at, context) = live.begin_call()?;
        let identity = live.identity();
        let (compaction_operation, provider_operation) = live.operations();
        let cancellation = live.cancellation().clone();
        let prepared_record = live.prepared_record().clone();
        let plan = live.plan().clone();
        let model = Arc::clone(&live.runtime().model);
        let provider_configuration = Arc::clone(&live.runtime().provider_configuration);
        let descriptor = provider_configuration.effective().descriptor().clone();
        let start = {
            let provider_dispatch_gate = Arc::clone(&self.provider_dispatch_gate);
            let _dispatch_guard = provider_dispatch_gate.lock().await;
            if self.shutdown.is_cancelled() || cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if tokio::time::Instant::now() >= deadline_at {
                return Err(self.model_timeout_error());
            }
            self.authority.check_heartbeat()?;
            let dispatch_checked_at = self.clock.now();
            let dispatch_authority = Arc::clone(&model)
                .revalidate_runtime_authority(dispatch_checked_at)
                .await
                .map_err(ModelPreparationError::from)
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            validate_configuration_runtime_authority(
                &provider_configuration,
                &descriptor,
                &dispatch_authority,
                dispatch_checked_at,
            )
            .map_err(ModelError::from)
            .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            prepared_record
                .revalidate(&descriptor, &plan, dispatch_checked_at)
                .and_then(|()| {
                    prepared_record.revalidate_selected_authority(
                        &dispatch_authority,
                        &descriptor,
                        dispatch_checked_at,
                    )
                })
                .map_err(ModelError::from)
                .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
            self.transition_compaction_operations(
                identity,
                compaction_operation,
                provider_operation,
                identity.dispatched_checkpoint,
                DurableOperationState::Dispatched,
            )
            .await?;
            let _lease_guard = self.start_authority().admit().await?;
            if self.shutdown.is_cancelled() || cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            if tokio::time::Instant::now() >= deadline_at {
                return Err(self.model_timeout_error());
            }
            let mut start = live.take_prepared()?.start_with_control(context);
            let mut task_context = std::task::Context::from_waker(std::task::Waker::noop());
            match start.as_mut().poll(&mut task_context) {
                std::task::Poll::Pending => ModelStartAdmission::Pending(start),
                std::task::Poll::Ready(result) => ModelStartAdmission::Ready(result),
            }
        };
        let mut timeout = Box::pin(tokio::time::sleep_until(deadline_at));
        let mut stream = match start {
            ModelStartAdmission::Ready(result) => result.map_err(compaction_provider_error)?,
            ModelStartAdmission::Pending(mut start) => loop {
                tokio::select! {
                    () = self.authority.lease_lost() => {
                        cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(ambiguous_compaction_error("branch lease was lost after compaction dispatch"));
                    }
                    () = cancellation.cancelled() => {
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the turn was cancelled; it was not applied and may have been billed"));
                    }
                    () = self.shutdown.cancelled() => {
                        cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(ambiguous_compaction_error("runtime stopped after compaction dispatch"));
                    }
                    () = &mut timeout => {
                        cancellation.cancel();
                        let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                        return Err(self.abandoned_compaction_timeout());
                    }
                    control = self.control_receiver.recv() => {
                        let Some(control) = control else {
                            cancellation.cancel();
                            let _ = join_cancelled_control_start(start, self.config.model_cleanup_timeout).await;
                            return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the runtime control closed; it was not applied and may have been billed"));
                        };
                        Box::pin(self.handle_control(live.active(), control)).await;
                    }
                    result = &mut start => break result.map_err(compaction_provider_error)?,
                }
            },
        };
        loop {
            let next = tokio::select! {
                () = self.authority.lease_lost() => {
                    cancellation.cancel();
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(ambiguous_compaction_error("branch lease was lost after compaction dispatch"));
                }
                () = cancellation.cancelled() => {
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the turn was cancelled; it was not applied and may have been billed"));
                }
                () = self.shutdown.cancelled() => {
                    cancellation.cancel();
                    let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                    return Err(ambiguous_compaction_error("runtime stopped after compaction dispatch"));
                }
                control = self.control_receiver.recv() => {
                    let Some(control) = control else {
                        cancellation.cancel();
                        let _ = join_cancelled_control_stream(&mut stream, self.config.model_cleanup_timeout).await;
                        return Err(abandoned_compaction_error(AgentErrorKind::Cancelled, "compaction was abandoned after dispatch because the runtime control closed; it was not applied and may have been billed"));
                    };
                    Box::pin(self.handle_control(live.active(), control)).await;
                    continue;
                }
                item = stream.next() => item,
            };
            let Some(item) = next else { break };
            match item.map_err(compaction_provider_error)? {
                ModelStreamItem::RecoveryLocatorBound(locator) => {
                    let locator = match live.validate_locator(locator) {
                        Ok(locator) => locator,
                        Err(error) => {
                            cancellation.cancel();
                            let _ = join_cancelled_control_stream(
                                &mut stream,
                                self.config.model_cleanup_timeout,
                            )
                            .await;
                            return Err(error);
                        }
                    };
                    if self
                        .persist_compaction_locator(
                            identity,
                            provider_operation,
                            identity.learned_locator_checkpoint,
                            locator.locator(),
                            ProviderCallRecoveryLocatorStage::LearnedAfterDispatch,
                        )
                        .await
                        .is_err()
                    {
                        cancellation.cancel();
                        let _ = join_cancelled_control_stream(
                            &mut stream,
                            self.config.model_cleanup_timeout,
                        )
                        .await;
                        return Err(ambiguous_compaction_error(
                            "learned compaction recovery locator was not durably bound",
                        ));
                    }
                    live.locator_published(locator);
                }
                ModelStreamItem::Event(event) => {
                    if let ModelEvent::PromptCacheDiagnosis(diagnosis) = &event {
                        self.emit_telemetry(
                            self.telemetry_context_for_turn(live.turn_id())
                                .with_attempt(live.identity().attempt_id),
                            TelemetryObservationKind::PromptCacheDiagnosis {
                                reason: diagnosis.reason,
                                missed_input_tokens: diagnosis.missed_input_tokens,
                            },
                        );
                    }
                    live.observe_event(event).map_err(|error| match error {
                        CompactionStreamError::ContradictoryTerminal => ambiguous_compaction_error(
                            "compaction provider contradicted an observed terminal response",
                        ),
                        CompactionStreamError::ForeignPlan => self.model_failure(
                            ModelErrorKind::InvalidEvent,
                            "compaction response belongs to another prepared plan",
                            ModelErrorPhase::Terminal,
                        ),
                        CompactionStreamError::InvalidTerminal(error) => {
                            self.project_model_error(error, ModelErrorPhase::Terminal)
                        }
                    })?;
                }
            }
        }
        live.finish_stream()
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "failure publication keeps origin and turn root beside the exact compact tuple"
    )]
    async fn terminalize_compaction_failure(
        &mut self,
        identity: DurableCompactionIdentity,
        compaction_operation: OperationId,
        provider_operation: OperationId,
        source: DurableSessionAddress,
        record: &DurableCompactionRequestRecord,
        error: &AgentError,
        origin: CompactOrigin,
        turn_id: Option<TurnId>,
        completed_usage: Option<Usage>,
        expected_dispatch: Option<(Digest, plakous_model::AttemptId)>,
    ) -> AgentCompactSessionOutcome {
        let Ok(evidence) = operation_result_component::<()>(&Err(error.clone())) else {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        };
        if error.kind() == AgentErrorKind::OutcomeUnknown {
            let result = self
                .transition_compaction_operations(
                    identity,
                    compaction_operation,
                    provider_operation,
                    identity.failure_evidence_checkpoint,
                    DurableOperationState::OutcomeUnknown(evidence),
                )
                .await;
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: result.map_or_else(
                    |_| self.authority.checkpoint().id(),
                    |()| identity.failure_evidence_checkpoint,
                ),
                diagnostic: Some(compact_diagnostic(error)),
            };
        }
        let states = {
            let authority = self.authority.checkpoint();
            [compaction_operation, provider_operation].map(|operation| {
                authority
                    .manifest()
                    .ledger()
                    .operation(operation.0)
                    .map(|record| record.state().clone())
            })
        };
        if states.iter().any(Option::is_none) {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        }
        let proven_pre_dispatch = matches!(
            &states,
            [
                Some(DurableOperationState::Prepared),
                Some(DurableOperationState::Prepared | DurableOperationState::LocatorBound(_)),
            ]
        );
        if proven_pre_dispatch {
            let (history, ledger, context, family) = {
                let authority = self.authority.checkpoint();
                let manifest = authority.manifest();
                let ledger = manifest
                    .ledger()
                    .transition_operation(
                        compaction_operation.0,
                        DurableOperationTransition::new(
                            identity.failure_evidence_checkpoint,
                            DurableOperationState::ProvenNotDispatched(evidence.clone()),
                        ),
                    )
                    .and_then(|ledger| {
                        ledger.transition_operation(
                            provider_operation.0,
                            DurableOperationTransition::new(
                                identity.failure_evidence_checkpoint,
                                DurableOperationState::ProvenNotDispatched(evidence.clone()),
                            ),
                        )
                    });
                let Ok(ledger) = ledger else {
                    return AgentCompactSessionOutcome::RecoveryRequired {
                        checkpoint: authority.id(),
                        diagnostic: None,
                    };
                };
                (
                    manifest.canonical().history().clone(),
                    ledger,
                    manifest.canonical().context().retained_for_exact_resume(),
                    manifest.family_artifact().cloned(),
                )
            };
            let Ok(lifecycle) = working_lifecycle_for_root(
                ModelOperationRoot::SessionWorkflow(identity.workflow_id),
                &ledger,
            ) else {
                return AgentCompactSessionOutcome::RecoveryRequired {
                    checkpoint: self.authority.checkpoint().id(),
                    diagnostic: None,
                };
            };
            if self
                .publish_checkpoint(
                    StoreAdmission::Settlement,
                    identity.failure_evidence_checkpoint,
                    lifecycle,
                    history,
                    ledger,
                    context,
                    family,
                )
                .await
                .is_err()
            {
                return AgentCompactSessionOutcome::RecoveryRequired {
                    checkpoint: self.authority.checkpoint().id(),
                    diagnostic: None,
                };
            }
        } else if states.iter().any(|state| {
            !matches!(
                state,
                Some(
                    DurableOperationState::Completed(_)
                        | DurableOperationState::ProvenNotDispatched(_),
                )
            )
        }) && self
            .transition_compaction_operations(
                identity,
                compaction_operation,
                provider_operation,
                identity.failure_evidence_checkpoint,
                // A start that local admission or final revalidation refused
                // carries exact proof that nothing was sent: unpaid, not
                // merely definite.
                if expected_dispatch.is_some_and(|(prepared_call, attempt_id)| {
                    error
                        .model_dispatch_rejection()
                        .is_some_and(|proof| proof.matches(prepared_call, attempt_id))
                }) {
                    DurableOperationState::ProvenNotDispatched(evidence.clone())
                } else {
                    DurableOperationState::Completed(evidence.clone())
                },
            )
            .await
            .is_err()
        {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        }
        let publication = (|| -> Result<_, AgentError> {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            let topology = manifest.topology();
            if (topology.session_id, topology.branch_id) != (source.session_id, source.branch_id)
                || manifest.canonical().history().identity() != record.source_history
            {
                return Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "compaction failure publication lost its exact semantic source",
                ));
            }
            let mut ledger = manifest.ledger().clone();
            for operation in [compaction_operation, provider_operation] {
                let terminal = if operation == compaction_operation {
                    evidence.clone()
                } else {
                    ledger
                        .operation(operation.0)
                        .and_then(|record| match record.state() {
                            DurableOperationState::Completed(value)
                            | DurableOperationState::ProvenNotDispatched(value) => {
                                Some(value.clone())
                            }
                            _ => None,
                        })
                        .ok_or_else(|| {
                            AgentError::new(
                                AgentErrorKind::OutcomeUnknown,
                                "compaction provider failure lacks definite terminal evidence",
                            )
                        })?
                };
                ledger = ledger
                    .transition_operation(
                        operation.0,
                        DurableOperationTransition::new(
                            identity.failure_checkpoint,
                            DurableOperationState::Finalized(terminal),
                        ),
                    )
                    .map_err(runtime_manifest_error)?;
            }
            Ok((
                manifest.canonical().history().clone(),
                ledger,
                manifest.canonical().context().retained_for_exact_resume(),
            ))
        })();
        let Ok((history, ledger, context)) = publication else {
            return AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            };
        };
        let family = self
            .authority
            .checkpoint()
            .manifest()
            .family_artifact()
            .cloned();
        let failure_lifecycle = match (origin, turn_id) {
            (CompactOrigin::TurnFence { .. }, Some(turn_id)) => {
                match working_lifecycle(turn_id, &ledger) {
                    Ok(lifecycle) => lifecycle,
                    Err(_) => {
                        return AgentCompactSessionOutcome::RecoveryRequired {
                            checkpoint: self.authority.checkpoint().id(),
                            diagnostic: None,
                        };
                    }
                }
            }
            _ => RuntimeLifecycle::Quiescent,
        };
        match self
            .publish_checkpoint(
                StoreAdmission::Settlement,
                identity.failure_checkpoint,
                failure_lifecycle,
                history,
                ledger,
                context,
                family,
            )
            .await
        {
            Ok(_) => AgentCompactSessionOutcome::Failed {
                checkpoint: identity.failure_checkpoint,
                diagnostic: Some(compact_diagnostic(error)),
                usage: completed_usage,
            },
            Err(_) => AgentCompactSessionOutcome::RecoveryRequired {
                checkpoint: self.authority.checkpoint().id(),
                diagnostic: None,
            },
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the exact compaction publication fence carries its complete persisted tuple"
    )]
    async fn publish_compaction_success(
        &mut self,
        identity: DurableCompactionIdentity,
        compaction_operation: OperationId,
        provider_operation: OperationId,
        record: &DurableCompactionRequestRecord,
        prepared: &PreparedCallRecord,
        response: &ModelResponse,
        model: Arc<dyn LanguageModel>,
        current_configuration: &CompiledProviderConfiguration,
        clock: &dyn Clock,
        authority_timeout: Duration,
        _origin: CompactOrigin,
        turn_id: Option<TurnId>,
    ) -> Result<(CheckpointId, Option<u64>), CompactionCompletionError> {
        let checkpoint = self.authority.checkpoint();
        let source_history = checkpoint.manifest().canonical().history().clone();
        let mut fold = build_compaction_success_fold(
            checkpoint.manifest(),
            identity,
            compaction_operation,
            provider_operation,
            record,
            prepared,
            response,
            model,
            current_configuration,
            clock,
            authority_timeout,
        )
        .await?;
        if let Some(note) = self
            .post_compaction_note
            .as_ref()
            .and_then(|derive| derive(&source_history, &fold.history))
        {
            let item = ConversationItem::new(
                self.entry_id()
                    .map_err(CompactionCompletionError::semantic)?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(note, None).map_err(
                    |_| {
                        CompactionCompletionError::semantic(AgentError::new(
                            AgentErrorKind::Internal,
                            "compaction note is invalid",
                        ))
                    },
                )?)],
                Provenance::Instruction,
            )
            .map_err(|_| {
                CompactionCompletionError::semantic(AgentError::new(
                    AgentErrorKind::Internal,
                    "compaction note is invalid",
                ))
            })?;
            let segment =
                plakous_model::HistorySegment::new(HistorySegmentKind::Steering, vec![item])
                    .map_err(|_| {
                        CompactionCompletionError::semantic(AgentError::new(
                            AgentErrorKind::Internal,
                            "compaction note is invalid",
                        ))
                    })?;
            fold.history = fold.history.append(segment).map_err(|_| {
                CompactionCompletionError::semantic(AgentError::new(
                    AgentErrorKind::Internal,
                    "compaction note could not be appended",
                ))
            })?;
        }
        // A turn-fence fold is still inside its admitted root even when every
        // operation visible in the folded ledger is already finalized.  Do
        // not infer quiescence from that ledger: the post-fold steering note
        // and resent round still belong to this turn.
        let lifecycle = match turn_id {
            Some(turn_id) => working_lifecycle(turn_id, &fold.ledger),
            None => compaction_success_lifecycle(&fold.ledger),
        }
        .map_err(CompactionCompletionError::semantic)?;
        // Semantic and native successors replace the measured request prefix.
        self.context_anchor = None;
        self.request_anchor = None;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            identity.success_checkpoint,
            lifecycle,
            fold.history,
            fold.ledger,
            fold.context,
            None,
        )
        .await
        .map_err(CompactionCompletionError::recovery)?;
        Ok((identity.success_checkpoint, fold.occupancy_tokens_after))
    }
}

/// A redacted operator reason for an endpoint refusal.
///
/// The typed error carries checkpoint identities on its stale variant; those
/// are useless to an operator and are not put in a session log, so each
/// variant maps to a fixed phrase instead of a `Display` impl.
fn compaction_endpoint_refusal(error: &AgentSessionEndpointError) -> &str {
    match error {
        AgentSessionEndpointError::Backpressure => "the actor ingress was full",
        AgentSessionEndpointError::Busy => "the branch had admitted work in flight",
        AgentSessionEndpointError::Stale { .. } => "the source no longer named this checkpoint",
        AgentSessionEndpointError::Unavailable => {
            "model preparation produced no inspectable status"
        }
        AgentSessionEndpointError::OutcomeUnknown => "the compaction outcome is unknown",
        AgentSessionEndpointError::InvalidInput => "the request failed canonical validation",
        AgentSessionEndpointError::Rejected { reason } => reason,
    }
}

fn compaction_refusal_reason(error: &AgentSessionEndpointError) -> crate::CompactionRefusalReason {
    use crate::CompactionRefusalReason as Reason;
    match error {
        AgentSessionEndpointError::Backpressure => Reason::Backpressure,
        AgentSessionEndpointError::Busy => Reason::Busy,
        AgentSessionEndpointError::Stale { .. } => Reason::Stale,
        AgentSessionEndpointError::Unavailable | AgentSessionEndpointError::OutcomeUnknown => {
            Reason::Unavailable
        }
        AgentSessionEndpointError::InvalidInput => Reason::InvalidInput,
        AgentSessionEndpointError::Rejected { .. } => Reason::Rejected,
    }
}
