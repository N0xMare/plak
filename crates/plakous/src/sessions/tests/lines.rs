//! The head fence for an attached predecessor, fork lines and the facade's
//! line reads (spec 01 §5.1, §5.6, §5.10).

use super::*;

/// A carry moves the line's head while the source's driver is still attached:
/// its next commit is refused by the head fence, and the driver stays fenced.
#[tokio::test]
async fn an_attached_predecessor_is_fenced_after_a_carry_moves_its_line() -> TestResult {
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
    let binding =
        || completing_runtime_binding_for(Arc::clone(&workspace), descriptor()?, b"account-alpha");
    let CreateSessionOutcome::Created {
        session: source, ..
    } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:fenced-source")?,
            binding()?,
        ))
        .await?;
    let line = source.line();
    let head = sessions.line_head(line).await?;
    assert_eq!(
        (head.session_id(), head.branch_id(), head.generation()),
        (
            source.locator().session_id(),
            source.locator().branch_id(),
            1
        )
    );
    let outcome = source
        .agent()
        .prompt("before the carry")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let before = source.locator();

    let CreateSessionOutcome::Created {
        session: carried, ..
    } = sessions
        .create(
            CreateSessionRequest::new(SessionOperationId::new("create:carried")?, binding()?)
                .continuing_line(before),
        )
        .await?;
    assert_eq!(carried.line(), line);
    let head = sessions.line_head(line).await?;
    assert_eq!(head.session_id(), carried.locator().session_id());
    assert_eq!(head.generation(), 2);

    // The predecessor's next commit is fenced; nothing reaches its branch.
    let fenced = match source.agent().prompt("after the carry").await {
        Ok(turn) => matches!(
            turn.result().await?.outcome(),
            TurnResultOutcome::Completed(_)
        ),
        Err(_) => false,
    };
    assert!(!fenced, "a superseded driver committed a turn");
    assert!(source.agent().prompt("still fenced").await.is_err());
    let loaded = store.load_branch(before.session_ref()).await?;
    assert_eq!(
        loaded.head().checkpoint().checkpoint_id(),
        before.checkpoint_id()
    );

    // The new head keeps working.
    let outcome = carried
        .agent()
        .prompt("on the head")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let _ = source.release().await;
    let carried_head = carried.release().await?;

    // A predecessor stays whole for gc and opens for no work; a fork of it
    // is a new line.
    assert_eq!(
        sessions.prune_history(before).await?,
        PruneHistoryOutcome::Superseded
    );
    let ForkSessionOutcome::Forked { target: fork, .. } = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:predecessor")?,
            before,
        ))
        .await?
    else {
        return Err("fork of a predecessor".into());
    };
    let fork_line = plakous_model::LineId::started_by(fork.branch_id());
    let fork_head = sessions.line_head(fork_line).await?;
    assert_eq!(fork_head.generation(), 1);
    assert_ne!(fork_line, line);
    assert!(matches!(
        sessions.prune_history(carried_head).await?,
        PruneHistoryOutcome::Pruned(_)
    ));
    Ok(())
}
