//! History retention: automatic pruning after terminal publication, and the
//! explicit prune a product drives for `gc`.

use super::*;
use plakous_store::{SqliteSessionStore, SqliteStoreOptions};

fn retention_binding(workspace: &Arc<WorkspacePolicy>) -> TestResult<SessionRuntimeBinding> {
    Ok(completing_runtime_binding_with_capture_and_durability(
        Arc::clone(workspace),
        descriptor()?,
        b"retention",
        LocalDurabilityDisposition::DurableRequired,
    )?
    .0)
}

fn open_retention_store(path: &Path) -> TestResult<(SqliteSessionStore, AgentSessions)> {
    let store = SqliteSessionStore::open_with_clock_and_options(
        path,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        SqliteStoreOptions::default(),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    Ok((store, sessions))
}

#[tokio::test]
async fn automatic_retention_keeps_hundreds_of_rounds_bounded_with_flat_growth() -> TestResult {
    const ROUNDS: u64 = 300;
    // Head, its parent, the create checkpoint, the retained terminal results
    // and less than one automatic prune's worth of superseded checkpoints.
    const CHECKPOINT_BOUND: usize = 128;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Disabled,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("retention:memory")?,
            completing_runtime_binding_with_capture_and_durability(
                Arc::clone(&workspace),
                descriptor()?,
                b"retention",
                LocalDurabilityDisposition::Ephemeral,
            )?
            .0,
        ))
        .await?;
    let start_bytes = store.stats()?.unique_object_bytes();
    let mut peak = 0;
    let mut half_bytes = 0;
    for round in 1..=ROUNDS {
        let result = session
            .agent()
            .prompt(format!("round {round}"))
            .await?
            .result()
            .await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        let stats = store.stats()?;
        peak = peak.max(stats.checkpoints());
        if round == ROUNDS / 2 {
            half_bytes = stats.unique_object_bytes();
        }
    }
    assert!(
        peak <= CHECKPOINT_BOUND,
        "automatic retention let {peak} checkpoints accumulate over {ROUNDS} rounds"
    );
    // Retained bytes grow only with the conversation itself: the second half
    // of the rounds adds no more than the first half did, with slack for one
    // automatic prune's worth of not yet reclaimed history.
    let end_bytes = store.stats()?.unique_object_bytes();
    let first_half = half_bytes.saturating_sub(start_bytes);
    let second_half = end_bytes.saturating_sub(half_bytes);
    assert!(
        second_half <= first_half.saturating_mul(3) / 2,
        "first half grew {first_half} bytes, second half {second_half}"
    );
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_retention_prunes_automatically_and_keeps_history_exact() -> TestResult {
    const ROUNDS: u64 = 48;
    const CHECKPOINT_BOUND: u64 = 128;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("retention.sqlite3");
    let (store, sessions) = open_retention_store(&database)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("retention:create")?,
            retention_binding(&workspace)?,
        ))
        .await?;
    let mut peak = 0;
    let mut previous = 0;
    let mut automatic = false;
    for round in 1..=ROUNDS {
        let result = session
            .agent()
            .prompt(format!("round {round}"))
            .await?
            .result()
            .await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        let checkpoints = store.stats().await?.checkpoints();
        automatic |= checkpoints < previous;
        previous = checkpoints;
        peak = peak.max(checkpoints);
    }
    assert!(automatic, "no automatic prune ran in {ROUNDS} rounds");
    assert!(peak <= CHECKPOINT_BOUND, "{peak} checkpoints accumulated");
    let locator = session.release().await?;
    let exported = AgentSessions::export_history_sqlite(&database, locator)?;

    let PruneHistoryOutcome::Pruned(report) = sessions.prune_history(locator).await? else {
        return Err("a released quiescent head was not pruned".into());
    };
    assert_eq!(report.locator(), locator);
    sessions.reclaim().await?;
    let pruned = store.stats().await?;
    let PruneHistoryOutcome::Pruned(repeat) = sessions.prune_history(locator).await? else {
        return Err("a repeated prune changed outcome".into());
    };
    assert_eq!(repeat.pruned_checkpoints(), 0);
    let repeated = store.stats().await?;
    assert_eq!(
        (
            repeated.checkpoints(),
            repeated.objects(),
            repeated.object_bytes()
        ),
        (
            pruned.checkpoints(),
            pruned.objects(),
            pruned.object_bytes()
        )
    );
    assert_eq!(
        AgentSessions::export_history_sqlite(&database, locator)?,
        exported,
        "pruning changed the carried history"
    );
    drop(sessions);
    store.shutdown().await?;
    drop(store);

    // The pruned store passes full verification, and the session resumes.
    drop(SqliteSessionStore::open_verified(&database)?);
    let (store, sessions) = open_retention_store(&database)?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("retention:reopen")?,
            locator,
            retention_binding(&workspace)?,
        ))
        .await?
    else {
        return Err("the pruned session did not reopen quiescently".into());
    };
    let result = session
        .agent()
        .prompt("after pruning")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    session.release().await?;
    drop(sessions);
    store.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn prune_history_refuses_an_attached_session_and_names_a_moved_head() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("retention-ownership.sqlite3");
    let (store, sessions) = open_retention_store(&database)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("retention:attached")?,
            retention_binding(&workspace)?,
        ))
        .await?;
    let created = session.locator();
    let attached = sessions
        .prune_history(created)
        .await
        .expect_err("an attached session was pruned");
    assert_eq!(attached.kind(), AgentSessionsErrorKind::Ownership);
    let result = session.agent().prompt("move").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let head = session.release().await?;
    let stale = sessions
        .prune_history(created)
        .await
        .expect_err("a stale locator was pruned");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(head));
    assert!(matches!(
        sessions.prune_history(head).await?,
        PruneHistoryOutcome::Pruned(_)
    ));
    assert!(sessions.archived_sessions().await?.is_empty());
    let archived = sessions.archive(head).await?;
    assert_eq!(
        sessions.archived_sessions().await?,
        vec![ArchivedSessionReport::new(
            archived.session_id(),
            archived.archived_revision()
        )]
    );
    sessions
        .delete(archived.session_id(), archived.archived_revision())
        .await?;
    assert!(sessions.archived_sessions().await?.is_empty());
    drop(sessions);
    store.shutdown().await?;
    Ok(())
}

/// Retention keeps the terminal results of the 16 most recent root commands:
/// a duplicate of one of them replays its exact result after automatic prunes
/// and a reopen, and a duplicate of an older one is refused, typed and
/// unrecorded, without running again.
#[tokio::test]
async fn duplicates_replay_within_the_retained_window_and_are_reclaimed_past_it() -> TestResult {
    use plakous_agent::{
        CommandAckDisposition, CommandId, ControlError, ControlErrorKind, TurnCommand,
    };

    const ROUNDS: usize = 80;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("retention-duplicates.sqlite3");
    let (store, sessions) = open_retention_store(&database)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("retention:duplicates")?,
            retention_binding(&workspace)?,
        ))
        .await?;
    let command = |round: usize| CommandId::new(format!("retention:duplicate:{round}"));
    let mut results = Vec::new();
    let mut admission_sequences = Vec::new();
    let mut previous = 0;
    let mut automatic = false;
    for round in 1..=ROUNDS {
        let submission = session
            .agent()
            .start(command(round)?, format!("round {round}"))
            .await?;
        admission_sequences.push(submission.acknowledgement().feed_seq());
        let turn = submission
            .into_turn()
            .ok_or("an accepted start omitted its turn")?;
        let result = turn.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        results.push((result.turn_id(), result.checkpoint_id()));
        let checkpoints = store.stats().await?.checkpoints();
        automatic |= checkpoints < previous;
        previous = checkpoints;
    }
    assert!(automatic, "no automatic prune ran in {ROUNDS} rounds");
    let locator = session.release().await?;
    // Reclaim everything outside the retained window before reopening.
    let PruneHistoryOutcome::Pruned(_) = sessions.prune_history(locator).await? else {
        return Err("a released quiescent head was not pruned".into());
    };
    drop(sessions);
    store.shutdown().await?;
    drop(store);

    let (store, sessions) = open_retention_store(&database)?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("retention:duplicates:reopen")?,
            locator,
            retention_binding(&workspace)?,
        ))
        .await?
    else {
        return Err("the pruned session did not reopen quiescently".into());
    };

    let recent = ROUNDS - 1;
    let replayed = session
        .agent()
        .command(TurnCommand::Start {
            command_id: command(recent)?,
            text: format!("round {recent}"),
        })
        .await?;
    assert_eq!(
        replayed.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert_eq!(
        replayed.acknowledgement().feed_seq(),
        admission_sequences[recent - 1],
        "reopen restores the admission sequence even after checkpoint pruning"
    );
    assert!(replayed.acknowledgement().feed_seq().is_some());
    assert!(replayed.turn().is_none(), "a duplicate started a turn");
    let result = replayed
        .result()
        .ok_or("a retained duplicate carried no result")?
        .clone()
        .result()
        .await?;
    assert_eq!(
        (result.turn_id(), result.checkpoint_id()),
        results[recent - 1]
    );

    let reclaimed_round = ROUNDS - 17;
    let reclaimed = session
        .agent()
        .command(TurnCommand::Start {
            command_id: command(reclaimed_round)?,
            text: format!("round {reclaimed_round}"),
        })
        .await?;
    assert!(
        reclaimed.turn().is_none(),
        "a reclaimed duplicate ran again"
    );
    assert!(reclaimed.result().is_none());
    assert_eq!(
        reclaimed
            .acknowledgement()
            .rejection()
            .map(ControlError::kind),
        Some(ControlErrorKind::Reclaimed)
    );
    assert_eq!(
        session.locator(),
        locator,
        "a refused duplicate moved the head"
    );
    session.release().await?;
    drop(sessions);
    store.shutdown().await?;
    Ok(())
}

/// A child recovery reopens at the tree's recorded locator, which may name a
/// checkpoint retention has since reclaimed: every read on that path answers
/// a stale locator naming the retained head, and the head opens. The create
/// checkpoint a replayed create loads is retained.
#[tokio::test]
async fn a_recovery_reopen_at_a_reclaimed_locator_adopts_the_retained_head() -> TestResult {
    const ROUNDS: usize = 72;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("retention-recovery.sqlite3");
    let (store, sessions) = open_retention_store(&database)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("retention:child:create")?,
            retention_binding(&workspace)?,
        ))
        .await?;
    let created = session.locator();
    let mut recorded = None;
    for round in 1..=ROUNDS {
        let result = session
            .agent()
            .prompt(format!("round {round}"))
            .await?
            .result()
            .await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        recorded.get_or_insert(session.locator());
    }
    let recorded = recorded.ok_or("no round ran")?;
    let head = session.release().await?;
    let PruneHistoryOutcome::Pruned(_) = sessions.prune_history(head).await? else {
        return Err("a released quiescent head was not pruned".into());
    };
    assert!(matches!(
        store.load_checkpoint(recorded.checkpoint_id()).await,
        Err(SessionStoreError::Pruned)
    ));

    let stale = sessions
        .created_request_output_bound(recorded)
        .await
        .expect_err("a reclaimed locator read its configuration");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(head));
    sessions.created_request_output_bound(head).await?;
    let stale = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("retention:child:reopen:0")?,
            recorded,
            retention_binding(&workspace)?,
        ))
        .await
        .expect_err("a reclaimed locator opened");
    assert_eq!(stale.kind(), AgentSessionsErrorKind::StaleLocator);
    assert_eq!(stale.current_locator(), Some(head));
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("retention:child:reopen:1")?,
            head,
            retention_binding(&workspace)?,
        ))
        .await?
    else {
        return Err("the retained head did not reopen quiescently".into());
    };
    assert_eq!(session.release().await?, head);
    store.load_checkpoint(created.checkpoint_id()).await?;
    drop(sessions);
    store.shutdown().await?;
    Ok(())
}
