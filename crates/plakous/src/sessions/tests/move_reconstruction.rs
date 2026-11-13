//! A constructor does not admit work; staged native state is the retry authority.
use super::*;
use crate::sessions::host_move::{
    MoveHeadRequest, MoveInstall, MoveLine, MoveOperation, MoveSession,
};
use plakous_agent::internal::{LineOrigin, LineRecord, SessionHead, SessionRevision};

fn request(runtime: &SessionRuntimeBinding) -> TestResult<MoveHeadRequest> {
    let (_, _, model, _, _, account) = agent_builder_target_authority(&runtime.builder);
    let source = SessionLocator::new(
        SessionId::try_from(Uuid::from_u128(1))?,
        BranchId::try_from(Uuid::from_u128(2))?,
        CheckpointId::try_from(Uuid::from_u128(3))?,
    );
    Ok(MoveHeadRequest {
        operation: MoveOperation::new([13; 32])?,
        source,
        source_revision: 7,
        source_fork: None,
        fence_floor: 10,
        line: plakous_model::LineId::started_by(source.branch_id()),
        generation: 1,
        binding: host_move::MoveBinding::from_runtime(runtime),
        history: PortableSessionHistory::new(
            Vec::new(),
            plakous_model::hex_lower(account.bytes()),
            model,
        )
        .with_named_state(BTreeMap::from([("answer".into(), json!(42))]))?,
        jobs: host_move::MoveJobs::default(),
        compaction: None,
        interpreter: None,
    })
}

#[tokio::test]
async fn move_reconstruction_compiles_a_new_head_without_a_driver_lease_or_dispatch() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (runtime, model) = runtime_binding_with_model_and_durability(
        workspace.clone(),
        descriptor()?,
        b"account-alpha",
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let store = MemorySessionStore::new(MemoryStoreLimits::default())?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let before = store.stats()?;
    let seed = request(&runtime)?;
    let source = seed.source;
    let operation = seed.operation;
    let compiled = sessions.reconstruct_move_head(runtime, seed).await?;
    assert_eq!(compiled.branch.session, source.session_ref());
    assert_eq!(compiled.branch.revision.get(), 8);
    assert_ne!(compiled.checkpoint.id(), source.checkpoint_id());
    assert_eq!(
        compiled
            .checkpoint
            .manifest()
            .topology()
            .parent_checkpoint_id,
        Some(source.checkpoint_id())
    );
    assert_eq!(compiled.checkpoint.manifest().topology().lease_fence, 10);
    assert_eq!(store.stats()?, before);
    assert_eq!(model.captured_plans().len(), 0);
    let again_runtime = runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?;
    let again_seed = request(&again_runtime)?;
    let again = sessions
        .reconstruct_move_head(again_runtime, again_seed)
        .await?;
    assert_eq!(again.checkpoint.id(), compiled.checkpoint.id());
    let target = SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        compiled.checkpoint.id(),
    );
    let install = single_install(source, compiled)?;
    sessions.stage_move(operation, install.clone()).await?;
    assert_eq!(
        sessions.recover_move(operation).await?.digest(),
        install.digest()
    );
    sessions
        .finish_move(
            operation,
            install.digest(),
            host_move::MoveDisposition::Publish,
        )
        .await?;
    assert!(matches!(
        sessions
            .verify_open(
                target,
                runtime_binding_for(workspace, descriptor()?, b"account-alpha")?
            )
            .await?,
        OpenVerification::Opens
    ));
    assert_eq!(model.captured_plans().len(), 0);
    sessions.runtime.shutdown().await?;
    Ok(())
}

fn single_install(
    source: SessionLocator,
    compiled: host_move::ReconstructedMoveHead,
) -> TestResult<MoveInstall> {
    let durability = compiled.checkpoint.manifest().durability();
    Ok(MoveInstall::new(
        vec![MoveSession {
            session: SessionHead::from_store(
                source.session_id(),
                source.branch_id(),
                SessionRevision::from_store(1),
                timestamp()?,
                durability,
                false,
            ),
            feed_tail_floor: 20,
            branches: vec![compiled.branch],
            checkpoints: vec![compiled.checkpoint],
        }],
        vec![MoveLine {
            line: LineRecord::from_store(
                plakous_model::LineId::started_by(source.branch_id()),
                source.session_ref(),
                1,
                LineOrigin::Created,
            ),
            terminals: vec![],
        }],
    )?)
}

#[tokio::test]
#[cfg(all(unix, feature = "sessions-sqlite"))]
async fn move_reconstruction_preserves_fork_provenance_without_recreating_source_checkpoints()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let runtime = durable_runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?;
    let backend = SessionBackend::sqlite(directory.path().join("runtime.sqlite3"))?;
    let sessions = AgentSessions::new(backend)?;
    let mut request = request(&runtime)?;
    let source = request.source;
    let operation = request.operation;
    let fork = host_move::MoveFork {
        session_id: SessionId::try_from(Uuid::from_u128(21))?,
        branch_id: BranchId::try_from(Uuid::from_u128(22))?,
        checkpoint_id: CheckpointId::try_from(Uuid::from_u128(23))?,
        branch_revision: 4,
    };
    request.source_fork = Some(fork);
    let compiled = sessions.reconstruct_move_head(runtime, request).await?;
    assert_eq!(compiled.checkpoint.manifest().topology().fork, Some(fork));
    let target = SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        compiled.checkpoint.id(),
    );
    let install = single_install(source, compiled)?;
    sessions.stage_move(operation, install.clone()).await?;
    sessions
        .finish_move(
            operation,
            install.digest(),
            host_move::MoveDisposition::Publish,
        )
        .await?;
    sessions.runtime.shutdown().await?;
    let reopened = SqliteSessionStore::open_verified(directory.path().join("runtime.sqlite3"))?;
    assert!(reopened.load_checkpoint(fork.checkpoint_id).await.is_err());
    reopened.shutdown().await?;
    drop(reopened);
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("runtime.sqlite3"),
    )?)?;
    assert!(matches!(
        sessions
            .verify_open(
                target,
                durable_runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?
            )
            .await?,
        OpenVerification::Opens
    ));
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("move:advance-imported-fork")?,
            target,
            durable_completing_runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?
    else {
        return Err("imported fork did not open".into());
    };
    let result = session
        .agent()
        .prompt("advance imported ancestry")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let advanced = session.release().await?;
    assert_ne!(advanced, target);
    sessions.runtime.shutdown().await?;
    let verified = SqliteSessionStore::open_verified(directory.path().join("runtime.sqlite3"))?;
    verified.shutdown().await?;
    drop(verified);
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("runtime.sqlite3"),
    )?)?;
    sessions.archive(advanced).await?;
    sessions
        .delete_archived(target.session_id())
        .await?
        .ok_or("imported session was not deleted")?;
    sessions.runtime.shutdown().await?;
    let verified = SqliteSessionStore::open_verified(directory.path().join("runtime.sqlite3"))?;
    verified.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn move_reconstruction_refuses_replay_drift_and_exhausted_floors_without_writes() -> TestResult
{
    let (_directory, runtime) = runtime_binding()?;
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let mut seed = request(&runtime)?;
    seed.fence_floor = u64::MAX;
    assert!(sessions.reconstruct_move_head(runtime, seed).await.is_err());
    let (_directory, runtime) = runtime_binding()?;
    let mut seed = request(&runtime)?;
    seed.history = PortableSessionHistory::new(
        Vec::new(),
        "different-account".into(),
        ModelId::new("test.model")?,
    );
    assert!(sessions.reconstruct_move_head(runtime, seed).await.is_err());
    sessions.runtime.shutdown().await?;
    Ok(())
}
