//! The live owner's mid-turn prune keep-rules.

use super::*;

#[tokio::test(start_paused = true)]
async fn the_owner_never_prunes_while_a_paid_fold_awaits_its_receipt() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = scheduled_tool_group_fence_builder(
        workspace,
        true,
        97_100,
        900_000,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let store = Arc::new(TestCreateStore::default());
    store.simulate_prune.store(true, Ordering::Release);
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime.create("create:prune-window", builder).await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    // Call 3 is the tool-group fence's paid fold: its workflow and provider
    // operations are durable, its receipt is not.
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(Arc::clone(&release));
    model.held_call.store(3, Ordering::Release);
    let turn = agent.prompt("tool then scheduled compaction").await?;
    tokio::time::timeout(Duration::from_secs(5), model.held_call_started.notified()).await?;

    let head = agent.checkpoint();
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Working { .. }
    ));
    assert!(
        head.manifest()
            .ledger()
            .operations()
            .into_iter()
            .any(|record| {
                matches!(record.kind(), DurableOperationKind::Compaction { .. })
                    && !matches!(record.state(), DurableOperationState::Finalized(_))
            })
    );
    assert!(!crate::runtime::history_prunable_at(&head));
    let prunes = lock_mutex(&store.prune_log).len();

    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result()).await??;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    // Nothing was pruned inside the fold's window; with its receipt
    // committed, its turn's later heads are prunable again.
    let accepted = lock_mutex(&store.accepted_commits).clone();
    assert!(
        lock_mutex(&store.prune_log)[prunes..]
            .iter()
            .all(|prune| !prune.working_head
                || crate::runtime::history_prunable_at(&accepted[prune.accepted - 1].0)),
        "a mid-turn prune ran while a session workflow was unfinalized"
    );
    assert!(crate::runtime::history_prunable_at(&agent.checkpoint()));
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn mid_turn_prunes_keep_exactly_the_four_latest_terminal_checkpoints() -> TestResult {
    const SHORT_TURNS: usize = 6;
    const TOOL_ROUNDS: usize = 24;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    // Calls 0-5 answer the short turns; the long turn's calls then probe.
    let rounds = (SHORT_TURNS..SHORT_TURNS + TOOL_ROUNDS).collect::<Vec<_>>();
    let (builder, _model) = placeholder_fixture(
        workspace,
        0x5702_0000,
        500_000_000,
        &[64; TOOL_ROUNDS],
        &rounds,
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let store = Arc::new(TestCreateStore::default());
    store.simulate_prune.store(true, Ordering::Release);
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:prune-terminal-window",
            builder.config(AgentConfig::default().with_max_model_rounds(TOOL_ROUNDS + 1)),
        )
        .await?;
    let mut terminals = Vec::new();
    for prompt in 0..SHORT_TURNS {
        let result = agent
            .prompt(format!("short turn {prompt}"))
            .await?
            .result()
            .await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        terminals.push(result.checkpoint_id());
    }
    let window = crate::runtime::checkpoint_retained_history(&agent.checkpoint());
    assert_eq!(
        window,
        terminals[SHORT_TURNS - 4..]
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        "the terminal window is the four latest root results"
    );
    let result = agent.prompt("probe repeatedly").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), TOOL_ROUNDS + 1);
    let log = lock_mutex(&store.prune_log).clone();
    let working = log
        .iter()
        .filter(|prune| prune.working_head)
        .collect::<Vec<_>>();
    assert!(!working.is_empty(), "the long turn pruned between rounds");
    for prune in working {
        assert_eq!(prune.retained, window);
        assert!(prune.removed.is_disjoint(&window));
    }
    agent.shutdown().await?;
    Ok(())
}

/// A paid endpoint compaction whose store halts right after the workflow's
/// completed checkpoint: identity and response are durable, the receipt is not.
struct HaltedFold {
    store: Arc<TestCreateStore>,
    sessions: DurableSessionRuntime,
    agent: Agent,
    workspace: Arc<WorkspacePolicy>,
    source: DurableSessionAddress,
    identity: DurableCompactionIdentity,
    recovery: plakous_test_support::ScriptedRecoveryState,
    live_model: Arc<RecoveryCompactionModel>,
    _directory: tempfile::TempDir,
}

const OPERATION: &str = "compact:prune-window";

async fn halted_between_identity_and_receipt() -> TestResult<HaltedFold> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    store.simulate_prune.store(true, Ordering::Release);
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let prompts = (0..6)
        .map(|index| format!("closed intent {index}"))
        .chain(["latest protected intent".to_owned()])
        .collect::<Vec<_>>();
    let (agent, source) = seeded_compaction_source_with_prompts(
        &sessions,
        Arc::clone(&workspace),
        "create:prune-compaction-window",
        91_000,
        &prompts,
    )
    .await?;
    let operation_id = OPERATION;
    let identity = DurableCompactionIdentity::new(operation_id, source)?;
    let recovery = plakous_test_support::ScriptedRecoveryState::new();
    let (builder, live_model) = recovery_compaction_fixture(
        Arc::clone(&workspace),
        recovery.clone(),
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    store.halt_after_checkpoint(identity.completed_checkpoint);
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
        .map_err(|_| "compaction endpoint rejected exact source")?;
    assert!(matches!(
        outcome,
        AgentCompactSessionOutcome::RecoveryRequired { checkpoint, .. }
            if checkpoint == identity.completed_checkpoint
    ));
    Ok(HaltedFold {
        store,
        sessions,
        agent,
        workspace,
        source,
        identity,
        recovery,
        live_model,
        _directory: directory,
    })
}

/// A crash between a compaction's identity commit and its receipt leaves a
/// Working head the owner may not prune: its recovery still loads the
/// workflow's source and the recovered commit's previous checkpoint by name,
/// and lands the same successor without a second paid call.
#[tokio::test]
async fn working_prune_waits_out_a_compaction_between_identity_and_receipt() -> TestResult {
    let HaltedFold {
        store,
        sessions,
        agent,
        workspace,
        source,
        identity,
        recovery,
        live_model,
        _directory,
    } = halted_between_identity_and_receipt().await?;
    let operation_id = OPERATION;
    let completed = lock_mutex(&store.state)
        .stored_commits
        .get(&identity.completed_checkpoint)
        .cloned()
        .ok_or("completed checkpoint was not durably published")?;
    assert!(
        source.checkpoint_id
            != completed
                .checkpoint()
                .manifest()
                .topology()
                .parent_checkpoint_id
                .ok_or("parent")?,
        "the workflow's source lies below the head's parent, where a prune reaches"
    );
    // Exactly what the owner's rule permits at this head: nothing.
    assert!(!crate::runtime::history_prunable_at(completed.checkpoint()));
    assert!(lock_mutex(&store.prune_log).is_empty());

    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let (restart_builder, restart_model) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::from_snapshot(&recovery.snapshot()?)?,
        CompactionFixtureOutcome::Summary,
        None,
    )?;
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
    assert_eq!(live_model.starts.load(Ordering::Acquire), 1);
    assert_eq!(restart_model.starts.load(Ordering::Acquire), 0);
    // With the receipt committed the Quiescent successor is prunable again.
    let success = lock_mutex(&store.state)
        .stored_commits
        .get(&identity.success_checkpoint)
        .cloned()
        .ok_or("success checkpoint")?;
    assert!(crate::runtime::history_prunable_at(success.checkpoint()));
    Ok(())
}

/// A finalized in-turn fold does not keep later rounds from pruning. The
/// removed checkpoints are all older than the recency margin.
#[tokio::test]
async fn a_finalized_fold_does_not_block_later_round_prunes() -> TestResult {
    // The fence folds once, at the third turn's tool group (prepare 4 reports
    // the over-window count; call 3 is that paid fold). Later ordinary calls
    // stay tools, so the turn keeps committing after the fold has finalized.
    const AFTER: usize = 40;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            4,
            97_400,
            false,
        )
        .with_tool_call_on(2)
        .with_context_tokens_from_prepare(4, 900_000)
        .with_scripted_tool_calls((4..4 + AFTER).map(|call| (call, "probe", "{}".to_owned()))),
    );
    let builder = scheduled_tool_group_fence_builder_with_model(
        Arc::clone(&workspace),
        true,
        Arc::clone(&model),
        LocalDurabilityDisposition::DurableRequired,
    )?
    .config(
        AgentConfig::default()
            .with_max_model_rounds(AFTER + 16)
            .with_scheduled_compaction_enabled(true),
    );
    let store = Arc::new(TestCreateStore::default());
    store.simulate_prune.store(true, Ordering::Release);
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime.create("create:prune-after-fold", builder).await?;
    for prompt in ["older replaceable intent", "latest protected intent"] {
        let result = agent.prompt(prompt).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let result = agent
        .prompt("tool then fold then more tools")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_prunes_after_the_fold_keep_the_margin(store.as_ref())?;
    agent.shutdown().await?;
    Ok(())
}

fn carries_finalized_compaction(checkpoint: &RuntimeCheckpoint) -> bool {
    checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .any(|record| {
            matches!(record.kind(), DurableOperationKind::Compaction { .. })
                && matches!(record.state(), DurableOperationState::Finalized(_))
        })
}

fn assert_prunes_after_the_fold_keep_the_margin(store: &TestCreateStore) -> TestResult {
    let accepted = lock_mutex(&store.accepted_commits).clone();
    let revisions = {
        let state = lock_mutex(&store.state);
        state
            .stored_commits
            .iter()
            .map(|(id, stored)| {
                (
                    *id,
                    stored.checkpoint().manifest().topology().branch_revision,
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let log = lock_mutex(&store.prune_log).clone();
    assert!(
        accepted
            .iter()
            .any(|(checkpoint, _)| carries_finalized_compaction(checkpoint)),
        "the turn never finalized a compaction"
    );
    let after_fold = log
        .iter()
        .filter(|prune| {
            prune.working_head
                && !prune.removed.is_empty()
                && accepted
                    .get(prune.accepted.saturating_sub(1))
                    .is_some_and(|(checkpoint, _)| carries_finalized_compaction(checkpoint))
        })
        .collect::<Vec<_>>();
    assert!(
        !after_fold.is_empty(),
        "no working prune removed checkpoints after the fold finalized; {} prunes, {} commits",
        log.len(),
        accepted.len()
    );
    for prune in after_fold {
        let head_rev = accepted[prune.accepted - 1]
            .0
            .manifest()
            .topology()
            .branch_revision;
        let floor = head_rev.saturating_sub(crate::PruneBranch::WORKING_RECENCY_MARGIN);
        for id in &prune.removed {
            let revision = revisions
                .get(id)
                .copied()
                .ok_or("removed checkpoint was not stored")?;
            assert!(
                revision < floor,
                "removed revision {revision} at head {head_rev}; the margin keeps {floor} and newer"
            );
        }
    }
    Ok(())
}

/// The owner's guard is what keeps an unfinalized compaction's source. A
/// prune that ignores it removes that source, and receipt recovery then
/// refuses instead of reporting the fold.
#[tokio::test]
async fn pruning_past_the_unfinalized_compaction_guard_refuses_receipt_recovery() -> TestResult {
    let HaltedFold {
        store,
        sessions,
        agent,
        workspace,
        source,
        identity,
        recovery,
        live_model: _,
        _directory,
    } = halted_between_identity_and_receipt().await?;
    let completed = lock_mutex(&store.state)
        .stored_commits
        .get(&identity.completed_checkpoint)
        .cloned()
        .ok_or("completed checkpoint was not durably published")?;
    assert!(!crate::runtime::history_prunable_at(completed.checkpoint()));
    let head = store.recover_commit(identity.completed_checkpoint).await?;
    let lease = store.active_lease().ok_or("lease")?;
    let pruned = store
        .prune(crate::PruneBranch::at_working_head_with_margin(
            lease,
            head.receipt().head(),
            BTreeSet::new(),
            1,
            0,
        ))
        .await?;
    assert!(
        pruned.pruned_checkpoints() > 0,
        "a prune with no recency margin removed nothing"
    );
    assert_eq!(
        store
            .load_checkpoint(source.checkpoint_id)
            .await
            .map(|_| ()),
        Err(SessionStoreError::Pruned),
        "the compaction source survived a prune that ignored the guard"
    );
    agent.shutdown().await?;
    store.resume_commits();
    store.expire_active_lease();
    let (restart_builder, _) = recovery_compaction_fixture(
        workspace,
        plakous_test_support::ScriptedRecoveryState::from_snapshot(&recovery.snapshot()?)?,
        CompactionFixtureOutcome::Summary,
        None,
    )?;
    let recovered = Box::pin(sessions.recover_compaction(OPERATION, source, restart_builder)).await;
    let refusal = match recovered {
        Err(error) => error,
        Ok(outcome) => sessions
            .compaction_receipt(OPERATION, source)
            .await
            .expect_err(&format!(
                "recovery {outcome:?} left the compaction receipt readable"
            )),
    };
    assert_eq!(
        refusal.store_error(),
        Some(SessionStoreError::Pruned),
        "{refusal:?}"
    );
    Ok(())
}
