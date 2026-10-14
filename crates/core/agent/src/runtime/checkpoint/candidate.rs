//! Pure successor checkpoint construction shared by live and recovery owners.

use crate::manifest::{InterpreterObservation, RuntimeContextState};
use crate::runtime::checkpoint::identity::{CheckpointCommitFacts, checkpoint_commit_identity};
use crate::runtime::continuation::{checkpoint_bundle_error, runtime_manifest_error};
use crate::{
    AgentError, AgentErrorKind, CheckpointId, CheckpointObjectBundle, CheckpointObjectRef,
    FamilyArtifact, RuntimeCanonicalState, RuntimeCheckpoint, RuntimeCheckpointCodec,
    RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger, RuntimeTopology,
};
use plakous_model::{CanonicalHistory, Digest, WorkingProjection};
use std::sync::Arc;
pub(in crate::runtime) struct SuccessorFacts {
    pub(in crate::runtime) checkpoint_id: CheckpointId,
    pub(in crate::runtime) lifecycle: RuntimeLifecycle,
    pub(in crate::runtime) history: CanonicalHistory,
    pub(in crate::runtime) ledger: RuntimeOperationLedger,
    pub(in crate::runtime) jobs: crate::manifest::JobLedger,
    pub(in crate::runtime) context: RuntimeContextState,
    pub(in crate::runtime) family: Option<FamilyArtifact>,
}

pub(in crate::runtime) struct CheckpointCandidate {
    pub(in crate::runtime) checkpoint: Arc<RuntimeCheckpoint>,
    pub(in crate::runtime) bundle: CheckpointObjectBundle,
    pub(in crate::runtime) expected_ref: CheckpointObjectRef,
    pub(in crate::runtime) commit_identity: Digest,
}

pub(in crate::runtime) fn successor_candidate(
    checkpoint: &RuntimeCheckpoint,
    prior_bundle: &CheckpointObjectBundle,
    facts: SuccessorFacts,
    lease_fence: u64,
    interpreter_observation: Option<InterpreterObservation>,
) -> Result<CheckpointCandidate, AgentError> {
    let SuccessorFacts {
        checkpoint_id,
        lifecycle,
        history,
        ledger,
        jobs,
        context,
        family,
    } = facts;
    let prior = checkpoint.manifest();
    let topology = prior.topology();
    let branch_revision = topology.branch_revision.checked_add(1).ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Session,
            "branch revision space was exhausted",
        )
    })?;
    let commit_identity = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id,
        parent_checkpoint_id: topology.checkpoint_id,
        branch_revision,
        lifecycle: &lifecycle,
        history_identity: history.identity(),
        ledger: &ledger,
        context_identity: context.identity(),
        family_artifact: family.as_ref(),
    })?;
    let canonical = RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        history,
        WorkingProjection::default(),
        context,
    )
    .map_err(runtime_manifest_error)?;
    let checkpoint = Arc::new(
        RuntimeCheckpoint::new(
            RuntimeManifest::new(
                prior.provenance().clone(),
                RuntimeTopology {
                    session_id: topology.session_id,
                    branch_id: topology.branch_id,
                    checkpoint_id,
                    parent_checkpoint_id: Some(topology.checkpoint_id),
                    branch_revision,
                    fork: topology.fork,
                    lease_fence,
                    commit_identity,
                },
                lifecycle,
                prior.harness_plan_component().clone(),
                prior.provider_configuration().clone(),
                canonical,
                prior.binding().clone(),
                prior.tools().clone(),
                ledger,
                family,
                interpreter_observation,
                jobs,
                prior.durability(),
            )
            .map_err(runtime_manifest_error)?,
        )
        .map_err(runtime_manifest_error)?,
    );
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, prior_bundle)
        .map_err(checkpoint_bundle_error)?;
    let expected_ref =
        CheckpointObjectRef::from_store(checkpoint_id, bundle.root(), checkpoint.integrity());
    Ok(CheckpointCandidate {
        checkpoint,
        bundle,
        expected_ref,
        commit_identity,
    })
}
