use super::*;

/// A second capacity check can replace an entire multi-call exchange after the
/// per-result observation budget has already clipped one result. The two
/// accounting stages must stay distinct and the replacement is announced only
/// after its canonical batch commits.
fn replacement_fixture(
    workspace: Arc<WorkspacePolicy>,
    ceiling: u64,
    entry_base: u128,
) -> TestResult<(AgentBuilder, Arc<ArtifactModel>)> {
    let descriptor = placeholder_test_descriptor(ceiling, false)?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            entry_base,
            false,
        )
        .with_scripted_tool_calls([(0, "probe", format!("{}{{}}", " ".repeat(43_000)))])
        .with_second_direct_tool()?
        .with_byte_derived_context_tokens(),
    );
    let registry = oversized_tool_registry(&[1_000, 15_000])?;
    let configured = configured_test_model(
        model.clone(),
        descriptor.clone(),
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .config(AgentConfig::default().with_observation_budget_cap_bytes(Some(5_000)))
        .with_scheduled_compaction_enabled(false);
    Ok((builder, model))
}

#[tokio::test]
async fn durable_multi_result_capacity_shrink_reports_each_committed_replacement() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = replacement_fixture(Arc::clone(&workspace), 23_300, 86_000)?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:observation-replacement", builder)
        .await?;
    let mut turn = agent
        .start(
            CommandId::new("observation:replacement")?,
            "read both values",
        )
        .await?
        .into_turn()
        .ok_or("replacement turn was not admitted")?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let mut started = Vec::new();
    let mut clipped = Vec::new();
    let mut replaced = Vec::new();
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::ToolStarted { .. } => {
                started.push((event.turn_id(), event.attempt_id(), event.call_id()));
            }
            AgentEventKind::ObservationClipped { bytes_dropped, .. } => {
                clipped.push((
                    event.turn_id(),
                    event.attempt_id(),
                    event.call_id(),
                    *bytes_dropped,
                ));
            }
            AgentEventKind::ObservationReplaced {
                bytes_before,
                bytes_after,
            } => replaced.push((
                event.turn_id(),
                event.attempt_id(),
                event.call_id(),
                *bytes_before,
                *bytes_after,
            )),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "unexpected outcome: {:?}",
        result.outcome()
    );
    assert_eq!(started.len(), 2);
    assert_eq!(clipped.len(), 1, "only the second raw result was clipped");
    assert!(clipped[0].3 > 0);
    assert_eq!(replaced.len(), 1, "the small result remains intact");
    assert!(replaced.iter().all(|(turn, attempt, call, before, after)| {
        started.contains(&(*turn, *attempt, *call)) && *before > *after
    }));
    assert!(clipped.iter().all(|(turn, attempt, call, _)| {
        replaced.iter().any(|(r_turn, r_attempt, r_call, _, _)| {
            (turn, attempt, call) == (r_turn, r_attempt, r_call)
        })
    }));
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    let history = agent.checkpoint().manifest().canonical().history().clone();
    let marker_count = history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => result.parts().first(),
            _ => None,
        })
        .filter(|part| {
            matches!(part, ToolResultPart::Text(text)
            if text.text() == crate::runtime::prompts::PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED)
        })
        .count();
    assert_eq!(marker_count, 1);
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let (reopen_builder, reopened_model) = replacement_fixture(workspace, 23_300, 86_500)?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:observation-replacement", address, reopen_builder)
        .await?
    else {
        return Err("committed replacement head should reopen quiescent".into());
    };
    assert_eq!(reopened_model.calls.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn committed_replacements_are_reported_even_if_shrunk_exchange_still_overflows() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = replacement_fixture(workspace, 12_000, 87_000)?;
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:observation-replacement-overflow", builder)
        .await?;
    let mut turn = agent
        .start(
            CommandId::new("observation:replacement-overflow")?,
            "read both values",
        )
        .await?
        .into_turn()
        .ok_or("replacement overflow turn was not admitted")?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let mut replaced = Vec::new();
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::ObservationReplaced { .. } => replaced.push(event.call_id()),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert!(result.context_capacity().is_some());
    assert_eq!(replaced.len(), 2);
    assert!(replaced.iter().all(Option::is_some));
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "no next model call was admitted"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_tool_exchange_promotion_emits_no_replacement_observation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _) = replacement_fixture(workspace, 23_300, 87_500)?;
    let store = Arc::new(TestCreateStore::default());
    store
        .fail_tool_exchange_promotion
        .store(true, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:observation-replacement-rejected", builder)
        .await?;
    let mut turn = agent
        .start(
            CommandId::new("observation:replacement-rejected")?,
            "read both values",
        )
        .await?
        .into_turn()
        .ok_or("replacement rejection turn was not admitted")?;
    let mut events = turn.take_events()?;
    let result = turn.result().await?;
    let mut replacements = 0;
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::ObservationReplaced { .. } => replacements += 1,
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    assert_eq!(
        replacements, 0,
        "rejected projection was never authoritative"
    );
    agent.shutdown().await?;
    Ok(())
}

/// Recurring oversized observations can fold again after new work fills the window.
#[tokio::test]
async fn recurring_capacity_pressure_rearms_after_the_paid_fold() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        76_000,
        40_000,
        &[20_000, 20_000, 60_000, 20_000, 20_000, 60_000, 2_000],
        &[0, 1, 2, 3, 4, 5, 6],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let result = agent.prompt("read both big files").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "a second fence must not kill the turn: {:?}",
        result.outcome()
    );
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    let history = manifest.canonical().history();
    assert_eq!(
        compaction_segment_count(history),
        1,
        "paid and offline compaction share the one summary segment"
    );
    assert_eq!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or("fold provenance")?
            .strategy(),
        RuntimeCompactionStrategy::SemanticSummaryPrefixV1,
        "later capacity pressure commits a new paid fold"
    );
    assert_eq!(
        history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::RecentIntent)
            .count(),
        1,
        "the intent survives every fold verbatim"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        11,
        "seven tool rounds, one completing round, and three pressure-paid folds; fit probes never dispatch"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A non-shrinking paid result backs off before newly consumed work pays again.
#[tokio::test]
async fn a_non_shrinking_paid_success_backs_off_before_a_new_workflow() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        76_500,
        100_000,
        &[60_000, 60_000, 60_000, 60_000, 60_000, 60_000, 2_000],
        &[0, 1, 2, 3, 4, 5, 6],
        true,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.script_tool_rounds_around_compactions();
    model.make_compaction_success_non_shrinking(400_000);
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let result = agent.prompt("read the big files").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        *lock_mutex(&model.paid_compaction_rounds),
        [3, 5],
        "non-shrinking results must not pay at consecutive tool fences"
    );
    assert!(
        !placeholder_bodies(agent.checkpoint().manifest().canonical().history()).is_empty(),
        "the turn continues through the offline rung"
    );
    agent.shutdown().await?;
    Ok(())
}

/// After a placeholder pass, the next turn remains usable.
///
/// The placeholder pass notice sits at position zero, so the planner's barrier rule
/// puts the replaced turn *after* the barrier. A trailing notice would make it
/// a barrier instead and later turns would be unplannable.
#[tokio::test]
async fn a_turn_after_placeholders_continues_without_stacking_compaction_segments() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        77_000,
        100_000,
        &[60_000, 60_000, 60_000, 60_000, 2_000, 72_000],
        &[0, 1, 2, 3, 4, 6],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.fail_compaction_call();
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let original_affinity = agent
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .affinity()
        .clone();
    let first = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    {
        let checkpoint = agent.checkpoint();
        assert_eq!(
            placeholder_bodies(checkpoint.manifest().canonical().history()).len(),
            2
        );
    }
    let first_checkpoint = agent.checkpoint();
    let first_line = first_checkpoint.manifest().canonical().history().segments()[0].items()[0]
        .blocks()
        .to_vec();
    assert_eq!(
        first_checkpoint.manifest().canonical().context().affinity(),
        &original_affinity,
        "offline passes preserve prompt cache affinity"
    );
    let second = agent
        .prompt("now read the other one")
        .await?
        .result()
        .await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "the turn after a placeholder pass must still run: {:?}",
        second.outcome()
    );
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    let history = manifest.canonical().history();
    assert_eq!(
        compaction_segment_count(history),
        1,
        "still exactly one compaction segment"
    );
    assert_eq!(
        manifest.canonical().context().affinity(),
        &original_affinity
    );
    assert_eq!(
        history.segments()[0].items()[0].blocks(),
        first_line.as_slice(),
        "position-zero text is constant across offline passes"
    );
    let first_segment = history.segments().get(0).ok_or("a compacted successor")?;
    assert_eq!(first_segment.kind(), HistorySegmentKind::Compaction);
    let provenance = manifest
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("second-turn compaction provenance")?;
    assert_eq!(
        provenance.summary_segment_identity(),
        first_segment.identity()
    );
    assert_eq!(
        provenance.strategy(),
        RuntimeCompactionStrategy::ToolResultPlaceholdersV1,
        "the retained compaction provenance remains the offline pass"
    );
    assert!(
        !placeholder_bodies(history).is_empty(),
        "the next turn must not silently erase the committed placeholders"
    );
    agent.shutdown().await?;
    Ok(())
}

/// A reopened replaced session compacts again instead of refusing.
#[tokio::test]
async fn a_reopened_capacity_clipped_session_can_compact_again() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (builder, _) = placeholder_fixture(
        Arc::clone(&workspace),
        78_000,
        40_000,
        &[72_000, 2_000],
        &[0, 1],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let agent = sessions
        .create(
            "create:placeholder-repeat",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    let first = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    agent.shutdown().await?;
    let (reopen_builder, _) = placeholder_fixture(
        workspace,
        78_500,
        40_000,
        &[72_000, 2_000],
        &[0, 1],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:placeholder-repeat", address, reopen_builder)
        .await?
    else {
        return Err("an replaced quiescent head must reopen without recovery".into());
    };
    let second = reopened.prompt("now read it again").await?.result().await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "a resumed replaced session must still compact: {:?}",
        second.outcome()
    );
    let reopened_checkpoint = reopened.checkpoint();
    let manifest = reopened_checkpoint.manifest();
    assert_eq!(
        compaction_segment_count(manifest.canonical().history()),
        1,
        "the resumed session did not stack a second compaction segment"
    );
    reopened.shutdown().await?;
    Ok(())
}

/// The recovery decoder recognises a committed placeholders instead of misreading it.
///
/// Every recovery path is about a dispatched provider operation. A placeholder pass
/// publishes a `Compaction` operation with no provider attempt and a placeholder pass
/// plan as its request, so a decoder that insisted on the paid request shape
/// would report corruption on a perfectly good ledger.
#[tokio::test]
async fn the_recovery_decoder_reads_committed_placeholders_as_offline_and_resolved() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        79_000,
        100_000,
        &[60_000, 60_000, 60_000, 60_000, 2_000],
        &[0, 1, 2, 3, 4],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.fail_compaction_call();
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let result = agent.prompt("read the big file").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let checkpoint = agent.checkpoint();
    let record = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .fold(None, |_, record| Some(record))
        .ok_or("the placeholder pass must publish a compaction operation")?;
    let DurableOperationKind::Compaction { workflow_id } = record.kind() else {
        return Err("expected a compaction operation".into());
    };
    // One atomic transition: no prepared or completed checkpoint is cited,
    // because none is ever published for an offline rung.
    assert!(matches!(
        record.state(),
        DurableOperationState::Finalized(_)
    ));
    let identity = DurableCompactionIdentity {
        source: DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id()),
        workflow_id: *workflow_id,
        ..DurableCompactionIdentity::new(
            "plak:placeholders-decoder-probe",
            DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id()),
        )?
    };
    match decode_compaction_operations(&checkpoint, identity)? {
        DecodedCompactionWorkflow::OfflinePlaceholders {
            compaction,
            resolved,
        } => {
            assert_eq!(compaction.0, record.operation_id());
            assert!(resolved, "the rung publishes only a finalized operation");
        }
        DecodedCompactionWorkflow::Paid(_) => {
            return Err("a placeholder pass must not decode as a paid workflow".into());
        }
    }
    // The paid decoder names the shape instead of claiming corruption.
    let Err(error) = decode_paid_compaction_operations(&checkpoint, identity) else {
        return Err("a paid recovery path has nothing to recover here".into());
    };
    assert!(
        error.message().contains("offline placeholders"),
        "unexpected diagnostic: {}",
        error.message()
    );
    agent.shutdown().await?;
    Ok(())
}

/// A schema rejection carries what the model sent and why it was refused.
///
/// A live run could not diagnose a rejected call after
/// the fact: the record's reason was the Debug of the error kind, and both
/// identifying fields were null, because a call that never starts leaves
/// nothing in the host's subject stash. The event now carries its own bounded
/// redacted arguments and the rejection's descriptive message.
#[tokio::test]
async fn a_schema_rejection_publishes_the_arguments_sent_and_the_reason() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            81_000,
            false,
        )
        // The probe's schema declares no properties and forbids extras, so
        // this is refused before validation, exactly like `wait` was.
        .with_scripted_tool_calls([(0, "probe", r#"{"cell_id": 2}"#.to_owned())]),
    );
    let authority = model.authority.clone();
    let runtime_model: Arc<dyn LanguageModel> = model.clone();
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        runtime_model,
        descriptor,
        authority,
        &registry,
        LocalDurabilityDisposition::Ephemeral,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    let agent = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        )
        .build()?;
    let mut turn = agent
        .start(CommandId::new("reject:schema")?, "call it wrong")
        .await?
        .into_turn()
        .ok_or("the rejection turn was not admitted")?;
    let mut events = turn.take_events()?;
    let mut rejection = None;
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::ToolPreparationRejected {
                attempt,
                reason,
                detail,
                observation,
                ..
            } => {
                rejection = Some((
                    attempt.model_facing_name().as_str().to_owned(),
                    *reason,
                    detail.as_ref().map(ToString::to_string),
                    observation
                        .as_ref()
                        .map(|observation| observation.arguments_preview().to_owned()),
                ));
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let (name, reason, detail, arguments) = rejection.ok_or("no rejection was published")?;
    assert_eq!(name, "probe");
    assert_eq!(reason, ToolRuntimeErrorKind::InvalidArguments);
    let detail = detail.ok_or("a rejection must carry its descriptive message")?;
    assert!(
        !detail.is_empty() && detail != "InvalidArguments",
        "the message must say which constraint was missed, got {detail:?}"
    );
    let arguments = arguments.ok_or("a rejection must carry what the model sent")?;
    assert!(
        arguments.contains("cell_id"),
        "the refused arguments must be recoverable, got {arguments:?}"
    );
    let _ = turn.result().await;
    agent.shutdown().await?;
    Ok(())
}

/// A compaction that cannot be prepared falls to placeholders, not to overflow.
///
/// 2026-09-18, live attempt 4: Grok growth JSONL recorded a genuine
/// `context_capacity` denial. Both compactions on that turn finished
/// `unavailable` because an active Responses artifact no longer matched the
/// request's canonical coverage, no `context.placeholders` record appeared anywhere,
/// and the next round then reached the provider over the window. A rung being
/// unavailable is not a terminal outcome for the ladder: nothing was
/// dispatched and nothing is owed, so the offline rung must still get its turn.
/// A paid fold that fails definitely mid-turn changed nothing, so the ladder
/// still reaches its free rung and the turn goes on under the window. The
/// failed fold is not redispatched, and no later fence in the same turn pays
/// again.
#[tokio::test]
async fn a_failed_paid_fold_without_an_eligible_placeholder_continues_when_history_fits()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        82_500,
        40_000,
        &[180_000],
        &[1],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.fail_compaction_call();
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let first = agent
        .prompt("older replaceable intent")
        .await?
        .result()
        .await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    let second = agent.prompt("read the big file").await?.result().await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "a failed fold must continue when the clipped history fits: {:?}",
        second.outcome()
    );
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    assert!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_none(),
        "the only tool result is fresh, so the failed fold cannot discard it"
    );
    assert_eq!(
        model.paid_compaction_calls.load(Ordering::Acquire),
        1,
        "offline fit probes must not add a paid dispatch"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn manual_summary_fits_by_reducing_old_results_and_reopens_without_dispatch() -> TestResult {
    manual_summary_fit_case(false, None).await
}

#[tokio::test]
async fn paid_fold_fit_probe_uses_anchor_and_dispatches_depth_zero_without_elision() -> TestResult {
    manual_summary_fit_case(true, None).await
}

#[tokio::test]
async fn summary_capacity_without_status_reduces_old_results_before_its_only_dispatch() -> TestResult
{
    manual_summary_fit_case(false, Some(SummaryCapacityRefusal::BareContextLimit)).await
}

#[tokio::test]
async fn oversized_summary_record_reduces_old_results_before_its_only_dispatch() -> TestResult {
    manual_summary_fit_case(false, Some(SummaryCapacityRefusal::RecordTooLarge)).await
}

#[tokio::test]
async fn unrelated_summary_preparation_failure_does_not_authorize_elision() -> TestResult {
    manual_summary_fit_case(false, Some(SummaryCapacityRefusal::InvalidConfiguration)).await
}

#[tokio::test]
async fn reduced_summary_preparation_failure_reports_its_own_error_not_prior_capacity() -> TestResult
{
    manual_summary_fit_case(
        false,
        Some(SummaryCapacityRefusal::CapacityThenInvalidConfiguration),
    )
    .await
}

async fn manual_summary_fit_case(
    anchored: bool,
    refusal: Option<SummaryCapacityRefusal>,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let (builder, source_model) = placeholder_fixture(
        workspace.clone(),
        92_000,
        40_000,
        &[32_000],
        &[0],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    if anchored {
        source_model.item_context.store(true, Ordering::Release);
        source_model
            .item_system_bytes
            .store(30_000, Ordering::Release);
        source_model
            .item_provider_input_tokens
            .store(8_000, Ordering::Release);
    }
    let agent = sessions
        .create(
            "create:summary-fit",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    for prompt in [
        "read the old large result",
        "keep this latest decision verbatim",
    ] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    assert_eq!(source_model.calls.load(Ordering::Acquire), 3);
    assert_eq!(
        agent
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_some(),
        anchored
    );
    let source = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let source_history = agent.checkpoint().manifest().canonical().history().clone();
    let (builder, model) = placeholder_fixture(
        workspace.clone(),
        93_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.item_context.store(true, Ordering::Release);
    *lock_mutex(&model.summary_capacity_refusal) = refusal;
    // The 32k-byte result plus 30k of summary framing cannot fit the 40k fold window.
    // Only a matching anchor can admit depth zero; otherwise the whole-wire
    // bound requires source-derived elision before dispatch.
    model
        .summary_context_overhead
        .store(30_000, Ordering::Release);
    let operation = "compact:summary-fit";
    let identity = DurableCompactionIdentity::new(operation, source)?;
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            operation.to_owned(),
            source.session_id,
            source.branch_id,
            source.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await;
    if matches!(
        refusal,
        Some(
            SummaryCapacityRefusal::InvalidConfiguration
                | SummaryCapacityRefusal::CapacityThenInvalidConfiguration
        )
    ) {
        assert!(
            matches!(outcome, Err(AgentSessionEndpointError::Rejected { ref reason })
            if reason.contains("InvalidConfiguration")
                && !reason.contains("summary request exceeds capacity"))
        );
        assert_eq!(model.calls.load(Ordering::Acquire), 0);
        assert_eq!(
            lock_mutex(&model.compaction_wire_estimates).len(),
            if refusal == Some(SummaryCapacityRefusal::InvalidConfiguration) {
                2
            } else {
                4
            },
            "no probes may follow the unrelated refusal"
        );
        assert_eq!(
            agent.checkpoint().manifest().canonical().history(),
            &source_history
        );
        agent.shutdown().await?;
        return Ok(());
    }
    let outcome = outcome.map_err(|error| format!("summary-fit compact rejected: {error:?}"))?;
    assert!(
        matches!(outcome, AgentCompactSessionOutcome::Compacted { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "one physical summary start"
    );
    if refusal.is_some() {
        let errors = lock_mutex(&model.summary_preparation_errors);
        assert_eq!(
            errors.len(),
            2,
            "full input and its unchanged portable projection refuse"
        );
        assert!(errors[0].context_status().is_none());
        assert_eq!(
            ModelError::from(errors[0].clone()).kind(),
            ModelErrorKind::ContextLimit
        );
    }
    let prepared = lock_mutex(&store.state)
        .stored_commits
        .get(&identity.prepared_checkpoint)
        .cloned()
        .ok_or("prepared checkpoint")?;
    let operations = decode_paid_compaction_operations(prepared.checkpoint(), identity)?;
    let projection = prepared.checkpoint().manifest().canonical().projection();
    let cut = plakous_model::SemanticCompactionCut::seal(
        &projection,
        &plakous_model::ContextHistoryPlan::new(&projection)?,
    )?;
    assert_eq!(
        crate::runtime::compaction::request::validate_harness_compaction_input(
            &cut,
            identity.workflow_id,
            operations.request.request.input(),
        )?,
        Some(usize::from(!anchored) * 2)
    );
    assert_eq!(
        prepared.checkpoint().manifest().canonical().history(),
        &source_history,
        "probe elision must not mutate the canonical source"
    );
    assert!(
        lock_mutex(&model.compaction_wire_estimates)[0] * 4 > 40_000,
        "the whole-wire-only depth-zero bound overflows in both cases"
    );
    assert_eq!(
        *lock_mutex(&model.observed_compaction_anchors),
        if anchored {
            vec![true, true]
        } else {
            vec![false, false, false, false]
        },
        "depth-zero anchored fit needs one probe and one dispatch preparation; no anchor needs an elided probe too"
    );
    let target = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let history = agent.checkpoint().manifest().canonical().history().clone();
    assert!(
        history
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block,
            ContentBlock::Text(text) if text.text() == "keep this latest decision verbatim"))
    );
    agent.shutdown().await?;
    let (builder, reopened_model) = placeholder_fixture(
        workspace,
        94_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let DurableRuntimeOpen::Opened(reopened) =
        sessions.open("open:summary-fit", target, builder).await?
    else {
        return Err("reduced summary successor did not reopen".into());
    };
    assert_eq!(
        reopened.checkpoint().manifest().canonical().history(),
        &history
    );
    assert_eq!(reopened_model.calls.load(Ordering::Acquire), 0);
    reopened.shutdown().await?;
    let receipt = sessions.compaction_receipt(operation, source).await?;
    assert_eq!(
        receipt,
        DurableCompactionReceipt::Compacted { source, target }
    );
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "verifying the reduced-request receipt cannot redispatch"
    );
    Ok(())
}

#[tokio::test]
async fn irreducible_summary_capacity_denial_keeps_source_and_dispatches_nothing() -> TestResult {
    irreducible_summary_capacity_case(None).await
}

#[tokio::test]
async fn irreducible_summary_without_status_exhausts_reductions_and_preserves_source() -> TestResult
{
    irreducible_summary_capacity_case(Some(SummaryCapacityRefusal::BareContextLimit)).await
}

#[tokio::test]
async fn irreducible_summary_record_exhausts_reductions_and_preserves_source() -> TestResult {
    irreducible_summary_capacity_case(Some(SummaryCapacityRefusal::RecordTooLarge)).await
}

async fn irreducible_summary_capacity_case(refusal: Option<SummaryCapacityRefusal>) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _) = placeholder_fixture(
        workspace.clone(),
        95_000,
        40_000,
        &[72_000],
        &[0],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.with_scheduled_compaction_enabled(false).build()?;
    for prompt in ["read the old result", "latest protected intent"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let before = agent.checkpoint();
    let topology = before.manifest().topology();
    let (builder, model) = placeholder_fixture(
        workspace,
        96_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(80_000, Ordering::Release);
    *lock_mutex(&model.summary_capacity_refusal) = refusal;
    model
        .summary_context_overhead
        .store(40_000, Ordering::Release);
    let outcome = agent
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:irreducible-summary".to_owned(),
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
            AgentCompactionReason::Manual,
            builder,
        ))
        .await;
    assert!(
        matches!(outcome, Err(AgentSessionEndpointError::Rejected { ref reason })
        if reason.contains("harness summary preparation failed")
            && reason.contains("ContextLimit")
            && reason.contains("source is unchanged")
            && reason.contains("start a new session or fork with a handoff"))
    );
    let projection = before.manifest().canonical().projection();
    let cut = plakous_model::SemanticCompactionCut::seal(
        &projection,
        &plakous_model::ContextHistoryPlan::new(&projection)?,
    )?;
    let errors = lock_mutex(&model.summary_preparation_errors).clone();
    assert_eq!(
        errors.len(),
        crate::runtime::compaction::request::harness_compaction_reduction_count(&cut) + 2,
        "every candidate must be probed before final preparation refuses without dispatch"
    );
    if refusal.is_some() {
        assert!(errors.iter().all(|error| error.context_status().is_none()));
    }
    assert!(
        errors.iter().all(|error| {
            ModelError::from(error.clone()).kind() == ModelErrorKind::ContextLimit
        })
    );
    assert!(
        lock_mutex(&model.compaction_wire_estimates)
            .iter()
            .any(|tokens| *tokens < 40_000),
        "at least one reduced paid input fits the bytes/4 estimate, but not its required bound"
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        agent.checkpoint().manifest().canonical().history(),
        before.manifest().canonical().history()
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn an_unpreparable_fold_continues_when_history_fits_without_a_placeholder() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    // Two turns, so the cut selects an older intent group and the ladder
    // genuinely reaches the fold before the fold fails to prepare.
    let (builder, model) =
        unpreparable_compaction_fixture(workspace, 82_000, 40_000, &[180_000], &[1])?;
    let sink = Arc::new(RecordingTelemetrySink {
        observations: Mutex::new(Vec::new()),
    });
    let builder = builder.telemetry(Arc::new(TelemetryEmitter::enabled(sink.clone())));
    let agent = builder
        .build()
        .map_err(|error| error.message().to_owned())?;
    let first = agent
        .prompt("older replaceable intent")
        .await?
        .result()
        .await?;
    assert!(matches!(first.outcome(), TurnResultOutcome::Completed(_)));
    let second = agent.prompt("read the big file").await?.result().await?;
    assert!(
        matches!(second.outcome(), TurnResultOutcome::Completed(_)),
        "an unavailable fold must continue when the clipped history fits: {:?}",
        second.outcome()
    );
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    assert!(
        manifest
            .canonical()
            .context()
            .compaction_provenance()
            .is_none(),
        "the unavailable fold cannot fall through by discarding the fresh batch"
    );
    assert_eq!(
        placeholder_bodies(manifest.canonical().history()),
        [] as [String; 0]
    );
    assert_eq!(
        placeholder_harness_notes(manifest.canonical().history()),
        [] as [String; 0]
    );
    let affinities = model.observed_affinities();
    assert_ne!(affinities, [] as [Option<ModelCacheAffinity>; 0]);
    assert!(
        affinities.iter().all(|affinity| affinity
            .as_ref()
            .is_some_and(|affinity| affinity.session() == manifest.topology().session_id)),
        "placeholder successor measurements retain the source logical session"
    );
    assert!(
        lock_mutex(&sink.observations)
            .iter()
            .any(|observation| matches!(
                observation.kind(),
                TelemetryObservationKind::CompactionFinished {
                    refusal: Some(crate::CompactionRefusalReason::Rejected),
                    tokens_before: Some(_),
                    ..
                }
            )),
        "the failed capacity fence must retain its refusal and source measurement"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn admission_headroom_with_a_foreign_anchor_compacts_reasoning_before_the_next_prompt()
-> TestResult {
    Box::pin(summary_headroom_case(None, true, 6_000, Some(1))).await
}

#[tokio::test]
async fn portable_summary_keeps_selected_tool_facts_when_replay_omission_alone_fits() -> TestResult
{
    Box::pin(summary_headroom_case(Some(4_000), true, 6_000, Some(1))).await
}

#[tokio::test]
async fn portable_summary_elides_tool_facts_only_when_the_portable_full_input_still_overflows()
-> TestResult {
    Box::pin(summary_headroom_case(Some(12_000), true, 30_000, Some(2))).await
}

#[tokio::test]
async fn estimated_headroom_without_a_required_bound_does_not_schedule_a_paid_summary() -> TestResult
{
    Box::pin(summary_headroom_case(None, false, 6_000, None)).await
}

async fn summary_headroom_case(
    tool_bytes: Option<usize>,
    required_bound: bool,
    summary_overhead: usize,
    expected_depth: Option<usize>,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let bodies = tool_bytes.map_or_else(Vec::new, |bytes| vec![bytes]);
    let rounds = tool_bytes.map_or_else(Vec::new, |_| vec![0]);
    let (builder, _) = placeholder_fixture(
        workspace.clone(),
        140_000,
        40_000,
        &bodies,
        &rounds,
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let agent = sessions
        .create(
            "create:summary-headroom",
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?;
    for prompt in ["keep the earlier requirement", "latest protected intent"] {
        assert!(matches!(
            agent.prompt(prompt).await?.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ));
    }
    let checkpoint = agent.checkpoint();
    let source = DurableSessionAddress::new(agent.session_id(), agent.branch_id(), checkpoint.id());
    agent.shutdown().await?;
    let prior = checkpoint.manifest();
    let mut segments = prior.canonical().history().segments().to_vec();
    let old_answer = &segments[1].items()[0];
    let mut blocks = old_answer.blocks().to_vec();
    blocks.push(ContentBlock::Reasoning(ReasoningBlock::new(
        ReasoningVisibility::Visible,
        Some("r".repeat(if required_bound {
            34_000 - tool_bytes.unwrap_or(0) - usize::from(tool_bytes.is_some()) * 1_000
        } else {
            145_000
        })),
        None,
    )?));
    let mut items = segments[1].items().to_vec();
    items[0] = ConversationItem::new(
        old_answer.id(),
        old_answer.actor(),
        blocks,
        old_answer.provenance(),
    )?;
    segments[1] = HistorySegment::new(
        if tool_bytes.is_some() {
            HistorySegmentKind::ToolExchange
        } else {
            HistorySegmentKind::ReasoningReplay
        },
        items,
    )?;
    let history = CanonicalHistory::new(segments)?;
    let descriptor = placeholder_test_descriptor(40_000, false)?;
    let anchor = plakous_model::ContextRequestAnchor::new(
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"previous-descriptor"])?,
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        Usage::new(32_000, 2, 0, 0, 0),
        plakous_model::ContextWireInput::measure_wire(
            br#"{"messages":[{"role":"user","content":"old"}]}"#,
            None,
        )?,
    )
    .ok_or("foreign anchor")?;
    assert!(!anchor.matches_descriptor(&descriptor));
    let context = prior
        .canonical()
        .context()
        .with_request_anchor(Some(anchor))?;
    let canonical = RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        history.clone(),
        WorkingProjection::default(),
        context,
    )?;
    let identity = DurableCompactionIdentity::new("fixture:headroom", source)?;
    let seeded = checkpoint_with_compaction_state(
        &checkpoint,
        identity,
        prior.ledger().operations().into_iter().cloned().collect(),
        canonical,
    )?;
    let bundle = RuntimeCheckpointCodec::bundle(&seeded, None)?;
    let reference = CheckpointObjectRef::from_store(seeded.id(), bundle.root(), seeded.integrity());
    let stored = StoredCheckpoint::from_store(reference, bundle)?;
    {
        let mut state = lock_mutex(&store.state);
        let loaded = state.loaded.as_ref().ok_or("loaded branch")?;
        let head = BranchHead::from_store(
            source.session(),
            BranchRevision::from_store(seeded.manifest().topology().branch_revision),
            reference,
        );
        state.loaded = Some(crate::LoadedBranch::from_store(
            loaded.session(),
            head,
            stored.clone(),
        ));
        state.stored = Some(stored.clone());
        state.stored_commits.insert(source.checkpoint_id, stored);
        state.active_lease = None;
        state.released_lease = None;
    }
    let (builder, model) = placeholder_fixture(
        workspace.clone(),
        141_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.item_context.store(true, Ordering::Release);
    model
        .require_input_bound
        .store(required_bound, Ordering::Release);
    model
        .summary_context_overhead
        .store(summary_overhead, Ordering::Release);
    let DurableRuntimeOpen::Opened(agent) = sessions
        .open("open:summary-headroom", source, builder)
        .await?
    else {
        return Err("reasoning source did not reopen".into());
    };
    let before = agent.checkpoint();
    let status = agent
        .session_endpoint()
        .context_status(agent.session_id(), agent.branch_id(), before.id(), None)
        .await
        .map_err(|error| format!("headroom status: {error:?}"))?;
    assert!(
        matches!(status.kind(), plakous_model::ContextStatusKind::Fits { headroom }
        if crate::compaction_verdict::input_headroom_tokens(*headroom) <= 4096),
        "{status:?}"
    );
    assert_eq!(status.uses_unanchored_estimate(), !required_bound);
    if !required_bound {
        assert_eq!(
            crate::runtime::compaction::request::scheduling_input_headroom(
                status.kind(),
                status.uses_unanchored_estimate()
            ),
            None
        );
        let mut turn = agent.prompt("small follow-up").await?;
        let mut events = turn.take_events()?;
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        while let Ok(event) = events.recv().await {
            assert!(
                !matches!(event.kind(), AgentEventKind::CompactionStarted { .. }),
                "an estimate alone cannot start a paid fold"
            );
            if matches!(
                event.kind(),
                AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
            ) {
                break;
            }
        }
        assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
        assert!(
            result
                .checkpoint()
                .manifest()
                .canonical()
                .history()
                .segments()
                .iter()
                .take(history.segments().len())
                .eq(history.segments().iter())
        );
        agent.shutdown().await?;
        return Ok(());
    }
    let pending = "next requirement ".repeat(500);
    let status = agent
        .session_endpoint()
        .context_status(
            agent.session_id(),
            agent.branch_id(),
            before.id(),
            Some(pending.clone().into()),
        )
        .await
        .map_err(|error| format!("pending status: {error:?}"))?;
    assert!(matches!(
        status.kind(),
        plakous_model::ContextStatusKind::WouldOverflow(_)
    ));
    let refusals = model.ordinary_capacity_refusals.load(Ordering::Acquire);
    let result = agent.prompt(pending).await?.result().await?;
    assert_eq!(
        model.ordinary_capacity_refusals.load(Ordering::Acquire),
        refusals,
        "scheduled compaction must precede ordinary admission's refusal, not recover after it"
    );
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        2,
        "summary then the pending prompt"
    );
    assert!(
        !lock_mutex(&model.summary_preparation_errors).is_empty(),
        "the unabridged reasoning-heavy summary must refuse before its portable candidate fits"
    );
    let prepared = lock_mutex(&store.state)
        .stored_commits
        .values()
        .find(|stored| {
            stored
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .iter()
                .any(|operation| {
                    matches!(operation.kind(), DurableOperationKind::Compaction { .. })
                        && matches!(operation.state(), DurableOperationState::Prepared)
                })
        })
        .cloned()
        .ok_or("prepared summary checkpoint")?;
    assert_eq!(
        prepared.checkpoint().manifest().canonical().history(),
        &history
    );
    let prepared_projection = prepared.checkpoint().manifest().canonical().projection();
    let cut = plakous_model::SemanticCompactionCut::seal(
        &prepared_projection,
        &plakous_model::ContextHistoryPlan::new(&prepared_projection)?,
    )?;
    let record = prepared
        .checkpoint()
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Compaction { .. }))
        .ok_or("summary operation")?;
    let paid: crate::runtime::compaction::DurableCompactionRequestRecord =
        serde_json::from_slice(record.request().bytes())?;
    let identity = DurableCompactionIdentity::new(&paid.operation_id, paid.source())?;
    let operations = decode_paid_compaction_operations(prepared.checkpoint(), identity)?;
    assert_eq!(
        crate::runtime::compaction::request::validate_harness_compaction_input(
            &cut,
            identity.workflow_id,
            operations.request.request.input(),
        )?,
        expected_depth
    );
    let selected_results = operations
        .request
        .request
        .input()
        .items()
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| {
            if let ContentBlock::ToolResult(result) = block {
                Some(result)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(selected_results.len(), usize::from(tool_bytes.is_some()));
    if let Some(bytes) = tool_bytes {
        let [ToolResultPart::Text(text)] = selected_results[0].parts() else {
            return Err("selected result shape".into());
        };
        if expected_depth == Some(1) {
            assert_eq!(
                text.text(),
                "x".repeat(bytes),
                "portable full input keeps tool facts"
            );
        } else {
            assert!(
                text.text().starts_with(
                    crate::runtime::compaction::placeholder::REPLACED_TOOL_RESULT_MARKER
                )
            );
        }
    }
    assert!(
        !operations
            .request
            .request
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Reasoning(_)))
    );
    let target = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        result.checkpoint().id(),
    );
    agent.shutdown().await?;
    let (builder, reopened_model) = placeholder_fixture(
        workspace,
        142_000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:portable-summary", target, builder)
        .await?
    else {
        return Err("portable successor did not reopen".into());
    };
    assert_eq!(reopened_model.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        crate::runtime::compaction::request::harness_compaction_reduction_count(&cut),
        1 + usize::from(tool_bytes.is_some())
    );
    reopened.shutdown().await?;
    assert!(matches!(
        sessions
            .compaction_receipt(&paid.operation_id, paid.source())
            .await?,
        DurableCompactionReceipt::Compacted { .. }
    ));
    assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 1);
    Ok(())
}
