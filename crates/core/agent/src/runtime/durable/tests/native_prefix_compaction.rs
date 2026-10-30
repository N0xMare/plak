#![allow(
    clippy::too_many_lines,
    reason = "Native prefix compaction follows protected history through summary publication and exact provider continuation"
)]

use super::*;

#[tokio::test]
async fn native_instruction_prefix_survives_a_single_harness_dispatch_and_reopen() -> TestResult {
    Box::pin(native_prefix_case()).await
}

async fn native_prefix_case() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let fixture = |base| {
        placeholder_fixture(
            workspace.clone(),
            base,
            40_000,
            &[],
            &[],
            true,
            LocalDurabilityDisposition::DurableRequired,
        )
    };
    let (builder, model) = fixture(800_000)?;
    let item = |id, actor, block| -> TestResult<ConversationItem> {
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(id))?,
            actor,
            vec![block],
            if actor == Actor::User {
                Provenance::UserInput
            } else {
                Provenance::ModelOutput
            },
        )?)
    };
    let text = |id, actor, value: String| {
        item(id, actor, ContentBlock::Text(TextBlock::new(value, None)?))
    };
    let mut prefix = Vec::new();
    for (index, kind) in [
        HistorySegmentKind::RecentIntent,
        HistorySegmentKind::Steering,
        HistorySegmentKind::Conversation,
    ]
    .into_iter()
    .enumerate()
    {
        prefix.push(HistorySegment::new(
            kind,
            vec![text(
                u128::try_from(index)? + 1,
                Actor::User,
                "retained requirement ".repeat(800),
            )?],
        )?);
    }
    let descriptor = &model.descriptor;
    let native = CompactionBlock::new("native summary ".repeat(1_000))?
        .with_native_item(r#"{"type":"compaction","encrypted_content":"fixture"}"#.to_owned())
        .with_native_binding(plakous_model::CompactionReplayBinding::new(
            descriptor.protocol().clone(),
            descriptor.model_family().cloned(),
            descriptor.requested_model().clone(),
            model.authority.account_binding(),
        ));
    let mut segments = prefix.clone();
    segments.extend([
        HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![item(
                10,
                Actor::Assistant,
                ContentBlock::Compaction(native),
            )?],
        )?,
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(
                11,
                Actor::User,
                "work after native compaction".to_owned(),
            )?],
        )?,
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(12, Actor::Assistant, "completed work ".repeat(3_000))?],
        )?,
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(13, Actor::User, "latest protected intent".to_owned())?],
        )?,
    ]);
    let source_history = CanonicalHistory::new(segments)?;
    let agent = sessions
        .create_seeded(
            "create:native-prefix",
            builder.with_scheduled_compaction_enabled(false),
            source_history.clone(),
        )
        .await?;
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let (compact_builder, compact_model) = fixture(805_000)?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:native-prefix".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            compact_builder,
        ))
        .await
        .map_err(|error| format!("compact: {error:?}"))?;
    assert!(
        matches!(outcome, AgentCompactSessionOutcome::Compacted { .. }),
        "{outcome:?}"
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    assert_eq!(
        compact_model.paid_compaction_calls.load(Ordering::Acquire),
        1
    );
    let checkpoint = agent.checkpoint();
    let history = checkpoint.manifest().canonical().history();
    assert_eq!(
        history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .count(),
        1
    );
    assert_eq!(&history.segments().to_vec()[1..=prefix.len()], prefix);
    assert_eq!(
        checkpoint
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("provenance")?
            .strategy(),
        RuntimeCompactionStrategy::SemanticSummaryPrefixV1
    );
    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    agent.shutdown().await?;
    let (builder, reopened_model) = fixture(810_000)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:native-prefix", address, builder)
        .await?
    else {
        return Err("native prefix did not reopen".into());
    };
    assert_eq!(
        reopened.checkpoint().manifest().canonical().history(),
        history
    );
    assert_eq!(
        reopened_model.paid_compaction_calls.load(Ordering::Acquire),
        0
    );
    reopened.shutdown().await?;
    Ok(())
}
