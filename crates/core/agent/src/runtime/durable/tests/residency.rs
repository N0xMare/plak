use super::*;
use crate::runtime::compaction::residency::record::{ResidencyEvidence, ResidencyRequest};
use crate::runtime::compaction::residency::{ImageTotals, image_totals};
use crate::runtime::durable::turn_recovery::{
    RootClosure, close_root_turn_for_test, root_turn_closure_for_test,
};
use plakous_model::{ImageBlock, MediaSource};

fn image_history(protected: usize) -> TestResult<CanonicalHistory> {
    let mut segments = Vec::new();
    for (id, count) in [(1, 7), (2, 7), (3, 7), (4, protected)] {
        let mut blocks = vec![ContentBlock::Text(TextBlock::new("retained intent", None)?)];
        for _ in 0..count {
            blocks.push(ContentBlock::Image(ImageBlock::new(
                "image/png",
                MediaSource::bytes(vec![1; 3_000])?,
            )?));
        }
        segments.push(HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(id))?,
                Actor::User,
                blocks,
                Provenance::UserInput,
            )?],
        )?);
    }
    Ok(CanonicalHistory::new(segments)?)
}

fn image_descriptor(limit: u64) -> TestResult<ResolvedModelDescriptor> {
    let base = placeholder_test_descriptor(limit, false)?;
    let mut features = base.capabilities().features().clone();
    features.insert(
        Capability::ImageInput,
        base.capabilities()
            .get(Capability::Tools)
            .ok_or("tools support")?
            .clone(),
    );
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        base.protocol().clone(),
        base.route().clone(),
        base.requested_model().clone(),
        base.reported_model().cloned(),
        base.model_family().cloned(),
        base.profile().clone(),
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(base.context_contract().ok_or("context contract")?.clone()))
}

fn image_builder(
    workspace: Arc<WorkspacePolicy>,
    limit: u64,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    image_builder_pausing(workspace, limit, 0, &[])
}

/// [`image_builder`] whose first `candidate_calls` responses carry a family
/// artifact and whose `pauses` calls answer with a server pause.
fn image_builder_pausing(
    workspace: Arc<WorkspacePolicy>,
    limit: u64,
    candidate_calls: usize,
    pauses: &[usize],
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let (builder, model) = placeholder_fixture_with_descriptor(
        workspace,
        0xA100_0000,
        image_descriptor(limit)?,
        &[],
        &[],
        LocalDurabilityDisposition::DurableRequired,
        candidate_calls,
        pauses,
    )?;
    Ok((builder.with_scheduled_compaction_enabled(false), model))
}

#[tokio::test]
async fn admission_residency_is_one_unpaid_cas_with_reconstructive_recovery_and_feed() -> TestResult
{
    admission_residency()
        .await
        .map_err(|error| error.to_string().into())
}

async fn admission_residency() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = image_builder(workspace, 1_000_000)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let original = image_history(1)?;
    let agent = sessions
        .create_seeded("create:image-residency", builder, original.clone())
        .await?;
    let result = agent.prompt("continue with images").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "offline pass dispatches no model request"
    );
    assert!(lock_mutex(&model.paid_compaction_rounds).is_empty());
    let commits = lock_mutex(&store.accepted_commits).clone();
    let published: Vec<_> = commits
        .iter()
        .filter(|(checkpoint, _)| {
            checkpoint
                .manifest()
                .ledger()
                .operations()
                .iter()
                .any(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
                && checkpoint.manifest().canonical().history().identity() != original.identity()
        })
        .collect();
    let (cut, facts) = published.first().ok_or("residency commit")?;
    let record = cut
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .ok_or("compaction record")?;
    let DurableOperationKind::Compaction { workflow_id } = record.kind() else {
        return Err("compaction".into());
    };
    let request = ResidencyRequest::decode(record.request().bytes(), *workflow_id)
        .ok_or("canonical v3 request")?;
    assert_canonical_decoders(record, &request)?;
    let source = commits
        .iter()
        .find(|(checkpoint, _)| checkpoint.id() == request.source_checkpoint)
        .ok_or("source commit")?
        .0
        .clone();
    let identity = DurableCompactionIdentity::new(
        &request.operation_id,
        DurableSessionAddress::new(
            request.source_session,
            request.source_branch,
            request.source_checkpoint,
        ),
    )?;
    assert_recovery_cut(cut, &source, identity)?;
    assert_repeat_receipt(&sessions, &store, &request.operation_id, identity, cut.id())
        .await
        .map_err(|error| format!("repeat receipt: {error}"))?;
    assert!(facts.iter().any(|fact| matches!(
        fact,
        crate::feed::FeedFact::WorkflowSettled {
            outcome: crate::feed::FeedWorkflowOutcome::Compacted {
                pass: crate::feed::FeedCompactPass::Residency,
                ..
            },
            ..
        }
    )));
    let bundle = RuntimeCheckpointCodec::bundle(cut, None)?;
    let reopened = RuntimeCheckpointCodec::materialize(&bundle)?;
    for checkpoint in [&**cut, &reopened] {
        assert_closed_recovery(checkpoint, &source, identity)
            .map_err(|error| format!("closure: {error}"))?;
    }
    assert_eq!(
        image_totals(&close_root_turn_for_test(&source)?.0, ImageTotals::ZERO).count,
        22
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

/// A source over the token window whose more than 20 images are mostly in
/// foldable history: the image-only candidate cannot fit the window, so it
/// defers to the ordinary ladder, whose one paid fold leaves both fitting.
fn over_window_image_history() -> TestResult<CanonicalHistory> {
    let item = |id, actor, blocks| -> TestResult<ConversationItem> {
        let provenance = if actor == Actor::User {
            Provenance::UserInput
        } else {
            Provenance::ModelOutput
        };
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(id))?,
            actor,
            blocks,
            provenance,
        )?)
    };
    let images = |label: &str, count| -> TestResult<Vec<ContentBlock>> {
        let mut blocks = vec![ContentBlock::Text(TextBlock::new(label, None)?)];
        for _ in 0..count {
            blocks.push(ContentBlock::Image(ImageBlock::new(
                "image/png",
                MediaSource::bytes(vec![1; 100])?,
            )?));
        }
        Ok(blocks)
    };
    let call = CallId::try_from(uuid::Uuid::from_u128(99))?;
    Ok(CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![item(1, Actor::User, images("older intent", 21)?)?],
        )?,
        HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![
                item(
                    2,
                    Actor::Assistant,
                    vec![ContentBlock::ToolCall(ToolCallBlock::new(
                        call,
                        ToolName::new("probe")?,
                        ToolExecutionOwner::Local,
                        ToolCallState::Requested,
                        ToolCorrelation::default(),
                        "{}",
                    )?)],
                )?,
                item(
                    3,
                    Actor::Tool,
                    vec![ContentBlock::ToolResult(ToolResultBlock::new(
                        call,
                        ToolExecutionOwner::Local,
                        ToolResultKind::Success,
                        vec![ToolResultPart::Text(TextBlock::new(
                            "x".repeat(240_000),
                            None,
                        )?)],
                        ToolCorrelation::default(),
                    )?)],
                )?,
            ],
        )?,
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![item(4, Actor::User, images("latest protected goal", 1)?)?],
        )?,
    ])?)
}

#[tokio::test]
async fn over_window_residency_defers_to_the_enabled_ordinary_ladder() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = image_builder(workspace, 40_000)?;
    let source = over_window_image_history()?;
    assert!(u64::try_from(source.retained_bytes_upper_bound())?.div_ceil(4) > 39_872);
    assert_eq!(image_totals(&source, ImageTotals::ZERO).count, 22);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create_seeded(
            "create:over-window-residency",
            builder.with_scheduled_compaction_enabled(true),
            source,
        )
        .await?;
    let result = agent.prompt("continue").await?.result().await?;
    if let TurnResultOutcome::Failed(error) = result.outcome() {
        return Err(format!("enabled ladder: {}", error.message()).into());
    }
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(
        lock_mutex(&model.paid_compaction_rounds).len(),
        1,
        "one paid fold, never redispatched"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "the paid fold and the admitted round"
    );
    let checkpoint = agent.checkpoint();
    assert_eq!(compaction_workflow_count(&checkpoint), 1);
    assert_ne!(
        checkpoint
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("provenance")?
            .strategy(),
        RuntimeCompactionStrategy::ResidencyPlaceholdersV1,
        "the deferred image-only candidate never published"
    );
    assert!(
        image_totals(
            checkpoint.manifest().canonical().history(),
            ImageTotals::ZERO
        )
        .fits(plakous_model::ResidentImageBudget::default())
    );
    agent.shutdown().await?;
    Ok(())
}

/// A committed source below the scheduling trigger with 22 images, and a
/// prompt that carries the next request over the token window.
fn prompt_crossing_source() -> TestResult<(CanonicalHistory, String)> {
    let user = |id, text: String, count| -> TestResult<ConversationItem> {
        let mut blocks = vec![ContentBlock::Text(TextBlock::new(text, None)?)];
        for _ in 0..count {
            blocks.push(ContentBlock::Image(ImageBlock::new(
                "image/png",
                MediaSource::bytes(vec![1; 100])?,
            )?));
        }
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(id))?,
            Actor::User,
            blocks,
            Provenance::UserInput,
        )?)
    };
    let source = CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![user(1, format!("older intent {}", "x".repeat(60_000)), 21)?],
        )?,
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user(2, "latest protected goal".to_owned(), 1)?],
        )?,
    ])?;
    let prompt = format!("follow-up {}", "y".repeat(24_000));
    let source_tokens = u64::try_from(source.retained_bytes_upper_bound())?.div_ceil(4);
    assert!(
        source_tokens < 35_648,
        "source must stay below the trigger: {source_tokens}"
    );
    Ok((source, prompt))
}

/// The committed source fits below the scheduling trigger, so admission does
/// not fold; the appended prompt then crosses the window. The round-start
/// image check must give the ordinary ladder that cut before it refuses.
#[tokio::test]
async fn prompt_crossing_the_window_defers_residency_to_the_round_start_ladder() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = image_builder(workspace, 40_000)?;
    let (source, prompt) = prompt_crossing_source()?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create_seeded(
            "create:prompt-crossing-residency",
            builder.with_scheduled_compaction_enabled(true),
            source,
        )
        .await?;
    let result = agent.prompt(prompt).await?.result().await?;
    if let TurnResultOutcome::Failed(error) = result.outcome() {
        return Err(format!("round-start ladder: {}", error.message()).into());
    }
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let paid = lock_mutex(&model.paid_compaction_rounds).len();
    assert_eq!(paid, 1, "one paid fold, never redispatched");
    assert_eq!(model.calls.load(Ordering::Acquire), paid + 1);
    assert!(
        image_totals(
            agent.checkpoint().manifest().canonical().history(),
            ImageTotals::ZERO
        )
        .fits(plakous_model::ResidentImageBudget::default())
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_hold_deferred_residency_parks_before_the_first_paid_fold() -> TestResult {
    use crate::control::boundary::{BoundaryError, BoundaryProgress};
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = image_builder(workspace, 40_000)?;
    let (source, prompt) = prompt_crossing_source()?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create_seeded(
            "create:boundary-residency",
            builder.with_scheduled_compaction_enabled(true),
            source,
        )
        .await?;
    let mut endpoint = Some(agent.session_endpoint());
    let requested = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&requested);
    *lock_mutex(&model.observe_scheduling_prepare) = Some(Box::new(move |plan| {
        if plan.request().input().items().iter().flat_map(ConversationItem::blocks).any(
            |block| matches!(block, ContentBlock::Text(text) if text.text().starts_with("follow-up ")),
        ) && let Some(endpoint) = endpoint.take() {
            *lock_mutex(&capture) = Some(endpoint.hold_step_boundary());
        }
    }));
    let prompting = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt(prompt).await }
    });
    let mut hold = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(hold) = lock_mutex(&requested).take() {
                return hold;
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !matches!(
            hold.progress()?,
            BoundaryProgress::Held { turn: Some(_), .. }
        ) {
            hold.changed().await?;
        }
        Ok::<_, BoundaryError>(())
    })
    .await??;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert!(lock_mutex(&model.paid_compaction_rounds).is_empty());
    hold.release().await?;
    let result = prompting.await??.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(lock_mutex(&model.paid_compaction_rounds).len(), 1);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}

/// Round-start fold cuts: halting the store after every accepted commit of
/// the prompt-crossing turn reopens exactly, without a provider dispatch, a
/// second paid fold or a republished compaction.
#[tokio::test]
async fn round_start_fold_commit_cuts_reopen_without_redispatch() -> TestResult {
    let mut total = None;
    let mut cut = 0;
    loop {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let (builder, model) = image_builder(Arc::clone(&workspace), 40_000)?;
        let (source, prompt) = prompt_crossing_source()?;
        let store = Arc::new(TestCreateStore::default());
        store
            .halt_after_accepted_commit
            .store(cut, Ordering::Release);
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create_seeded(
                "create:round-start-fold-cut",
                builder.with_scheduled_compaction_enabled(true),
                source,
            )
            .await?;
        if let Ok(turn) = agent.prompt(prompt).await {
            let _ = tokio::time::timeout(Duration::from_secs(30), turn.result()).await?;
        }
        let address = DurableSessionAddress::new(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
        );
        let _ = agent.shutdown().await;
        let accepted = lock_mutex(&store.accepted_commits).clone();
        let paid = lock_mutex(&model.paid_compaction_rounds).len();
        let Some(total) = total else {
            assert_eq!(paid, 1, "baseline pays one round-start fold");
            assert!(accepted.len() >= 3, "{} accepted commits", accepted.len());
            total = Some(accepted.len());
            cut = 1;
            continue;
        };
        if cut >= total {
            break;
        }
        assert_eq!(accepted.len(), cut, "cut {cut}");
        assert!(paid <= 1, "cut {cut}: {paid} paid folds");
        assert!(model.calls.load(Ordering::Acquire) <= 2, "cut {cut}");
        let has_compaction = |checkpoint: &RuntimeCheckpoint| {
            checkpoint
                .manifest()
                .canonical()
                .history()
                .segments()
                .iter()
                .any(|segment| segment.kind() == HistorySegmentKind::Compaction)
        };
        let folded = accepted
            .last()
            .is_some_and(|(checkpoint, _)| has_compaction(checkpoint));
        store.resume_commits();
        store.halt_after_accepted_commit.store(0, Ordering::Release);
        store.expire_active_lease();
        let head = store
            .load_branch(address.session())
            .await?
            .head()
            .checkpoint()
            .checkpoint_id();
        let (reopen_builder, reopen_model) = image_builder(workspace, 40_000)?;
        let opened = sessions
            .open(
                "open:round-start-fold-cut",
                DurableSessionAddress::new(address.session_id, address.branch_id, head),
                reopen_builder.with_scheduled_compaction_enabled(true),
            )
            .await?;
        let DurableRuntimeOpen::Opened(reopened) = opened else {
            return Err(format!("cut {cut}: reopen was not exact").into());
        };
        assert_eq!(
            reopen_model.calls.load(Ordering::Acquire),
            0,
            "cut {cut}: reopen dispatched"
        );
        let checkpoint = reopened.checkpoint();
        assert!(
            matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Quiescent
            ),
            "cut {cut}"
        );
        assert!(compaction_workflow_count(&checkpoint) <= 1, "cut {cut}");
        if folded {
            assert!(has_compaction(&checkpoint), "cut {cut}: fold lost");
        }
        reopened.shutdown().await?;
        cut += 1;
    }
    Ok(())
}

fn png_parts(count: usize) -> Vec<crate::UserPart> {
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&4_u32.to_be_bytes());
    png.extend_from_slice(&3_u32.to_be_bytes());
    png.resize(png.len() + 64, 0);
    (0..count)
        .map(|_| crate::UserPart::Image {
            media_type: "image/png".to_owned(),
            bytes: png.clone(),
        })
        .collect()
}

fn assert_finalized_continuation(
    result: &TurnResult,
    continuation: fn(&DurableOperationKind) -> bool,
) -> TestResult {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!("expected RequestBudget, got {:?}", result.outcome()).into());
    };
    assert_eq!(error.kind(), AgentErrorKind::RequestBudget, "{error:?}");
    let checkpoint = result.checkpoint();
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    let operations = checkpoint.manifest().ledger().operations();
    let opened = operations
        .iter()
        .filter(|record| continuation(record.kind()))
        .collect::<Vec<_>>();
    assert_eq!(opened.len(), 1);
    assert!(matches!(
        opened[0].state(),
        DurableOperationState::Finalized(_)
    ));
    Ok(())
}

/// Steering that arrives during a server pause is protected, so its images
/// can exceed the budget with nothing removable; the refusal still closes the
/// pause continuation and the turn instead of leaving the head Working.
#[tokio::test]
async fn server_pause_steered_images_refuse_and_finalize_the_pause() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = image_builder_pausing(workspace, 1_000_000, 1, &[0])?;
    let release = Arc::new(Notify::new());
    model.held_call.store(0, Ordering::Release);
    *lock_mutex(&model.held_call_release) = Some(Arc::clone(&release));
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create("create:pause-steer-images", builder)
        .await?;
    let turn = agent.prompt(crate::UserInput::new(png_parts(8))).await?;
    tokio::time::timeout(Duration::from_secs(5), model.held_call_started.notified()).await?;
    for _ in 0..2 {
        turn.control()
            .steer(crate::UserInput::new(png_parts(8)))
            .await?;
    }
    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert_finalized_continuation(&result, |kind| {
        matches!(kind, DurableOperationKind::ServerPauseContinuation { .. })
    })?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

/// A provider-programmatic batch keeps its continuation open across the
/// tool-batch fence; an image refusal there finalizes it before failing.
#[tokio::test]
async fn programmatic_tool_batch_image_refusal_finalizes_its_continuation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = image_descriptor(1_000_000)?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            1,
            0xA200_0000,
            false,
        )
        .with_tool_first()
        .with_programmatic_local_tool()?,
    );
    let image = ImageBlock::new("image/png", MediaSource::bytes(vec![1; 100])?)?;
    let registry = mutating_tool_registry_with_results([ToolOutcome::success(vec![
        ToolResultPart::Image(image);
        21
    ])])?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:programmatic-image-refusal",
            durable_decision_builder(workspace, Arc::clone(&model), registry)?
                .image_normalizer(Arc::new(FixtureImageNormalizer))
                .tool_policy(Arc::new(AlwaysAllowPolicy))
                .with_scheduled_compaction_enabled(false),
        )
        .await?;
    let result = agent.prompt("run the image tool").await?.result().await?;
    assert_finalized_continuation(&result, |kind| {
        matches!(kind, DurableOperationKind::ToolBatchContinuation { .. })
    })?;
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

async fn assert_repeat_receipt(
    sessions: &DurableSessionRuntime,
    store: &TestCreateStore,
    operation: &str,
    identity: DurableCompactionIdentity,
    target: CheckpointId,
) -> TestResult {
    let commits = store.commit_calls.load(Ordering::Acquire);
    for _ in 0..2 {
        let receipt = sessions
            .compaction_receipt(operation, identity.source)
            .await?;
        assert!(
            matches!(receipt, DurableCompactionReceipt::Compacted { target: actual, .. } if actual.checkpoint_id == target)
        );
    }
    assert_eq!(
        store.commit_calls.load(Ordering::Acquire),
        commits,
        "a known committed operation is never republished"
    );
    Ok(())
}

fn assert_closed_recovery(
    checkpoint: &RuntimeCheckpoint,
    source: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> TestResult {
    let Some(RootClosure::Abandon { claim, .. }) = root_turn_closure_for_test(checkpoint)? else {
        return Err("committed admission cut must close without dispatch".into());
    };
    assert_eq!(claim, crate::AbandonedTurnClaim::NothingStarted);
    crate::runtime::compaction::residency::receipt::validate(checkpoint, source, identity)?;
    let (closed, ledger) = close_root_turn_for_test(checkpoint)?;
    assert!(
        serde_json::to_string(&closed)?
            .contains("The committed compaction remains in history and is not retried")
    );
    assert_eq!(
        ledger
            .operations()
            .iter()
            .filter(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
            .count(),
        0
    );
    assert_eq!(
        ledger
            .operations()
            .iter()
            .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
            .count(),
        1
    );
    assert_eq!(
        close_root_turn_for_test(checkpoint)?.0.identity(),
        closed.identity()
    );
    Ok(())
}

fn assert_recovery_cut(
    cut: &RuntimeCheckpoint,
    source: &RuntimeCheckpoint,
    identity: DurableCompactionIdentity,
) -> TestResult {
    let record = cut
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .ok_or("compaction record")?;
    assert_eq!(record.transitions().len(), 1);
    assert_eq!(record.transitions()[0].checkpoint_id(), cut.id());
    assert!(matches!(
        decode_compaction_operations(cut, identity)?,
        DecodedCompactionWorkflow::OfflinePlaceholders { resolved: true, .. }
    ));
    assert!(decode_paid_compaction_operations(cut, identity).is_err());
    crate::runtime::compaction::residency::receipt::validate(cut, source, identity)?;
    assert_eq!(
        image_totals(cut.manifest().canonical().history(), ImageTotals::ZERO).count,
        15
    );
    assert_eq!(
        cut.manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("provenance")?
            .strategy(),
        RuntimeCompactionStrategy::ResidencyPlaceholdersV1
    );
    assert!(cut.manifest().family_artifact().is_none());
    Ok(())
}

fn assert_canonical_decoders(
    record: &DurableOperationRecord,
    request: &ResidencyRequest,
) -> TestResult {
    let mut bad = request.clone();
    bad.version = 4;
    assert!(ResidencyRequest::decode(&serde_json::to_vec(&bad)?, request.workflow_id).is_none());
    let mut bad = request.clone();
    bad.plan.selection.swap(0, 1);
    assert!(ResidencyRequest::decode(&serde_json::to_vec(&bad)?, request.workflow_id).is_none());
    let mut bad = request.clone();
    bad.plan.selection[0].part = Some(0);
    assert!(ResidencyRequest::decode(&serde_json::to_vec(&bad)?, request.workflow_id).is_none());
    let mut bad = request.clone();
    bad.plan.after.count += 1;
    assert!(ResidencyRequest::decode(&serde_json::to_vec(&bad)?, request.workflow_id).is_none());
    let DurableOperationState::Finalized(component) = record.state() else {
        return Err("finalized evidence".into());
    };
    let mut evidence = ResidencyEvidence::decode(component.bytes(), request.workflow_id)
        .ok_or("canonical evidence")?;
    evidence.version = 4;
    assert!(
        ResidencyEvidence::decode(&serde_json::to_vec(&evidence)?, request.workflow_id).is_none()
    );
    Ok(())
}

#[tokio::test]
async fn residency_never_publishes_a_non_fitting_or_non_shrinking_successor() -> TestResult {
    for (name, limit, bytes) in [
        ("cannot-fit", 10_000, 3_000),
        ("cannot-shrink", 1_000_000, 1),
    ] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let (builder, model) = image_builder(workspace, limit)?;
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store,
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let source = if bytes == 1 {
            let mut segments = image_history(1)?.segments().to_vec();
            for segment in &mut segments {
                let mut items = Vec::new();
                for item in segment.items() {
                    let blocks = item
                        .blocks()
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Image(_) => {
                                ImageBlock::new("image/png", MediaSource::bytes(vec![1])?)
                                    .map(ContentBlock::Image)
                            }
                            block => Ok(block.clone()),
                        })
                        .collect::<Result<Vec<_>, plakous_model::ContentError>>()?;
                    items.push(ConversationItem::new(
                        item.id(),
                        item.actor(),
                        blocks,
                        item.provenance(),
                    )?);
                }
                *segment = HistorySegment::new(segment.kind(), items)?;
            }
            CanonicalHistory::new(segments)?
        } else {
            image_history(1)?
        };
        let agent = sessions
            .create_seeded(name, builder, source.clone())
            .await?;
        let result = agent.prompt("continue").await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Failed(_)),
            "{name}: {:?}",
            result.outcome()
        );
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Failed(error) if error.kind() == AgentErrorKind::RequestBudget),
            "{name}: {}",
            match result.outcome() {
                TurnResultOutcome::Failed(error) => error.message(),
                _ => "unexpected outcome",
            }
        );
        if let TurnResultOutcome::Failed(error) = result.outcome()
            && name == "cannot-fit"
        {
            assert_eq!(
                error.message(),
                "resident image request budget exceeded: no residency successor fits this request's context; compact the session, or enable scheduled compaction"
            );
        }
        assert_eq!(model.calls.load(Ordering::Acquire), 0);
        assert_eq!(compaction_workflow_count(&agent.checkpoint()), 0);
        assert_eq!(
            image_totals(
                agent.checkpoint().manifest().canonical().history(),
                ImageTotals::ZERO
            )
            .count,
            22
        );
        agent.shutdown().await?;
    }
    Ok(())
}
