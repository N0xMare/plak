//! Durable session storage and private recovery publication state.

use crate::runtime::compaction::DurableCompactionRequestRecord;
use crate::runtime::composition::record::DurableLocalCompositionEvaluationRequestV2;
use crate::runtime::{
    Agent, AgentBuilder, OperationId, RecoveredTurnHandle, RuntimeInstanceNonce, StoreTimeouts,
};
use crate::{
    AgentError, AgentErrorKind, BranchHead, BranchLease, CheckpointId, CheckpointObjectBundle,
    CommandId, ModelRouteBinding, ReconstructiveComponent, RuntimeCheckpoint, RuntimeDurability,
    RuntimeLifecycle, RuntimeOperationLedger, SessionStore, SessionWorkflowId, ToolRuntimeManifest,
};
use plakous_model::{
    BranchId, CanonicalHistory, CompiledProviderConfiguration, ConversationItem, Digest,
    EffectiveProviderConfigurationId, HistorySegment, ModelId, PreparedCallRecord, RunId,
    SelectedProviderConfigurationId, SessionId, StableRequestPrefix, TurnId,
};
use plakous_tools::{ToolExecutorId, ToolOutcome};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
pub(in crate::runtime) mod branch;
mod compaction_inspection;
pub(in crate::runtime) mod compaction_recovery;
pub(in crate::runtime) use compaction_inspection::checkpoint_can_abandon_compaction;
pub(in crate::runtime) mod composition_recovery;
pub(in crate::runtime) mod effect_recovery;
mod retention;
mod session;
pub(in crate::runtime) mod turn_recovery;
pub(in crate::runtime) mod workflow_closure;

/// Component-internal high-level create/open owner over one semantic store.
///
/// This value keeps store rows, object graphs, leases, fences, and recovery
/// receipts below the facade. Create and open derive fresh authority from the
/// actual selected model object plus builder-owned plan/tool/workspace/policy
/// facts; callers cannot submit a historical [`crate::RevalidationFact`].
pub struct DurableSessionRuntime {
    store: Arc<dyn SessionStore>,
    durability: RuntimeDurability,
    lease_seconds: NonZeroU64,
    instance_nonce: RuntimeInstanceNonce,
    store_timeouts: StoreTimeouts,
}
impl DurableSessionRuntime {
    /// Creates the current release-owned high-level runtime seam.
    ///
    /// This constructor is intended for facade composition. The resulting
    /// create/open methods expose no store record or lease.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the runtime instance nonce cannot be generated.
    #[doc(hidden)]
    pub fn current(
        store: Arc<dyn SessionStore>,
        durability: RuntimeDurability,
        lease_seconds: NonZeroU64,
    ) -> Result<Self, AgentError> {
        let instance_nonce = RuntimeInstanceNonce::generate()?;
        Ok(Self {
            store,
            durability,
            lease_seconds,
            instance_nonce,
            store_timeouts: StoreTimeouts::default(),
        })
    }

    /// Replaces the per-operation and joined-cleanup store deadlines.
    ///
    /// # Errors
    ///
    /// Returns invalid input for zero or unreasonably large deadlines.
    #[doc(hidden)]
    pub fn with_store_timeouts(
        mut self,
        operation: Duration,
        cleanup: Duration,
    ) -> Result<Self, AgentError> {
        if operation.is_zero()
            || cleanup.is_zero()
            || operation > Duration::from_secs(3_600)
            || cleanup > Duration::from_secs(3_600)
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "session store deadlines must be nonzero and bounded",
            ));
        }
        self.store_timeouts = StoreTimeouts { operation, cleanup };
        Ok(self)
    }
}

impl fmt::Debug for DurableSessionRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableSessionRuntime")
            .field("store", &"<semantic session store>")
            .field("durability", &self.durability)
            .field("lease_seconds", &self.lease_seconds)
            .field("instance_nonce", &"<redacted>")
            .field("store_timeouts", &self.store_timeouts)
            .finish_non_exhaustive()
    }
}

pub(in crate::runtime) mod bootstrap;

#[cfg(test)]
mod tests;

/// Exact semantic address accepted by the high-level durable runtime seam.
#[allow(
    clippy::struct_field_names,
    reason = "the exact topology address deliberately preserves each opaque identity's full role"
)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DurableSessionAddress {
    pub(in crate::runtime) session_id: SessionId,
    pub(in crate::runtime) branch_id: BranchId,
    pub(in crate::runtime) checkpoint_id: CheckpointId,
}

/// Honest result of opening an exact runtime checkpoint.
pub enum DurableRuntimeOpen {
    /// A certified quiescent checkpoint was attached to the one private driver.
    Opened(Agent),
    /// A certified pre-dispatch decision barrier was attached without replay.
    RestoredWorking {
        /// Existing session runtime; no new root was admitted.
        agent: Agent,
        /// Opaque capability for the one original admitted turn.
        turn: RecoveredTurnHandle,
    },
    /// Persisted work needs a closed recovery implementation unavailable here.
    RecoveryRequired {
        /// Exact unchanged recovery-only checkpoint address.
        address: DurableSessionAddress,
        /// How long until this branch could be acquired, when the only thing
        /// standing in the way is another process's lease.
        ///
        /// A process that died mid-turn keeps its lease until it expires, so
        /// its session is recovery-only for that long and ordinarily openable
        /// afterwards. Saying so is the difference between a user reading
        /// "broken" and reading "wait". `None` when recovery is required for a
        /// reason waiting will not fix.
        retry_after_seconds: Option<u64>,
    },
}

/// Receipt-first classification for one exact deterministic compaction operation.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCompactionReceipt {
    /// No finalized publication receipt exists.
    NotFound,
    /// The exact semantic successor was committed.
    Compacted {
        /// Exact source named by the operation.
        source: DurableSessionAddress,
        /// Exact committed successor.
        target: DurableSessionAddress,
    },
    /// A quiescent failure descendant was committed over unchanged semantics.
    Failed {
        /// Exact source named by the operation.
        source: DurableSessionAddress,
        /// Exact committed failure descendant.
        target: DurableSessionAddress,
    },
    /// Explicit abandonment committed a source-preserving descendant.
    Abandoned {
        /// Exact source named by the operation.
        source: DurableSessionAddress,
        /// Exact committed abandonment descendant.
        target: DurableSessionAddress,
    },
    /// The branch remains recovery-only for this exact operation.
    RecoveryRequired(DurableSessionAddress),
}

/// Result of certified restart reconciliation for one compaction workflow.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCompactionRecoveryOutcome {
    /// No matching operation was admitted.
    NotRequired(DurableSessionAddress),
    /// The exact semantic successor was recovered or finalized.
    Compacted {
        /// Exact unchanged source.
        source: DurableSessionAddress,
        /// Exact committed successor.
        target: DurableSessionAddress,
    },
    /// The exact operation was definitively terminalized without a summary.
    Failed {
        /// Exact unchanged semantic source.
        source: DurableSessionAddress,
        /// Quiescent failure descendant.
        target: DurableSessionAddress,
    },
    /// The exact ambiguous operation was deliberately abandoned.
    Abandoned {
        /// Exact unchanged semantic source.
        source: DurableSessionAddress,
        /// Quiescent abandonment descendant.
        target: DurableSessionAddress,
    },
    /// Evidence is absent, ambiguous, unavailable, or unsupported.
    RecoveryRequired(DurableSessionAddress),
}

/// Exact caller-authorized reconciliation of one recovery-only Code Cell root.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableCodeCellAbandonmentRequest {
    operation_id: String,
    source: DurableSessionAddress,
}

/// Receipt-first outcome of explicit Code Cell abandonment and root finalization.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCodeCellAbandonmentOutcome {
    /// The exact operation already committed its one quiescent successor.
    Finalized {
        /// Exact unchanged `RecoveryRequired` source.
        source: DurableSessionAddress,
        /// Exact committed quiescent successor.
        target: DurableSessionAddress,
    },
    /// The named checkpoint is quiescent and no abandonment was required.
    NotRequired(DurableSessionAddress),
    /// The exact root remains recovery-only; no source or child was replayed.
    RecoveryRequired(DurableSessionAddress),
}

/// Exact caller-authorized finalization of one Working effect without re-execution.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct DurableEffectEvidenceRecoveryRequest {
    operation_id: String,
    source: DurableSessionAddress,
    recovery_operation: u64,
    claim_digest: Option<Digest>,
    dispatched: bool,
    executor: ToolExecutorId,
    outcome: ToolOutcome,
}

/// Receipt-first outcome of topology-blind effect-evidence recovery.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableEffectEvidenceRecoveryOutcome {
    /// The exact effect was finalized, or an identical retry found that receipt.
    Finalized {
        /// Exact unchanged `RecoveryRequired` source.
        source: DurableSessionAddress,
        /// Exact committed successor that holds the finalized effect.
        target: DurableSessionAddress,
    },
    /// The named checkpoint is quiescent and required no effect finalization.
    NotRequired(DurableSessionAddress),
    /// Evidence conflicted or the effect remains recovery-only; no new identity was minted.
    RecoveryRequired(DurableSessionAddress),
}

/// High-level result of one exact quiescent new-branch workflow.
#[doc(hidden)]
pub enum DurableRuntimeBranchOutcome {
    /// One exact target checkpoint was already present or atomically published.
    Published(Box<DurableBranchPublication>),
    /// The exact source is recovery-only and no lease or target was created.
    RecoveryRequired {
        /// Exact unchanged recovery-only source.
        address: DurableSessionAddress,
    },
}

/// Exact high-level publication facts returned above the semantic store.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct DurableBranchPublication {
    source: DurableSessionAddress,
    target: DurableSessionAddress,
    binding: ModelRouteBinding,
    feed_seq: Option<crate::feed::FeedSeq>,
}

pub(in crate::runtime) struct DurableRebindRequest {
    target: DurableTargetSelectors,
    builder: AgentBuilder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum DurableRebindToolTransition {
    PreserveExact,
    RecompileForTarget,
}

pub(in crate::runtime) enum DurableBranchRequest {
    OrdinaryFork,
    /// A fork that recompiles from a builder for the source's own binding: an
    /// ordinary fork when nothing changed, a rebind onto that target otherwise.
    RecompilingFork(Box<AgentBuilder>),
    Rebind(Box<DurableRebindRequest>),
}

#[derive(Clone, Eq, PartialEq)]
pub(in crate::runtime) struct DurableTargetSelectors {
    protocol: plakous_model::ProtocolId,
    route: plakous_model::RouteId,
    model: ModelId,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    account_binding: Digest,
    tool_transition: DurableRebindToolTransition,
}

#[allow(
    clippy::struct_field_names,
    reason = "genesis derives four distinct opaque identity types from one operation"
)]
#[derive(Clone, Copy)]
pub(in crate::runtime) struct DurableGenesisIdentities {
    session_id: SessionId,
    branch_id: BranchId,
    run_id: RunId,
    checkpoint_id: CheckpointId,
}

#[derive(Clone, Copy)]
pub(in crate::runtime) struct DurableCodeCellAbandonmentIdentity {
    pub(in crate::runtime) source: DurableSessionAddress,
    pub(in crate::runtime) workflow: Digest,
    pub(in crate::runtime) evidence_checkpoint: CheckpointId,
    pub(in crate::runtime) final_checkpoint: CheckpointId,
}

#[derive(Clone, Copy)]
pub(in crate::runtime) struct DurableEffectEvidenceIdentity {
    source: DurableSessionAddress,
    recovery_operation: u64,
    workflow: Digest,
    evidence_checkpoint: CheckpointId,
}

pub(in crate::runtime) enum EffectEvidencePreparation {
    NotRequired,
    Conflict,
    Publish {
        history: CanonicalHistory,
        ledger: RuntimeOperationLedger,
        lifecycle: RuntimeLifecycle,
    },
}

#[derive(Clone, Copy)]
pub(in crate::runtime) struct DurableBranchRequestIdentity {
    workflow_id: SessionWorkflowId,
    target_branch: BranchId,
    semantic_request: Digest,
}

#[derive(Clone, Copy)]
pub(in crate::runtime) struct DurableBranchMaterial {
    checkpoint_id: CheckpointId,
    context_entropy: [u8; 32],
    commit_identity: Digest,
}

pub(in crate::runtime) struct PreparedRebindTarget {
    harness: ReconstructiveComponent,
    provider_configuration: CompiledProviderConfiguration,
    stable_prefix: StableRequestPrefix,
    binding: ModelRouteBinding,
    tools: ToolRuntimeManifest,
}

/// One working root turn a newly opened owner may abandon.
///
/// Either the turn's own command is still dispatched (its process was lost
/// before it delivered a result) or it is already finalized as
/// recovery-required (a live owner fenced it and delivered that result). Every
/// other outstanding obligation is one this fold knows how to close without
/// acting on it: a provider attempt, a tool effect, a joined process session,
/// or a compaction whose response was never applied. A pending effect decision
/// or hosted continuation is restorable instead, and a commit obligation is
/// resolved by commit recovery first.
pub(in crate::runtime) struct AbandonedTurnRecoveryFold {
    pub(in crate::runtime) turn_id: TurnId,
    pub(in crate::runtime) command_id: CommandId,
    pub(in crate::runtime) root_operation: OperationId,
    /// Attempts whose outcome nobody observed. Closed as unknown.
    pub(in crate::runtime) provider_operations: Vec<OperationId>,
    /// Completed records waiting only for finalization. Their existing evidence
    /// is preserved, never replaced with an unknown outcome.
    pub(in crate::runtime) settled_operations: Vec<OperationId>,
    /// Completed effects whose raw outcome was not promoted into model history.
    /// Finalize their exact evidence, but retain the `RunningTool` warning.
    pub(in crate::runtime) uncommitted_effect_operations: Vec<OperationId>,
    /// Tool effects that were in flight. Closed as unknown, because the command
    /// may have run to completion and changed the workspace.
    ///
    /// `Prepared` and `Claimed` belong here as much as `Dispatched` does: the
    /// ledger's contract is that the absence of a later state proves nothing,
    /// so neither is evidence that the command did not start.
    pub(in crate::runtime) effect_operations: Vec<OperationId>,
    /// Tool effects that exact evidence proves never started.
    ///
    /// The one effect state that is proof rather than silence. These are
    /// finalized with the evidence they already carry, and they do not make the
    /// turn's note claim that anything may have run, because nothing did.
    pub(in crate::runtime) unrun_effect_operations: Vec<OperationId>,
    /// Joined process sessions whose teardown was never proven. The orphan is
    /// not signalled: no process identity is stored to signal.
    pub(in crate::runtime) process_operations: Vec<OperationId>,
    /// One compaction and its provider attempt whose response was never
    /// applied. Closed without redispatch.
    pub(in crate::runtime) compaction_operations: Vec<OperationId>,
    /// Whether the root command still awaits its terminal result.
    pub(in crate::runtime) root: AbandonedRoot,
    pub(in crate::runtime) tool_round_completed: bool,
}

/// The root command's state when its turn is abandoned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum AbandonedRoot {
    /// Accepted but not dispatched: close with proven-not-dispatched evidence.
    Prepared,
    /// Dispatched with no delivered result: the publisher finalizes it as
    /// cancelled, which is the turn's one terminal result.
    Dispatched,
    /// Already finalized as recovery-required. That result was delivered, so
    /// the publisher leaves the root untouched and writes no second one.
    AlreadyTerminal,
    /// A delivered recovery-required result still owes its finalization.
    PendingTerminal,
}

pub(in crate::runtime) struct LocalTerminalRecoveryFold {
    synthesized: Option<(OperationId, ReconstructiveComponent)>,
    turn_id: TurnId,
    pub(in crate::runtime) command_id: CommandId,
    operations: Vec<OperationId>,
    output: Vec<ConversationItem>,
    /// The finish row of a `FailTurn` decision whose prompt was withheld.
    withhold_prompt: bool,
    /// The note a failed finish row appends, as the live terminal commit does.
    tail: Option<HistorySegment>,
    family_artifact: Option<crate::FamilyArtifact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::runtime) enum DurableCodeCellAbandonmentDisposition {
    ProvenNotDispatched,
    PossiblyStarted,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct DurableCodeCellAbandonmentEvidence {
    version: u32,
    workflow: Digest,
    source_checkpoint: CheckpointId,
    source_history: Digest,
    operation_id: u64,
    request_identity: Digest,
    disposition: DurableCodeCellAbandonmentDisposition,
}

pub(in crate::runtime) struct CodeCellAbandonmentFold {
    pub(in crate::runtime) turn_id: TurnId,
    pub(in crate::runtime) root_finalized: bool,
    pub(in crate::runtime) turn_abandonment: bool,
    evaluation_request: DurableLocalCompositionEvaluationRequestV2,
    pub(in crate::runtime) operations: Vec<(OperationId, DurableCodeCellAbandonmentDisposition)>,
}

pub(in crate::runtime) struct CompactionRecoveryOwner {
    store: Arc<dyn SessionStore>,
    store_timeout: Duration,
    lease: BranchLease,
    head: BranchHead,
    checkpoint: Arc<RuntimeCheckpoint>,
    bundle: CheckpointObjectBundle,
}
/// What a checkpoint's ledger holds for one compaction workflow identity.
///
/// The ladder has a paid rung and an offline one. A paid compaction carries a
/// provider attempt and a prepared call that recovery can resolve; the placeholder
/// rung carries neither, because it never dispatches and is finalized in the
/// same commit that publishes its successor.
pub(in crate::runtime) enum DecodedCompactionWorkflow {
    Paid(Box<DurableCompactionOperations>),
    OfflinePlaceholders {
        compaction: OperationId,
        /// True when the ledger already shows it finalized, which is the only
        /// state the rung can publish.
        resolved: bool,
    },
}

pub(in crate::runtime) struct DurableCompactionOperations {
    pub(in crate::runtime) compaction: OperationId,
    pub(in crate::runtime) provider: OperationId,
    pub(in crate::runtime) request: DurableCompactionRequestRecord,
    pub(in crate::runtime) prepared: PreparedCallRecord,
}
