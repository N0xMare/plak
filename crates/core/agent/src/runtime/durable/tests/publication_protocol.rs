use super::landed_round_abandonment::root_without_provider_cut;
use super::*;
use crate::runtime::checkpoint::storage_pause::StoragePause;
use crate::runtime::durable::{
    DurableEffectEvidenceRecoveryOutcome, DurableEffectEvidenceRecoveryRequest,
};

async fn fixture(
    checkpoint: &RuntimeCheckpoint,
) -> TestResult<(
    Arc<TestCreateStore>,
    DurableSessionRuntime,
    DurableSessionAddress,
)> {
    let topology = checkpoint.manifest().topology();
    let source =
        DurableSessionAddress::new(topology.session_id, topology.branch_id, checkpoint.id());
    let store = Arc::new(TestCreateStore::default());
    let created = store
        .create(CreateSession::new(
            source.session(),
            Timestamp::new(10, 0)?,
            RunId::try_from(uuid::Uuid::from_u128(40_001))?,
            NonZeroU64::new(30).ok_or("lease")?,
            RuntimeCheckpointCodec::bundle(checkpoint, None)?,
        ))
        .await?;
    store.release(created.lease().clone()).await?;
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    *lock_mutex(&store.admission_jobs) = Some(Vec::new());
    Ok((store, runtime, source))
}

fn effect_request(
    source: DurableSessionAddress,
) -> TestResult<DurableEffectEvidenceRecoveryRequest> {
    Ok(DurableEffectEvidenceRecoveryRequest::new(
        "protocol:effect",
        source,
        999,
        None,
        true,
        ToolExecutorId::new("fixture.executor")?,
        ToolOutcome::success_text("outcome")?,
    )?)
}

async fn code_cell_fixture() -> TestResult<(
    Arc<TestCreateStore>,
    DurableSessionRuntime,
    DurableSessionAddress,
)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let model = artifact_test_model(0, 89_900)?;
    let agent = runtime
        .create(
            "create:publication-protocol",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    store.fail_quiescent_terminal();
    let turn = agent.prompt("publication protocol").await?;
    let turn_id = turn.id();
    let _ = turn.result().await?;
    let checkpoint = agent.checkpoint();
    let _ = agent.shutdown().await;
    let source = seed_code_cell_recovery_checkpoint(&store, checkpoint, turn_id).await?;
    *lock_mutex(&store.admission_jobs) = Some(Vec::new());
    Ok((store, runtime, source))
}

#[tokio::test]
async fn effect_receipts_precede_owner_load_and_release_the_fence() -> TestResult {
    let cut = root_without_provider_cut(false).await?;
    let (store, runtime, source) = fixture(&cut).await?;
    assert_eq!(
        runtime
            .recover_effect_evidence(effect_request(source)?)
            .await?,
        DurableEffectEvidenceRecoveryOutcome::RecoveryRequired(source)
    );
    assert_eq!(
        *lock_mutex(&store.admission_jobs),
        Some(vec![
            "recover_commit",
            "load_branch",
            "acquire",
            "recover_commit",
            "load_branch",
            "load_branch",
            "release",
        ])
    );
    assert!(store.active_lease().is_none());
    Ok(())
}

#[tokio::test]
async fn code_cell_receipts_precede_owner_load_and_publish_end_to_end() -> TestResult {
    let (store, runtime, source) = code_cell_fixture().await?;
    let request = DurableCodeCellAbandonmentRequest::new("protocol:code-cell", source)?;
    let outcome = runtime.abandon_code_cell(request.clone()).await?;
    assert!(
        matches!(outcome, DurableCodeCellAbandonmentOutcome::Finalized { source: exact, .. } if exact == source)
    );
    assert_eq!(
        &lock_mutex(&store.captured_admission_jobs)[..6],
        &[
            "recover_commit",
            "load_branch",
            "acquire",
            "recover_commit",
            "load_branch",
            "load_branch",
        ]
    );
    assert!(store.active_lease().is_none());
    assert_eq!(runtime.abandon_code_cell(request).await?, outcome);
    Ok(())
}

fn loaded(
    checkpoint: &RuntimeCheckpoint,
    session: crate::SessionHead,
) -> TestResult<crate::LoadedBranch> {
    let bundle = RuntimeCheckpointCodec::bundle(checkpoint, None)?;
    let reference =
        CheckpointObjectRef::from_store(checkpoint.id(), bundle.root(), checkpoint.integrity());
    let topology = checkpoint.manifest().topology();
    Ok(crate::LoadedBranch::from_store(
        session,
        BranchHead::from_store(
            SessionRef::new(topology.session_id, topology.branch_id),
            BranchRevision::from_store(topology.branch_revision),
            reference,
        ),
        StoredCheckpoint::from_store(reference, bundle)?,
    ))
}

#[tokio::test]
async fn publication_error_wins_over_release_error_but_success_does_not_hide_it() -> TestResult {
    for publication_fails in [false, true] {
        let (store, runtime, source) = code_cell_fixture().await?;
        if publication_fails {
            *lock_mutex(&store.reject_next_commit) = Some(SessionStoreError::corrupt());
        }
        store.full_on_release.store(true, Ordering::Release);
        let releases = store.releases.load(Ordering::Acquire);
        let pause = StoragePause::default();
        pause.cancel();
        let request = DurableCodeCellAbandonmentRequest::new("protocol:cleanup", source)?;
        let error = pause
            .scope(runtime.abandon_code_cell(request.clone()))
            .await
            .unwrap_err();
        if publication_fails {
            assert_eq!(error.kind(), AgentErrorKind::Internal);
            assert_eq!(
                error.message(),
                "Code Cell abandonment evidence checkpoint was invalid"
            );
        } else {
            assert_eq!(error.store_error(), Some(SessionStoreError::StorageFull));
        }
        assert!(store.full_writes.load(Ordering::Acquire));
        assert_eq!(store.releases.load(Ordering::Acquire), releases + 1);
        store.full_writes.store(false, Ordering::Release);
        store
            .release(store.active_lease().ok_or("missing lease")?)
            .await?;
        assert!(store.active_lease().is_none());
        if !publication_fails {
            assert!(matches!(
                runtime.abandon_code_cell(request).await?,
                DurableCodeCellAbandonmentOutcome::Finalized { .. }
            ));
        }
    }
    Ok(())
}

#[tokio::test]
async fn compaction_phase_change_loads_before_receipts_and_releases_the_fence() -> TestResult {
    for abandon in [false, true] {
        let quiescent = crate::checkpoint_bundle::tests::fixture_with_history(
            CanonicalHistory::default(),
            0,
            false,
        )?;
        let topology = quiescent.manifest().topology();
        let source =
            DurableSessionAddress::new(topology.session_id, topology.branch_id, quiescent.id());
        let identity = DurableCompactionIdentity::new("protocol:phase", source)?;
        let record = DurableOperationRecord::new(
            1,
            DurableOperationKind::Compaction {
                workflow_id: identity.workflow_id,
            },
            runtime_component(RuntimeComponentKind::Operation, b"unused request".to_vec())?,
            vec![DurableOperationTransition::new(
                source.checkpoint_id(),
                DurableOperationState::Prepared,
            )],
        )?;
        let ledger = RuntimeOperationLedger::new(Vec::new(), vec![record])?;
        let lifecycle = working_lifecycle_for_root(
            ModelOperationRoot::SessionWorkflow(identity.workflow_id),
            &ledger,
        )?;
        let working = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
            CanonicalHistory::default(),
            0,
            true,
            Some(ledger),
            Some(lifecycle),
        )?;
        let (store, runtime, source) = fixture(&working).await?;
        let session = lock_mutex(&store.state)
            .loaded
            .as_ref()
            .ok_or("branch")?
            .session();
        *lock_mutex(&store.branch_after_acquire) = Some(loaded(&quiescent, session)?);
        let outcome = if abandon {
            runtime.abandon_compaction("protocol:phase", source).await?
        } else {
            let directory = tempfile::tempdir()?;
            let builder = durable_test_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                ToolRegistryBuilder::default().freeze(),
                "test.route",
                b"alpha",
                false,
            )?;
            runtime
                .recover_compaction("protocol:phase", source, builder)
                .await?
        };
        assert_eq!(
            outcome,
            DurableCompactionRecoveryOutcome::NotRequired(source)
        );
        assert_eq!(
            *lock_mutex(&store.admission_jobs),
            Some(vec![
                "recover_commit",
                "recover_commit",
                "recover_commit",
                "load_branch",
                "acquire",
                "load_branch",
                "recover_commit",
                "recover_commit",
                "recover_commit",
                "load_branch",
                "release",
            ])
        );
        assert!(store.active_lease().is_none());
    }
    Ok(())
}

#[tokio::test]
async fn missing_compaction_receipts_keep_the_distinct_recover_and_abandon_reads() -> TestResult {
    let checkpoint = crate::checkpoint_bundle::tests::fixture_with_history(
        CanonicalHistory::default(),
        0,
        false,
    )?;
    let (store, runtime, source) = fixture(&checkpoint).await?;
    let stale = DurableSessionAddress::new(
        source.session_id(),
        source.branch_id(),
        CheckpointId::try_from(uuid::Uuid::from_u128(40_002))?,
    );
    let directory = tempfile::tempdir()?;
    let builder = durable_test_builder(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        ToolRegistryBuilder::default().freeze(),
        "test.route",
        b"alpha",
        false,
    )?;
    assert_eq!(
        runtime
            .recover_compaction("protocol:missing", stale, builder)
            .await
            .unwrap_err()
            .store_error(),
        Some(SessionStoreError::IdentityConflict)
    );
    assert_eq!(
        *lock_mutex(&store.admission_jobs),
        Some(vec![
            "recover_commit",
            "recover_commit",
            "recover_commit",
            "load_branch",
            "load_branch"
        ])
    );
    *lock_mutex(&store.admission_jobs) = Some(Vec::new());
    assert_eq!(
        runtime
            .abandon_compaction("protocol:missing", stale)
            .await?,
        DurableCompactionRecoveryOutcome::NotRequired(stale)
    );
    assert_eq!(
        *lock_mutex(&store.admission_jobs),
        Some(vec![
            "recover_commit",
            "recover_commit",
            "recover_commit",
            "load_branch"
        ])
    );
    Ok(())
}
