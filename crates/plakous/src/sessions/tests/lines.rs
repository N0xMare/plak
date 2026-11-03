//! The head fence for an attached predecessor, fork lines and the facade's
//! line reads (spec 01 §5.1, §5.6, §5.10).

use super::*;

/// A carry is fenced like a rebind: it never moves the head from under an
/// attached driver. Once the source is released it does, the source becomes
/// read-only history, and deleting the new head's session ends the line.
#[tokio::test]
async fn a_carry_waits_for_the_attached_source_and_deletion_ends_the_line() -> TestResult {
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
    let carry = || -> TestResult<CreateSessionRequest> {
        Ok(
            CreateSessionRequest::new(SessionOperationId::new("create:carried")?, binding()?)
                .continuing_line(source.locator()),
        )
    };
    let busy = sessions
        .create(carry()?)
        .await
        .err()
        .ok_or("a carry moved the head from under an attached driver")?;
    assert_eq!(busy.kind(), AgentSessionsErrorKind::Ownership);
    assert_eq!(sessions.line_head(line).await?.generation(), 1);
    let before = source.locator();
    let request = carry()?;
    source.release().await?;

    let CreateSessionOutcome::Created {
        session: carried, ..
    } = sessions.create(request).await?;
    assert_eq!(carried.line(), line);
    let head = sessions.line_head(line).await?;
    assert_eq!(head.session_id(), carried.locator().session_id());
    assert_eq!(head.generation(), 2);
    let loaded = store.load_branch(before.session_ref()).await?;
    assert_eq!(
        loaded.head().checkpoint().checkpoint_id(),
        before.checkpoint_id()
    );

    // The predecessor opens for no work, verified or not.
    let refused = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:predecessor")?,
            before,
            binding()?,
        ))
        .await
        .err()
        .ok_or("a predecessor opened")?;
    assert_eq!(refused.kind(), AgentSessionsErrorKind::NotLineHead);
    let unverified = sessions
        .verify_open(before, binding()?)
        .await
        .err()
        .ok_or("a predecessor verified")?;
    assert_eq!(unverified.kind(), AgentSessionsErrorKind::NotLineHead);

    // The new head keeps working.
    let outcome = carried
        .agent()
        .prompt("on the head")
        .await?
        .result()
        .await?;
    assert!(matches!(outcome.outcome(), TurnResultOutcome::Completed(_)));
    let carried_head = carried.release().await?;

    Box::pin(predecessor_history_and_the_end_of_its_line(
        &sessions,
        line,
        before,
        carried_head,
        binding()?,
    ))
    .await
}

/// A predecessor stays whole for gc and forks onto a new line; deleting the
/// head's session ends the line, and the predecessor says so.
async fn predecessor_history_and_the_end_of_its_line(
    sessions: &AgentSessions,
    line: plakous_model::LineId,
    before: SessionLocator,
    carried_head: SessionLocator,
    binding: SessionRuntimeBinding,
) -> TestResult {
    // A predecessor stays whole for gc; a fork of it is a new line.
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

    // Deleting the head's session ends the line: its predecessor stays
    // read-only history and says so.
    sessions.archive(carried_head).await?;
    sessions
        .delete_archived(carried_head.session_id())
        .await?
        .ok_or("the archived head was not deleted")?;
    assert_eq!(
        sessions
            .line_head(line)
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::NotFound)
    );
    let ended = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:ended")?,
            before,
            binding,
        ))
        .await
        .err()
        .ok_or("a predecessor of an ended line opened")?;
    assert_eq!(ended.kind(), AgentSessionsErrorKind::LineEnded);
    assert_eq!(
        sessions.prune_history(before).await?,
        PruneHistoryOutcome::LineEnded
    );
    Ok(())
}
