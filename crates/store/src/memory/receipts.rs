use super::{
    BranchRevision, BranchTransitionMode, Candidate, CheckpointObjectRef, CommitCheckpoint,
    CommittedCheckpoint, CreateReceipt, CreateSession, DigestDomain, ForkBranch, ForkLineage,
    ForkReceipt, NonZeroU64, RecoverFork, RuntimeCheckpoint, RuntimeCheckpointKind,
    RuntimeContextTransitionKind, RuntimeDurability, RuntimeLifecycle, SessionRef,
    SessionStoreError, StoreInvalidReason, StoredCheckpoint,
};
use plakous_agent::internal::Digest;

pub(super) fn validate_genesis(
    checkpoint: &RuntimeCheckpoint,
    request: &CreateSession,
) -> Result<(), SessionStoreError> {
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    if checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || request.initial().base_root().is_some()
        || topology.session_id != request.session().session_id()
        || topology.branch_id != request.session().branch_id()
        || topology.checkpoint_id != request.initial().checkpoint_id()
        || topology.parent_checkpoint_id.is_some()
        || topology.branch_revision != BranchRevision::INITIAL.get()
        || topology.fork.is_some()
        || topology.lease_fence != 1
        || manifest.durability() != RuntimeDurability::Disabled
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::GenesisCheckpointTopology,
        ));
    }
    Ok(())
}

pub(super) fn validate_commit(
    candidate: &Candidate,
    request: &CommitCheckpoint,
    expected: &RuntimeCheckpoint,
) -> Result<(), SessionStoreError> {
    let checkpoint = &candidate.checkpoint;
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    let next = request
        .expected()
        .revision()
        .checked_next()
        .ok_or(SessionStoreError::Invalid(
            StoreInvalidReason::BranchRevisionExhausted,
        ))?;
    if topology.session_id != request.expected().session().session_id()
        || request.checkpoint().base_root() != Some(request.expected().checkpoint().root())
        || topology.branch_id != request.expected().session().branch_id()
        || topology.checkpoint_id != candidate.reference.checkpoint_id()
        || topology.parent_checkpoint_id != Some(request.expected().checkpoint().checkpoint_id())
        || topology.branch_revision != next.get()
        || topology.fork != expected.manifest().topology().fork
        || topology.lease_fence != request.lease().fence().get()
        || manifest.durability() != RuntimeDurability::Disabled
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointCommitTopology,
        ));
    }
    Ok(())
}

pub(super) fn validate_fork(
    candidate: &Candidate,
    source: &RuntimeCheckpoint,
    request: &ForkBranch,
    target: SessionRef,
) -> Result<(), SessionStoreError> {
    let checkpoint = &candidate.checkpoint;
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    let expected_lineage = ForkLineage {
        session_id: request.source().session().session_id(),
        branch_id: request.source().session().branch_id(),
        checkpoint_id: request.source().checkpoint().checkpoint_id(),
        branch_revision: request.source().revision().get(),
    };
    if checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || request.target().base_root() != Some(request.source().checkpoint().root())
        || topology.session_id != target.session_id()
        || topology.branch_id != target.branch_id()
        || topology.checkpoint_id != candidate.reference.checkpoint_id()
        || topology.parent_checkpoint_id != Some(expected_lineage.checkpoint_id)
        || topology.branch_revision != BranchRevision::INITIAL.get()
        || topology.fork != Some(expected_lineage)
        || topology.lease_fence != 1
        || manifest.durability() != source.manifest().durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkCheckpointSemantics,
        ));
    }
    validate_branch_manifest_transition(source, checkpoint, request.transition())
}

pub(crate) fn validate_fork_semantic_request(
    request: &ForkBranch,
) -> Result<(), SessionStoreError> {
    if request.semantic_request().domain() == DigestDomain::RequestPlan {
        Ok(())
    } else {
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::ForkRequestDigestDomain,
        ))
    }
}

pub(crate) fn validate_branch_manifest_transition(
    source: &RuntimeCheckpoint,
    target: &RuntimeCheckpoint,
    mode: BranchTransitionMode,
) -> Result<(), SessionStoreError> {
    let source_manifest = source.manifest();
    let target_manifest = target.manifest();
    let transition = match mode {
        BranchTransitionMode::OrdinaryFork => RuntimeContextTransitionKind::OrdinaryFork,
        BranchTransitionMode::Rebind => RuntimeContextTransitionKind::Rebind,
    };
    target_manifest
        .canonical()
        .context()
        .validate_branch_transition_from(
            source_manifest.canonical().context(),
            source_manifest.topology().checkpoint_id,
            transition,
        )
        .map_err(|_| {
            SessionStoreError::Invalid(StoreInvalidReason::InvalidBranchContextTransition)
        })?;
    if source_manifest.lifecycle() != &RuntimeLifecycle::Quiescent
        || target_manifest.lifecycle() != &RuntimeLifecycle::Quiescent
        || target_manifest.canonical().history() != source_manifest.canonical().history()
        || target_manifest.canonical().working() != source_manifest.canonical().working()
        || target_manifest.ledger() != source_manifest.ledger()
        || target_manifest.family_artifact().is_some()
        || target_manifest.durability() != source_manifest.durability()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::UnauthorizedBranchManifestTransition,
        ));
    }
    let same_configuration = target_manifest.harness_plan_component()
        == source_manifest.harness_plan_component()
        && target_manifest.canonical().stable_prefix()
            == source_manifest.canonical().stable_prefix()
        && target_manifest.binding() == source_manifest.binding()
        && target_manifest.tools() == source_manifest.tools();
    match mode {
        BranchTransitionMode::OrdinaryFork => {
            if !same_configuration || target_manifest.provenance() != source_manifest.provenance() {
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::OrdinaryForkChangedSemantics,
                ));
            }
        }
        BranchTransitionMode::Rebind => {
            if same_configuration {
                return Err(SessionStoreError::Invalid(
                    StoreInvalidReason::RebindConfigurationUnchanged,
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_materialized_fork_receipt(
    receipt: &ForkReceipt,
    source: &StoredCheckpoint,
    target: &StoredCheckpoint,
) -> Result<(), SessionStoreError> {
    let operation = receipt.operation();
    let source_checkpoint = source.checkpoint().as_ref();
    let target_checkpoint = target.checkpoint().as_ref();
    let source_topology = source_checkpoint.manifest().topology();
    let target_topology = target_checkpoint.manifest().topology();
    let previous_session = receipt.previous_session();
    let session = receipt.session();
    let expected_lineage = ForkLineage {
        session_id: operation.source().session().session_id(),
        branch_id: operation.source().session().branch_id(),
        checkpoint_id: operation.source().checkpoint().checkpoint_id(),
        branch_revision: operation.source().revision().get(),
    };
    if receipt.source() != operation.source()
        || receipt.target().session() != operation.target()
        || receipt.target().revision() != BranchRevision::INITIAL
        || receipt.target().checkpoint() != operation.checkpoint()
        || receipt.lease().session() != operation.target()
        || receipt.lease().fence() != NonZeroU64::MIN
        || previous_session.session_id() != operation.target().session_id()
        || previous_session.revision() != operation.expected_session_revision()
        || previous_session.revision().checked_next() != Some(session.revision())
        || session.session_id() != previous_session.session_id()
        || session.initial_branch() != previous_session.initial_branch()
        || session.created_at() != previous_session.created_at()
        || session.durability() != previous_session.durability()
        || session.is_archived()
        || previous_session.is_archived()
        || source.reference() != operation.source().checkpoint()
        || target.reference() != operation.checkpoint()
        || target_checkpoint.checkpoint_commit_identity() != operation.checkpoint_commit_identity()
        || source_checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || target_checkpoint.kind() != RuntimeCheckpointKind::Quiescent
        || source_topology.session_id != operation.source().session().session_id()
        || source_topology.branch_id != operation.source().session().branch_id()
        || source_topology.checkpoint_id != operation.source().checkpoint().checkpoint_id()
        || source_topology.branch_revision != operation.source().revision().get()
        || target_topology.session_id != operation.target().session_id()
        || target_topology.branch_id != operation.target().branch_id()
        || target_topology.checkpoint_id != operation.checkpoint().checkpoint_id()
        || target_topology.parent_checkpoint_id != Some(expected_lineage.checkpoint_id)
        || target_topology.branch_revision != BranchRevision::INITIAL.get()
        || target_topology.fork != Some(expected_lineage)
        || target_topology.lease_fence != receipt.lease().fence().get()
        || source_checkpoint.manifest().durability() != session.durability()
        || target_checkpoint.manifest().durability() != session.durability()
    {
        return Err(SessionStoreError::corrupt());
    }
    validate_branch_manifest_transition(
        source_checkpoint,
        target_checkpoint,
        operation.transition(),
    )
    .map_err(|_| SessionStoreError::corrupt())
}

pub(crate) fn create_matches(
    receipt: &CreateReceipt,
    request: &CreateSession,
    reference: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
) -> bool {
    let operation = receipt.operation();
    operation.session() == request.session()
        && operation.created_at() == request.created_at()
        && operation.owner() == request.owner()
        && operation.lease_seconds() == request.lease_seconds()
        && operation.initial() == reference
        && operation.checkpoint_commit_identity() == checkpoint_commit_identity
}

pub(crate) fn commit_matches(
    committed: &CommittedCheckpoint,
    request: &CommitCheckpoint,
    reference: CheckpointObjectRef,
    checkpoint_commit_identity: Digest,
) -> bool {
    committed.operation().lease() == request.lease()
        && committed.operation().expected() == request.expected()
        && committed.operation().checkpoint() == reference
        && committed.operation().checkpoint_commit_identity() == checkpoint_commit_identity
}

pub(super) fn fork_matches(
    receipt: &ForkReceipt,
    request: &ForkBranch,
    candidate: &Candidate,
) -> bool {
    let operation = receipt.operation();
    operation.workflow_id() == request.workflow_id()
        && operation.semantic_request() == request.semantic_request()
        && operation.expected_session_revision() == request.expected_session_revision()
        && operation.source() == request.source()
        && operation.target().branch_id() == request.target_branch()
        && operation.transition() == request.transition()
        && operation.checkpoint() == candidate.reference
        && operation.checkpoint_commit_identity()
            == candidate.checkpoint.checkpoint_commit_identity()
        && candidate.base_root == Some(operation.source().checkpoint().root())
}

pub(super) fn recover_fork_matches(receipt: &ForkReceipt, request: &RecoverFork) -> bool {
    let operation = receipt.operation();
    operation.workflow_id() == request.workflow_id()
        && operation.source() == request.source()
        && operation.target().branch_id() == request.target_branch()
        && operation.transition() == request.transition()
        && operation.semantic_request() == request.semantic_request()
}
