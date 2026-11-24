use super::*;

#[tokio::test]
async fn unanchored_carry_seed_compacts_at_first_prompt_above_trigger_and_over_window() -> TestResult
{
    for (name, bytes) in [("above-trigger", 72_000), ("over-window", 240_000)] {
        let root = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(root.path(), ".")?);
        let (builder, model) = placeholder_fixture(
            workspace,
            990_000,
            40_000,
            &[],
            &[],
            false,
            LocalDurabilityDisposition::DurableRequired,
        )?;
        assert_eq!(builder.session_input_window(), Some((Some(40_000), 39_872)));
        let item = |id, actor, blocks| -> TestResult<ConversationItem> {
            Ok(ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(id))?,
                actor,
                blocks,
                if actor == Actor::User {
                    Provenance::UserInput
                } else {
                    Provenance::ModelOutput
                },
            )?)
        };
        let text = |value| TextBlock::new(value, None).map(ContentBlock::Text);
        let call = CallId::try_from(uuid::Uuid::from_u128(99))?;
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![item(
                    1,
                    Actor::User,
                    vec![text("older intent".to_owned())?],
                )?],
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
                                "x".repeat(bytes),
                                None,
                            )?)],
                            ToolCorrelation::default(),
                        )?)],
                    )?,
                ],
            )?,
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![item(
                    4,
                    Actor::User,
                    vec![text("latest protected goal".to_owned())?],
                )?],
            )?,
        ])?;
        let source_tokens = u64::try_from(history.retained_bytes_upper_bound())?.div_ceil(4);
        if name == "above-trigger" {
            assert!(
                (35_648..39_872).contains(&source_tokens),
                "seed must fit above trigger: {source_tokens}"
            );
        } else {
            assert!(
                source_tokens > 39_872,
                "seed must exceed window: {source_tokens}"
            );
        }
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store,
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions.create_seeded(name, builder, history).await?;
        assert_eq!(agent.checkpoint().manifest().ledger().operations().len(), 0);
        let result = agent
            .prompt("continue carried conversation")
            .await?
            .result()
            .await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{name}: {:?}",
            result.outcome()
        );
        assert_eq!(
            agent
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .filter(|op| matches!(op.kind(), DurableOperationKind::Compaction { .. }))
                .count(),
            1,
            "{name}: exactly one preflight compaction context, excluding offline fit preparations"
        );
        assert_eq!(
            lock_mutex(&model.paid_compaction_rounds).len(),
            1,
            "{name}: one paid provider operation"
        );
        assert_eq!(
            agent
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .filter(|op| matches!(
                    op.kind(),
                    DurableOperationKind::Provider {
                        root: ModelOperationRoot::SessionWorkflow(_),
                        ..
                    }
                ))
                .count(),
            1,
            "{name}: exactly one paid compaction Provider operation"
        );
        assert!(
            agent
                .checkpoint()
                .manifest()
                .canonical()
                .history()
                .segments()
                .iter()
                .any(|g| g.kind() == HistorySegmentKind::Compaction)
        );
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn irreducible_pure_text_seed_refuses_without_repeated_paid_dispatch() -> TestResult {
    let root = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(root.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        991_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let text = |id, body| -> TestResult<ConversationItem> {
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(id))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(body, None)?)],
            Provenance::UserInput,
        )?)
    };
    let history = CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(1, "old source prose ".repeat(20_000))?],
        )?,
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(2, "protected latest goal".to_owned())?],
        )?,
    ])?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create_seeded("pure-text-seed", builder, history)
        .await?;
    let first = agent.prompt("continue").await?.result().await?;
    assert!(first.context_capacity().is_some(), "{:?}", first.outcome());
    let paid = lock_mutex(&model.paid_compaction_rounds).len();
    assert_eq!(
        paid, 0,
        "irreducible input refuses before any paid dispatch"
    );
    let second = agent.prompt("continue again").await?.result().await?;
    assert!(
        second.context_capacity().is_some(),
        "{:?}",
        second.outcome()
    );
    assert_eq!(lock_mutex(&model.paid_compaction_rounds).len(), paid);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn carried_replay_is_lowered_before_create_without_dispatch() -> TestResult {
    let root = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(root.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        992_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.allow_unbound_opaque.store(false, Ordering::Release);
    let envelope = plakous_model::ReplayEnvelope::new(
        ProtocolId::new("test.responses")?,
        None,
        plakous_model::ReplayScope::Protocol,
        plakous_model::ReplayKindId::new("code-mode-exec")?,
        1,
        plakous_model::DataClassification::Sensitive,
        serde_json::value::RawValue::from_string("{}".to_owned())?,
    )?;
    let segment = HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(123))?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(plakous_model::OpaqueBlock::new(
                envelope,
                plakous_model::OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let error = crate::runtime::validate_seed_replay(&builder, &[segment])
        .await
        .err()
        .ok_or("invalid replay was accepted")?;
    assert_eq!(error.kind(), AgentErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("carried replay refused by destination dialect")
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert!(lock_mutex(&model.paid_compaction_rounds).is_empty());
    Ok(())
}
