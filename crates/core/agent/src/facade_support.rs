//! Narrow implementation bridge for the high-level `plakous::sessions` facade.
//!
//! This module is not an embedder API. It lets the facade translate private
//! runtime authority into its smaller session workflow without adding public
//! accessors to [`crate::Agent`] or [`crate::AgentError`].

use std::sync::Arc;

use crate::internal::{Agent, AgentBuilder, AgentError, SessionStoreCapacity, SessionStoreError};
pub use crate::internal::{
    AgentCompactSessionOutcome, AgentCompactSessionRequest, AgentCompactionReason,
    AgentSessionEndpoint, AgentSessionEndpointError, CompactionRung, CompactionSummary,
    ModelRouteBinding, RuntimeCheckpoint, WeakAgentSessionEndpoint,
};
pub use crate::runtime::feed_views::{
    FeedOperationCall, checkpoint_completed_terminal_source, checkpoint_operation_call,
    checkpoint_retained_turn_terminal, checkpoint_turn_terminal,
};
use plakous_model::{
    Digest, EffectiveProviderConfigurationId, ModelId, ProtocolId, RouteId,
    SelectedProviderConfigurationId,
};

/// Advisory configured window and reserved input ceiling; no request admission.
#[must_use]
pub fn agent_builder_input_window(builder: &AgentBuilder) -> Option<(Option<u64>, u64)> {
    builder.session_input_window()
}

/// Projects reconstruction-safe target identity from an exact runtime builder.
#[must_use]
pub fn agent_builder_target_authority(
    builder: &AgentBuilder,
) -> (
    ProtocolId,
    RouteId,
    ModelId,
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
    Digest,
) {
    builder.session_target_authority()
}

/// Facade-relevant classification of an originating semantic-store refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStoreOrigin {
    /// The selected semantic authority is absent or archived.
    NotFound,
    /// The selected checkpoint was superseded history that retention has
    /// reclaimed.
    Pruned,
    /// An idempotency identity is occupied by different semantic bytes.
    IdentityConflict,
    /// Revision, lease, or fencing authority was refused.
    Ownership,
    /// A retention bound refused the write; deleting sessions frees it.
    Capacity(SessionStoreCapacity),
    /// Physical storage is exhausted, not a logical retention bound.
    StorageFull,
    /// Another connection held the backend, or its request queue was full.
    Busy,
    /// Backend availability or shutdown prevented the operation.
    Unavailable,
    /// The store rejected the caller's semantic input.
    InvalidInput,
    /// Persistent data uses a schema or codec unsupported by this release.
    UnsupportedPersistence,
    /// Stored semantics failed an integrity check.
    Integrity,
}

/// Returns an agent's full checkpoint only to the facade implementation.
#[must_use]
pub fn agent_checkpoint(agent: &Agent) -> Arc<RuntimeCheckpoint> {
    agent.checkpoint()
}

/// Reads the construction-bound process budgets from an exact checkpoint.
///
/// # Errors
/// Returns an incompatible-session error when the stored environment is invalid.
pub fn checkpoint_job_config(
    checkpoint: &RuntimeCheckpoint,
) -> Result<plakous_tools::JobConfig, AgentError> {
    crate::runtime::stored_job_config(checkpoint.manifest().tools().environment().bytes())
}

/// Reads the instruction sources an exact checkpoint was compiled from.
///
/// # Errors
/// Returns an incompatible-session error when the stored plan is unreadable.
pub fn checkpoint_instruction_sources(
    checkpoint: &RuntimeCheckpoint,
) -> Result<Vec<crate::ResolvedContextSourceFact>, AgentError> {
    checkpoint
        .manifest()
        .rehydrate_harness_plan()
        .map(|plan| plan.instructions().sources().to_vec())
        .map_err(|error| {
            AgentError::new(
                crate::AgentErrorKind::IncompatibleSession,
                format!("stored harness plan is unreadable: {error}"),
            )
        })
}

/// Exact evaluator reconstruction facts for facade implementation only.
pub struct StoredCompositionArtifact {
    /// Complete recorded profile digest identity.
    pub profile_identity: Vec<u8>,
    /// Complete canonical artifact digest identity.
    pub artifact_identity: Vec<u8>,
    /// Complete recorded attachment binding digest identity.
    pub binding_identity: Vec<u8>,
    /// Complete canonical engine contract digest identity.
    pub engine_contract_identity: Vec<u8>,
    /// Length-delimited integration host preimage, including product evidence.
    pub host: Vec<u8>,
}

/// Reads exact evaluator reconstruction facts, without exposing the checkpoint.
///
/// # Errors
/// Returns an incompatible-session error for an invalid stored environment.
pub fn checkpoint_composition_artifact(
    checkpoint: &RuntimeCheckpoint,
) -> Result<Option<StoredCompositionArtifact>, AgentError> {
    crate::runtime::stored_composition_artifact(checkpoint.manifest().tools().environment().bytes())
}

/// Returns the private-driver inspection endpoint only to facade composition.
#[must_use]
pub fn agent_session_endpoint(agent: &Agent) -> AgentSessionEndpoint {
    agent.session_endpoint()
}

/// Classifies an agent failure's semantic-store origin, when present.
#[must_use]
pub fn agent_store_origin(error: &AgentError) -> Option<SessionStoreOrigin> {
    error.store_error().map(session_store_origin)
}

/// Classifies a direct semantic-store refusal for facade-safe translation.
#[must_use]
pub const fn session_store_origin(error: SessionStoreError) -> SessionStoreOrigin {
    match error {
        SessionStoreError::NotFound | SessionStoreError::Archived => SessionStoreOrigin::NotFound,
        SessionStoreError::Pruned => SessionStoreOrigin::Pruned,
        SessionStoreError::IdentityConflict => SessionStoreOrigin::IdentityConflict,
        SessionStoreError::Conflict
        | SessionStoreError::LeaseHeld
        | SessionStoreError::LeaseExpired
        | SessionStoreError::LeaseDomainMismatch
        | SessionStoreError::FenceSuperseded => SessionStoreOrigin::Ownership,
        SessionStoreError::Capacity(capacity) => SessionStoreOrigin::Capacity(capacity),
        SessionStoreError::StorageFull => SessionStoreOrigin::StorageFull,
        SessionStoreError::BackendBusy => SessionStoreOrigin::Busy,
        SessionStoreError::BackendUnavailable | SessionStoreError::Shutdown => {
            SessionStoreOrigin::Unavailable
        }
        SessionStoreError::Invalid(_) => SessionStoreOrigin::InvalidInput,
        SessionStoreError::UnsupportedSchema | SessionStoreError::UnsupportedCodec => {
            SessionStoreOrigin::UnsupportedPersistence
        }
        SessionStoreError::Corrupt(_) => SessionStoreOrigin::Integrity,
    }
}

/// The observation a live `ToolStarted` event carries for `call`, read from
/// committed history: identical to the live one whenever the arguments fit
/// the history redaction window, and never redacting more than that window
/// of any one string.
#[must_use]
pub fn history_tool_call_observation(
    call: &plakous_model::ToolCallBlock,
) -> crate::ToolCallObservation {
    crate::runtime::history_tool_call_observation(call)
}

/// The observation a live `ToolCompleted` event carries for `result`, read
/// from committed history under the same window as
/// [`history_tool_call_observation`].
#[must_use]
pub fn history_tool_result_observation(
    result: &plakous_model::ToolResultBlock,
    subject: Option<&str>,
) -> crate::ToolResultObservation {
    crate::runtime::history_tool_result_observation_with_subject(result, subject)
}

/// Unix milliseconds of the newest operator prompt or steer in `agent`'s
/// committed canonical history, from its `UUIDv7` entry identity; `None` when
/// no such entry carries a time.
#[must_use]
pub fn last_operator_input_unix_ms(agent: &Agent) -> Option<u64> {
    crate::runtime::last_operator_input_unix_ms(agent.checkpoint().manifest().canonical().history())
}

/// Attaches to `agent`'s live view and returns the committed history at the
/// attachment's cursor with its checkpoint's topology and ledger, atomically.
///
/// # Errors
///
/// Returns the attachment's subscriber-admission or view-scope error.
pub fn attach_with_committed_history(
    agent: &Agent,
    request: crate::ViewRequest,
) -> Result<
    (
        crate::ViewAttachment,
        plakous_model::CanonicalHistory,
        crate::manifest::RuntimeTopology,
        crate::manifest::RuntimeOperationLedger,
    ),
    crate::ViewError,
> {
    agent.attach_with_committed_history(request)
}

/// Reads the explicit owning marker of a retained composition evaluation.
/// The ledger is immutable; lookup visits only its logarithmic index path.
#[must_use]
pub fn history_composition_owner(
    ledger: &crate::manifest::RuntimeOperationLedger,
    evaluation_operation_id: u64,
) -> Option<plakous_model::CallId> {
    match ledger.operation(evaluation_operation_id)?.kind() {
        crate::manifest::DurableOperationKind::CompositionEvaluation { call_id, .. } => {
            Some(*call_id)
        }
        _ => None,
    }
}

/// Reads marker and continuation bindings from at most the newest
/// `max_operations` ledger records. No request or result content is returned.
#[must_use]
pub fn history_composition_calls(
    ledger: &crate::manifest::RuntimeOperationLedger,
    max_operations: usize,
) -> Vec<(plakous_model::CallId, plakous_model::CallId)> {
    ledger
        .newest_operations(max_operations)
        .into_iter()
        .filter_map(|record| match record.kind() {
            crate::manifest::DurableOperationKind::CompositionEvaluation { call_id, .. } => {
                Some((*call_id, *call_id))
            }
            crate::manifest::DurableOperationKind::CompositionContinuation {
                call_id,
                evaluation_operation_id,
                ..
            } => history_composition_owner(ledger, *evaluation_operation_id)
                .map(|owner| (*call_id, owner)),
            _ => None,
        })
        .collect()
}

/// Reads exact canonical child bindings from at most the newest
/// `max_operations` records. Evaluation ids alone are not ownership proof:
/// seeded history may carry ids minted by another session's ledger.
#[must_use]
pub fn history_composition_child_owners(
    ledger: &crate::manifest::RuntimeOperationLedger,
    max_operations: usize,
) -> Vec<(plakous_model::CallId, u64, plakous_model::CallId)> {
    ledger
        .newest_operations(max_operations)
        .into_iter()
        .filter_map(|record| {
            let crate::manifest::DurableOperationKind::CompositionChild {
                call_id,
                evaluation_operation_id,
                ..
            } = record.kind()
            else {
                return None;
            };
            history_composition_owner(ledger, *evaluation_operation_id)
                .map(|owner| (*call_id, *evaluation_operation_id, owner))
        })
        .collect()
}

/// One retained nested effect, projected to bounded redaction-safe observations.
pub struct HistoryNestedTool {
    /// Exact nested call.
    pub call_id: plakous_model::CallId,
    /// Canonical executable name.
    pub canonical_name: plakous_model::ToolName,
    /// Exact name the cell called.
    pub exposed_name: plakous_model::ToolName,
    /// Explicit owning evaluation marker.
    pub owner: plakous_model::CallId,
    /// Durable acceptance order within the evaluation.
    pub sequence: u32,
    /// Same lineage the live events carry.
    pub lineage: crate::ToolCallerLineageView,
    /// Same bounded start observation the live events carry.
    pub call: crate::ToolCallObservation,
    /// Definitive bounded completion observation.
    pub result: crate::ToolResultObservation,
}

/// Projects at most the newest `max_operations` records, spending at most
/// `max_decode_bytes` on request/outcome decoding, and never more than 1 MiB
/// on one effect. Oversized, pruned, undecodable or unresolved effects are
/// absent. Only exact child/evaluation bindings with an owner in `owners`
/// qualify; seeded history alone never supplies such a binding.
#[must_use]
pub fn history_nested_tools<S: std::hash::BuildHasher>(
    ledger: &crate::manifest::RuntimeOperationLedger,
    owners: &std::collections::HashSet<plakous_model::CallId, S>,
    max_operations: usize,
    max_decode_bytes: usize,
) -> Vec<HistoryNestedTool> {
    use crate::manifest::{DurableOperationKind, DurableOperationState};
    use crate::runtime::{
        DurableAgentError, DurableEffectIntentEnvelope, DurableUndispatchedToolRequest,
        ToolExecutionResult,
    };
    use plakous_model::{Digest, EntryId, ToolCallBlock, ToolCallerLineage};
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Terminal {
        Completed(ToolExecutionResult),
        Failed(DurableAgentError),
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Request {
        Read((Digest, ToolCallBlock)),
        Mutating(DurableEffectIntentEnvelope),
        Undispatched(Box<DurableUndispatchedToolRequest>),
    }
    let records = ledger.newest_operations(max_operations);
    let edges: std::collections::HashMap<_, _> = records
        .iter()
        .filter_map(|record| {
            let DurableOperationKind::CompositionChild {
                call_id,
                evaluation_operation_id,
                request_sequence,
                ..
            } = record.kind()
            else {
                return None;
            };
            let owner = history_composition_owner(ledger, *evaluation_operation_id)?;
            owners.contains(&owner).then_some((
                *call_id,
                (*evaluation_operation_id, owner, *request_sequence),
            ))
        })
        .collect();
    let mut remaining = max_decode_bytes;
    let mut nested = Vec::new();
    for record in records {
        let DurableOperationKind::Effect { call_id, .. } = record.kind() else {
            continue;
        };
        let Some((evaluation, owner, sequence)) = edges.get(call_id) else {
            continue;
        };
        let (DurableOperationState::Completed(evidence)
        | DurableOperationState::Finalized(evidence)
        | DurableOperationState::ProvenNotDispatched(evidence)) = record.state()
        else {
            continue;
        };
        let bytes = record
            .request()
            .bytes()
            .len()
            .saturating_add(evidence.bytes().len());
        if bytes > 1_048_576 || bytes > remaining {
            continue;
        }
        remaining -= bytes;
        let Ok(request) = serde_json::from_slice::<Request>(record.request().bytes()) else {
            continue;
        };
        let call = match request {
            Request::Read((_, call)) => call,
            Request::Mutating(envelope) => envelope.call,
            Request::Undispatched(request) => {
                let (tag, _, call, _, _) = *request;
                if tag != "proven-not-dispatched-tool-v1" {
                    continue;
                }
                call
            }
        };
        let ToolCallerLineage::LocalProgrammatic {
            child_call_id,
            evaluation_operation_id,
            request_sequence,
            ..
        } = call.correlation().caller_lineage()
        else {
            continue;
        };
        if call.call_id() != *call_id
            || child_call_id != call_id
            || evaluation_operation_id != evaluation
            || request_sequence != sequence
        {
            continue;
        }
        let Ok((tag, _, outcome)) =
            serde_json::from_slice::<(String, EntryId, Terminal)>(evidence.bytes())
        else {
            continue;
        };
        let (result, subject) = match (tag.as_str(), outcome) {
            ("completed-tool-outcome-v1", Terminal::Completed(outcome)) => {
                (outcome.result, outcome.presentation_subject)
            }
            ("failed-tool-outcome-v1", Terminal::Failed(failure)) => {
                let Ok(failure) = failure.into_error() else {
                    continue;
                };
                let error = plakous_tools::ToolRuntimeError::new(
                    plakous_tools::ToolRuntimeErrorKind::Execution,
                    failure.message(),
                );
                let Ok(result) = crate::runtime::normalized_tool_failure(&call, &error) else {
                    continue;
                };
                (result, None)
            }
            _ => continue,
        };
        if result.call_id() != *call_id
            || result.correlation().caller_lineage() != call.correlation().caller_lineage()
        {
            continue;
        }
        nested.push(HistoryNestedTool {
            call_id: *call_id,
            canonical_name: call.canonical_name().clone(),
            exposed_name: call.exposed_name().clone(),
            owner: *owner,
            sequence: *sequence,
            lineage: crate::ToolCallerLineageView::from_lineage(
                call.correlation().caller_lineage(),
                Some(*owner),
            ),
            call: history_tool_call_observation(&call),
            result: history_tool_result_observation(&result, subject.as_deref()),
        });
    }
    nested.sort_by_key(|call| (call.owner, call.sequence));
    nested
}

/// Content-free exact-route probe from the selected model, before the builder is consumed.
#[must_use]
pub fn agent_builder_reachability_probe(
    builder: &AgentBuilder,
) -> Option<plakous_model::ReachabilityProbe> {
    builder.reachability_probe()
}

/// Offline destination-dialect validation before a trusted seed is persisted.
///
/// # Errors
/// Returns a typed construction or replay-decoding refusal; never dispatches.
pub async fn validate_seed_replay(
    builder: &AgentBuilder,
    segments: &[plakous_model::HistorySegment],
) -> Result<(), AgentError> {
    crate::runtime::validate_seed_replay(builder, segments).await
}
