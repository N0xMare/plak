use super::*;

#[tokio::test]
async fn abandon_and_release_stops_admission_without_a_receipt_or_flush() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:abandon",
            durable_test_builder(
                workspace,
                ToolRegistryBuilder::default().freeze(),
                "test.route",
                b"alpha",
                false,
            )?,
        )
        .await?;
    let head = agent.checkpoint().id();
    let commits = store.commit_calls.load(Ordering::Acquire);
    let flushes = store.flushes.load(Ordering::Acquire);
    agent.begin_abandon();
    assert!(
        lock_mutex(&store.state).active_lease.is_some(),
        "handoff save retains the lease"
    );
    agent.abandon_and_release().await?;
    assert!(lock_mutex(&store.state).active_lease.is_none());
    assert!(
        agent
            .start(CommandId::new("abandoned:start")?, "must not dispatch")
            .await
            .is_err()
    );
    let _ = tokio::time::timeout(Duration::from_secs(1), agent.shutdown()).await?;
    assert_eq!(agent.checkpoint().id(), head);
    assert_eq!(store.commit_calls.load(Ordering::Acquire), commits);
    assert_eq!(store.flushes.load(Ordering::Acquire), flushes);
    Ok(())
}

#[tokio::test]
async fn fresh_native_compaction_projects_opaque_local_tool_history() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        89_000,
        40_000,
        &[40_000],
        &[0],
        true,
        LocalDurabilityDisposition::Ephemeral,
        20,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(12_000, Ordering::Release);
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let result = agent
        .prompt("consume opaque code results")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let provenance = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("opaque history must commit compaction")?;
    assert_eq!(
        provenance.strategy(),
        RuntimeCompactionStrategy::NativeResponsesTriggerV1
    );
    assert_eq!(*lock_mutex(&model.observed_compaction_anchors), vec![true]);
    assert!(provenance.after_audit().input_tokens() < provenance.before_audit().input_tokens());
    assert_eq!(
        model.observed_compaction_contexts().len(),
        2,
        "one ProtectedHistory refusal and one portable retry"
    );
    assert!(
        model
            .native_protected_history_refusals
            .load(Ordering::Acquire)
            == 1,
        "first native lowering must refuse ProtectedHistory"
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn opaque_native_prepare_failure_commits_portable_placeholders_at_capacity_fence()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        90_000,
        40_000,
        &[60_000, 60_000, 60_000],
        &[0, 1, 2],
        true,
        LocalDurabilityDisposition::DurableRequired,
        20,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.fail_compaction_prepare.store(true, Ordering::Release);
    let agent = sessions
        .create("create:portable-placeholder-scope", builder)
        .await?;
    let result = agent
        .prompt("consume code results at capacity")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let provenance = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("capacity fence must commit a portable successor")?;
    assert_eq!(
        provenance.strategy(),
        RuntimeCompactionStrategy::ToolResultPlaceholdersV1
    );
    assert!(provenance.after_audit().input_tokens() < provenance.before_audit().input_tokens());
    assert_ne!(
        placeholder_bodies(checkpoint.manifest().canonical().history()),
        [] as [String; 0]
    );
    assert!(
        checkpoint
            .manifest()
            .canonical()
            .projection()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .all(|block| !matches!(block, ContentBlock::Opaque(_)))
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        4,
        "tools must not be rerun or a paid summary dispatched"
    );
    assert!(history_note_has_replay_omission(
        checkpoint.manifest().canonical().history()
    ));
    let record = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .fold(None, |_, record| Some(record))
        .ok_or("record")?;
    let request: crate::runtime::compaction::DurablePlaceholderRequestRecord =
        serde_json::from_slice(record.request().bytes())?;
    assert_eq!(request.selected_start, 0);
    let source = store.load_checkpoint(request.source_checkpoint).await?;
    assert!(request.replaced_results > 0);
    assert_eq!(
        request.selected_end as usize,
        source
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .segments()
            .len(),
        "the recorded range covers reasoning and opaque replay outside the replaced-result span"
    );
    let source_segments = source
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .segments();
    let fresh_exchange = source_segments
        .get(source_segments.len().saturating_sub(1))
        .ok_or("fresh exchange")?;
    assert_eq!(fresh_exchange.kind(), HistorySegmentKind::ToolExchange);
    assert!(
        fresh_exchange
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Reasoning(_))),
        "reasoning in the protected fresh exchange is outside the replaced-result selection"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn capacity_fence_commits_replay_omission_without_replacing_the_fresh_result() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace.clone(),
        91_000,
        40_000,
        &[180_000],
        &[0],
        true,
        LocalDurabilityDisposition::DurableRequired,
        20,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.fail_compaction_prepare.store(true, Ordering::Release);
    let agent = sessions.create("create:replay-omission", builder).await?;
    let result = agent
        .prompt("consume a fresh code result")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let history = checkpoint.manifest().canonical().history();
    let provenance = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("replay omission must publish durable provenance")?;
    assert_eq!(
        provenance.strategy(),
        RuntimeCompactionStrategy::ReplayOmissionV1
    );
    assert!(provenance.after_audit().input_tokens() < provenance.before_audit().input_tokens());
    assert!(provenance.after_audit().input_tokens() < 40_000);
    assert!(
        placeholder_bodies(history).is_empty(),
        "the fresh tool result must not be replaced"
    );
    assert!(history.segments().iter().any(|segment| segment.kind()
        == HistorySegmentKind::Steering
        && segment.items().iter().any(|item| {
            item.provenance() == Provenance::Instruction
                && item.blocks().iter().any(|block| {
                    matches!(block, ContentBlock::Text(text)
                if text.text().contains("earlier reasoning dropped to fit"))
                })
        })));
    let record = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .ok_or("missing offline compaction record")?;
    let request: crate::runtime::compaction::DurablePlaceholderRequestRecord =
        serde_json::from_slice(record.request().bytes())?;
    assert_eq!(request.version, 2);
    assert_eq!(request.replaced_results, 0);
    assert!(matches!(
        record.state(),
        DurableOperationState::Finalized(_)
    ));
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "no paid dispatch or tool rerun"
    );
    let workflow_id = match record.kind() {
        DurableOperationKind::Compaction { workflow_id } => *workflow_id,
        _ => return Err("kind".into()),
    };
    let identity = DurableCompactionIdentity {
        workflow_id,
        ..DurableCompactionIdentity::new(
            "offline:decode",
            DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id()),
        )?
    };
    assert!(matches!(
        decode_compaction_operations(&checkpoint, identity)?,
        DecodedCompactionWorkflow::OfflinePlaceholders { resolved: true, .. }
    ));
    for (version, replaced_results, replaced_bytes) in [(1, 0, 0), (2, 1, 1), (2, 0, 1)] {
        let mut invalid = request.clone();
        invalid.version = version;
        invalid.replaced_results = replaced_results;
        invalid.replaced_bytes = replaced_bytes;
        let replacement = DurableOperationRecord::new(
            record.operation_id(),
            record.kind().clone(),
            runtime_component(
                RuntimeComponentKind::Operation,
                serde_json::to_vec(&invalid)?,
            )?,
            record.transitions().to_vec(),
        )?;
        let records = checkpoint
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|candidate| {
                if candidate.operation_id() == record.operation_id() {
                    replacement.clone()
                } else {
                    candidate.clone()
                }
            })
            .collect();
        let poisoned = checkpoint_with_compaction_records(&checkpoint, identity, records)?;
        assert!(
            decode_compaction_operations(&poisoned, identity).is_err(),
            "accepted {version}/{replaced_results}/{replaced_bytes}"
        );
    }
    let source_history = store
        .load_checkpoint(request.source_checkpoint)
        .await?
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .clone();
    let portable =
        crate::runtime::compaction::request::portable_compaction_history(&source_history)?;
    let first = crate::runtime::compaction::placeholder::replay_omission_successor(
        &source_history,
        &portable,
        request.workflow_id,
    )?;
    let first_bytes = serde_json::to_vec(&first.history)?;
    // Lose the unpublished candidate as at a pre-CAS crash, then reconstruct
    // only from the durable immutable source and the sealed workflow identity.
    drop(first);
    let recovered_source = store
        .load_checkpoint(request.source_checkpoint)
        .await?
        .checkpoint()
        .manifest()
        .canonical()
        .history()
        .clone();
    let recovered_portable =
        crate::runtime::compaction::request::portable_compaction_history(&recovered_source)?;
    let retried = crate::runtime::compaction::placeholder::replay_omission_successor(
        &recovered_source,
        &recovered_portable,
        request.workflow_id,
    )?;
    assert_eq!(
        first_bytes,
        serde_json::to_vec(&retried.history)?,
        "retry from the unchanged pre-CAS source is byte-identical"
    );
    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    let history_identity = history.identity();
    agent.shutdown().await?;
    let (builder, _) = placeholder_fixture(
        workspace,
        91_500,
        40_000,
        &[180_000],
        &[0],
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:replay-omission", address, builder)
        .await?
    else {
        return Err("replay-omission head did not reopen".into());
    };
    let checkpoint = reopened.checkpoint();
    assert_eq!(
        checkpoint.manifest().canonical().history().identity(),
        history_identity
    );
    assert!(matches!(
        decode_compaction_operations(&checkpoint, identity)?,
        DecodedCompactionWorkflow::OfflinePlaceholders { resolved: true, .. }
    ));
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_single_result_is_canonically_clipped_with_ample_token_headroom() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        92_000,
        1_000_000,
        &[500_000],
        &[0],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let result = agent
        .prompt("read one oversized observation")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let text = checkpoint
        .manifest()
        .canonical()
        .projection()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => result.parts().iter().find_map(|part| match part {
                ToolResultPart::Text(text) => Some(text.text().to_owned()),
                _ => None,
            }),
            _ => None,
        })
        .ok_or("missing committed tool result")?;
    assert!(text.len() <= crate::runtime::observation::budget::CANONICAL_RESULT_BYTES);
    assert!(
        text.contains("omitted"),
        "the omitted-bytes marker must remain model-visible"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "canonical clipping must not force a paid fold"
    );
    agent.shutdown().await?;
    Ok(())
}

fn history_note_has_replay_omission(history: &CanonicalHistory) -> bool {
    history.segments().iter().filter(|segment| segment.kind() == HistorySegmentKind::Steering)
        .flat_map(HistorySegment::items).flat_map(ConversationItem::blocks)
        .any(|block| matches!(block, ContentBlock::Text(text) if text.text().contains("earlier reasoning dropped to fit")))
}

#[tokio::test]
async fn anchored_headroom_clips_between_one_and_four_times_before_next_admission() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        93_000,
        60_000,
        &[40_000],
        &[0],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(30_000, Ordering::Release);
    let agent = builder.with_scheduled_compaction_enabled(false).build()?;
    let result = agent
        .prompt("read between H and 4H bytes")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let text = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => result.parts().iter().find_map(|part| match part {
                ToolResultPart::Text(text) => Some(text.text()),
                _ => None,
            }),
            _ => None,
        })
        .ok_or("result")?;
    assert!(text.len() < 30_000);
    assert!(text.contains("omitted"));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_compaction_admits_anchored_input_when_whole_wire_exceeds_ceiling() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, model) = placeholder_fixture(
        workspace.clone(),
        94_000,
        40_000,
        &[10_000],
        &[0],
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(80_000, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(12_000, Ordering::Release);
    let agent = sessions
        .create(
            "create:native-anchor-bound",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    let result = agent
        .prompt("read tool observations")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_some()
    );
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let (builder, compact_model) = placeholder_fixture(
        workspace,
        95_000,
        40_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    compact_model.item_context.store(true, Ordering::Release);
    compact_model
        .item_system_bytes
        .store(80_000, Ordering::Release);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:native-anchor-bound".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(
        matches!(outcome, AgentCompactSessionOutcome::Compacted { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("provenance")?
            .strategy(),
        RuntimeCompactionStrategy::NativeResponsesTriggerV1
    );
    assert_eq!(
        compact_model.calls.load(Ordering::Acquire),
        1,
        "one paid native dispatch"
    );
    assert_eq!(
        *lock_mutex(&compact_model.observed_compaction_anchors),
        vec![true]
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_placeholder_pass_preserves_preparable_unselected_replay() -> TestResult {
    preparable_native_placeholder_case(false, false).await
}

#[tokio::test]
async fn unbound_rescue_rejects_placeholders_above_the_landing_fence() -> TestResult {
    unbound_rescue_landing_case(false, true).await
}

#[tokio::test]
async fn unbound_rescue_rejects_replay_omission_above_the_landing_fence() -> TestResult {
    unbound_rescue_landing_case(false, false).await
}

#[tokio::test]
async fn fresh_preparable_source_keeps_strict_shrink_without_rescue_fence() -> TestResult {
    unbound_rescue_landing_case(true, true).await
}

async fn unbound_rescue_landing_case(fresh: bool, replaceable: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let bodies = if replaceable {
        vec![60_000; 4]
    } else {
        vec![35]
    };
    let rounds = (0..bodies.len()).collect::<Vec<_>>();
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace.clone(),
        116_000,
        100_000,
        &bodies,
        &rounds,
        true,
        LocalDurabilityDisposition::Ephemeral,
        20,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.opaque_repeats.store(8192, Ordering::Release);
    model.allow_unbound_opaque.store(true, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(12_000, Ordering::Release);
    let agent = builder.with_scheduled_compaction_enabled(false).build()?;
    assert!(matches!(
        agent
            .prompt("retain replay")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let source = agent.checkpoint();
    let topology = source.manifest().topology();
    let (builder, model) = placeholder_fixture(
        workspace,
        117_000,
        100_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.allow_unbound_opaque.store(fresh, Ordering::Release);
    model
        .native_protected_prepare
        .store(true, Ordering::Release);
    model.fail_compaction_prepare.store(true, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(240_000, Ordering::Release);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:rescue-landing".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await;
    if fresh {
        assert!(
            matches!(outcome, Ok(AgentCompactSessionOutcome::Compacted { .. })),
            "{outcome:?}"
        );
        let checkpoint = agent.checkpoint();
        let provenance = checkpoint
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("fresh shrink provenance")?;
        let Ok(AgentCompactSessionOutcome::Compacted {
            summary: Some(summary),
            ..
        }) = outcome
        else {
            return Err("fresh shrink summary".into());
        };
        assert!(
            summary
                .occupancy_tokens_after
                .is_some_and(|tokens| tokens > 50_000)
        );
        assert!(provenance.after_audit().input_tokens() < provenance.before_audit().input_tokens());
    } else {
        assert!(
            !matches!(outcome, Ok(AgentCompactSessionOutcome::Compacted { .. })),
            "an unbound rescue must clear the landing fence: {outcome:?}"
        );
        assert_eq!(
            agent.checkpoint().manifest().canonical().history(),
            source.manifest().canonical().history()
        );
    }
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_protected_refusal_authorizes_portable_even_when_placeholders_fit() -> TestResult {
    preparable_native_placeholder_case(true, false).await
}

#[tokio::test]
async fn native_protected_refusal_keeps_unchanged_successor_when_portable_notice_expands()
-> TestResult {
    preparable_native_placeholder_case(true, true).await
}

async fn preparable_native_placeholder_case(protected: bool, tiny_replay: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace.clone(),
        96_000,
        100_000,
        if tiny_replay {
            &[1_765, 35]
        } else {
            &[60_000; 4]
        },
        if tiny_replay { &[0, 1] } else { &[0, 1, 2, 3] },
        true,
        LocalDurabilityDisposition::DurableRequired,
        20,
    )?;
    model
        .opaque_tool_history
        .store(!tiny_replay, Ordering::Release);
    model
        .tiny_reasoning_history
        .store(tiny_replay, Ordering::Release);
    model.allow_unbound_opaque.store(true, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(80_000, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(12_000, Ordering::Release);
    let agent = sessions
        .create(
            "create:preparable-replay",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    let result = agent
        .prompt("preserve preparable replay")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    if tiny_replay {
        use crate::runtime::compaction::placeholder::{
            collect_placeholder_candidates, placeholder_successor, portable_placeholder_successor,
            replay_omission_successor,
        };
        let checkpoint = agent.checkpoint();
        let history = checkpoint.manifest().canonical().history();
        let portable = crate::runtime::compaction::request::portable_compaction_history(history)?;
        let workflow = SessionWorkflowId::try_from([1; 32])?;
        let unchanged = placeholder_successor(
            history,
            &collect_placeholder_candidates(history),
            1,
            workflow,
        )?;
        let with_omission = portable_placeholder_successor(
            history,
            &portable,
            &collect_placeholder_candidates(&portable),
            1,
            workflow,
        )?;
        let replay_only = replay_omission_successor(history, &portable, workflow)?;
        let before = history.retained_bytes_upper_bound();
        assert!(
            unchanged.history.retained_bytes_upper_bound() < before,
            "unchanged {} < source {before}",
            unchanged.history.retained_bytes_upper_bound()
        );
        assert!(
            with_omission.history.retained_bytes_upper_bound() >= before,
            "portable {} >= source {before}",
            with_omission.history.retained_bytes_upper_bound()
        );
        assert!(replay_only.history.retained_bytes_upper_bound() >= before);
    }
    let (builder, compact_model) = placeholder_fixture(
        workspace,
        97_000,
        100_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    compact_model
        .opaque_tool_history
        .store(true, Ordering::Release);
    compact_model
        .allow_unbound_opaque
        .store(true, Ordering::Release);
    compact_model
        .fail_compaction_prepare
        .store(true, Ordering::Release);
    // In the large-body case, 80k of prefix plus the fresh 60k result Fits
    // only ordinary successor admission, not the paid bound.
    compact_model.item_context.store(true, Ordering::Release);
    compact_model
        .item_system_bytes
        .store(80_000, Ordering::Release);
    compact_model
        .native_protected_prepare
        .store(protected, Ordering::Release);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:preparable-replay".to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(
        matches!(outcome, AgentCompactSessionOutcome::Compacted { .. }),
        "{outcome:?}"
    );
    let checkpoint = agent.checkpoint();
    assert_eq!(
        checkpoint
            .manifest()
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("provenance")?
            .strategy(),
        RuntimeCompactionStrategy::ToolResultPlaceholdersV1
    );
    let opaque = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter(|block| matches!(block, ContentBlock::Opaque(_)))
        .count();
    let portable = protected && !tiny_replay;
    let replay_count = if tiny_replay { 2 } else { 4 };
    assert_eq!(
        opaque,
        if portable || tiny_replay {
            0
        } else {
            replay_count
        },
        "only the native refusal authorizes replay loss when unchanged placeholders fit"
    );
    assert_eq!(
        history_note_has_replay_omission(checkpoint.manifest().canonical().history()),
        portable
    );
    assert_eq!(
        compact_model.paid_compaction_calls.load(Ordering::Acquire),
        0
    );
    assert_eq!(
        checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .filter(|block| matches!(block, ContentBlock::Reasoning(_)))
            .count(),
        if portable { 0 } else { replay_count }
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn restored_approval_after_steer_is_superseded() -> TestResult {
    Box::pin(reopen_pending_decision(true, false, true)).await
}

#[tokio::test]
async fn live_approval_after_steer_is_superseded() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            97_200,
            false,
        )
        .with_tool_first(),
    );
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:steered-approval",
            durable_decision_builder(workspace, model, one_mutating_tool_registry()?)?,
        )
        .await?;
    let mut turn = agent.prompt("approve later").await?;
    let mut events = turn.take_events()?;
    let decision = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await?;
            if let AgentEventKind::EffectDecisionRequired { decision, .. } = event.kind() {
                break Ok::<_, Box<dyn std::error::Error>>(decision.clone());
            }
        }
    })
    .await??;
    turn.control().steer("supersede approved effect").await?;
    turn.control()
        .allow_effect(
            decision,
            EffectDecisionAuditId::new("test:steered-approval")?,
        )
        .await?;
    let result = turn.result().await?;
    agent.shutdown().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(result.conversation().items().iter().flat_map(ConversationItem::blocks).any(|block|
        matches!(block, ContentBlock::ToolResult(tool) if tool.parts().iter().any(|part|
            matches!(part, ToolResultPart::Text(text) if text.text() == "not run: superseded by your message")))));
    let effects = result
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|op| matches!(op.kind(), DurableOperationKind::Effect { .. }))
        .collect::<Vec<_>>();
    assert_eq!(effects.len(), 1);
    assert!(
        !effects[0]
            .transitions()
            .iter()
            .any(|t| matches!(t.state(), DurableOperationState::Dispatched))
    );
    Ok(())
}

/// A scaled Codex-shaped head: encrypted replay dominates the wire, while
/// provider usage leaves zero observation allowance for a tiny fresh result.
#[tokio::test]
async fn codex_shaped_capacity_turn_commits_an_offline_successor() -> TestResult {
    codex_offline_successor_case(false, false).await
}

#[tokio::test]
async fn native_protected_refusal_and_nonfitting_probes_commit_replay_omission() -> TestResult {
    codex_offline_successor_case(true, false).await
}

#[tokio::test]
async fn provider_overflow_after_an_offline_successor_recovers_once_without_looping() -> TestResult
{
    codex_offline_successor_case(false, true).await
}

async fn codex_offline_successor_case(protected: bool, overflow: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let (builder, model) = placeholder_fixture_producing_artifacts(
        workspace,
        96_000,
        40_000,
        &[35],
        &[0],
        true,
        LocalDurabilityDisposition::DurableRequired,
        20,
    )?;
    model.opaque_tool_history.store(true, Ordering::Release);
    model.opaque_repeats.store(8192, Ordering::Release);
    model.allow_unbound_opaque.store(true, Ordering::Release);
    model
        .native_protected_prepare
        .store(protected, Ordering::Release);
    model.fail_compaction_prepare.store(true, Ordering::Release);
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(80_000, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(39_000, Ordering::Release);
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let builder = builder.telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone())));
    if overflow {
        model.overflow_from_call.store(1, Ordering::Release);
    }
    let agent = sessions.create("create:codex-offline-fit", builder).await?;
    let mut turn = agent.prompt("consume a tiny fresh code result").await?;
    let mut events = turn.take_events()?;
    let mut detail = None;
    while let Ok(event) = events.recv().await {
        match event.kind() {
            AgentEventKind::CompactionFinished { detail: value, .. } => detail = value.clone(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let result = turn.result().await?;
    if overflow {
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Failed(error)
                if error.model_error().is_some_and(ModelError::is_provider_context_overflow)),
            "{:?}; compaction detail: {detail:?}",
            result.outcome()
        );
        let compacts = lock_mutex(&sink.observations)
            .iter()
            .filter(|observation| {
                matches!(
                    observation.kind(),
                    TelemetryObservationKind::CompactionStarted { .. }
                )
            })
            .count();
        assert_eq!(
            compacts, 2,
            "one capacity pass and one provider-overflow recovery"
        );
    } else {
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}; compaction detail: {detail:?}",
            result.outcome()
        );
    }
    let checkpoint = agent.checkpoint();
    let history = checkpoint.manifest().canonical().history();
    let provenance = checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("offline successor")?;
    assert_eq!(
        provenance.strategy(),
        RuntimeCompactionStrategy::ReplayOmissionV1
    );
    assert!(provenance.after_audit().input_tokens() < provenance.before_audit().input_tokens());
    assert!(history_note_has_replay_omission(history));
    assert_eq!(placeholder_bodies(history), [] as [String; 0]);
    let fresh = history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => result.parts().iter().find_map(|part| match part {
                ToolResultPart::Text(text) => Some(text.text()),
                _ => None,
            }),
            _ => None,
        })
        .ok_or("fresh result")?;
    assert_eq!(fresh.len(), 35, "a notice must not replace a smaller body");
    let record = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .ok_or("offline record")?;
    let request: crate::runtime::compaction::DurablePlaceholderRequestRecord =
        serde_json::from_slice(record.request().bytes())?;
    assert_eq!(request.version, 2);
    assert_eq!(request.replaced_results, 0);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
    assert_eq!(
        model
            .native_protected_history_refusals
            .load(Ordering::Acquire),
        2 * usize::from(protected)
    );
    let refused_attempts = lock_mutex(&model.native_protected_refusal_attempts).clone();
    if protected {
        let ModelOperationRoot::SessionWorkflow(workflow) = refused_attempts[0].root() else {
            return Err("native probe is not a workflow measurement".into());
        };
        let probe = compaction_attempt_id(workflow, b"native-fit")?;
        assert!(
            refused_attempts
                .iter()
                .any(|metadata| metadata.attempt_id() == probe)
        );
        assert_ne!(
            refused_attempts[0].attempt_id(),
            refused_attempts[1].attempt_id()
        );
    }
    assert!(
        lock_mutex(&model.compaction_start_rounds).is_empty(),
        "no compaction start, including unpaid probes"
    );
    assert!(
        !checkpoint
            .manifest()
            .ledger()
            .operations()
            .iter()
            .any(|operation| matches!(
                operation.kind(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::SessionWorkflow(_),
                    ..
                }
            )),
        "offline fit probes create no provider operation or dispatch"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn exhausted_native_ladder_reports_the_actual_unavailable_rung() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace.clone(),
        97_000,
        40_000,
        &[35],
        &[0],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.with_scheduled_compaction_enabled(false).build()?;
    assert!(matches!(
        agent
            .prompt("fresh result")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let source = agent.checkpoint();
    let topology = source.manifest().topology();
    let (builder, compact_model) = placeholder_fixture(
        workspace,
        98_000,
        40_000,
        &[],
        &[],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    compact_model
        .fail_compaction_prepare
        .store(true, Ordering::Release);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:native-rung".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await;
    assert!(
        matches!(outcome, Err(AgentSessionEndpointError::Rejected { ref reason })
        if reason.contains("native facet preparation failed")
            && reason.contains("no fitting offline successor")),
        "{outcome:?}"
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(
        compact_model.paid_compaction_calls.load(Ordering::Acquire),
        0
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn turn_fence_without_a_shrinking_offline_successor_reports_why() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        98_000,
        400_000,
        &[35, 35],
        &[0, 1],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(80_000, Ordering::Release);
    model
        .item_provider_input_tokens
        .store(360_000, Ordering::Release);
    // The first failed native call spends the paid guard. At the next fence,
    // replacing the older tiny body expands, so only NoEligibleRange remains.
    model.script_tool_rounds_around_compactions();
    model.fail_compaction_call();
    let agent = builder.build()?;
    let mut turn = agent.prompt("consume the fresh tiny result").await?;
    let mut events = turn.take_events()?;
    let mut finished = Vec::new();
    while let Ok(event) = events.recv().await {
        match event.kind() {
            AgentEventKind::CompactionFinished {
                outcome, detail, ..
            } => {
                finished.push((*outcome, detail.clone()));
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(finished.len(), 2, "{finished:?}");
    assert_eq!(finished[1].0, CompactionEventOutcome::NoEligibleRange);
    assert_eq!(
        finished[1].1.as_deref(),
        Some("no strictly shrinking offline successor fits")
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    agent.shutdown().await?;
    Ok(())
}
