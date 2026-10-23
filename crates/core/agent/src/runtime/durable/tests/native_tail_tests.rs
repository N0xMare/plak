//! Codex native successors that keep the current round's tool batch.

use super::*;
use crate::SemanticCompactionPublication;
use crate::runtime::PostCompactionNote;
use crate::runtime::compaction::DurableCompactionRequestRecord;
use crate::runtime::compaction::continuation::{
    TURN_CONTINUATION_NOTE_MARKER, is_covered_harness_note,
};
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
    let builder = with_plan_note(builder.with_scheduled_compaction_enabled(false), plan_note);
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
    compact_native_endpoint(
        NativeEndpointSource {
            directory,
            workspace,
            store,
            sessions,
            agent,
        },
        entry_base,
        summary_bytes,
        operation_id,
        recover,
        plan_note.map(|note| -> PostCompactionNote { Arc::new(move |_, _| Some(note.to_owned())) }),
    )
    .await
}

fn with_plan_note(builder: AgentBuilder, plan_note: Option<&'static str>) -> AgentBuilder {
    with_note(
        builder,
        plan_note.map(|note| -> PostCompactionNote { Arc::new(move |_, _| Some(note.to_owned())) }),
    )
}

fn with_note(builder: AgentBuilder, note: Option<PostCompactionNote>) -> AgentBuilder {
    match note {
        Some(note) => builder.post_compaction_note(note),
        None => builder,
    }
}

/// A durable session whose head is the source of an endpoint compaction.
struct NativeEndpointSource {
    directory: tempfile::TempDir,
    workspace: Arc<WorkspacePolicy>,
    store: Arc<TestCreateStore>,
    sessions: DurableSessionRuntime,
    agent: Agent,
}

/// Runs an endpoint native compaction of `prepared`'s head, live or, with
/// `recover`, published by the next owner after the provider completion is
/// durable.
async fn compact_native_endpoint(
    prepared: NativeEndpointSource,
    entry_base: u128,
    summary_bytes: usize,
    operation_id: &'static str,
    recover: bool,
    note: Option<PostCompactionNote>,
) -> TestResult<NativeEndpointCompaction> {
    let NativeEndpointSource {
        directory,
        workspace,
        store,
        sessions,
        agent,
    } = prepared;
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
        let recovered = Box::pin(sessions.recover_compaction(
            operation_id,
            source,
            with_note(restart_builder, note),
        ))
        .await?;
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
    // does not name.
    let item = segments.get(summary).cloned().ok_or("item")?;
    let other = native_compaction_successor(
        source_history,
        item.clone(),
        NativeSuccessorShape::WithoutTail,
    )?
    .ok_or("tail-less successor")?;
    let (other_item, other) = record_summarized_turns(source_history, item, other)?;
    assert!(
        receipt_refuses_forged_successor(&run, &other_item, other).await?,
        "a successor the publication evidence does not name is corrupt"
    );
    run.assert_receipt_compacted().await?;
    run.shutdown().await
}

/// Stores `forged` (with compaction item `item`) as the success checkpoint,
/// with provenance, context and commit identity recomputed for it but the
/// recorded publication evidence unchanged, so the receipt can refuse it only
/// for its history (the successor shape, the published-successor check or the
/// recomputed context). Returns whether the receipt refused it, after
/// restoring the real checkpoint.
async fn receipt_refuses_forged_successor(
    run: &NativeEndpointCompaction,
    item: &HistorySegment,
    forged: CanonicalHistory,
) -> TestResult<bool> {
    let identity = run.identity;
    let source_stored = run.store.load_checkpoint(run.source.checkpoint_id).await?;
    let source_manifest = source_stored.checkpoint().manifest();
    let success = run
        .store
        .load_checkpoint(identity.success_checkpoint)
        .await?;
    let manifest = success.checkpoint().manifest();
    let provenance = manifest
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("provenance")?;
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
    let forged_context = source_manifest
        .canonical()
        .context()
        .successful_native_compaction(
            &publication,
            item,
            &forged,
            operations.request.source_history,
            operations.request.cut,
            operations.request.target_affinity_entropy,
            source_manifest.canonical().history().segments().len(),
        )?;
    let canonical = RuntimeCanonicalState::new(
        manifest.canonical().stable_prefix().clone(),
        forged,
        WorkingProjection::default(),
        forged_context,
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
    let refused = run
        .sessions
        .compaction_receipt(run.operation_id, run.source)
        .await
        .is_err();
    {
        let mut state = lock_mutex(&run.store.state);
        state
            .stored_commits
            .insert(identity.success_checkpoint, success);
        state.commits.insert(identity.success_checkpoint, valid);
    }
    Ok(refused)
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

/// A native compaction recovered after its owner was lost publishes the plan
/// note once, as the live publication would have, and its receipt validates
/// from a reopened runtime.
#[tokio::test]
async fn a_noted_native_compaction_recovered_after_owner_loss_keeps_its_note() -> TestResult {
    const NOTE: &str = "restored plan: native recovery";
    let run = native_endpoint_compaction(
        378_000,
        &[60_000, 2_000],
        0,
        "compact:native-noted-recovered",
        true,
        Some(NOTE),
    )
    .await?;
    let (segments, _) = run.published().await?;
    let notes = segments
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter(|block| matches!(block, ContentBlock::Text(text) if text.text() == NOTE))
        .count();
    assert_eq!(notes, 1, "the recovered successor holds the note once");
    let last = segments.last().ok_or("empty successor")?;
    assert_eq!(last.kind(), HistorySegmentKind::Steering);
    assert!(matches!(
        last.items().first().and_then(|item| item.blocks().first()),
        Some(ContentBlock::Text(text)) if text.text() == NOTE
    ));
    let reopened = DurableSessionRuntime::current(
        run.store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    assert_eq!(
        reopened
            .compaction_receipt(run.operation_id, run.source)
            .await?,
        DurableCompactionReceipt::Compacted {
            source: run.source,
            target: DurableSessionAddress::new(
                run.source.session_id,
                run.source.branch_id,
                run.identity.success_checkpoint,
            ),
        }
    );
    run.shutdown().await
}

impl NativeEndpointCompaction {
    /// The receipt re-read by a later process after this owner is gone.
    async fn assert_receipt_compacted_after_reopen(self) -> TestResult {
        if let Some(agent) = self.agent {
            agent.shutdown().await?;
        }
        let reopened = DurableSessionRuntime::current(
            self.store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        assert_eq!(
            reopened
                .compaction_receipt(self.operation_id, self.source)
                .await?,
            DurableCompactionReceipt::Compacted {
                source: self.source,
                target: DurableSessionAddress::new(
                    self.source.session_id,
                    self.source.branch_id,
                    self.identity.success_checkpoint,
                ),
            }
        );
        Ok(())
    }
}

fn continuation_notes(segments: &[HistorySegment]) -> Vec<usize> {
    segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| {
            is_covered_harness_note(segment)
                && segment
                    .items()
                    .iter()
                    .flat_map(ConversationItem::blocks)
                    .any(|block| {
                        matches!(block, ContentBlock::Text(text)
                            if text.text().starts_with(TURN_CONTINUATION_NOTE_MARKER))
                    })
        })
        .map(|(index, _)| index)
        .collect()
}

/// Asserts that `segments`, published with provenance `trailing`, is the pure
/// native derivation from `source` followed only by what publication appends:
/// the note `note` derives for that successor, then at most the continuation
/// note. No harness note sits ahead of the compaction item. Returns the
/// derived successor's length.
fn assert_pure_native_derivation(
    source: &CanonicalHistory,
    segments: &[HistorySegment],
    trailing: u32,
    note: Option<&PostCompactionNote>,
) -> TestResult<usize> {
    let item = compaction_position(segments)?;
    assert!(
        !segments.iter().take(item).any(is_covered_harness_note),
        "a harness note sits ahead of the compaction item: {:?}",
        kinds(segments)
    );
    let shape = if trailing == 0 {
        NativeSuccessorShape::WithoutTail
    } else {
        NativeSuccessorShape::WithTail
    };
    let summary = segments.get(item).cloned().ok_or("item")?;
    let derived =
        native_compaction_successor(source, summary.clone(), shape)?.ok_or("published shape")?;
    let (_, derived) = record_summarized_turns(source, summary, derived)?;
    let length = derived.segments().len();
    assert!(
        segments.get(..length).map(<[HistorySegment]>::to_vec) == Some(derived.segments().to_vec()),
        "the published successor is the source derivation: {:?}",
        kinds(segments)
    );
    let appended = segments.get(length..).unwrap_or_default();
    assert!(appended.len() <= 2 && appended.iter().all(is_covered_harness_note));
    let expected = note.and_then(|derive| derive(source, &derived));
    let first = appended.first().and_then(|segment| match segment.items() {
        [item] => match item.blocks() {
            [ContentBlock::Text(text)] => Some(text.text().to_owned()),
            _ => None,
        },
        _ => None,
    });
    match expected {
        Some(text) => assert_eq!(first, Some(text), "the derived note comes first"),
        None => assert!(
            first.is_none_or(|text| text.starts_with(TURN_CONTINUATION_NOTE_MARKER)),
            "no post-compaction note was derived"
        ),
    }
    Ok(length)
}

fn kinds(segments: &[HistorySegment]) -> Vec<HistorySegmentKind> {
    segments.iter().map(HistorySegment::kind).collect()
}

/// Bodies that cross a 40k-token fence more than once in one turn.
const REPEATED_FENCE_BODIES: [usize; 7] = [20_000, 20_000, 60_000, 20_000, 20_000, 60_000, 2_000];

/// One durable turn whose tool rounds cross the fence repeatedly, so the turn
/// compacts at its fences more than once.
async fn turn_with_repeated_fences(
    entry_base: u128,
    create: &str,
    native: bool,
    note: Option<PostCompactionNote>,
) -> TestResult<(NativeEndpointSource, Arc<ArtifactModel>)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, model) = placeholder_fixture(
        workspace.clone(),
        entry_base,
        40_000,
        &REPEATED_FENCE_BODIES,
        &(0..REPEATED_FENCE_BODIES.len()).collect::<Vec<_>>(),
        native,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = sessions.create(create, with_note(builder, note)).await?;
    let result = agent.prompt("read every page").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(
        model.paid_compaction_calls.load(Ordering::Acquire) >= 2,
        "the turn compacts at more than one fence"
    );
    Ok((
        NativeEndpointSource {
            directory,
            workspace,
            store,
            sessions,
            agent,
        },
        model,
    ))
}

/// Every paid compaction the store recorded, by operation id.
fn recorded_compactions(store: &TestCreateStore) -> BTreeMap<String, DurableSessionAddress> {
    let state = lock_mutex(&store.state);
    state
        .stored_commits
        .values()
        .flat_map(|stored| stored.checkpoint().manifest().ledger().operations())
        .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .filter_map(|record| {
            serde_json::from_slice::<DurableCompactionRequestRecord>(record.request().bytes()).ok()
        })
        .map(|request| (request.operation_id.clone(), request.source()))
        .collect()
}

/// Each native compaction at a fence of one turn ends its retained tail with
/// its own continuation note; no note is kept ahead of a compaction item, the
/// provenance counts only the tail, and every receipt re-reads as compacted,
/// live and from a later process.
#[tokio::test]
async fn repeated_native_compactions_in_one_turn_end_each_tail_with_a_fresh_note() -> TestResult {
    let (prepared, _model) =
        turn_with_repeated_fences(384_000, "create:native-repeated-notes", true, None).await?;
    let compactions = recorded_compactions(&prepared.store);
    let mut fresh = BTreeSet::new();
    let mut with_tail = 0;
    for (operation_id, source) in &compactions {
        let receipt = prepared
            .sessions
            .compaction_receipt(operation_id, *source)
            .await?;
        let DurableCompactionReceipt::Compacted { target, .. } = receipt else {
            return Err(format!("{operation_id}: {receipt:?}").into());
        };
        let success = prepared.store.load_checkpoint(target.checkpoint_id).await?;
        let (segments, trailing) = published_native_shape(success.checkpoint())?;
        let source_stored = prepared.store.load_checkpoint(source.checkpoint_id).await?;
        assert_pure_native_derivation(
            source_stored.checkpoint().manifest().canonical().history(),
            &segments,
            trailing,
            None,
        )?;
        let item = compaction_position(&segments)?;
        let notes = continuation_notes(&segments);
        assert!(
            notes.iter().all(|note| *note > item),
            "a continuation note precedes the compaction item: {:?}",
            kinds(&segments)
        );
        if trailing == 0 {
            assert!(notes.is_empty(), "{:?}", kinds(&segments));
            continue;
        }
        assert_eq!(
            segments.get(item + 1).map(HistorySegment::kind),
            Some(HistorySegmentKind::ToolExchange)
        );
        assert_eq!(
            notes,
            vec![segments.len() - 1],
            "the note follows the retained tail: {:?}",
            kinds(&segments)
        );
        assert_eq!(
            usize::try_from(trailing)?,
            segments.len() - item - 2,
            "the provenance counts the retained tail, not the note"
        );
        let note = segments
            .last()
            .and_then(|segment| segment.items().first())
            .ok_or("note item")?;
        fresh.insert(note.id());
        with_tail += 1;
    }
    assert!(
        with_tail >= 2,
        "more than one native successor kept its tail: {with_tail} of {}",
        compactions.len()
    );
    assert_eq!(
        fresh.len(),
        with_tail,
        "each compaction appends its own note"
    );

    let checkpoint = prepared.agent.checkpoint();
    let (segments, _) = published_native_shape(&checkpoint)?;
    let notes = continuation_notes(&segments);
    assert_eq!(notes.len(), 1, "{:?}", kinds(&segments));
    assert!(notes[0] > compaction_position(&segments)?);
    prepared.agent.shutdown().await?;

    let reopened = DurableSessionRuntime::current(
        prepared.store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    for (operation_id, source) in &compactions {
        assert!(
            matches!(
                reopened.compaction_receipt(operation_id, *source).await?,
                DurableCompactionReceipt::Compacted { .. }
            ),
            "{operation_id}"
        );
    }
    Ok(())
}

/// A native compaction of a head that carries a turn's continuation note
/// publishes the same successor live and after owner loss: the pure native
/// derivation from the source, without the earlier note, with a receipt that
/// re-reads from a later process.
#[tokio::test]
async fn a_native_successor_drops_the_earlier_note_live_and_recovered() -> TestResult {
    let shapes = native_endpoint_parity(
        [
            (386_000, "compact:native-drops-note-live"),
            (392_000, "compact:native-drops-note-recovered"),
        ],
        None,
    )
    .await?;
    for (segments, _) in &shapes {
        assert!(
            continuation_notes(segments).is_empty(),
            "{:?}",
            kinds(segments)
        );
    }
    Ok(())
}

/// Runs a turn with repeated native fences and then an endpoint native
/// compaction of its head, once live and once recovered after owner loss,
/// with the same post-compaction note function. Both publish the source
/// derivation, the same shape, and receipts that re-read live and from a
/// later process. Returns each published successor and its trailing count.
async fn native_endpoint_parity(
    runs: [(u128, &'static str); 2],
    note: Option<PostCompactionNote>,
) -> TestResult<Vec<(Vec<HistorySegment>, u32)>> {
    let mut published = Vec::new();
    for ((entry_base, operation_id), recover) in runs.into_iter().zip([false, true]) {
        let (prepared, _model) = turn_with_repeated_fences(
            entry_base,
            &format!("create:{operation_id}"),
            true,
            note.clone(),
        )
        .await?;
        let source = prepared
            .agent
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .clone();
        assert!(
            !continuation_notes(&source.segments().to_vec()).is_empty(),
            "the source carries the turn's note"
        );
        assert!(
            note.is_none() || !plan_notes(&source.segments().to_vec()).is_empty(),
            "the source carries a plan note"
        );
        let run =
            compact_native_endpoint(prepared, entry_base, 0, operation_id, recover, note.clone())
                .await?;
        let (segments, trailing) = run.published().await?;
        assert_pure_native_derivation(&source, &segments, trailing, note.as_ref())?;
        run.assert_receipt_compacted().await?;
        run.assert_receipt_compacted_after_reopen().await?;
        published.push((segments, trailing));
    }
    let shape = |(segments, trailing): &(Vec<HistorySegment>, u32)| (kinds(segments), *trailing);
    assert_eq!(shape(&published[0]), shape(&published[1]));
    Ok(published)
}

/// A semantic fold is unchanged: steering is never folded inside its own
/// turn, so the first mid-turn fold's note stays and later folds append none.
/// The turn holds one note, after the summary and just before a tool batch.
#[tokio::test]
async fn repeated_semantic_folds_in_one_turn_keep_the_first_note() -> TestResult {
    let (prepared, _model) =
        turn_with_repeated_fences(393_000, "create:semantic-repeated-notes", false, None).await?;
    let checkpoint = prepared.agent.checkpoint();
    let manifest = checkpoint.manifest();
    assert_eq!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("fold provenance")?
            .strategy(),
        RuntimeCompactionStrategy::SemanticSummaryPrefixV1
    );
    let segments = manifest.canonical().history().segments().to_vec();
    let notes = continuation_notes(&segments);
    assert_eq!(notes.len(), 1, "{:?}", kinds(&segments));
    assert!(notes[0] > compaction_position(&segments)?);
    assert_eq!(
        segments.get(notes[0] + 1).map(HistorySegment::kind),
        Some(HistorySegmentKind::ToolExchange),
        "{:?}",
        kinds(&segments)
    );
    let mut first_note = None;
    for (operation_id, source) in recorded_compactions(&prepared.store) {
        let DurableCompactionReceipt::Compacted { target, .. } = prepared
            .sessions
            .compaction_receipt(&operation_id, source)
            .await?
        else {
            return Err(format!("{operation_id} did not compact").into());
        };
        let success = prepared.store.load_checkpoint(target.checkpoint_id).await?;
        let published = success
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .to_vec();
        if let [note] = continuation_notes(&published)[..] {
            let id = published[note].items().first().ok_or("note item")?.id();
            assert!(
                first_note.is_none_or(|first| first == id),
                "one note per turn"
            );
            first_note = Some(id);
        }
    }
    assert_eq!(
        first_note,
        segments[notes[0]].items().first().map(ConversationItem::id),
        "the note the turn ends with is the first fold's"
    );
    prepared.agent.shutdown().await?;
    Ok(())
}

/// Opens every note [`round_plan_note`] derives.
const ROUND_PLAN_HEADER: &str = "restored plan (test)";

/// A plan-like post-compaction note whose body names how many tool rounds the
/// source holds, so most compactions derive a changed plan, and which derives
/// nothing when the successor already shows that exact note.
fn round_plan_note() -> PostCompactionNote {
    Arc::new(|source: &CanonicalHistory, successor: &CanonicalHistory| {
        let rounds = source
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::ToolExchange)
            .count();
        let text = format!("{ROUND_PLAN_HEADER}\n- [in_progress] round {rounds}");
        let shown = successor
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Text(note) if note.text() == text));
        (!shown).then_some(text)
    })
}

/// The first line of a steering segment's text.
fn note_header_of(segment: &HistorySegment) -> Option<&str> {
    if segment.kind() != HistorySegmentKind::Steering {
        return None;
    }
    segment
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::Text(text) => text.text().split('\n').next(),
            _ => None,
        })
}

fn plan_notes(segments: &[HistorySegment]) -> Vec<usize> {
    segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| note_header_of(segment) == Some(ROUND_PLAN_HEADER))
        .map(|(index, _)| index)
        .collect()
}

/// Each native compaction at a fence of one turn carries at most one plan
/// note, where a single compaction puts it: after the retained tail and
/// before the continuation note, or last behind a tail-less item. An earlier
/// plan note is not retained ahead of the item, and every receipt re-reads
/// as compacted, live and from a later process.
#[tokio::test]
async fn repeated_native_compactions_in_one_turn_put_the_plan_note_after_the_tail() -> TestResult {
    let (prepared, _model) = turn_with_repeated_fences(
        394_000,
        "create:native-repeated-plan",
        true,
        Some(round_plan_note()),
    )
    .await?;
    let compactions = recorded_compactions(&prepared.store);
    let note = round_plan_note();
    let mut superseding = 0;
    for (operation_id, source) in &compactions {
        let receipt = prepared
            .sessions
            .compaction_receipt(operation_id, *source)
            .await?;
        let DurableCompactionReceipt::Compacted { target, .. } = receipt else {
            return Err(format!("{operation_id}: {receipt:?}").into());
        };
        let success = prepared.store.load_checkpoint(target.checkpoint_id).await?;
        let (segments, trailing) = published_native_shape(success.checkpoint())?;
        let source_stored = prepared.store.load_checkpoint(source.checkpoint_id).await?;
        let source_history = source_stored.checkpoint().manifest().canonical().history();
        assert_pure_native_derivation(source_history, &segments, trailing, Some(&note))?;
        if !plan_notes(&source_history.segments().to_vec()).is_empty() {
            superseding += 1;
        }
        let item = compaction_position(&segments)?;
        let plans = plan_notes(&segments);
        assert!(plans.len() <= 1, "{:?}", kinds(&segments));
        let Some(&plan) = plans.first() else {
            continue;
        };
        assert!(plan > item, "{:?}", kinds(&segments));
        if trailing == 0 {
            assert_eq!(plan, segments.len() - 1, "{:?}", kinds(&segments));
        } else {
            assert_eq!(
                continuation_notes(&segments),
                vec![segments.len() - 1],
                "{:?}",
                kinds(&segments)
            );
            assert_eq!(plan, segments.len() - 2, "{:?}", kinds(&segments));
            assert_eq!(
                usize::try_from(trailing)?,
                segments.len() - item - 3,
                "the provenance counts the retained tail, not the notes"
            );
        }
    }
    assert!(
        superseding >= 1,
        "a native compaction had an earlier plan note in its source: {superseding} of {}",
        compactions.len()
    );
    let checkpoint = prepared.agent.checkpoint();
    let (segments, _) = published_native_shape(&checkpoint)?;
    assert_eq!(plan_notes(&segments).len(), 1, "{:?}", kinds(&segments));
    prepared.agent.shutdown().await?;
    let reopened = DurableSessionRuntime::current(
        prepared.store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    for (operation_id, source) in &compactions {
        assert!(
            matches!(
                reopened.compaction_receipt(operation_id, *source).await?,
                DurableCompactionReceipt::Compacted { .. }
            ),
            "{operation_id}"
        );
    }
    Ok(())
}

/// An endpoint native compaction of a head that carries a plan note keeps none
/// of it ahead of the item and ends with the note derived for the successor,
/// live and after owner loss alike.
#[tokio::test]
async fn a_native_successor_ends_with_a_fresh_plan_note_live_and_recovered() -> TestResult {
    let published = native_endpoint_parity(
        [
            (395_000, "compact:native-plan-live"),
            (401_000, "compact:native-plan-recovered"),
        ],
        Some(round_plan_note()),
    )
    .await?;
    for (segments, _) in &published {
        assert_eq!(
            plan_notes(segments),
            vec![segments.len() - 1],
            "{:?}",
            kinds(segments)
        );
    }
    Ok(())
}

/// A receipt refuses a native successor that retains a harness note ahead of
/// its compaction item, even with provenance and context recomputed for it.
#[tokio::test]
async fn a_receipt_refuses_a_native_successor_retaining_a_harness_note() -> TestResult {
    let operation_id = "compact:native-forged-note";
    let note = round_plan_note();
    let (prepared, _model) = turn_with_repeated_fences(
        402_000,
        "create:native-forged-note",
        true,
        Some(Arc::clone(&note)),
    )
    .await?;
    let source = prepared
        .agent
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .clone();
    let retained = source
        .segments()
        .iter()
        .find(|segment| is_covered_harness_note(segment))
        .cloned()
        .ok_or("the source carries a harness note")?;
    let run = compact_native_endpoint(
        prepared,
        402_000,
        0,
        operation_id,
        false,
        Some(note.clone()),
    )
    .await?;
    let (segments, trailing) = run.published().await?;
    let length = assert_pure_native_derivation(&source, &segments, trailing, Some(&note))?;
    let item = compaction_position(&segments)?;
    let mut forged = segments.get(..length).ok_or("successor")?.to_vec();
    forged.insert(item, retained);
    let summary = segments.get(item).cloned().ok_or("item")?;
    assert!(
        receipt_refuses_forged_successor(&run, &summary, CanonicalHistory::new(forged)?).await?,
        "a successor retaining a harness note ahead of its item is corrupt"
    );
    run.assert_receipt_compacted().await?;
    run.shutdown().await
}
