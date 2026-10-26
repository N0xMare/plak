//! A tool exchange's next-round admission prepares the request the next round
//! dispatches.
use super::cancelled_steer::{HeldTool, held_probe_registry};
use super::*;

async fn prepared_rounds(
    entry_base: u128,
    tool_rounds: &[usize],
    config: AgentConfig,
) -> TestResult<(TurnResult, Arc<ArtifactModel>, Vec<Vec<String>>)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        entry_base,
        400_000,
        &[200; 4],
        tool_rounds,
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.config(config).build()?;
    let mut turn = agent.prompt("probe once").await?;
    let mut events = turn.take_events()?;
    let mut tools = Vec::new();
    loop {
        match events.recv().await?.kind() {
            AgentEventKind::RequestPrepared { tool_names, .. } => {
                tools.push(tool_names.iter().map(ToString::to_string).collect());
            }
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => break,
            _ => {}
        }
    }
    let result = turn.result().await?;
    agent.shutdown().await?;
    Ok((result, model, tools))
}

/// The round after a tool exchange dispatches the request the exchange's
/// admission prepared: one preparation per round, none repeated.
#[tokio::test]
async fn a_tool_round_dispatches_the_request_its_admission_prepared() -> TestResult {
    let (result, model, tools) = prepared_rounds(0xad_0000, &[0], AgentConfig::default()).await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    assert_eq!(model.prepares.load(Ordering::Acquire), 2);
    let records = model.prepared_records();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].identity(), records[1].identity());
    assert_eq!(tools, vec![vec!["probe".to_owned()]; 2]);
    Ok(())
}

/// A wrap-up round withdraws tool selection, so the request the admission
/// prepared is not the one the round sends: the round prepares afresh.
#[tokio::test]
async fn a_wrap_up_round_prepares_its_own_tool_free_request() -> TestResult {
    let (result, model, tools) = prepared_rounds(
        0xad_1000,
        &[0],
        AgentConfig::default().with_max_model_rounds(1),
    )
    .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    // Round one, the admission of its exchange, and the wrap-up round.
    assert_eq!(model.prepares.load(Ordering::Acquire), 3);
    assert_eq!(model.prepared_records().len(), 3);
    assert_eq!(tools.len(), 2);
    Ok(())
}

/// The round's preparations since `from`: the admission's attempt and the
/// round's differ when the round prepared afresh.
fn round_attempts(model: &ArtifactModel, from: usize) -> Vec<AttemptId> {
    lock_mutex(&model.round_prepares)[from..].to_vec()
}

/// A credential refreshed after the admission (fence (b)'s measurement here)
/// authorizes the same captured request without another preparation or attempt.
#[tokio::test]
async fn a_credential_refresh_after_the_admission_dispatches_the_same_prepared_round() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        0xad_2000,
        400_000,
        &[200; 4],
        &[0],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    *lock_mutex(&model.refresh_at_tool_group_measurement) =
        Some(artifact_authority(&model.descriptor, 2)?);
    let agent = builder.with_scheduled_compaction_enabled(true).build()?;
    let result = agent.prompt("probe once").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(lock_mutex(&model.refreshed_authority).is_some());
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    let rounds = round_attempts(&model, 0);
    assert_eq!(rounds.len(), 2, "{rounds:?}");
    // Two model rounds and the tool-group scheduling measurement.
    assert_eq!(model.prepares.load(Ordering::Acquire), 3);
    assert!(lock_mutex(&model.prepared_attempts).contains(&(rounds[1], 1)));
    agent.shutdown().await?;
    Ok(())
}

/// A steer queued while the tool group runs is applied after the exchange's
/// admission, so the next round prepares afresh under its own attempt.
#[tokio::test]
async fn a_steer_during_the_tool_group_prepares_the_round_afresh() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0xad_3000,
            false,
        )
        .with_tool_first(),
    );
    let tool = Arc::new(HeldTool::default());
    let runtime = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:admitted-steer",
            session_update_builder(workspace, model.clone(), held_probe_registry(tool.clone())?)?,
        )
        .await?;
    let turn = agent.prompt("read").await?;
    wait_for_scheduler_fact(&tool.started).await;
    turn.control().steer("queued instruction").await?;
    tool.release.store(true, Ordering::Release);
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 2);
    let rounds = round_attempts(&model, 0);
    assert_eq!(rounds.len(), 3, "{rounds:?}");
    assert_ne!(rounds[1], rounds[2]);
    agent.shutdown().await?;
    Ok(())
}

/// A fold attempt at fence (b) appends to the ledger after the exchange's
/// admission, so the round after it prepares afresh.
#[tokio::test]
async fn a_fold_attempt_at_the_tool_group_fence_prepares_the_round_afresh() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = scheduled_tool_group_fence_builder(
        workspace,
        true,
        0xad_4000,
        900_000,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.build()?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let from = lock_mutex(&model.round_prepares).len();
    let result = agent
        .prompt("use the growing probe at the tool-group fence")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.tool_calls(), 1);
    assert!(compaction_workflow_count(result.checkpoint()) >= 1);
    let rounds = round_attempts(&model, from);
    assert_eq!(rounds.len(), 3, "{rounds:?}");
    assert_ne!(rounds[1], rounds[2]);
    agent.shutdown().await?;
    Ok(())
}

/// Informational, not a gate: the wall time and model preparations of a
/// two-round tool turn over a 1,000-prompt history (3,000 segments). Run with
/// `cargo nextest run --cargo-profile release --run-ignored only -E
/// 'test(tool_turn_cost_at_a_thousand_prompts)' --no-capture`.
#[tokio::test]
#[ignore = "informational timing; not a gate"]
#[allow(clippy::print_stderr, reason = "opt-in informational timing output")]
async fn tool_turn_cost_at_a_thousand_prompts() -> TestResult {
    const PROMPTS: u128 = 1_000;
    const TURNS: usize = 10;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let tool_rounds = (0..TURNS).map(|turn| turn * 2).collect::<Vec<_>>();
    let (builder, model) = placeholder_fixture(
        workspace,
        0xbe_0000,
        4_000_000,
        &[200; 16],
        &tool_rounds,
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.script_tool_rounds_around_compactions();
    let text = |index: u128, body: &str| -> TestResult<ConversationItem> {
        let user = index.is_multiple_of(3);
        Ok(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(0xbe_0000_0000 + index))?,
            if user { Actor::User } else { Actor::Assistant },
            vec![ContentBlock::Text(TextBlock::new(body.repeat(8), None)?)],
            if user {
                Provenance::UserInput
            } else {
                Provenance::ModelOutput
            },
        )?)
    };
    let mut segments = Vec::new();
    for prompt in 0..PROMPTS {
        segments.push(HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(prompt * 3, "a question about the code ")?],
        )?);
        for offset in 1..3 {
            segments.push(HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(prompt * 3 + offset, "an answer about the code ")?],
            )?);
        }
    }
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create_seeded(
            "bench:tool-turn",
            builder.with_scheduled_compaction_enabled(false),
            CanonicalHistory::new(segments)?,
        )
        .await?;
    let mut elapsed = Vec::new();
    let mut prepares = Vec::new();
    for turn in 0..TURNS {
        let before = model.prepares.load(Ordering::Acquire);
        let started = std::time::Instant::now();
        let result = agent
            .prompt(format!("probe {turn}"))
            .await?
            .result()
            .await?;
        elapsed.push(started.elapsed().as_secs_f64() * 1_000.0);
        prepares.push(model.prepares.load(Ordering::Acquire) - before);
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(result.model_rounds(), 2);
    }
    agent.shutdown().await?;
    let mut sorted = elapsed.clone();
    sorted.sort_by(f64::total_cmp);
    eprintln!(
        "{}",
        serde_json::json!({
            "kind": "tool-turn-cost",
            "history_segments": PROMPTS * 3,
            "turn_ms_median": sorted[TURNS / 2],
            "turn_ms": elapsed,
            "prepares_per_turn": prepares,
        })
    );
    Ok(())
}
