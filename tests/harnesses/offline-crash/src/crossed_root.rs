//! A permanently refused cut: operation request bytes cross the accepted root.

use plakous::model::{CheckpointId, Digest, DigestDomain, RunId, TurnId};
use plakous::sessions::SessionLocator;
use plakous_agent::internal::{
    AcquireBranch, CommitCheckpoint, DurableCommandDisposition, DurableCommandRecord,
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent, RecoveryAction, RecoveryHandlerSignature,
    RecoveryObligation, RecoveryObligationKind, RuntimeCheckpoint, RuntimeCheckpointCodec,
    RuntimeComponentKind, RuntimeLifecycle, RuntimeManifest, RuntimeTopology, RuntimeWorkingRoot,
    SessionRef, SessionStore, WorkingRecoveryPhase,
};
use plakous_store::SqliteSessionStore;
use std::{error::Error, num::NonZeroU64, path::Path};
use uuid::Uuid;

/// Seeds an operation whose request does not match its accepted root command.
///
/// # Errors
/// Returns fixture construction or durable commit failures.
#[allow(
    clippy::too_many_lines,
    reason = "fixture commits one complete crossed-root manifest"
)]
pub async fn seed_crossed_root(
    database: &Path,
    source: SessionLocator,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let store = SqliteSessionStore::open(database)?;
    let session = SessionRef::new(source.session_id(), source.branch_id());
    let loaded = store.load_branch(session).await?;
    let prior = loaded.checkpoint().checkpoint().manifest();
    let accepted = prior
        .ledger()
        .commands()
        .into_iter()
        .last()
        .ok_or("accepted command")?;
    let turn_id = TurnId::try_from(Uuid::new_v4())?;
    let operation_id = prior
        .ledger()
        .operations()
        .last()
        .ok_or("operation")?
        .operation_id()
        + 1;
    let command = DurableCommandRecord::new(
        "crossed-root-fixture",
        accepted.sequence() + 1,
        Some(turn_id),
        Digest::sha256(DigestDomain::Blob, 1, &[b"crossed-root-input"])?,
        ReconstructiveComponent::new(
            RuntimeComponentKind::Command,
            1,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "canonical_input": b"crossed-root-input".as_slice(),
                "effect": "Root",
                "rejection": null,
            }))?,
        )?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let crossed = DurableOperationRecord::new(
        operation_id,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        ReconstructiveComponent::new(
            RuntimeComponentKind::Operation,
            1,
            b"crossed accepted root request".as_slice(),
        )?,
        vec![
            DurableOperationTransition::new(
                source.checkpoint_id(),
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(
                CheckpointId::try_from(Uuid::new_v4())?,
                DurableOperationState::Dispatched,
            ),
        ],
    )?;
    let ledger = prior
        .ledger()
        .append_command(command)?
        .append_operation(crossed)?;
    let lease = store
        .acquire(AcquireBranch::new(
            session,
            RunId::try_from(Uuid::new_v4())?,
            NonZeroU64::new(30).ok_or("lease")?,
        ))
        .await?;
    let checkpoint_id = CheckpointId::try_from(Uuid::new_v4())?;
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        RuntimeTopology {
            checkpoint_id,
            parent_checkpoint_id: Some(source.checkpoint_id()),
            branch_revision: prior.topology().branch_revision + 1,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"crossed-root-fixture"],
            )?,
            ..prior.topology()
        },
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id,
                next_round: 1,
            },
            recovery_phase: WorkingRecoveryPhase::Finalization,
            obligations: vec![RecoveryObligation::new(
                operation_id,
                RecoveryHandlerSignature::new(
                    RecoveryObligationKind::Command,
                    RecoveryAction::FinalizationRequired,
                ),
            )],
        },
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, loaded.checkpoint().bundle())?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    Ok(())
}
