//! Compaction receipts whose successor carries a post-compaction note.

use super::*;
use crate::runtime::compaction::DurableCompactionRequestRecord;
use crate::runtime::compaction::continuation::TURN_CONTINUATION_NOTE_MARKER;

const PLAN_NOTE: &str = "restored plan: receipt";

fn note_segment(
    entry: u128,
    actor: Actor,
    provenance: Provenance,
    kind: HistorySegmentKind,
) -> TestResult<HistorySegment> {
    let item = ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(entry))?,
        actor,
        vec![ContentBlock::Text(TextBlock::new(PLAN_NOTE, None)?)],
        provenance,
    )?;
    Ok(HistorySegment::new(kind, vec![item])?)
}

/// Stores `history` as the success checkpoint with an otherwise coherent
/// manifest and commit tuple, so only receipt validation can refuse it.
fn install_success_history(
    store: &TestCreateStore,
    identity: DurableCompactionIdentity,
    success: &StoredCheckpoint,
    valid: &CommittedCheckpoint,
    history: CanonicalHistory,
) -> TestResult {
    let manifest = success.checkpoint().manifest();
    let canonical = RuntimeCanonicalState::new(
        manifest.canonical().stable_prefix().clone(),
        history,
        WorkingProjection::default(),
        manifest.canonical().context().clone(),
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

/// A manual compaction whose successor got a plan note is still a valid
/// receipt for its operation: queried live, by a later process, and through
/// recovery, which answers from the receipt without a provider call.
#[tokio::test]
async fn a_noted_endpoint_compaction_keeps_a_valid_receipt_across_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let lease = NonZeroU64::new(30).ok_or("nonzero lease")?;
    let sessions =
        DurableSessionRuntime::current(store.clone(), RuntimeDurability::Required, lease)?;
    let agent = sessions
        .create(
            "create:noted-receipt",
            durable_artifact_test_builder(Arc::clone(&workspace), artifact_test_model(0, 96_500)?)
                .post_compaction_note(Arc::new(|_, _| Some(PLAN_NOTE.to_owned()))),
        )
        .await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let operation_id = "compact:noted-receipt";
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let (builder, model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|_| "noted compaction rejected its exact source")?;
    assert!(
        matches!(
            outcome,
            AgentCompactSessionOutcome::Compacted { target, .. }
                if target == identity.success_checkpoint
        ),
        "{outcome:?}"
    );
    let published = agent.checkpoint();
    let last = published
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .last()
        .cloned()
        .ok_or("empty successor")?;
    assert_eq!(last.kind(), HistorySegmentKind::Steering);
    assert!(matches!(
        last.items().first().and_then(|item| item.blocks().first()),
        Some(ContentBlock::Text(text)) if text.text() == PLAN_NOTE
    ));
    let target = DurableSessionAddress::new(
        source.session_id,
        source.branch_id,
        identity.success_checkpoint,
    );
    let compacted = DurableCompactionReceipt::Compacted { source, target };
    assert_eq!(
        sessions.compaction_receipt(operation_id, source).await?,
        compacted
    );
    agent.shutdown().await?;

    let reopened =
        DurableSessionRuntime::current(store.clone(), RuntimeDurability::Required, lease)?;
    assert_eq!(
        reopened.compaction_receipt(operation_id, source).await?,
        compacted
    );
    let (recover_builder, recover_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        plakous_test_support::ScriptedRecoveryState::new(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    assert_eq!(
        Box::pin(reopened.recover_compaction(operation_id, source, recover_builder)).await?,
        DurableCompactionRecoveryOutcome::Compacted { source, target }
    );
    assert_eq!(model.starts.load(Ordering::Acquire), 1);
    assert_eq!(recover_model.starts.load(Ordering::Acquire), 0);
    assert_eq!(recover_model.recover_calls.load(Ordering::Acquire), 0);

    // The receipt accepts exactly the shapes publication can produce: the
    // fold successor alone (a recovery-published fold appends no note) or
    // followed by one harness note. Anything else after it is corrupt.
    let success = store.load_checkpoint(identity.success_checkpoint).await?;
    let valid = lock_mutex(&store.state)
        .commits
        .get(&identity.success_checkpoint)
        .cloned()
        .ok_or("success commit receipt was not retained")?;
    let history = success
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .clone();
    let segments = history.segments().to_vec();
    let bare = CanonicalHistory::new(segments[..segments.len() - 1].to_vec())?;
    install_success_history(&store, identity, &success, &valid, bare.clone())?;
    assert_eq!(
        reopened.compaction_receipt(operation_id, source).await?,
        compacted,
        "a successor without a note is what recovery publishes"
    );
    let tampered = [
        (
            "a second note",
            history.append(note_segment(
                0x0dd_0001,
                Actor::User,
                Provenance::Instruction,
                HistorySegmentKind::Steering,
            )?)?,
        ),
        (
            "a note the operator typed",
            bare.append(note_segment(
                0x0dd_0002,
                Actor::User,
                Provenance::UserInput,
                HistorySegmentKind::Steering,
            )?)?,
        ),
        (
            "a note outside steering",
            bare.append(note_segment(
                0x0dd_0003,
                Actor::User,
                Provenance::Instruction,
                HistorySegmentKind::Conversation,
            )?)?,
        ),
    ];
    for (case, forged) in tampered {
        install_success_history(&store, identity, &success, &valid, forged)?;
        assert!(
            reopened
                .compaction_receipt(operation_id, source)
                .await
                .is_err(),
            "accepted a successor with {case}"
        );
    }
    {
        let mut state = lock_mutex(&store.state);
        state
            .stored_commits
            .insert(identity.success_checkpoint, success);
        state.commits.insert(identity.success_checkpoint, valid);
    }
    assert_eq!(
        reopened.compaction_receipt(operation_id, source).await?,
        compacted
    );
    Ok(())
}

/// A mid-turn fold publishes the plan note and then the turn continuation
/// note; its receipt is valid with both.
#[tokio::test]
async fn a_noted_turn_fence_fold_keeps_a_valid_receipt() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let bodies = [20_000, 20_000, 60_000, 20_000, 20_000, 60_000, 2_000];
    let (builder, model) = placeholder_fixture(
        workspace,
        377_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = sessions
        .create(
            "create:noted-turn-fence",
            builder.post_compaction_note(Arc::new(|_, _| Some(PLAN_NOTE.to_owned()))),
        )
        .await?;
    let result = agent.prompt("read every page").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert!(model.paid_compaction_calls.load(Ordering::Acquire) >= 1);
    let requests = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .values()
            .flat_map(|stored| stored.checkpoint().manifest().ledger().operations())
            .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
            .filter_map(|record| {
                serde_json::from_slice::<DurableCompactionRequestRecord>(record.request().bytes())
                    .ok()
            })
            .map(|request| (request.operation_id.clone(), request.source()))
            .collect::<BTreeMap<_, _>>()
    };
    let mut continued = 0;
    for (operation_id, source) in requests {
        let receipt = sessions.compaction_receipt(&operation_id, source).await?;
        let DurableCompactionReceipt::Compacted { target, .. } = receipt else {
            return Err(format!("{operation_id}: {receipt:?}").into());
        };
        let success = store.load_checkpoint(target.checkpoint_id).await?;
        let segments = success
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .to_vec();
        let texts = segments
            .iter()
            .rev()
            .take(2)
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if texts.len() == 2
            && texts[0].starts_with(TURN_CONTINUATION_NOTE_MARKER)
            && texts[1] == PLAN_NOTE
        {
            continued += 1;
        }
    }
    assert!(
        continued >= 1,
        "no receipt covered a fold with both trailing notes"
    );
    agent.shutdown().await?;
    Ok(())
}
