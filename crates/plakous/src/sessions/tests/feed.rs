//! The change feed through the facade (spec 01 §5.11): pages and cursors,
//! the tail subscription, the bounded content reads at a record's checkpoint,
//! and the feed sequences that acknowledgements, results, events and
//! workflow outcomes name.

mod counts;

use super::portable_history::{account_reference, carried_segments};
use super::*;
use crate::sessions::SessionTranscriptPart;
use crate::sessions::feed::{
    FeedCall, FeedCallOutcome, FeedCallRequest, FeedCallTarget, FeedChange, FeedCursor, FeedFact,
    FeedItemsOutcome, FeedItemsRequest, FeedPage, FeedPageRequest, FeedRecord, FeedSeq,
    FeedTerminalOutcome, FeedTerminalRequest,
};
use plakous_model::ContentBlock;
use std::num::NonZeroUsize;
use std::time::Duration;

async fn read_all(sessions: &AgentSessions, session: SessionId) -> TestResult<Vec<FeedRecord>> {
    let mut cursor = FeedCursor::start(session);
    let mut all = Vec::new();
    loop {
        let FeedPage::Records { records, .. } =
            sessions.feed_page(FeedPageRequest::after(cursor)).await?
        else {
            return Err("a fresh feed is never behind retention".into());
        };
        if records.is_empty() {
            return Ok(all);
        }
        cursor = records.iter().fold(cursor, FeedCursor::advanced_past);
        all.extend(records);
    }
}

fn record_at(records: &[FeedRecord], seq: FeedSeq) -> TestResult<&FeedRecord> {
    records
        .iter()
        .find(|record| record.header().seq == seq)
        .ok_or_else(|| format!("no feed record {}", seq.get()).into())
}

fn checkpoint_of(record: &FeedRecord) -> TestResult<CheckpointId> {
    match record.header().change {
        FeedChange::Created { genesis, .. } => Ok(genesis),
        FeedChange::Advanced { checkpoint, .. } => Ok(checkpoint),
        FeedChange::Branched { target, .. } => Ok(target),
        _ => Err("the record names no published checkpoint".into()),
    }
}

fn accepted(record: &FeedRecord, command: &CommandId) -> bool {
    record.facts().iter().any(|fact| {
        matches!(fact, FeedFact::CommandAccepted { command_id, .. } if command_id == command)
    })
}

async fn completed_session(
    sessions: &AgentSessions,
    workspace: &Arc<WorkspacePolicy>,
    name: &str,
) -> TestResult<SessionLocator> {
    let binding =
        completing_runtime_binding_for(Arc::clone(workspace), descriptor()?, b"account-alpha")?;
    completed_with(sessions, binding, name).await
}

/// A released session that ran one completed turn under `binding`.
async fn completed_with(
    sessions: &AgentSessions,
    binding: SessionRuntimeBinding,
    name: &str,
) -> TestResult<SessionLocator> {
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(format!("create:{name}"))?,
            binding,
        ))
        .await?;
    let result = session.agent().prompt("one turn").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    Ok(session.release().await?)
}

#[tokio::test]
async fn feed_pages_follow_the_cursor_to_the_tail() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let released = completed_session(&sessions, &workspace, "feed-pages").await?;
    let session = released.session_id();

    let records = read_all(&sessions, session).await?;
    let sequences: Vec<u64> = records
        .iter()
        .map(|record| record.header().seq.get())
        .collect();
    let expected: Vec<u64> = (1..=u64::try_from(sequences.len())?).collect();
    assert_eq!(sequences, expected, "one gap-free sequence from 1");
    assert!(matches!(
        records[0].header().change,
        FeedChange::Created { .. }
    ));
    assert!(records.iter().any(|record| matches!(
        record.header().change,
        FeedChange::LeaseReleased { head, .. } if head == released.checkpoint_id()
    )));
    let tail = FeedSeq::new(*sequences.last().ok_or("records")?);
    assert!(tail.get() >= 3, "create, a turn and a release");

    let two = FeedPageRequest {
        cursor: FeedCursor::start(session),
        max_records: NonZeroUsize::new(2).ok_or("two")?,
    };
    let FeedPage::Records {
        records: first,
        retained_from,
        tail: observed,
    } = sessions.feed_page(two).await?
    else {
        return Err("records".into());
    };
    assert_eq!(first.len(), 2);
    assert_eq!(retained_from, FeedSeq::new(1));
    assert_eq!(observed, tail);
    let cursor = first
        .iter()
        .fold(FeedCursor::start(session), FeedCursor::advanced_past);
    assert_eq!(cursor.applied(), FeedSeq::new(2));
    let FeedPage::Records { records: rest, .. } =
        sessions.feed_page(FeedPageRequest::after(cursor)).await?
    else {
        return Err("records".into());
    };
    assert_eq!(
        rest.first().map(|record| record.header().seq),
        Some(FeedSeq::new(3))
    );

    let FeedPage::Records {
        records: empty,
        tail: at_tail,
        ..
    } = sessions
        .feed_page(FeedPageRequest::after(FeedCursor::new(session, tail)))
        .await?
    else {
        return Err("records".into());
    };
    assert!(empty.is_empty(), "a cursor at the tail reads an empty page");
    assert_eq!(at_tail, tail);

    let oversized = FeedPageRequest {
        cursor: FeedCursor::start(session),
        max_records: NonZeroUsize::new(257).ok_or("257")?,
    };
    assert_eq!(
        sessions
            .feed_page(oversized)
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    let unknown = SessionId::try_from(Uuid::from_u128(0xfeed))?;
    assert_eq!(
        sessions
            .feed_page(FeedPageRequest::after(FeedCursor::start(unknown)))
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::NotFound)
    );
    Ok(())
}

#[tokio::test]
async fn subscribe_feed_wakes_after_each_acknowledged_mutation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:feed-subscribe")?,
            completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
            )?,
        ))
        .await?;
    let id = session.locator().session_id();
    let mut subscription = sessions.subscribe_feed(id).await?;
    let created = subscription.seen();
    assert!(created >= FeedSeq::new(1));

    let result = session.agent().prompt("wake").await?.result().await?;
    let changed = tokio::time::timeout(Duration::from_secs(10), subscription.changed()).await??;
    let terminal = result
        .feed_seq()
        .ok_or("a committed terminal names its record")?;
    assert!(changed > created);
    assert!(changed <= terminal);

    let released = session.release().await?;
    let changed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let tail = subscription.changed().await?;
            let records = read_all(&sessions, id).await?;
            if records.iter().any(|record| {
                matches!(
                    record.header().change,
                    FeedChange::LeaseReleased { head, .. } if head == released.checkpoint_id()
                ) && record.header().seq == tail
            }) {
                return TestResult::Ok(tail);
            }
        }
    })
    .await??;
    assert_eq!(subscription.seen(), changed);

    let unknown = SessionId::try_from(Uuid::from_u128(0xfeed))?;
    assert_eq!(
        sessions
            .subscribe_feed(unknown)
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::NotFound)
    );
    Ok(())
}

#[tokio::test]
async fn acks_results_and_events_name_the_records_that_made_them_durable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:feed-seq")?,
            blocked_runtime_binding(
                workspace,
                descriptor()?,
                b"account-alpha",
                LocalDurabilityDisposition::Ephemeral,
            )?,
        ))
        .await?;
    let locator = session.locator();
    assert_eq!(sessions.head_running_turn(locator).await?, None);
    let root = CommandId::new("root:feed-seq")?;
    let submission = session.agent().start(root.clone(), "hold active").await?;
    let root_seq = submission
        .acknowledgement()
        .feed_seq()
        .ok_or("an accepted root names its admission record")?;
    let mut turn = submission.into_turn().ok_or("root turn")?;
    let turn_id = turn.id();
    assert_eq!(
        sessions.head_running_turn(session.locator()).await?,
        Some(turn_id)
    );
    let mut events = turn.take_events()?;
    loop {
        if matches!(
            events.recv().await?.kind(),
            plakous_agent::AgentEventKind::ModelDispatchStarted
        ) {
            break;
        }
    }
    let steer = CommandId::new("steer:feed-seq")?;
    let steer_ack = turn
        .control()
        .steer_with_id(steer.clone(), "recorded steering")
        .await?;
    let steer_seq = steer_ack
        .feed_seq()
        .ok_or("an accepted steer names its record")?;
    turn.control().cancel().await?;
    let terminal_event = loop {
        let event = events.recv().await?;
        if matches!(
            event.kind(),
            plakous_agent::AgentEventKind::TurnCompleted { .. }
                | plakous_agent::AgentEventKind::TurnFailed { .. }
        ) {
            break event;
        }
    };
    let result = turn.result().await?;
    let terminal_seq = result
        .feed_seq()
        .ok_or("a committed terminal names its record")?;
    assert_eq!(terminal_event.feed_seq(), Some(terminal_seq));
    let duplicate = session.agent().start(root.clone(), "hold active").await?;
    assert_eq!(duplicate.acknowledgement().feed_seq(), Some(root_seq));

    let records = read_all(&sessions, locator.session_id()).await?;
    assert!(accepted(record_at(&records, root_seq)?, &root));
    assert!(accepted(record_at(&records, steer_seq)?, &steer));
    let terminal = record_at(&records, terminal_seq)?;
    assert!(terminal.facts().iter().any(|fact| matches!(
        fact,
        FeedFact::TurnTerminal { turn_id: terminal_turn, command_id, .. }
            if *terminal_turn == turn_id && *command_id == root
    )));

    let read = sessions
        .feed_terminal(FeedTerminalRequest {
            session: locator.session_id(),
            branch: locator.branch_id(),
            checkpoint: checkpoint_of(terminal)?,
            turn_id,
        })
        .await?;
    let FeedTerminalOutcome::Terminal(read) = read else {
        return Err("the terminal's own checkpoint holds its record".into());
    };
    assert_eq!(read.checkpoint(), checkpoint_of(terminal)?);
    assert_eq!(read.result().turn_id(), turn_id);
    assert!(matches!(
        read.result().outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    let genesis = checkpoint_of(&records[0])?;
    assert_eq!(
        sessions
            .feed_terminal(FeedTerminalRequest {
                session: locator.session_id(),
                branch: locator.branch_id(),
                checkpoint: genesis,
                turn_id,
            })
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput),
        "an exact checkpoint without the terminal is not a guess"
    );
    session.release().await?;
    Ok(())
}

#[tokio::test]
async fn fork_outcome_names_its_branched_record() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let released = completed_session(&sessions, &workspace, "feed-fork").await?;
    let outcome = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:feed-fork")?,
            released,
        ))
        .await?;
    let ForkSessionOutcome::Forked { target, .. } = &outcome else {
        return Err("fork".into());
    };
    let seq = outcome.feed_seq().ok_or("a fork names its record")?;
    let records = read_all(&sessions, released.session_id()).await?;
    let record = record_at(&records, seq)?;
    assert_eq!(record.header().branch_id, target.branch_id());
    assert!(matches!(
        record.header().change,
        FeedChange::Branched { target: published, .. } if published == target.checkpoint_id()
    ));
    Ok(())
}

#[tokio::test]
async fn recompiling_fork_publication_is_read_from_the_feed_without_a_runtime() -> TestResult {
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
    let instructed = |instructions: &str| {
        instructed_runtime_binding_for(
            Arc::clone(&workspace),
            descriptor()?,
            b"account-alpha",
            instructions,
        )
    };
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:fork-feed-source")?,
            instructed("original instructions")?,
        ))
        .await?;
    let result = session.agent().prompt("work").await?.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let source = session.release().await?;
    let before = store.stats()?;
    assert!(
        read_all(&sessions, source.session_id())
            .await?
            .iter()
            .all(|record| !matches!(record.header().change, FeedChange::Branched { .. }))
    );
    assert_eq!(store.stats()?, before);

    for (operation, instructions, transition) in [
        (
            "fork:feed-unchanged",
            "original instructions",
            crate::sessions::feed::FeedBranchTransition::Fork,
        ),
        (
            "fork:feed-edited",
            "edited instructions",
            crate::sessions::feed::FeedBranchTransition::Rebind,
        ),
    ] {
        let outcome = sessions
            .fork(ForkSessionRequest::recompiling(
                SessionOperationId::new(operation)?,
                source,
                instructed(instructions)?,
            ))
            .await?;
        let ForkSessionOutcome::Forked { target, .. } = &outcome else {
            return Err("the fork did not publish".into());
        };
        let before = store.stats()?;
        let records = read_all(&sessions, source.session_id()).await?;
        let record = record_at(&records, outcome.feed_seq().ok_or("fork feed sequence")?)?;
        assert!(matches!(
            record.header().change,
            FeedChange::Branched { target: published, transition: kind, .. }
                if published == target.checkpoint_id() && kind == transition
        ));
        assert_eq!(record.header().branch_id, target.branch_id());
        assert_eq!(store.stats()?, before, "feed inspection writes nothing");
    }
    Ok(())
}

#[tokio::test]
async fn feed_items_fall_back_by_entry_identity_and_never_guess() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let released = completed_session(&sessions, &workspace, "feed-items").await?;
    let records = read_all(&sessions, released.session_id()).await?;
    let (record, first_ordinal, count, first_entry, last_entry) = records
        .iter()
        .find_map(|record| {
            record.facts().iter().find_map(|fact| match fact {
                FeedFact::HistoryAppended {
                    first_ordinal,
                    count,
                    first_entry,
                    last_entry,
                    ..
                } => Some((record, *first_ordinal, *count, *first_entry, *last_entry)),
                _ => None,
            })
        })
        .ok_or("the turn appended history")?;
    let request = FeedItemsRequest {
        session: released.session_id(),
        branch: released.branch_id(),
        checkpoint: checkpoint_of(record)?,
        first_ordinal,
        count,
        first_entry: Some(first_entry),
        last_entry: Some(last_entry),
    };
    let FeedItemsOutcome::Items { checkpoint, items } = sessions.feed_items(request).await? else {
        return Err("the record's own checkpoint holds its items".into());
    };
    assert_eq!(checkpoint, request.checkpoint);
    assert_eq!(u64::try_from(items.len())?, count);
    assert!(
        items[0]
            .parts()
            .iter()
            .any(|part| matches!(part, SessionTranscriptPart::Text(text) if text == "one turn"))
    );
    // A chunk of a split range names neither entry and is checked by its
    // ordinals against the record's own checkpoint.
    let anonymous = FeedItemsRequest {
        first_entry: None,
        last_entry: None,
        ..request
    };
    let FeedItemsOutcome::Items {
        items: anonymous_items,
        ..
    } = sessions.feed_items(anonymous).await?
    else {
        return Err("the record's own checkpoint holds an anonymous chunk".into());
    };
    assert_eq!(anonymous_items, items);
    let shifted = FeedItemsRequest {
        first_ordinal: first_ordinal + 1,
        ..request
    };
    assert_eq!(
        sessions
            .feed_items(shifted)
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );

    let PruneHistoryOutcome::Pruned(_) = sessions.prune_history(released).await? else {
        return Err("a quiescent head prunes".into());
    };
    let FeedItemsOutcome::Items {
        checkpoint: fallback,
        items: retained,
    } = sessions.feed_items(request).await?
    else {
        return Err("the head still holds the appended items".into());
    };
    assert_eq!(fallback, released.checkpoint_id());
    assert_eq!(retained, items);
    // The fallback head locates a range only by a named first entry.
    assert_eq!(
        sessions.feed_items(anonymous).await?,
        FeedItemsOutcome::NotRetained
    );
    let removed = FeedItemsRequest {
        first_entry: Some(EntryId::try_from(Uuid::from_u128(0xdead))?),
        ..request
    };
    assert_eq!(
        sessions.feed_items(removed).await?,
        FeedItemsOutcome::NotRetained
    );
    Ok(())
}

#[tokio::test]
async fn feed_call_returns_a_carried_call_and_its_result_whole() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = AgentSessions::new(SessionBackend::memory()?)?;
    let account = b"carry-account";
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("create:feed-call")?,
                completing_runtime_binding_for(workspace, descriptor()?, account)?,
            )
            .with_history(PortableSessionHistory::new(
                carried_segments()?,
                account_reference(account)?,
                ModelId::new("test.model")?,
            )),
        )
        .await?;
    let genesis = session.locator();
    let call_id = CallId::try_from(Uuid::from_u128(0xca11))?;
    let request = FeedCallRequest {
        session: genesis.session_id(),
        branch: genesis.branch_id(),
        checkpoint: genesis.checkpoint_id(),
        call: FeedCallTarget::Call(call_id),
    };
    let FeedCallOutcome::Call { checkpoint, call } = sessions.feed_call(request).await? else {
        return Err("the genesis holds the carried call".into());
    };
    assert_eq!(checkpoint, genesis.checkpoint_id());
    let FeedCall::Tool { call, result } = call else {
        return Err("a history call".into());
    };
    assert_eq!(call.call_id(), call_id);
    let expected = carried_segments()?
        .iter()
        .flat_map(|segment| segment.items().to_vec())
        .flat_map(|item| item.blocks().to_vec())
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) if result.call_id() == call_id => Some(result),
            _ => None,
        })
        .ok_or("carried result")?;
    assert_eq!(result.as_deref(), Some(&expected), "the result is whole");

    let unknown = FeedCallRequest {
        call: FeedCallTarget::Call(CallId::try_from(Uuid::from_u128(0xdead))?),
        ..request
    };
    assert_eq!(
        sessions
            .feed_call(unknown)
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    assert_eq!(
        sessions
            .feed_call(FeedCallRequest {
                call: FeedCallTarget::Operation(u64::MAX),
                ..request
            })
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    session.release().await?;
    Ok(())
}

async fn sqlite_completed_session(
    sessions: &AgentSessions,
    workspace: &Arc<WorkspacePolicy>,
    name: &str,
) -> TestResult<SessionLocator> {
    let binding = durable_completing_runtime_binding_for(
        Arc::clone(workspace),
        descriptor()?,
        b"account-alpha",
    )?;
    completed_with(sessions, binding, name).await
}

#[tokio::test]
async fn pruned_terminal_lookup_does_not_compare_an_older_turn_with_later_history() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("terminal-lookup.sqlite3");
    let store = Arc::new(SqliteSessionStore::open_with_clock(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?);
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store.clone(),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:older-terminal")?,
            durable_completing_runtime_binding_for(workspace, descriptor()?, b"account-alpha")?,
        ))
        .await?;
    let first = session.agent().prompt("first turn").await?.result().await?;
    let records = read_all(&sessions, session.locator().session_id()).await?;
    let admitted = records
        .iter()
        .find(|record| {
            record.facts().iter().any(|fact| matches!(
        fact, FeedFact::CommandAccepted { turn_id: Some(turn), .. } if *turn == first.turn_id()
    ))
        })
        .ok_or("first turn admission")?;
    let request = FeedTerminalRequest {
        session: session.locator().session_id(),
        branch: session.locator().branch_id(),
        checkpoint: checkpoint_of(admitted)?,
        turn_id: first.turn_id(),
    };
    session
        .agent()
        .prompt("second turn changes history")
        .await?
        .result()
        .await?;
    let head = session.release().await?;
    sessions.prune_history(head).await?;
    assert!(matches!(
        store.load_checkpoint(request.checkpoint).await,
        Err(SessionStoreError::Pruned)
    ));
    // The terminal's own retained checkpoint still authenticates the first history.
    let exact = sessions
        .feed_terminal(FeedTerminalRequest {
            checkpoint: first.checkpoint_id(),
            ..request
        })
        .await?;
    assert!(matches!(exact, FeedTerminalOutcome::Terminal(_)));
    let mut outcomes = sessions.feed_terminals(vec![request, request]).await;
    outcomes.push(sessions.feed_terminal(request).await);
    outcomes.push(AgentSessions::inspect_feed_terminal_sqlite(
        &database, &request,
    ));
    for outcome in outcomes {
        let FeedTerminalOutcome::Terminal(read) = outcome? else {
            return Err("the first terminal is still retained".into());
        };
        assert_eq!(read.result().turn_id(), first.turn_id());
        assert!(matches!(
            read.result().outcome(),
            TurnResultOutcome::Completed(_)
        ));
        assert_eq!(read.result().conversation(), first.conversation());
    }
    Ok(())
}

fn sqlite_sessions_with_feed_limits(
    path: &Path,
    feed: plakous_store::MemoryFeedLimits,
) -> TestResult<AgentSessions> {
    let store = SqliteSessionStore::open_with_clock_and_options(
        path,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        plakous_store::SqliteStoreOptions::new(
            MemoryStoreLimits::default().with_feed_limits(feed),
            NonZeroUsize::new(8).ok_or("queue")?,
        ),
    )?;
    Ok(AgentSessions::new(SessionBackend::from_store(
        Arc::new(store),
        RuntimeDurability::Required,
    ))?)
}

fn inspected_records(path: &Path, request: FeedPageRequest) -> TestResult<Vec<FeedRecord>> {
    match AgentSessions::inspect_feed_sqlite(path, request)? {
        FeedPage::Records { records, .. } => Ok(records),
        FeedPage::BehindRetention { .. } => Err("the inspected cursor fell behind".into()),
    }
}

/// The read-only twin pages the same records over one snapshot, follows the
/// cursor, and takes no lease: a session holding its lease reads too and
/// keeps running afterwards.
#[tokio::test]
async fn inspect_feed_sqlite_pages_a_snapshot_without_a_lease() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("feed.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = sqlite_test_sessions(&database)?;
    let released = sqlite_completed_session(&sessions, &workspace, "inspect-pages").await?;
    let session = released.session_id();
    let records = read_all(&sessions, session).await?;
    let tail = FeedSeq::new(u64::try_from(records.len())?);
    assert!(tail.get() >= 3, "create, a turn and a release");

    let CreateSessionOutcome::Created { session: held, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:inspect-held")?,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"account-alpha",
            )?,
        ))
        .await?;
    let held_id = held.locator().session_id();

    let two = FeedPageRequest {
        cursor: FeedCursor::start(session),
        max_records: NonZeroUsize::new(2).ok_or("two")?,
    };
    let FeedPage::Records {
        records: first,
        retained_from,
        tail: observed,
    } = AgentSessions::inspect_feed_sqlite(&database, two)?
    else {
        return Err("records".into());
    };
    assert_eq!(first, records[..2]);
    assert_eq!(retained_from, FeedSeq::new(1));
    assert_eq!(observed, tail);
    let cursor = first
        .iter()
        .fold(FeedCursor::start(session), FeedCursor::advanced_past);
    assert_eq!(cursor.applied(), FeedSeq::new(2));
    assert_eq!(
        inspected_records(&database, FeedPageRequest::after(cursor))?,
        records[2..]
    );
    assert!(
        inspected_records(
            &database,
            FeedPageRequest::after(FeedCursor::new(session, tail))
        )?
        .is_empty(),
        "a cursor at the tail reads an empty page"
    );

    let held_records = inspected_records(
        &database,
        FeedPageRequest::after(FeedCursor::start(held_id)),
    )?;
    assert!(matches!(
        held_records.first().map(|record| &record.header().change),
        Some(FeedChange::Created { .. })
    ));

    let oversized = FeedPageRequest {
        cursor: FeedCursor::start(session),
        max_records: NonZeroUsize::new(257).ok_or("257")?,
    };
    assert_eq!(
        AgentSessions::inspect_feed_sqlite(&database, oversized)
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    let unknown = SessionId::try_from(Uuid::from_u128(0xfeed))?;
    assert_eq!(
        AgentSessions::inspect_feed_sqlite(
            &database,
            FeedPageRequest::after(FeedCursor::start(unknown))
        )
        .err()
        .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::NotFound)
    );
    assert_eq!(
        AgentSessions::inspect_feed_sqlite(
            directory.path().join("missing.sqlite3"),
            FeedPageRequest::after(FeedCursor::start(session))
        )
        .err()
        .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::NotFound)
    );

    let result = held
        .agent()
        .prompt("after inspection")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    held.release().await?;
    assert_eq!(read_all(&sessions, session).await?, records);
    Ok(())
}

/// Below the retained window the twin answers as the runtime read does: the
/// window and the session's heads, never records.
#[tokio::test]
async fn inspect_feed_sqlite_answers_behind_retention_below_the_window() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("feed.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = sqlite_sessions_with_feed_limits(
        &database,
        plakous_store::MemoryFeedLimits::new(2, u64::MAX, NonZeroUsize::MIN),
    )?;
    let released = sqlite_completed_session(&sessions, &workspace, "inspect-behind").await?;
    let session = released.session_id();

    let start = FeedPageRequest::after(FeedCursor::start(session));
    let inspected = AgentSessions::inspect_feed_sqlite(&database, start)?;
    assert_eq!(inspected, sessions.feed_page(start).await?);
    let FeedPage::BehindRetention {
        retained_from,
        tail,
        heads,
    } = inspected
    else {
        return Err("a trimmed cursor read records".into());
    };
    assert!(retained_from.get() > 1);
    assert_eq!(heads, [released]);

    let resumed = FeedCursor::new(session, FeedSeq::new(retained_from.get() - 1));
    let retained = inspected_records(&database, FeedPageRequest::after(resumed))?;
    assert_eq!(
        retained
            .iter()
            .map(|record| record.header().seq)
            .collect::<Vec<_>>(),
        (retained_from.get()..=tail.get())
            .map(FeedSeq::new)
            .collect::<Vec<_>>()
    );
    Ok(())
}

/// The content twins read the same items, call and terminal as the runtime
/// reads, refuse the same way, and fall back to the head after a prune.
#[tokio::test]
async fn inspect_feed_content_twins_read_sqlite_snapshots() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("feed.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let sessions = sqlite_test_sessions(&database)?;
    let released = sqlite_completed_session(&sessions, &workspace, "inspect-content").await?;
    let records = read_all(&sessions, released.session_id()).await?;

    let (record, first_ordinal, count, first_entry, last_entry) = records
        .iter()
        .find_map(|record| {
            record.facts().iter().find_map(|fact| match fact {
                FeedFact::HistoryAppended {
                    first_ordinal,
                    count,
                    first_entry,
                    last_entry,
                    ..
                } => Some((record, *first_ordinal, *count, *first_entry, *last_entry)),
                _ => None,
            })
        })
        .ok_or("the turn appended history")?;
    let items = FeedItemsRequest {
        session: released.session_id(),
        branch: released.branch_id(),
        checkpoint: checkpoint_of(record)?,
        first_ordinal,
        count,
        first_entry: Some(first_entry),
        last_entry: Some(last_entry),
    };
    let inspected = AgentSessions::inspect_feed_items_sqlite(&database, &items)?;
    assert_eq!(inspected, sessions.feed_items(items).await?);
    let FeedItemsOutcome::Items { checkpoint, .. } = &inspected else {
        return Err("the record's own checkpoint holds its items".into());
    };
    assert_eq!(*checkpoint, items.checkpoint);
    let shifted = FeedItemsRequest {
        first_ordinal: first_ordinal + 1,
        ..items
    };
    assert_eq!(
        AgentSessions::inspect_feed_items_sqlite(&database, &shifted)
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    let unknown = FeedItemsRequest {
        session: SessionId::try_from(Uuid::from_u128(0xfeed))?,
        ..items
    };
    // The named checkpoint is read directly, as the live read does, so a
    // checkpoint named under another session is refused the same way by both.
    assert_eq!(
        AgentSessions::inspect_feed_items_sqlite(&database, &unknown)
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    assert_eq!(
        sessions
            .feed_items(unknown)
            .await
            .err()
            .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );

    let (terminal_record, turn_id) = records
        .iter()
        .find_map(|record| {
            record.facts().iter().find_map(|fact| match fact {
                FeedFact::TurnTerminal { turn_id, .. } => Some((record, *turn_id)),
                _ => None,
            })
        })
        .ok_or("the turn committed its terminal")?;
    let terminal = FeedTerminalRequest {
        session: released.session_id(),
        branch: released.branch_id(),
        checkpoint: checkpoint_of(terminal_record)?,
        turn_id,
    };
    let FeedTerminalOutcome::Terminal(read) =
        AgentSessions::inspect_feed_terminal_sqlite(&database, &terminal)?
    else {
        return Err("the terminal's own checkpoint holds its record".into());
    };
    assert_eq!(read.checkpoint(), terminal.checkpoint);
    assert_eq!(read.result().turn_id(), turn_id);
    assert!(matches!(
        read.result().outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert!(read.abandoned_turn().is_none());
    assert_eq!(
        AgentSessions::inspect_feed_terminal_sqlite(
            &database,
            &FeedTerminalRequest {
                checkpoint: checkpoint_of(&records[0])?,
                ..terminal
            }
        )
        .err()
        .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput),
        "an exact checkpoint without the terminal is not a guess"
    );

    let PruneHistoryOutcome::Pruned(_) = sessions.prune_history(released).await? else {
        return Err("a quiescent head prunes".into());
    };
    let fallback = AgentSessions::inspect_feed_items_sqlite(&database, &items)?;
    assert_eq!(fallback, sessions.feed_items(items).await?);
    let FeedItemsOutcome::Items { checkpoint, .. } = fallback else {
        return Err("the head still holds the appended items".into());
    };
    assert_eq!(checkpoint, released.checkpoint_id());
    let removed = FeedItemsRequest {
        first_entry: Some(EntryId::try_from(Uuid::from_u128(0xdead))?),
        ..items
    };
    assert_eq!(
        AgentSessions::inspect_feed_items_sqlite(&database, &removed)?,
        FeedItemsOutcome::NotRetained
    );

    let account = b"carry-account";
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("create:inspect-call")?,
                durable_completing_runtime_binding_for(
                    Arc::clone(&workspace),
                    descriptor()?,
                    account,
                )?,
            )
            .with_history(PortableSessionHistory::new(
                carried_segments()?,
                account_reference(account)?,
                ModelId::new("test.model")?,
            )),
        )
        .await?;
    let genesis = session.locator();
    let call = FeedCallRequest {
        session: genesis.session_id(),
        branch: genesis.branch_id(),
        checkpoint: genesis.checkpoint_id(),
        call: FeedCallTarget::Call(CallId::try_from(Uuid::from_u128(0xca11))?),
    };
    let inspected = AgentSessions::inspect_feed_call_sqlite(&database, &call)?;
    assert_eq!(inspected, sessions.feed_call(call).await?);
    assert!(matches!(
        inspected,
        FeedCallOutcome::Call {
            checkpoint,
            call: FeedCall::Tool { result: Some(_), .. },
        } if checkpoint == genesis.checkpoint_id()
    ));
    assert_eq!(
        AgentSessions::inspect_feed_call_sqlite(
            &database,
            &FeedCallRequest {
                call: FeedCallTarget::Call(CallId::try_from(Uuid::from_u128(0xdead))?),
                ..call
            }
        )
        .err()
        .map(|error| error.kind()),
        Some(AgentSessionsErrorKind::InvalidInput)
    );
    session.release().await?;
    Ok(())
}

/// The live runtime's published checkpoints serve the content reads exactly
/// as the store does (spec 01 §5.11): a projector fed from either path
/// derives the same views.
#[tokio::test]
async fn published_checkpoint_reads_equal_the_store_reads() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("feed.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let live = sqlite_test_sessions(&database)?;
    let binding = durable_completing_runtime_binding_for(
        Arc::clone(&workspace),
        descriptor()?,
        b"account-alpha",
    )?;
    let CreateSessionOutcome::Created { session: held, .. } = live
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:published-parity")?,
            binding,
        ))
        .await?;
    held.agent().prompt("one turn").await?.result().await?;
    let stored = sqlite_test_sessions(&database)?;
    let session = held.locator().session_id();
    let mut served = 0;
    let mut compared = 0;
    for record in read_all(&live, session).await? {
        let Ok(checkpoint) = checkpoint_of(&record) else {
            continue;
        };
        if live
            .feed
            .published_checkpoint(session, checkpoint)
            .is_some()
        {
            served += 1;
        }
        assert!(
            stored
                .feed
                .published_checkpoint(session, checkpoint)
                .is_none()
        );
        let branch = record.header().branch_id;
        for fact in record.facts() {
            match fact {
                FeedFact::HistoryAppended {
                    first_ordinal,
                    count,
                    first_entry,
                    last_entry,
                    ..
                } => {
                    let request = FeedItemsRequest {
                        session,
                        branch,
                        checkpoint,
                        first_ordinal: *first_ordinal,
                        count: *count,
                        first_entry: Some(*first_entry),
                        last_entry: Some(*last_entry),
                    };
                    assert_eq!(
                        live.feed_items(request).await?,
                        stored.feed_items(request).await?
                    );
                    compared += 1;
                }
                FeedFact::TurnTerminal { turn_id, .. } => {
                    let request = FeedTerminalRequest {
                        session,
                        branch,
                        checkpoint,
                        turn_id: *turn_id,
                    };
                    assert_eq!(
                        format!("{:?}", live.feed_terminal(request).await?),
                        format!("{:?}", stored.feed_terminal(request).await?)
                    );
                    compared += 1;
                }
                _ => {}
            }
        }
    }
    assert!(served > 0, "the live runtime published its commits");
    assert!(compared >= 2, "the turn appended history and ended");
    held.release().await?;
    for record in read_all(&live, session).await? {
        if let Ok(checkpoint) = checkpoint_of(&record) {
            assert!(
                live.feed
                    .published_checkpoint(session, checkpoint)
                    .is_none(),
                "released sessions must read the store, not stale cached objects"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn abandonment_terminal_report_survives_parent_pruning_and_sqlite_inspection() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("working-inspect.sqlite3");
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = SqliteSessionStore::open_with_clock(
        &database,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("inspect:working:create")?,
            durable_completing_runtime_binding_for(
                Arc::clone(&workspace),
                descriptor()?,
                b"working-inspect-account",
            )?,
        ))
        .await?;
    let source = session.release().await?;
    let genesis_loaded = store.load_branch(source.session_ref()).await?;
    let genesis = Arc::clone(genesis_loaded.checkpoint().checkpoint());
    let source_topology = genesis.manifest().topology();
    let lease = store
        .acquire(AcquireBranch::new(
            source.session_ref(),
            RunId::try_from(Uuid::from_u128(0x5111))?,
            NonZeroU64::new(30).ok_or("lease duration must be nonzero")?,
        ))
        .await?;
    let turn_id = TurnId::try_from(Uuid::from_u128(0x5112))?;
    let checkpoint_id = CheckpointId::try_from(Uuid::from_u128(0x5113))?;
    let root = RuntimeWorkingRoot::Turn {
        turn_id,
        next_round: 1,
    };
    let canonical_input = b"diagnostic-working-root";
    let command_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "canonical_input": canonical_input,
        "effect": "Root",
        "rejection": null,
    }))?;
    let command = DurableCommandRecord::new(
        "diagnostic-working-root",
        1,
        Some(turn_id),
        Digest::sha256(DigestDomain::Blob, 1, &[canonical_input])?,
        ReconstructiveComponent::new(RuntimeComponentKind::Command, 1, command_bytes.as_slice())?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let command_operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, command_bytes.as_slice())?,
        vec![
            DurableOperationTransition::new(
                source.checkpoint_id(),
                DurableOperationState::Prepared,
            ),
            DurableOperationTransition::new(checkpoint_id, DurableOperationState::Dispatched),
        ],
    )?;
    let provider_operation = DurableOperationRecord::new(
        2,
        DurableOperationKind::Provider {
            root: root.operation_root(),
            attempt_id: AttemptId::try_from(Uuid::from_u128(0x5114))?,
        },
        ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, [1_u8].as_slice())?,
        vec![DurableOperationTransition::new(
            checkpoint_id,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger =
        RuntimeOperationLedger::new(vec![command], vec![command_operation, provider_operation])?;
    let command_signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::Command,
        RecoveryAction::FinalizationRequired,
    );
    let provider_signature = RecoveryHandlerSignature::new(
        RecoveryObligationKind::ProviderAttempt,
        RecoveryAction::ProviderEvidenceRequired,
    );
    let manifest = genesis.manifest();
    let working = Arc::new(RuntimeCheckpoint::new(RuntimeManifest::new(
        manifest.provenance().clone(),
        RuntimeTopology {
            session_id: source_topology.session_id,
            branch_id: source_topology.branch_id,
            checkpoint_id,
            parent_checkpoint_id: Some(source_topology.checkpoint_id),
            branch_revision: 1,
            fork: None,
            lease_fence: lease.fence().get(),
            commit_identity: Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"working-inspection"],
            )?,
        },
        RuntimeLifecycle::Working {
            root,
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            obligations: vec![
                RecoveryObligation::new(1, command_signature),
                RecoveryObligation::new(2, provider_signature),
            ],
        },
        manifest.harness_plan_component().clone(),
        manifest.provider_configuration().clone(),
        manifest.canonical().clone(),
        manifest.binding().clone(),
        manifest.tools().clone(),
        ledger,
        manifest.family_artifact().cloned(),
        manifest.interpreter_observation().cloned(),
        plakous_agent::internal::JobLedger::default(),
        manifest.durability(),
    )?)?);
    let bundle = RuntimeCheckpointCodec::advance(&working, genesis_loaded.checkpoint().bundle())?;
    let committed = store
        .commit(CommitCheckpoint::new(
            lease.clone(),
            genesis_loaded.head(),
            bundle,
        ))
        .await?;
    store.release(lease).await?;
    let head = SessionLocator::new(
        committed.head().session().session_id(),
        committed.head().session().branch_id(),
        committed.head().checkpoint().checkpoint_id(),
    );
    let OpenSessionOutcome::Opened {
        session: reopened, ..
    } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:recorded-abandonment")?,
            head,
            durable_completing_runtime_binding_for(
                workspace,
                descriptor()?,
                b"working-inspect-account",
            )?,
        ))
        .await?
    else {
        return Err("unfinished root did not reopen".into());
    };
    let expected = reopened
        .agent()
        .abandoned_turn()
        .cloned()
        .ok_or("open report")?;
    let terminal = reopened.locator();
    let request = FeedTerminalRequest {
        session: terminal.session_id(),
        branch: terminal.branch_id(),
        checkpoint: terminal.checkpoint_id(),
        turn_id,
    };
    let assert_report = |outcome: FeedTerminalOutcome| -> TestResult {
        let FeedTerminalOutcome::Terminal(read) = outcome else {
            return Err("terminal not retained".into());
        };
        assert_eq!(read.abandoned_turn(), Some(&expected));
        assert_eq!(read.result().usage().model_operations(), 0);
        Ok(())
    };
    assert_report(AgentSessions::inspect_feed_terminal_sqlite(
        &database, &request,
    )?)?;
    assert_report(sessions.feed_terminal(request).await?)?;
    // Pruning keeps the current head's immediate parent. Advance past the
    // closure so the abandoned Working cut is outside that retained pair.
    let advanced = reopened
        .agent()
        .prompt("advance pruning horizon")
        .await?
        .result()
        .await?;
    assert!(!matches!(
        advanced.outcome(),
        TurnResultOutcome::RecoveryRequired(_)
    ));
    let released = reopened.release().await?;
    sessions.prune_history(released).await?;
    assert!(matches!(
        store.load_checkpoint(head.checkpoint_id()).await,
        Err(SessionStoreError::Pruned)
    ));
    assert_report(AgentSessions::inspect_feed_terminal_sqlite(
        &database, &request,
    )?)?;
    assert_report(sessions.feed_terminal(request).await?)?;
    Ok(())
}
