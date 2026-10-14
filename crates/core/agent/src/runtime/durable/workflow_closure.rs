//! The session-workflow compaction rows of the closure table (spec 01 §5.5):
//! a Working head whose open obligations all belong to one admitted
//! compaction, with no open root turn, closes at open in one commit that
//! carries the workflow's `WorkflowSettled` fact. No row redispatches.

use std::sync::Arc;
use std::time::Duration;

use plakous_model::{Clock, CompiledProviderConfiguration, LanguageModel, ModelResponse};

use super::compaction_recovery::{
    close_abandoned_compaction, decode_compaction_failure_evidence,
    decode_paid_compaction_operations, recovery_fail_compaction,
    recovery_publish_compaction_success,
};
use super::{CompactionRecoveryOwner, DurableCompactionOperations};
use crate::runtime::compaction::fold::reconstruct_compaction_cut;
use crate::runtime::compaction::{
    CompactionCompletionError, DurableCompactionIdentity, DurableCompactionRequestRecord,
};
use crate::{
    AgentError, AgentErrorKind, BranchHead, BranchLease, CheckpointObjectBundle,
    DurableOperationKind, DurableOperationState, RuntimeCheckpoint, RuntimeLifecycle,
    RuntimeWorkingRoot, SessionStore,
};

/// The row a workflow head matches, decided from the ledger alone.
pub(in crate::runtime) struct WorkflowClosure {
    identity: DurableCompactionIdentity,
    operations: DurableCompactionOperations,
    row: WorkflowRow,
}

pub(in crate::runtime) enum WorkflowRow {
    /// The summary was Prepared (settles with `summary_unsent`) or is
    /// uncertain; the closure commit's evidence carries which.
    Abandon,
    /// The recorded summary, applied through the live fold publication.
    ApplySummary(Box<ModelResponse>),
    /// The live workflow recorded its failure evidence and lost its owner
    /// before the failure publication; that publication is replayed.
    PublishRecordedFailure(Box<AgentError>),
}

/// What a closure's publication needs from the opening agent: the Completed
/// row measures the successor exactly as the live publication does.
pub(in crate::runtime) struct WorkflowClosureRuntime<'a> {
    pub(in crate::runtime) model: Arc<dyn LanguageModel>,
    pub(in crate::runtime) configuration: &'a CompiledProviderConfiguration,
    pub(in crate::runtime) clock: &'a dyn Clock,
    pub(in crate::runtime) authority_timeout: Duration,
}

fn integrity(message: &'static str) -> AgentError {
    AgentError::new(AgentErrorKind::Session, message)
}

/// The workflow row for this head: `None` when the head is not a session
/// workflow's, an error (typed integrity) for workflow debt no row admits.
pub(in crate::runtime) fn workflow_compaction_closure(
    head: &RuntimeCheckpoint,
) -> Result<Option<WorkflowClosure>, AgentError> {
    let manifest = head.manifest();
    let RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::SessionWorkflow(workflow_id),
        ..
    } = manifest.lifecycle()
    else {
        return Ok(None);
    };
    let ledger = manifest.ledger();
    let request = ledger
        .operations()
        .into_iter()
        .find(|record| {
            matches!(record.kind(), DurableOperationKind::Compaction { workflow_id: candidate }
                if candidate == workflow_id)
        })
        .ok_or_else(|| integrity("an admitted session workflow has no compaction record"))
        .and_then(|record| {
            serde_json::from_slice::<DurableCompactionRequestRecord>(record.request().bytes())
                .map_err(|_| integrity("a working session workflow is not a paid compaction"))
        })?;
    if request.turn_fence {
        return Ok(None);
    }
    let identity = DurableCompactionIdentity::new(&request.operation_id, request.source())?;
    if identity.workflow_id != *workflow_id {
        return Err(integrity(
            "a compaction request names another workflow identity",
        ));
    }
    let operations = decode_paid_compaction_operations(head, identity)?;
    if operations.request.version < 3 {
        reconstruct_compaction_cut(manifest, identity, &operations.request)?;
    }
    let pair = [operations.compaction.0, operations.provider.0];
    if ledger.operations().into_iter().any(|record| {
        !pair.contains(&record.operation_id())
            && !matches!(record.state(), DurableOperationState::Finalized(_))
    }) {
        return Err(integrity("a working session workflow carries foreign debt"));
    }
    let state = |operation: u64| {
        ledger
            .operation(operation)
            .map(|record| record.state().clone())
            .ok_or_else(|| integrity("a compaction record disappeared"))
    };
    let compaction = state(operations.compaction.0)?;
    let provider = state(operations.provider.0)?;
    let uncertain = |state: &DurableOperationState| {
        matches!(
            state,
            DurableOperationState::Dispatched
                | DurableOperationState::LocatorBound(_)
                | DurableOperationState::OutcomeUnknown(_)
        )
    };
    let open = |state: &DurableOperationState| {
        matches!(state, DurableOperationState::Prepared) || uncertain(state)
    };
    let row = match (&compaction, &provider) {
        (
            DurableOperationState::Completed(failure)
            | DurableOperationState::ProvenNotDispatched(failure),
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_),
        ) if head.id() == identity.failure_evidence_checkpoint => {
            WorkflowRow::PublishRecordedFailure(Box::new(decode_compaction_failure_evidence(
                failure,
            )?))
        }
        (DurableOperationState::Completed(applied), DurableOperationState::Completed(summary))
            if head.id() == identity.completed_checkpoint && applied == summary =>
        {
            WorkflowRow::ApplySummary(Box::new(
                serde_json::from_slice::<ModelResponse>(summary.bytes())
                    .map_err(|_| integrity("a completed compaction summary does not decode"))?,
            ))
        }
        (compaction, provider)
            if open(compaction)
                && (matches!(provider, DurableOperationState::Prepared) || uncertain(provider)) =>
        {
            WorkflowRow::Abandon
        }
        _ => {
            return Err(integrity(
                "a working session workflow has a shape no closure row admits",
            ));
        }
    };
    Ok(Some(WorkflowClosure {
        identity,
        operations,
        row,
    }))
}

/// The opening owner's branch authority over the head it closes.
pub(in crate::runtime) struct WorkflowClosureBranch {
    pub(in crate::runtime) store: Arc<dyn SessionStore>,
    pub(in crate::runtime) store_timeout: Duration,
    pub(in crate::runtime) lease: BranchLease,
    pub(in crate::runtime) head: BranchHead,
    pub(in crate::runtime) checkpoint: Arc<RuntimeCheckpoint>,
    pub(in crate::runtime) bundle: CheckpointObjectBundle,
}

/// Publishes the closure under the opening owner's lease, in one commit, and
/// returns the closed head.
pub(in crate::runtime) async fn publish_workflow_closure(
    branch: WorkflowClosureBranch,
    closure: WorkflowClosure,
    runtime: WorkflowClosureRuntime<'_>,
) -> Result<(Arc<RuntimeCheckpoint>, CheckpointObjectBundle, BranchHead), AgentError> {
    let mut owner = CompactionRecoveryOwner {
        store: branch.store,
        store_timeout: branch.store_timeout,
        lease: branch.lease,
        head: branch.head,
        checkpoint: branch.checkpoint,
        bundle: branch.bundle,
    };
    Box::pin(publish_row(&mut owner, closure, runtime)).await?;
    Ok((owner.checkpoint, owner.bundle, owner.head))
}

async fn publish_row(
    owner: &mut CompactionRecoveryOwner,
    closure: WorkflowClosure,
    runtime: WorkflowClosureRuntime<'_>,
) -> Result<(), AgentError> {
    let WorkflowClosure {
        identity,
        operations:
            DurableCompactionOperations {
                compaction,
                provider,
                request,
                prepared,
            },
        row,
    } = closure;
    match row {
        WorkflowRow::Abandon => {
            close_abandoned_compaction(owner, identity, compaction, provider, &request).await?;
        }
        WorkflowRow::PublishRecordedFailure(error) => {
            recovery_fail_compaction(
                owner, identity, compaction, provider, &request, None, &error,
            )
            .await?;
        }
        WorkflowRow::ApplySummary(response) => {
            match recovery_publish_compaction_success(
                owner,
                identity,
                compaction,
                provider,
                &request,
                &prepared,
                &response,
                runtime.model,
                runtime.configuration,
                runtime.clock,
                runtime.authority_timeout,
            )
            .await
            {
                Ok(_) => {}
                // An authority or measurement fault fences this open; the
                // head stays Working and the next open replays the row.
                Err(CompactionCompletionError::RecoveryRequired(error)) => return Err(error),
                // The recorded summary fails the live publication's own
                // checks: it settles `Failed` exactly as it would have live.
                Err(CompactionCompletionError::Semantic(error)) => {
                    recovery_fail_compaction(
                        owner, identity, compaction, provider, &request, None, &error,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}
