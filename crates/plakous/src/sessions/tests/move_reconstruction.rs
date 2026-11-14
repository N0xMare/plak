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
        job_cut: host_move::MoveJobCut::Settled,
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
async fn live_cut_reconstruction_requires_explicit_retained_job_policy() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let runtime = runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?;
    let mut seed = request(&runtime)?;
    let job = host_move::MoveJob {
        number: 1,
        command: "retained process".into(),
        started_at: timestamp()?,
        owner: plakous_tools::JobOwner {
            line: seed.line,
            generation: seed.generation,
            session_id: seed.source.session_id(),
            branch_id: seed.source.branch_id(),
            fence: 9,
        },
        session_id: host_move::MoveProcessId::new([7; 16])?,
        owner_generation: host_move::MoveProcessGeneration::new(2)?,
        terminal: None,
    };
    seed.jobs = host_move::MoveJobs::new(2, vec![job.clone()])?;
    let compiler = AgentSessions::move_preflight_compiler(RuntimeDurability::Disabled)?;
    assert!(compiler.reconstruct_move_head(runtime, seed).await.is_err());
    let (runtime, model) = runtime_binding_with_model_and_durability(
        workspace.clone(),
        descriptor()?,
        b"account-alpha",
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let mut seed = request(&runtime)?;
    seed.job_cut = host_move::MoveJobCut::Retained;
    seed.jobs = host_move::MoveJobs::new(2, vec![job.clone()])?;
    let rebuilt = compiler.reconstruct_move_head(runtime, seed).await?;
    assert_eq!(model.captured_plans().len(), 0);
    assert_eq!(rebuilt.checkpoint.manifest().jobs().rows(), &[job]);
    assert!(
        host_move::MoveHeadFacts::capture(&rebuilt.checkpoint, &[], host_move::MoveJobCut::Settled)
            .is_err()
    );
    let facts = host_move::MoveHeadFacts::capture(
        &rebuilt.checkpoint,
        &[],
        host_move::MoveJobCut::Retained,
    )?;
    assert!(facts.jobs.rows()[0].terminal.is_none());
    compiler.runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn live_cut_job_owner_fence_cannot_exceed_the_batch_floor() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    for fence in [10, 11] {
        let runtime = runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?;
        let mut seed = request(&runtime)?;
        seed.jobs = host_move::MoveJobs::new(
            2,
            vec![host_move::MoveJob {
                number: 1,
                command: "finished process".into(),
                started_at: timestamp()?,
                owner: plakous_tools::JobOwner {
                    line: seed.line,
                    generation: seed.generation,
                    session_id: seed.source.session_id(),
                    branch_id: seed.source.branch_id(),
                    fence: 10,
                },
                session_id: host_move::MoveProcessId::new([7; 16])?,
                owner_generation: host_move::MoveProcessGeneration::new(2)?,
                terminal: Some(host_move::MoveJobTerminal {
                    status: host_move::MoveJobStatus::Exited,
                    exit_code: Some(0),
                    stopper: None,
                    elapsed: Duration::ZERO,
                    tail: String::new(),
                }),
            }],
        )?;
        let source = seed.source;
        let compiler = AgentSessions::move_preflight_compiler(RuntimeDurability::Disabled)?;
        let mut rebuilt = compiler.reconstruct_move_head(runtime, seed).await?;
        let manifest = rebuilt.checkpoint.manifest();
        let mut jobs = manifest.jobs().rows().to_vec();
        jobs[0].owner.fence = fence;
        rebuilt.checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
            manifest.provenance().clone(),
            manifest.topology(),
            RuntimeLifecycle::Quiescent,
            manifest.harness_plan_component().clone(),
            manifest.provider_configuration().clone(),
            manifest.canonical().clone(),
            manifest.binding().clone(),
            manifest.tools().clone(),
            RuntimeOperationLedger::new(Vec::new(), Vec::new())?,
            None,
            None,
            host_move::MoveJobs::new(2, jobs)?,
            manifest.named_state().clone(),
            manifest.durability(),
        )?)?;
        assert_eq!(single_install(source, rebuilt).is_ok(), fence == 10);
        compiler.runtime.shutdown().await?;
    }
    Ok(())
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
    assert_readonly_move_source(
        &directory.path().join("runtime.sqlite3"),
        target.session_id(),
    )?;
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

#[cfg(all(unix, feature = "sessions-sqlite"))]
fn assert_readonly_move_source(path: &Path, session: SessionId) -> TestResult {
    let before = std::fs::read(path)?;
    let dependencies = SessionBackend::inspect_move_dependencies_sqlite(path, &[session])?;
    assert_eq!(
        dependencies[&session],
        std::collections::BTreeSet::from([session])
    );
    let snapshot = SessionBackend::inspect_move_source_sqlite(path, &[session])?;
    assert_eq!(snapshot.sessions().len(), 1);
    assert_eq!(
        snapshot.sessions()[0].checkpoints.len(),
        snapshot.sessions()[0].branches.len(),
        "only current heads, not the entire old working-checkpoint journal, are snapshotted"
    );
    assert_eq!(before, std::fs::read(path)?);
    Ok(())
}

#[tokio::test]
async fn move_guarded_abandon_distinguishes_read_only_verification_from_idle_driver_open()
-> TestResult {
    for execute in [false, true] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let runtime = runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?;
        let sessions = AgentSessions::new(SessionBackend::memory()?)?;
        let request = request(&runtime)?;
        let source = request.source;
        let operation = request.operation;
        let compiled = sessions.reconstruct_move_head(runtime, request).await?;
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
        if execute {
            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("move:idle-open")?,
                    target,
                    runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?,
                ))
                .await?
            else {
                return Err("idle session did not open".into());
            };
            assert_eq!(
                session.release().await?,
                target,
                "idle open must not advance the checkpoint"
            );
        } else {
            assert!(matches!(
                sessions
                    .verify_open(
                        target,
                        runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha")?
                    )
                    .await?,
                OpenVerification::Opens
            ));
        }
        let hold = sessions
            .hold_branch(&SessionOperationId::new("move:guard")?, target)
            .await?;
        let result = sessions
            .abandon_unopened_move(operation, install.digest(), &[&hold])
            .await;
        if execute {
            assert!(
                result.is_err(),
                "even an idle ordinary open irrevocably prevents guarded abandonment"
            );
            hold.release().await?;
        } else {
            result?;
        }
        sessions.runtime.shutdown().await?;
    }
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
