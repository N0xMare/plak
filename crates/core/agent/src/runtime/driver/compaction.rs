//! Driver-owned compaction dispatch, controls and durable publication.

use crate::runtime::PromptCommand;
use crate::runtime::compaction::record::compaction_checkpoint_id;
use crate::runtime::compaction::request::{
    admitted_compaction_runtime, compact_runtime_endpoint_error,
};
use crate::runtime::compaction::{
    CompactOrigin, DurableCompactionIdentity, ExactCompactionRuntime,
};
use crate::runtime::driver::Driver;
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::observation::{
    compaction_event_reason, compaction_session_event_outcome, compaction_telemetry_tokens,
};
use crate::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentError, AgentErrorKind,
    AgentSessionEndpointError, CompactionEventOutcome, CompactionMeasurement,
    ContextEvaluationAudit, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    RuntimeCheckpoint, RuntimeLifecycle, TelemetryObservationKind,
};
use plakous_model::{
    ContextStatus, ContextStatusKind, ConversationView, Digest, ModelCacheAffinity, ModelError,
    ModelOperationRoot, ModelRequest, PreparedModelCall, ValidatedRequestPlan,
};
use std::sync::Arc;

mod dispatch;
mod failure;
mod fences;
mod measure;
mod prepare;
mod publish;
mod settle;

/// The remedy a refusal names when only manual compaction can make room.
pub(in crate::runtime::driver) const MANUAL_COMPACTION_REMEDY: &str =
    "compact the session, or enable scheduled compaction";
const PAID_FOLD_BACKOFF_ROUNDS: usize = 2;
const FAILED_PAID_FOLD_CAP: usize = 3;
/// Committed pressure-forced paid folds one root turn may pay for. Recurring
/// oversized output re-arms pressure after every commit; past this cap a
/// pressure fence uses only the free placeholder rung for the rest of the turn.
const COMMITTED_PRESSURE_FOLD_CAP: usize = 3;

#[derive(Default)]
struct CompactionDetail {
    diagnostic: Option<String>,
    no_eligible_detail: Option<&'static str>,
    exhausted: bool,
    rung: Option<&'static str>,
    /// Idle gap before the paid call, when one dispatched.
    idle_gap: Option<std::time::Duration>,
}

/// The fixed inputs of one compaction attempt, shared by its phases.
struct CompactionAttempt<'a> {
    active: Option<&'a PromptCommand>,
    request: &'a AgentCompactSessionRequest,
    origin: CompactOrigin,
    source: DurableSessionAddress,
    identity: DurableCompactionIdentity,
}

/// The head one pass of the compaction ladder measured as its source.
struct MeasuredSource {
    checkpoint: Arc<RuntimeCheckpoint>,
    cache_affinity: ModelCacheAffinity,
    projection: ConversationView,
    harness_plan: Digest,
    before: ContextEvaluationAudit,
    occupancy_before: Option<u64>,
    fits: bool,
    native: bool,
    scheduled_pending_qualified: bool,
}

/// The paid request one pass selected, before provider preparation.
struct CompactionRequestPlan {
    request: ModelRequest,
    cut_identity: Digest,
    record_version: u32,
}

/// The selected request with its sole prepared dispatch capability.
struct PreparedCompaction {
    request: ModelRequest,
    cut_identity: Digest,
    record_version: u32,
    validated: ValidatedRequestPlan,
    prepared: PreparedModelCall,
    native_protected_history: bool,
}

/// How one compaction phase ended.
enum CompactionPhase<T> {
    Next(T),
    /// The native facet cannot hold this source; rerun the pass on the sealed
    /// harness cut, keeping the workflow's sole dispatch attempt.
    RetryWithoutNative,
    Done(Result<AgentCompactSessionOutcome, AgentSessionEndpointError>),
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

/// A finalized compaction provider record that failed after dispatch, so it
/// was billed and did not commit.
fn failed_paid_fold(workflow: crate::SessionWorkflowId, record: &DurableOperationRecord) -> bool {
    record.transitions().last().is_some_and(|finalization| {
        compaction_checkpoint_id(workflow, b"failed").ok() == Some(finalization.checkpoint_id())
    }) && record
        .transitions()
        .iter()
        .any(|transition| matches!(transition.state(), DurableOperationState::Dispatched))
        && !record.transitions().iter().any(|transition| {
            matches!(
                transition.state(),
                DurableOperationState::ProvenNotDispatched(_)
            )
        })
}

/// Whether `checkpoint`'s newest finalized provider call is a fold that failed
/// after dispatch. The same source would fail the same way, so neither a turn
/// fence nor the host pays for another fold before a model round runs or a fold
/// commits. A placeholder successor makes no provider call and leaves it set; a
/// fold a lost process abandoned does not set it.
pub(crate) fn paid_fold_failed_since_last_round(checkpoint: &RuntimeCheckpoint) -> bool {
    ledger_paid_fold_failed_since_last_round(checkpoint.manifest().ledger())
}

/// [`paid_fold_failed_since_last_round`] over a ledger. Quiescent retention
/// keeps the newest finalized provider record, so retention never changes it.
pub(crate) fn ledger_paid_fold_failed_since_last_round(
    ledger: &crate::RuntimeOperationLedger,
) -> bool {
    let operations = ledger.operations();
    for record in operations.iter().rev() {
        if !matches!(record.state(), DurableOperationState::Finalized(_)) {
            continue;
        }
        match record.kind() {
            DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(workflow),
                ..
            } => return failed_paid_fold(*workflow, record),
            DurableOperationKind::Provider { .. } => return false,
            _ => {}
        }
    }
    false
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
    NativeFoldInput,
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
    pub(super) async fn compact_session(
        &mut self,
        active: Option<&PromptCommand>,
        request: AgentCompactSessionRequest,
        origin: CompactOrigin,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        self.compact_session_with_detail(
            active,
            request,
            origin,
            &mut CompactionDetail::default(),
            None,
        )
        .await
    }

    async fn compact_session_with_detail(
        &mut self,
        active: Option<&PromptCommand>,
        request: AgentCompactSessionRequest,
        origin: CompactOrigin,
        committed_detail: &mut CompactionDetail,
        measurement: Option<CompactionMeasurement>,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        if self.compaction_in_flight {
            return Err(AgentSessionEndpointError::Busy);
        }
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
        if let Some((operation_id, source)) = &request.fallback_of {
            let identity = DurableCompactionIdentity::new(operation_id, *source)
                .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
            let original = crate::runtime::durable::compaction_fallback::failed_endpoint_request(
                &checkpoint,
                identity,
            )
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
            if request.operation_id != format!("{operation_id}:placeholders")
                || original.is_none_or(|original| original.operation_id != *operation_id)
            {
                return Err(AgentSessionEndpointError::InvalidInput);
            }
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
        self.compaction_in_flight = true;
        let outcome = Box::pin(self.compact_session_admitted(
            active,
            request,
            origin,
            committed_detail,
            measurement,
        ))
        .await;
        self.compaction_in_flight = false;
        outcome
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Admitted compaction checks quiescence and records the workflow before entering its paid execution path"
    )]
    async fn compact_session_admitted(
        &mut self,
        active: Option<&PromptCommand>,
        mut request: AgentCompactSessionRequest,
        origin: CompactOrigin,
        committed_detail: &mut CompactionDetail,
        measurement: Option<CompactionMeasurement>,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let checkpoint = self.authority.checkpoint();
        let source = DurableSessionAddress::new(
            request.session_id,
            request.branch_id,
            request.checkpoint_id,
        );
        let identity = DurableCompactionIdentity::new(&request.operation_id, source)
            .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
        let runtime = match (origin, request.runtime.take()) {
            (CompactOrigin::TurnFence { .. } | CompactOrigin::Endpoint, None)
                if origin.is_turn_fence() || request.fallback_of.is_some() =>
            {
                ExactCompactionRuntime {
                    model: Arc::clone(&self.model),
                    provider_configuration: Arc::clone(&self.provider_configuration),
                    clock: Arc::clone(&self.clock),
                    model_operation_timeout: self.config.model_operation_timeout,
                }
            }
            (CompactOrigin::Endpoint, Some(builder)) => {
                let (authority, checked_at) = self
                    .request_authority(Arc::clone(&builder.model), builder.clock.now())
                    .await
                    .map_err(|error| compact_runtime_endpoint_error(&error))?;
                crate::runtime::admission::validate_configuration_runtime_authority(
                    &builder.provider_configuration,
                    builder.provider_configuration.effective().descriptor(),
                    &authority,
                    checked_at,
                )
                .map_err(|error| {
                    compact_runtime_endpoint_error(&AgentError::new(
                        AgentErrorKind::InvalidInput,
                        error.to_string(),
                    ))
                })?;
                let current = self.authority.checkpoint();
                let topology = current.manifest().topology();
                if current.id() != checkpoint.id() {
                    return Err(AgentSessionEndpointError::Stale {
                        session_id: topology.session_id,
                        branch_id: topology.branch_id,
                        checkpoint_id: topology.checkpoint_id,
                    });
                }
                if self.active_root.is_some()
                    || !matches!(current.manifest().lifecycle(), RuntimeLifecycle::Quiescent)
                {
                    return Err(AgentSessionEndpointError::Busy);
                }
                admitted_compaction_runtime(builder, &current)
                    .map_err(|error| compact_runtime_endpoint_error(&error))?
            }
            _ => return Err(AgentSessionEndpointError::InvalidInput),
        };
        let compaction_reason = compaction_event_reason(request.reason);
        let compaction_turn_id = active.map(|command| command.turn_id);
        let compaction_context = self.telemetry_context_for_turn(compaction_turn_id);
        self.emit_telemetry(
            compaction_context,
            TelemetryObservationKind::CompactionStarted {
                reason: compaction_reason,
            },
        );
        let compaction_started = crate::executor::now();
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
        let compaction_elapsed = compaction_started.elapsed();
        if matches!(outcome, Ok(AgentCompactSessionOutcome::Compacted { .. })) {
            // A successor is a different request prefix: nothing the provider
            // counted before it measures it.
            self.nested_committed_observation_tokens = 0;
        }
        let (tokens_before, tokens_after, compaction_usage) =
            compaction_telemetry_tokens(&self.authority, &outcome);
        let measurement = CompactionMeasurement {
            idle_gap: committed_detail.idle_gap,
            ..measurement.unwrap_or_else(|| self.compaction_measurement(None))
        };
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
                measurement,
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
        let attempt = CompactionAttempt {
            active,
            request: &request,
            origin,
            source,
            identity,
        };
        // A native successor keeps every newest user message its token budget
        // admits, so only the sealed harness cut bounds the segment count.
        let mut native_allowed = !origin.segment_pressure_only();
        loop {
            let measured = self
                .measure_compaction_source(
                    &attempt,
                    &runtime,
                    native_allowed,
                    measured_source_tokens,
                    committed_detail,
                )
                .await?;
            if request.fallback_of.is_some()
                || (origin.is_turn_fence() && !self.paid_fold_admissible(origin.pressure_forced()))
            {
                return self
                    .compaction_source_placeholders(
                        &attempt,
                        &runtime,
                        &measured,
                        false,
                        committed_detail,
                    )
                    .await;
            }
            let plan = match self
                .select_compaction_request(&attempt, &runtime, &measured, committed_detail)
                .await?
            {
                CompactionPhase::Next(plan) => plan,
                CompactionPhase::RetryWithoutNative => {
                    native_allowed = false;
                    continue;
                }
                CompactionPhase::Done(outcome) => return outcome,
            };
            let prepared = match self
                .prepare_compaction_dispatch(&attempt, &runtime, &measured, plan, committed_detail)
                .await?
            {
                CompactionPhase::Next(prepared) => prepared,
                CompactionPhase::RetryWithoutNative => {
                    native_allowed = false;
                    continue;
                }
                CompactionPhase::Done(outcome) => return outcome,
            };
            return Box::pin(self.dispatch_compaction(
                &attempt,
                runtime,
                &measured,
                prepared,
                committed_detail,
            ))
            .await;
        }
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
