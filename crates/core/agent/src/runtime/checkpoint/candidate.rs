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
    pub(in crate::runtime) named_state: crate::manifest::NamedState,
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
        named_state,
        context,
        family,
    } = facts;
    let prior = checkpoint.manifest();
    let ledger = validate_successor_ledger(prior, ledger, &lifecycle)?;
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
        named_state: &named_state,
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
                    line: topology.line,
                    line_generation: topology.line_generation,
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
                named_state,
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

fn validate_successor_ledger(
    prior: &RuntimeManifest,
    ledger: RuntimeOperationLedger,
    lifecycle: &RuntimeLifecycle,
) -> Result<RuntimeOperationLedger, AgentError> {
    if !ledger.preserves_high_water_of(prior.ledger()) {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            "checkpoint successor regressed a ledger high-water mark",
        ));
    }
    if let Some(source) = ledger.retirement_source() {
        let RuntimeLifecycle::Working {
            root: crate::RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } = lifecycle
        else {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "live retirement lacks its working root",
            ));
        };
        if !ledger.retirement_preserves_owned_roots()
            || !source.is_retained_successor_of(prior.ledger())
            || !counter_successor_ok(prior.ledger(), source, lifecycle)
            || prior
                .ledger()
                .operations()
                .iter()
                .any(|record| source.operation(record.operation_id()).is_none())
        {
            return Err(AgentError::new(
                AgentErrorKind::Internal,
                "retirement source disagrees with its parent",
            ));
        }
        crate::runtime::operation_record::validate_live_retirement(source, &ledger, *turn_id)?;
    } else if !counter_successor_ok(prior.ledger(), &ledger, lifecycle) {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            "checkpoint successor changed retired attempts without retirement",
        ));
    } else if matches!(lifecycle, RuntimeLifecycle::Working { .. })
        && matches!(prior.lifecycle(), RuntimeLifecycle::Working { .. })
        && prior
            .ledger()
            .operations()
            .iter()
            .any(|record| ledger.operation(record.operation_id()).is_none())
    {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            "working ledger removal lacks its staged retirement evidence",
        ));
    }
    Ok(ledger.without_retirement_source())
}

fn counter_successor_ok(
    prior: &RuntimeOperationLedger,
    ledger: &RuntimeOperationLedger,
    lifecycle: &RuntimeLifecycle,
) -> bool {
    if !ledger.preserves_retired_counter_of(prior) {
        return false;
    }
    if ledger.retired_counter() == prior.retired_counter() {
        return true;
    }
    let Some((old_root, _)) = prior.retired_counter() else {
        return false;
    };
    let RuntimeLifecycle::Working {
        root: crate::RuntimeWorkingRoot::Turn { turn_id, .. },
        ..
    } = lifecycle
    else {
        return false;
    };
    if *turn_id == old_root {
        return false;
    }
    let Some(command) = ledger.commands().into_iter().find(|command| {
        command.turn_id() == Some(*turn_id)
            && crate::runtime::operation_record::is_accepted_root(command)
    }) else {
        return false;
    };
    let Some(root) = ledger.operations().into_iter().find(|record| matches!(record.kind(), crate::DurableOperationKind::Command { turn_id: Some(turn) } if *turn == *turn_id)) else { return false; };
    root.request().bytes() == command.command_component().bytes()
        && match (prior.operation(root.operation_id()), root.state()) {
            (
                None,
                crate::DurableOperationState::Prepared | crate::DurableOperationState::Dispatched,
            ) => true,
            (Some(before), crate::DurableOperationState::Dispatched) => {
                matches!(before.state(), crate::DurableOperationState::Prepared)
            }
            _ => false,
        }
}
