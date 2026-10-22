//! Codex native successors that keep the current round's tool batch.

use super::*;
use crate::SemanticCompactionPublication;
use crate::runtime::compaction::continuation::TURN_CONTINUATION_NOTE_MARKER;
use crate::runtime::compaction::fold::record_summarized_turns;

struct NativeEndpointCompaction {
    _directory: tempfile::TempDir,
    store: Arc<TestCreateStore>,
    sessions: DurableSessionRuntime,
    /// `None` once a recovery case has shut the live owner down.
    agent: Option<Agent>,
    source: DurableSessionAddress,
    identity: DurableCompactionIdentity,
    operation_id: &'static str,
}

/// One turn per body, each a single tool round and an answer, then an
/// endpoint native compaction whose summary is `summary_bytes` long. With
/// `recover`, the owner is lost right after the provider completion is durable
/// and the next owner publishes the successor from that recorded response.
async fn native_endpoint_compaction(
    entry_base: u128,
    bodies: &[usize],
    summary_bytes: usize,
    operation_id: &'static str,
    recover: bool,
    plan_note: Option<&'static str>,
) -> TestResult<NativeEndpointCompaction> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let rounds = (0..bodies.len()).map(|turn| turn * 2).collect::<Vec<_>>();
    let (builder, _model) = placeholder_fixture(
        workspace.clone(),
        entry_base,
        40_000,
        bodies,
        &rounds,
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let builder = builder.with_scheduled_compaction_enabled(false);
    let builder = match plan_note {
        Some(note) => builder.post_compaction_note(Arc::new(move |_, _| Some(note.to_owned()))),
        None => builder,
    };
    let agent = sessions
        .create(&format!("create:{operation_id}"), builder)
        .await?;
    for turn in 0..bodies.len() {
        let result = agent
            .prompt(format!("read page {turn}"))
            .await?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
    }
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let compact_fixture = || {
        let (builder, model) = placeholder_fixture(
            workspace.clone(),
            entry_base + 5_000,
            40_000,
            &[],
            &[],
            true,
            LocalDurabilityDisposition::DurableRequired,
        )?;
        model.make_compaction_success_non_shrinking(summary_bytes);
        Ok::<_, Box<dyn std::error::Error>>((builder, model))
    };
    let (compact_builder, compact_model) = compact_fixture()?;
    if recover {
        store.halt_after_checkpoint(identity.completed_checkpoint);
    }
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation_id.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            compact_builder,
        ))
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert_eq!(
        compact_model.paid_compaction_calls.load(Ordering::Acquire),
        1
    );
    let agent = if recover {
        assert!(
            matches!(
                outcome,
                AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
                    if checkpoint == identity.completed_checkpoint
            ),
            "{outcome:?}"
        );
        agent.shutdown().await?;
        store.resume_commits();
        store.expire_active_lease();
        let (restart_builder, restart_model) = compact_fixture()?;
        let recovered =
            Box::pin(sessions.recover_compaction(operation_id, source, restart_builder)).await?;
        assert_eq!(
            recovered,
            DurableCompactionRecoveryOutcome::Compacted {
                source,
                target: DurableSessionAddress::new(
                    source.session_id,
                    source.branch_id,
                    identity.success_checkpoint,
                ),
            }
        );
        assert_eq!(
            restart_model.paid_compaction_calls.load(Ordering::Acquire),
            0,
            "recovery publishes the recorded response without another dispatch"
        );
        None
    } else {
        assert!(
            matches!(outcome, AgentCompactSessionOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        Some(agent)
    };
    Ok(NativeEndpointCompaction {
        _directory: directory,
        store,
        sessions,
        agent,
        source,
        identity,
        operation_id,
    })
}

impl NativeEndpointCompaction {
    async fn published(&self) -> TestResult<(Vec<HistorySegment>, u32)> {
        let stored = self
            .store
            .load_checkpoint(self.identity.success_checkpoint)
            .await?;
        published_native_shape(stored.checkpoint())
    }

    async fn assert_receipt_compacted(&self) -> TestResult {
        assert!(matches!(
            self.sessions
                .compaction_receipt(self.operation_id, self.source)
                .await?,
            DurableCompactionReceipt::Compacted { .. }
        ));
        Ok(())
    }

    async fn shutdown(self) -> TestResult {
        if let Some(agent) = self.agent {
            agent.shutdown().await?;
        }
        Ok(())
    }
}

/// The published native successor and its provenance's trailing count.
fn published_native_shape(
    checkpoint: &RuntimeCheckpoint,
) -> TestResult<(Vec<HistorySegment>, u32)> {
    let provenance = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("native compaction provenance")?;
    assert_eq!(
        provenance.strategy(),
        RuntimeCompactionStrategy::NativeResponsesTriggerV1
    );
    Ok((
        checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .to_vec(),
        provenance.trailing_segment_count(),
    ))
}

fn compaction_position(segments: &[HistorySegment]) -> TestResult<usize> {
    segments
        .iter()
        .position(|segment| segment.kind() == HistorySegmentKind::Compaction)
        .ok_or_else(|| "native successor has no compaction segment".into())
}

#[tokio::test]
async fn native_successor_keeps_the_newest_batch_and_its_receipt_validates() -> TestResult {
    let run = native_endpoint_compaction(
        371_000,
        &[60_000, 2_000],
        0,
        "compact:native-tail-kept",
        false,
        None,
    )
    .await?;
    let source_stored = run.store.load_checkpoint(run.source.checkpoint_id).await?;
    let source_history = source_stored.checkpoint().manifest().canonical().history();
    let newest_batch = source_history
        .segments()
        .iter()
        .filter(|segment| segment.kind() == HistorySegmentKind::ToolExchange)
        .last()
        .cloned()
        .ok_or("source has a tool batch")?;
    let (segments, trailing) = run.published().await?;
    let summary = compaction_position(&segments)?;
    assert_eq!(
        usize::try_from(trailing)?,
        segments.len() - summary - 1,
        "every segment after the item is the recorded tail"
    );
    assert!(trailing >= 1);
    let kept = segments.get(summary + 1).ok_or("retained tail")?;
    assert_eq!(kept.kind(), HistorySegmentKind::ToolExchange);
    assert_eq!(kept.identity(), newest_batch.identity());
    assert_eq!(
        segments
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::ToolExchange)
            .count(),
        1,
        "the consumed older batch is not retained"
    );
    run.assert_receipt_compacted().await?;

    // A coherent successor of the other shape that the publication evidence
    // does not name: provenance, context and commit identity all agree with
    // the forged history, so only the receipt's shape check can refuse it.
    let identity = run.identity;
    let success = run
        .store
        .load_checkpoint(identity.success_checkpoint)
        .await?;
    let manifest = success.checkpoint().manifest();
    let source_manifest = source_stored.checkpoint().manifest();
    let provenance = manifest
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("provenance")?;
    let item = segments.get(summary).cloned().ok_or("item")?;
    let other = native_compaction_successor(
        source_history,
        item.clone(),
        NativeSuccessorShape::WithoutTail,
    )?
    .ok_or("tail-less successor")?;
    let (other_item, other) = record_summarized_turns(source_history, item, other)?;
    let operations = decode_paid_compaction_operations(success.checkpoint(), identity)?;
    let publication = SemanticCompactionPublication::new(
        identity.source.checkpoint_id,
        provenance.operation_identity(),
        source_manifest.binding().route_binding_identity()?,
        source_manifest.binding().model_binding(),
        operations.request.source_harness_plan,
        provenance.before_audit(),
        provenance.after_audit(),
        provenance.usage(),
    )?;
    let other_context = source_manifest
        .canonical()
        .context()
        .successful_native_compaction(
            &publication,
            &other_item,
            &other,
            operations.request.source_history,
            operations.request.cut,
            operations.request.target_affinity_entropy,
            source_history.segments().len(),
        )?;
    let canonical = RuntimeCanonicalState::new(
        manifest.canonical().stable_prefix().clone(),
        other,
        WorkingProjection::default(),
        other_context,
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
    let valid = lock_mutex(&run.store.state)
        .commits
        .get(&identity.success_checkpoint)
        .cloned()
        .ok_or("success commit receipt was not retained")?;
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
    {
        let mut state = lock_mutex(&run.store.state);
        state
            .stored_commits
            .insert(identity.success_checkpoint, forged_stored);
        state
            .commits
            .insert(identity.success_checkpoint, forged_receipt);
    }
    assert!(
        run.sessions
            .compaction_receipt(run.operation_id, run.source)
            .await
            .is_err(),
        "a successor the publication evidence does not name is corrupt"
    );
    {
        let mut state = lock_mutex(&run.store.state);
        state
            .stored_commits
            .insert(identity.success_checkpoint, success);
        state.commits.insert(identity.success_checkpoint, valid);
    }
    run.assert_receipt_compacted().await?;
    run.shutdown().await
}

#[tokio::test]
async fn protected_only_native_source_keeps_the_item_last() -> TestResult {
    // The whole session is the intent and its batch: the tail would only add
    // the item, so the tail-less successor is the one that shrinks.
    let run = native_endpoint_compaction(
        372_000,
        &[10_000],
        0,
        "compact:native-tail-protected-only",
        false,
        None,
    )
    .await?;
    let (segments, trailing) = run.published().await?;
    assert_eq!(trailing, 0);
    assert_eq!(compaction_position(&segments)?, segments.len() - 1);
    assert!(
        !segments
            .iter()
            .any(|segment| segment.kind() == HistorySegmentKind::ToolExchange)
    );
    run.assert_receipt_compacted().await?;
    run.shutdown().await
}

#[tokio::test]
async fn native_tail_that_does_not_shrink_falls_back_to_the_same_item() -> TestResult {
    // Dropping the small older batch does not pay for the item plus the large
    // current batch, so the with-tail successor does not shrink; the
    // tail-less successor from the same item does, without another dispatch.
    let run = native_endpoint_compaction(
        373_000,
        &[2_000, 20_000],
        10_000,
        "compact:native-tail-nonshrinking",
        false,
        None,
    )
    .await?;
    let (segments, trailing) = run.published().await?;
    assert_eq!(trailing, 0);
    assert_eq!(compaction_position(&segments)?, segments.len() - 1);
    run.assert_receipt_compacted().await?;
    run.shutdown().await
}

#[tokio::test]
async fn recovered_native_completion_publishes_the_same_tail() -> TestResult {
    let run = native_endpoint_compaction(
        374_000,
        &[60_000, 2_000],
        0,
        "compact:native-tail-recovered",
        true,
        None,
    )
    .await?;
    let (segments, trailing) = run.published().await?;
    let summary = compaction_position(&segments)?;
    assert!(trailing >= 1);
    assert_eq!(
        segments.get(summary + 1).map(HistorySegment::kind),
        Some(HistorySegmentKind::ToolExchange)
    );
    run.assert_receipt_compacted().await?;
    run.shutdown().await
}

#[tokio::test]
async fn mid_turn_native_successor_keeps_the_batch_and_the_continuation_note() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let bodies = [20_000, 20_000, 60_000, 20_000, 20_000, 60_000, 2_000];
    let (builder, model) = placeholder_fixture(
        workspace,
        375_000,
        40_000,
        &bodies,
        &(0..bodies.len()).collect::<Vec<_>>(),
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = builder.build()?;
    let result = agent.prompt("read every page").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(result.tool_calls(), bodies.len());
    assert!(model.paid_compaction_calls.load(Ordering::Acquire) >= 1);
    let checkpoint = agent.checkpoint();
    let (segments, trailing) = published_native_shape(&checkpoint)?;
    assert!(
        trailing >= 1,
        "the mid-turn successor keeps the current batch"
    );
    let summary = compaction_position(&segments)?;
    assert_eq!(
        segments.get(summary + 1).map(HistorySegment::kind),
        Some(HistorySegmentKind::ToolExchange)
    );
    let notes = segments
        .iter()
        .filter(|segment| segment.kind() == HistorySegmentKind::Steering)
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter(|block| {
            matches!(block, ContentBlock::Text(text)
                if text.text().starts_with(TURN_CONTINUATION_NOTE_MARKER))
        })
        .count();
    assert_eq!(notes, 1, "one continuation note for the turn");
    agent.shutdown().await?;
    Ok(())
}

/// A native successor that got a plan note is still a valid receipt for its
/// operation, live and from a later process, and recovery answers from it
/// without a provider call.
#[tokio::test]
async fn a_noted_native_successor_keeps_a_valid_receipt_across_reopen() -> TestResult {
    const NOTE: &str = "restored plan: native receipt";
    let run = native_endpoint_compaction(
        376_000,
        &[60_000, 2_000],
        0,
        "compact:native-noted",
        false,
        Some(NOTE),
    )
    .await?;
    let (segments, _) = run.published().await?;
    let last = segments.last().ok_or("empty successor")?;
    assert_eq!(last.kind(), HistorySegmentKind::Steering);
    assert!(matches!(
        last.items().first().and_then(|item| item.blocks().first()),
        Some(ContentBlock::Text(text)) if text.text() == NOTE
    ));
    run.assert_receipt_compacted().await?;
    let NativeEndpointCompaction {
        _directory: directory,
        store,
        agent,
        source,
        identity,
        operation_id,
        ..
    } = run;
    if let Some(agent) = agent {
        agent.shutdown().await?;
    }
    let reopened = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let target = DurableSessionAddress::new(
        source.session_id,
        source.branch_id,
        identity.success_checkpoint,
    );
    assert_eq!(
        reopened.compaction_receipt(operation_id, source).await?,
        DurableCompactionReceipt::Compacted { source, target }
    );
    let (builder, model) = placeholder_fixture(
        Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
        381_000,
        40_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    assert_eq!(
        Box::pin(reopened.recover_compaction(operation_id, source, builder)).await?,
        DurableCompactionRecoveryOutcome::Compacted { source, target }
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    Ok(())
}
