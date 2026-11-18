//! A previous build's semantic receipt recorded the planner's pre-omission
//! retained-tail digest and the post-omission count. Replay must accept it.

use super::*;
use crate::manifest::RuntimeContextState;
use crate::runtime::compaction::DurableCompactionRequestRecord;
use crate::runtime::compaction::fold::reconstruct_compaction_cut;
use std::sync::atomic::Ordering;

fn install_success_context(
    store: &TestCreateStore,
    identity: DurableCompactionIdentity,
    success: &StoredCheckpoint,
    valid: &CommittedCheckpoint,
    context: RuntimeContextState,
) -> TestResult {
    let manifest = success.checkpoint().manifest();
    let canonical = RuntimeCanonicalState::new(
        manifest.canonical().stable_prefix().clone(),
        manifest.canonical().history().clone(),
        WorkingProjection::default(),
        context,
    )?;
    let forged = checkpoint_with_compaction_state(
        success.checkpoint(),
        identity,
        manifest
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect(),
        canonical,
    )?;
    let operation = valid.operation();
    let receipt = valid.receipt();
    let bundle = RuntimeCheckpointCodec::bundle(&forged, None)?;
    let reference = CheckpointObjectRef::from_store(forged.id(), bundle.root(), forged.integrity());
    let forged_stored = StoredCheckpoint::from_store(reference, bundle)?;
    let forged_receipt = CommittedCheckpoint::from_store(
        CommitOperation::from_store(
            operation.lease().clone(),
            operation.expected(),
            reference,
            forged.checkpoint_commit_identity(),
        ),
        CommitReceipt::from_store(
            reference,
            receipt.previous(),
            BranchHead::from_store(
                receipt.head().session(),
                receipt.head().revision(),
                reference,
            ),
            receipt.feed_seq(),
        ),
    );
    let mut state = lock_mutex(&store.state);
    state
        .stored_commits
        .insert(identity.success_checkpoint, forged_stored);
    state
        .commits
        .insert(identity.success_checkpoint, forged_receipt);
    Ok(())
}

fn semantic_requests(store: &TestCreateStore) -> Vec<(String, DurableSessionAddress)> {
    lock_mutex(&store.state)
        .stored_commits
        .values()
        .flat_map(|stored| stored.checkpoint().manifest().ledger().operations())
        .filter_map(|record| {
            serde_json::from_slice::<DurableCompactionRequestRecord>(record.request().bytes()).ok()
        })
        .filter(|request| request.version < 3)
        .map(|request| (request.operation_id.clone(), request.source()))
        .collect()
}

#[tokio::test]
async fn a_pre_omission_retained_tail_digest_still_validates() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let descriptor = durable_test_descriptor("test.route")?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor, authority, 4, 9_710, false)
            .with_tool_call_on(2)
            .with_context_tokens_from_prepare(4, 900_000),
    );
    model.replay_tool_reasoning.store(true, Ordering::Release);
    let agent = sessions
        .create(
            "create:recorded-retained-tail",
            scheduled_tool_group_fence_builder_with_model(
                Arc::clone(&workspace),
                true,
                Arc::clone(&model),
                LocalDurabilityDisposition::DurableRequired,
            )?,
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let result = agent
        .prompt("use the growing probe at the tool-group fence")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{result:?}"
    );
    let requests = semantic_requests(&store);
    assert!(!requests.is_empty(), "semantic compaction did not run");
    let mut rewritten = 0;
    for (operation_id, source) in requests {
        rewritten += rewrite_pre_omission_receipt(&sessions, &store, &operation_id, source).await?;
    }
    assert!(
        rewritten >= 1,
        "omission did not change the retained-tail digest"
    );
    agent.shutdown().await?;
    Ok(())
}

async fn rewrite_pre_omission_receipt(
    sessions: &DurableSessionRuntime,
    store: &TestCreateStore,
    operation_id: &str,
    source: DurableSessionAddress,
) -> TestResult<u32> {
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let receipt = sessions.compaction_receipt(operation_id, source).await?;
    let DurableCompactionReceipt::Compacted { target, .. } = receipt else {
        return Ok(0);
    };
    assert_eq!(target.checkpoint_id, identity.success_checkpoint);
    let success = store.load_checkpoint(target.checkpoint_id).await?;
    let source_stored = store.load_checkpoint(source.checkpoint_id).await?;
    let request = success
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .iter()
        .find_map(|record| {
            serde_json::from_slice::<DurableCompactionRequestRecord>(record.request().bytes())
                .ok()
                .filter(|candidate| candidate.operation_id == operation_id)
        })
        .ok_or("semantic request missing from the success ledger")?;
    let cut =
        reconstruct_compaction_cut(source_stored.checkpoint().manifest(), identity, &request)?;
    let (published_digest, count) = success
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("semantic provenance")?
        .retained_tail();
    let recorded_digest = cut.retained_tail_identity();
    if published_digest == recorded_digest {
        return Ok(0);
    }
    let compatible = success
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .with_recorded_retained_tail(recorded_digest, count)?;
    let valid = lock_mutex(&store.state)
        .commits
        .get(&identity.success_checkpoint)
        .cloned()
        .ok_or("success commit receipt was not retained")?;
    install_success_context(store, identity, &success, &valid, compatible)?;
    assert_eq!(
        sessions.compaction_receipt(operation_id, source).await?,
        DurableCompactionReceipt::Compacted { source, target },
        "a 0.0.4 retained-tail digest must still validate"
    );
    Ok(1)
}
