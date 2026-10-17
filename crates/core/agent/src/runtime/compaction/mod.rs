//! Live compaction evidence and one-shot capability ownership.

use crate::manifest::RuntimeContextState;
use crate::runtime::durable::DurableSessionAddress;
use crate::{AgentError, CheckpointId, RuntimeOperationLedger, SessionWorkflowId};
use plakous_model::{
    AttemptId, BranchId, CanonicalHistory, Clock, CompiledProviderConfiguration, Digest,
    LanguageModel, ModelRequest, SEMANTIC_COMPACTION_INSTRUCTION, SessionId,
};
use std::sync::Arc;
use std::time::Duration;
pub(in crate::runtime) mod live;

pub(in crate::runtime) mod record;

pub(in crate::runtime) mod request;

pub(in crate::runtime) mod continuation;

pub(in crate::runtime) mod fold;

pub(in crate::runtime) mod placeholder;

#[cfg(test)]
mod tests;

const COMPACTION_INSTRUCTION: &str = SEMANTIC_COMPACTION_INSTRUCTION;

const NATIVE_COMPACTION_NAMESPACE: &str = "plakous.compaction";

const NATIVE_COMPACTION_TRIGGER_V1: &str = "native_responses_trigger_v1";

/// Placeholder successor must land at or below this percent of the window so
/// the next tool group cannot immediately re-trigger. Grok Build's compact is
/// a full-replace summary (thousands of tokens); this is the unpaid rung's
/// equivalent of "dramatically reduce" without inventing a second fold.
pub(in crate::runtime) const PLACEHOLDER_LANDING_PERCENT: u8 = 50;

#[derive(Clone, Copy)]
pub(in crate::runtime) struct DurableCompactionIdentity {
    pub(in crate::runtime) source: DurableSessionAddress,
    pub(in crate::runtime) workflow_id: SessionWorkflowId,
    pub(in crate::runtime) attempt_id: AttemptId,
    pub(in crate::runtime) prepared_checkpoint: CheckpointId,
    pub(in crate::runtime) prepared_locator_checkpoint: CheckpointId,
    pub(in crate::runtime) dispatched_checkpoint: CheckpointId,
    pub(in crate::runtime) learned_locator_checkpoint: CheckpointId,
    pub(in crate::runtime) completed_checkpoint: CheckpointId,
    pub(in crate::runtime) failure_evidence_checkpoint: CheckpointId,
    pub(in crate::runtime) abandonment_evidence_checkpoint: CheckpointId,
    pub(in crate::runtime) success_checkpoint: CheckpointId,
    pub(in crate::runtime) failure_checkpoint: CheckpointId,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) struct DurableCompactionRequestRecord {
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) workflow_id: SessionWorkflowId,
    pub(in crate::runtime) turn_fence: bool,
    /// The request's session operation identity, the workflow's join key.
    pub(in crate::runtime) operation_id: String,
    pub(in crate::runtime) source_session: SessionId,
    pub(in crate::runtime) source_branch: BranchId,
    pub(in crate::runtime) source_checkpoint: CheckpointId,
    pub(in crate::runtime) source_history: Digest,
    pub(in crate::runtime) cut: Digest,
    pub(in crate::runtime) request: ModelRequest,
    pub(in crate::runtime) provider_configuration: CompiledProviderConfiguration,
    pub(in crate::runtime) source_harness_plan: Digest,
    pub(in crate::runtime) before_measurement: Digest,
    pub(in crate::runtime) before_status: Digest,
    pub(in crate::runtime) before_input_tokens: u64,
    pub(in crate::runtime) target_affinity_entropy: [u8; 32],
}

/// The offline placeholders plan, persisted as the compaction operation request.
///
/// It is reconstructive: the source address, the exact selection and the
/// independently measured before-audit are enough to reproduce the successor
/// from the immutable source checkpoint without any provider call.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct DurablePlaceholderRequestRecord {
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) workflow_id: SessionWorkflowId,
    pub(in crate::runtime) turn_fence: bool,
    /// The request's session operation identity, the workflow's join key.
    pub(in crate::runtime) operation_id: String,
    pub(in crate::runtime) source_session: SessionId,
    pub(in crate::runtime) source_branch: BranchId,
    pub(in crate::runtime) source_checkpoint: CheckpointId,
    pub(in crate::runtime) source_history: Digest,
    pub(in crate::runtime) source_projection: Digest,
    pub(in crate::runtime) source_harness_plan: Digest,
    pub(in crate::runtime) selected_start: u32,
    pub(in crate::runtime) selected_end: u32,
    pub(in crate::runtime) replaced_results: u32,
    pub(in crate::runtime) replaced_bytes: u64,
    pub(in crate::runtime) before_measurement: Digest,
    pub(in crate::runtime) before_status: Digest,
    pub(in crate::runtime) before_input_tokens: u64,
    pub(in crate::runtime) target_affinity_entropy: [u8; 32],
}

/// Terminal evidence for one committed placeholders: no provider outcome exists.
#[derive(Clone, Copy, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct DurablePlaceholderEvidence {
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) workflow_id: SessionWorkflowId,
    pub(in crate::runtime) source_checkpoint: CheckpointId,
    pub(in crate::runtime) operation: Digest,
    pub(in crate::runtime) successor_history: Digest,
    pub(in crate::runtime) notice_segment: Digest,
    pub(in crate::runtime) replaced_results: u32,
    pub(in crate::runtime) replaced_bytes: u64,
    pub(in crate::runtime) after_measurement: Digest,
    pub(in crate::runtime) after_status: Digest,
    pub(in crate::runtime) after_input_tokens: u64,
}

impl DurablePlaceholderEvidence {
    pub(in crate::runtime) fn decode_publication(
        bytes: &[u8],
        workflow_id: SessionWorkflowId,
    ) -> Option<Self> {
        let publication: Self = serde_json::from_slice(bytes).ok()?;
        (matches!(publication.version, 1 | 2) && publication.workflow_id == workflow_id)
            .then_some(publication)
    }
}

#[derive(Clone, Copy, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct DurableCompactionPublicationEvidence {
    pub(in crate::runtime) version: u32,
    pub(in crate::runtime) workflow_id: SessionWorkflowId,
    source_checkpoint: CheckpointId,
    cut: Digest,
    operation: Digest,
    successor_history: Digest,
    after_measurement: Digest,
    after_status: Digest,
    after_input_tokens: u64,
}

impl DurableCompactionPublicationEvidence {
    pub(in crate::runtime) const fn after_input_tokens(&self) -> u64 {
        self.after_input_tokens
    }
}

/// Who admitted this compact. Host endpoint stays Quiescent-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum CompactOrigin {
    Endpoint,
    TurnFence { pressure_forced: bool },
}

impl CompactOrigin {
    pub(in crate::runtime) const fn is_turn_fence(self) -> bool {
        matches!(self, Self::TurnFence { .. })
    }
}

pub(in crate::runtime) struct ExactCompactionRuntime {
    pub(in crate::runtime) model: Arc<dyn LanguageModel>,
    pub(in crate::runtime) provider_configuration: Arc<CompiledProviderConfiguration>,
    pub(in crate::runtime) clock: Arc<dyn Clock>,
    pub(in crate::runtime) model_operation_timeout: Duration,
}

/// Official Codex remote compact v2 keeps at most this many tokens of newest
/// retained user/developer/system messages (`ceil(utf8/4)` of item text).
const NATIVE_COMPACTION_RETAINED_MESSAGE_TOKEN_BUDGET: u64 = 64_000;

pub(in crate::runtime) enum CompactionCompletionError {
    Semantic(AgentError),
    RecoveryRequired(AgentError),
}

pub(in crate::runtime) struct CompactionSuccessFold {
    pub(in crate::runtime) history: CanonicalHistory,
    pub(in crate::runtime) ledger: RuntimeOperationLedger,
    pub(in crate::runtime) context: RuntimeContextState,
    /// The successor's unanchored measure from the bounded successor
    /// measurement; `None` when the native successor could not be measured.
    pub(in crate::runtime) occupancy_tokens_after: Option<u64>,
}
