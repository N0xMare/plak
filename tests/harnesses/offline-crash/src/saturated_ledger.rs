//! A full, settled operation prefix behind a real outcome-unknown child turn.

use std::{error::Error, num::NonZeroU64, path::Path};

use plakous::model::{AttemptId, CallId, CheckpointId, Digest, DigestDomain, RunId, TurnId};
use plakous::sessions::SessionLocator;
use plakous_agent::internal::{
    AcquireBranch, CommitCheckpoint, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, DurableOperationTransition, ReconstructiveComponent, RuntimeCheckpoint,
    RuntimeCheckpointCodec, RuntimeComponentKind, RuntimeManifest, RuntimeOperationLedger,
    RuntimeTopology, SessionRef, SessionStore,
};
use plakous_store::SqliteSessionStore;
use uuid::Uuid;

type FixtureResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Fills a fixture turn's ledger with finalized effects through the real store.
///
/// The turn's existing unresolved obligations and terminal remain unchanged.
/// This operates only on a test-owned database, never an operator session.
///
/// # Errors
/// Returns fixture construction, lease or commit failures.
pub async fn seed_saturated_operation_ledger(
    database: &Path,
    source: SessionLocator,
    turn_id: TurnId,
) -> FixtureResult<SessionLocator> {
    let store = SqliteSessionStore::open(database)?;
    let session = SessionRef::new(source.session_id(), source.branch_id());
    let loaded = store.load_branch(session).await?;
    let prior = loaded.checkpoint().checkpoint().manifest();
    let ledger = saturated_ledger(prior.ledger(), turn_id)?;
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
            parent_checkpoint_id: Some(prior.topology().checkpoint_id),
            branch_revision: prior.topology().branch_revision + 1,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"saturated-operation-ledger-fixture"],
            )?,
            ..prior.topology()
        },
        prior.lifecycle().clone(),
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        ledger,
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        prior.named_state().clone(),
        prior.durability(),
    )?)?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, loaded.checkpoint().bundle())?;
    store
        .commit(CommitCheckpoint::new(lease.clone(), loaded.head(), bundle))
        .await?;
    store.release(lease).await?;
    Ok(SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        checkpoint_id,
    ))
}

fn saturated_ledger(
    ledger: &RuntimeOperationLedger,
    turn_id: TurnId,
) -> FixtureResult<RuntimeOperationLedger> {
    let count = ledger.operations().len();
    let next = ledger
        .operations()
        .last()
        .ok_or("operation")?
        .operation_id()
        + 1;
    let evidence = ReconstructiveComponent::new(
        RuntimeComponentKind::Operation,
        1,
        b"settled fixture effect".as_slice(),
    )?;
    let attempt_id = AttemptId::try_from(Uuid::new_v4())?;
    let mut ledger = ledger.clone();
    for id in next..next + u64::try_from(4096 - count)? {
        let checkpoint = || CheckpointId::try_from(Uuid::new_v4());
        ledger = ledger.append_operation(DurableOperationRecord::new(
            id,
            DurableOperationKind::Effect {
                turn_id,
                attempt_id,
                call_id: CallId::try_from(Uuid::new_v4())?,
                source_index: 0,
            },
            evidence.clone(),
            vec![
                DurableOperationTransition::new(checkpoint()?, DurableOperationState::Prepared),
                DurableOperationTransition::new(checkpoint()?, DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    checkpoint()?,
                    DurableOperationState::Completed(evidence.clone()),
                ),
                DurableOperationTransition::new(
                    checkpoint()?,
                    DurableOperationState::Finalized(evidence.clone()),
                ),
            ],
        )?)?;
    }
    Ok(ledger)
}
