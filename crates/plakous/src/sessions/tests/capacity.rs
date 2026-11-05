//! Store admission reserve: ordinary work stops before the headroom that
//! settlement, terminal publication and recovery need.

use super::*;
use plakous_agent::internal::{PruneBranch, StoreAdmission, StoreInvalidReason};
use plakous_agent::{AgentErrorKind, CommandId, ControlError, ControlErrorKind, TurnCommand};
use plakous_store::{SqliteSessionStore, SqliteStoreOptions};
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

const ROOMY_CHECKPOINTS: usize = 4096;

fn checkpoint_options(checkpoints: usize) -> TestResult<SqliteStoreOptions> {
    let nonzero = |value| NonZeroUsize::new(value).ok_or("nonzero SQLite fixture limit");
    Ok(SqliteStoreOptions::new(
        MemoryStoreLimits::new(
            nonzero(64)?,
            nonzero(64)?,
            nonzero(checkpoints)?,
            nonzero(1 << 20)?,
            nonzero(256 << 20)?,
            nonzero(16)?,
        ),
        nonzero(8)?,
    ))
}

fn open_store(path: &Path, checkpoints: usize) -> TestResult<(SqliteSessionStore, AgentSessions)> {
    let store = SqliteSessionStore::open_with_clock_and_options(
        path,
        Arc::new(FixedLeaseClock::new(timestamp()?)),
        checkpoint_options(checkpoints)?,
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store.clone()),
        RuntimeDurability::Required,
    ))?;
    Ok((store, sessions))
}

async fn close_store(store: SqliteSessionStore, sessions: AgentSessions) -> TestResult {
    drop(sessions);
    store.shutdown().await?;
    Ok(())
}

/// Smallest checkpoint limit at which `admission` stops exactly at `used`.
fn limit_stopping_at(admission: StoreAdmission, used: u64) -> TestResult<usize> {
    let limit = (used..=used.saturating_mul(2).saturating_add(16))
        .find(|limit| admission.threshold(*limit) == used)
        .ok_or("no checkpoint limit stops at the requested usage")?;
    Ok(usize::try_from(limit)?)
}

fn binding(
    workspace: &Arc<WorkspacePolicy>,
    account: &[u8],
) -> TestResult<(
    SessionRuntimeBinding,
    Arc<Mutex<Option<PreparedCallRecord>>>,
)> {
    completing_runtime_binding_with_capture_and_durability(
        Arc::clone(workspace),
        descriptor()?,
        account,
        LocalDurabilityDisposition::DurableRequired,
    )
}

/// Root turns per filler session: few enough that no filler builds the
/// superseded history its turns' automatic retention would reclaim.
const FILLER_TURNS: usize = 4;

/// Runs turns in filler sessions until the store retains `target`
/// checkpoints, so that the reserve of the limit chosen next is several
/// commits wide. Each filler stays below automatic retention's threshold.
async fn fill_to(
    sessions: &AgentSessions,
    store: &SqliteSessionStore,
    workspace: &Arc<WorkspacePolicy>,
    target: u64,
) -> TestResult<Vec<SessionLocator>> {
    let mut fillers = Vec::new();
    while store.stats().await?.checkpoints() < target {
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("capacity:filler-{}", fillers.len()))?,
                binding(workspace, b"capacity-filler")?.0,
            ))
            .await?;
        for _ in 0..FILLER_TURNS {
            if store.stats().await?.checkpoints() >= target {
                break;
            }
            let result = session.agent().prompt("fill").await?.result().await?;
            assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
        }
        fillers.push(session.release().await?);
    }
    Ok(fillers)
}

async fn start_rejection(session: &OpenedSession, command: &str) -> TestResult<ControlError> {
    let submission = session
        .agent()
        .command(TurnCommand::Start {
            command_id: CommandId::new(command)?,
            input: "begin more work".to_owned().into(),
        })
        .await;
    Ok(match submission {
        Ok(submission) => submission
            .acknowledgement()
            .rejection()
            .cloned()
            .ok_or("a root was admitted without ordinary headroom")?,
        Err(error) => error,
    })
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "mid-turn refusal, pre-admission refusal and the deletion remedy are one store lifetime"
)]
async fn sqlite_ordinary_pressure_fails_the_running_turn_then_refuses_roots_until_deletion()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("pressure.sqlite3");
    let (store, sessions) = open_store(&database, ROOMY_CHECKPOINTS)?;
    let fillers = fill_to(&sessions, &store, &workspace, 160).await?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("capacity:main")?,
            binding(&workspace, b"capacity-main")?.0,
        ))
        .await?;
    let main = session.release().await?;
    let used = store.stats().await?.checkpoints();
    close_store(store, sessions).await?;

    // The next root's classification is the last ordinary commit that fits.
    let ordinary_stop = used + 1;
    let limit = limit_stopping_at(StoreAdmission::Ordinary, ordinary_stop)?;
    let settlement_stop = StoreAdmission::Settlement.threshold(u64::try_from(limit)?);
    let (store, sessions) = open_store(&database, limit)?;
    let (runtime, prepared) = binding(&workspace, b"capacity-main")?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:open-main")?,
            main,
            runtime,
        ))
        .await?
    else {
        return Err("main session did not reopen quiescently".into());
    };
    assert_eq!(store.stats().await?.checkpoints(), used, "open wrote state");

    // (b) Admitted, then refused at its first ordinary commit: the turn ends
    // with one published storage-pressure failure, never recovery_required,
    // and nothing reached the provider.
    let result = session
        .agent()
        .prompt("begin under pressure")
        .await?
        .result()
        .await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!(
            "expected a storage-pressure failure: {:?}",
            result.outcome()
        )
        .into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Session);
    let pressure = SessionStorePressure::from_agent_error(error)
        .ok_or("the failure carries no typed storage pressure")?;
    assert_eq!(pressure.admission(), SessionStoreAdmission::Ordinary);
    assert_eq!(pressure.threshold(), ordinary_stop);
    assert!(
        error
            .message()
            .contains("session store capacity was reached: checkpoints"),
        "{error}"
    );
    assert!(
        error.message().contains(&format!(
            "/{limit} (ordinary writes stop at {ordinary_stop})"
        )),
        "{error}"
    );
    assert!(
        test_lock(&prepared).is_none(),
        "a provider call was prepared"
    );
    let settled = store.stats().await?.checkpoints();
    assert!(
        settled > ordinary_stop && settled <= settlement_stop,
        "used={used} settled={settled} limit={limit}"
    );

    // (a) No ordinary headroom: the next root is refused before admission,
    // typed, and writes nothing.
    let refused = start_rejection(&session, "capacity:refused-root").await?;
    assert_eq!(refused.kind(), ControlErrorKind::StoragePressure);
    assert!(
        refused
            .message()
            .contains(&format!("(ordinary writes stop at {ordinary_stop})")),
        "{}",
        refused.message()
    );
    let prompt_error = session
        .agent()
        .prompt("still refused")
        .await
        .expect_err("prompt was admitted without ordinary headroom");
    assert_eq!(prompt_error.kind(), AgentErrorKind::Session);
    assert_eq!(store.stats().await?.checkpoints(), settled);
    assert!(test_lock(&prepared).is_none());
    let main = session.release().await?;
    assert_eq!(
        AgentSessions::inspect_recovery_sqlite(&database, main)?,
        None
    );
    // The product reads usage against the default limits it opens with.
    let usage = SessionBackend::inspect_sqlite_usage(&database)?.ok_or("current store")?;
    let checkpoints = usage.bounds()[0];
    assert_eq!(checkpoints.bound(), SessionStoreBound::Checkpoints);
    assert_eq!(checkpoints.used(), settled);
    assert!(!checkpoints.ordinary_exhausted());

    // Deletion is never refused for capacity, and it is the remedy.
    for filler in fillers {
        let archived = sessions.archive(filler).await?;
        sessions
            .delete(archived.session_id(), archived.archived_revision())
            .await?;
    }
    assert!(store.stats().await?.checkpoints() < ordinary_stop);
    let (runtime, prepared) = binding(&workspace, b"capacity-main")?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:open-after-delete")?,
            main,
            runtime,
        ))
        .await?
    else {
        return Err("main session did not reopen after deletion".into());
    };
    let result = session
        .agent()
        .prompt("continue after deletion")
        .await?
        .result()
        .await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert!(test_lock(&prepared).is_some());
    session.release().await?;
    close_store(store, sessions).await
}

async fn open_released(
    sessions: &AgentSessions,
    workspace: &Arc<WorkspacePolicy>,
    operation: &str,
    locator: SessionLocator,
    account: &[u8],
) -> TestResult<OpenedSession> {
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new(operation)?,
            locator,
            binding(workspace, account)?.0,
        ))
        .await?
    else {
        return Err(format!("{operation}: the session did not reopen quiescently").into());
    };
    Ok(session)
}

/// A store retaining a root and a child session at `used` checkpoints, open
/// with the checkpoint limit whose subordinate mark is `used + ahead`.
struct SubordinateStore {
    directory: TempDir,
    workspace: Arc<WorkspacePolicy>,
    database: PathBuf,
    store: SqliteSessionStore,
    sessions: AgentSessions,
    root: SessionLocator,
    child: SessionLocator,
    used: u64,
    limit: u64,
}

async fn subordinate_store(name: &str, ahead: u64) -> TestResult<SubordinateStore> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join(format!("{name}.sqlite3"));
    let (store, sessions) = open_store(&database, ROOMY_CHECKPOINTS)?;
    fill_to(&sessions, &store, &workspace, 160).await?;
    let mut locators = Vec::new();
    for role in ["root", "child"] {
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("{name}:{role}"))?,
                binding(&workspace, role.as_bytes())?.0,
            ))
            .await?;
        locators.push(session.release().await?);
    }
    let used = store.stats().await?.checkpoints();
    close_store(store, sessions).await?;
    let mark = used + ahead;
    let limit = (mark..=mark.saturating_mul(2))
        .find(|limit| backend::subordinate_threshold(*limit) == mark)
        .ok_or("no checkpoint limit puts the subordinate mark where asked")?;
    let (store, sessions) = open_store(&database, usize::try_from(limit)?)?;
    Ok(SubordinateStore {
        directory,
        workspace,
        database,
        store,
        sessions,
        root: locators[0],
        child: locators[1],
        used,
        limit,
    })
}

/// A subordinate handle's new work stops one reserve before the ordinary
/// threshold, definitely and writing nothing, while a plain handle on the
/// same store still runs its turns.
#[tokio::test]
async fn sqlite_subordinate_sessions_stop_one_reserve_before_the_root() -> TestResult {
    let SubordinateStore {
        directory: _directory,
        workspace,
        database,
        store,
        sessions,
        root,
        child,
        used,
        limit,
    } = subordinate_store("subordinate", 0).await?;
    let locators = [root, child];
    let ordinary_stop = StoreAdmission::Ordinary.threshold(limit);
    assert!(
        ordinary_stop > used + 8,
        "the root needs headroom for a turn"
    );
    let children = sessions.with_subordinate_admission();

    let child = open_released(
        &children,
        &workspace,
        "capacity:open-child",
        locators[1],
        b"child",
    )
    .await?;
    let refused = start_rejection(&child, "capacity:child-turn").await?;
    assert_eq!(refused.kind(), ControlErrorKind::StoragePressure);
    assert!(
        refused.message().contains(&format!(
            "checkpoints {used}/{limit} (subordinate ordinary writes stop at {used}, before the store's {ordinary_stop})"
        )),
        "{}",
        refused.message()
    );
    child.release().await?;
    let spawn = children
        .create(CreateSessionRequest::new(
            SessionOperationId::new("capacity:child-spawn")?,
            binding(&workspace, b"child")?.0,
        ))
        .await;
    let Err(error) = spawn else {
        return Err("a subordinate create was admitted past the subordinate mark".into());
    };
    assert_eq!(
        error.kind(),
        AgentSessionsErrorKind::StoragePressure,
        "{error}"
    );
    let pressure = error.storage_pressure().ok_or("untyped create refusal")?;
    assert_eq!(pressure.admission(), SessionStoreAdmission::Ordinary);
    assert_eq!(pressure.threshold(), used);
    assert_eq!(
        store.stats().await?.checkpoints(),
        used,
        "a refusal wrote state"
    );

    let root = open_released(
        &sessions,
        &workspace,
        "capacity:open-root",
        locators[0],
        b"root",
    )
    .await?;
    let result = root
        .agent()
        .prompt("the root keeps working")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(store.stats().await?.checkpoints() > used);
    root.release().await?;

    assert_exact_retention(&database, &store, locators).await?;
    drop(children);
    close_store(store, sessions).await
}

/// Per-session retention covers every retained checkpoint, largest first,
/// and each figure counts every object the session reaches exactly once.
async fn assert_exact_retention(
    database: &Path,
    store: &SqliteSessionStore,
    locators: [SessionLocator; 2],
) -> TestResult {
    let retention = SessionBackend::inspect_sqlite_session_retention(database)?;
    assert_eq!(
        retention
            .iter()
            .map(SessionRetention::checkpoints)
            .sum::<u64>(),
        store.stats().await?.checkpoints()
    );
    assert!(
        retention
            .windows(2)
            .all(|pair| pair[0].object_bytes() >= pair[1].object_bytes())
    );
    // Exact figures: every object a session's checkpoints reach, once, by an
    // independent recursive walk; the root and the child share objects, and
    // each still counts every object it reaches.
    let reader = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    for locator in &locators {
        let session = retention
            .iter()
            .find(|session| session.session_id() == locator.session_id)
            .ok_or("a session with retained checkpoints is missing")?;
        assert!(session.checkpoints() > 0);
        assert_eq!(
            session.object_bytes(),
            reached_bytes(&reader, &locator.session_id.to_string())?
        );
    }
    let shared: i64 = reader.query_row(
        &format!(
            "WITH a(k,c,d) AS ({REACH_FROM}), b(k,c,d) AS ({REACH_FROM_SECOND})
             SELECT count(*) FROM (SELECT k,c,d FROM a INTERSECT SELECT k,c,d FROM b)"
        ),
        [
            locators[0].session_id.to_string(),
            locators[1].session_id.to_string(),
        ],
        |row| row.get(0),
    )?;
    assert!(shared > 0, "the root and the child share no object");
    Ok(())
}

/// Every object the checkpoints of the session bound to `?1` reach.
const REACH_FROM: &str = "WITH RECURSIVE reach(k,c,d) AS (
     SELECT root_kind, root_codec, root_digest FROM checkpoints WHERE session_id = ?1
     UNION
     SELECT e.child_kind, e.child_codec, e.child_digest FROM runtime_object_edges e
     JOIN reach r ON e.parent_kind = r.k AND e.parent_codec = r.c AND e.parent_digest = r.d)
   SELECT k, c, d FROM reach";
const REACH_FROM_SECOND: &str = "WITH RECURSIVE reach(k,c,d) AS (
     SELECT root_kind, root_codec, root_digest FROM checkpoints WHERE session_id = ?2
     UNION
     SELECT e.child_kind, e.child_codec, e.child_digest FROM runtime_object_edges e
     JOIN reach r ON e.parent_kind = r.k AND e.parent_codec = r.c AND e.parent_digest = r.d)
   SELECT k, c, d FROM reach";

fn reached_bytes(reader: &rusqlite::Connection, session: &str) -> TestResult<u64> {
    let bytes: i64 = reader.query_row(
        &format!(
            "WITH objects(k,c,d) AS ({REACH_FROM})
             SELECT COALESCE(SUM(o.byte_len), 0) FROM objects
             JOIN runtime_objects o ON o.kind = objects.k AND o.codec = objects.c
               AND o.digest = objects.d"
        ),
        [session],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(bytes)?)
}

/// A subordinate child's turn admitted just below its mark ends at its next
/// ordinary write with one `Failed` result settled past the mark (settlement
/// is never held to it), nothing reaches the provider, nothing is left for
/// recovery, a reopen writes nothing, and the root keeps working.
#[tokio::test]
async fn sqlite_a_subordinate_turn_crossing_the_mark_fails_once_and_settles_past_it() -> TestResult
{
    // The child's root receipt is the last write below the mark.
    let SubordinateStore {
        directory: _directory,
        workspace,
        database,
        store,
        sessions,
        root,
        child,
        used,
        limit,
    } = subordinate_store("crossing", 1).await?;
    let mark = used + 1;
    let ordinary_stop = StoreAdmission::Ordinary.threshold(limit);
    let children = sessions.with_subordinate_admission();
    let (runtime, prepared) = binding(&workspace, b"child")?;
    let OpenSessionOutcome::Opened { session: child, .. } = children
        .open(OpenSessionRequest::new(
            SessionOperationId::new("crossing:open-child")?,
            child,
            runtime,
        ))
        .await?
    else {
        return Err("the child did not reopen quiescently".into());
    };
    let result = child
        .agent()
        .prompt("cross the mark")
        .await?
        .result()
        .await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err(format!(
            "expected one storage-pressure failure: {:?}",
            result.outcome()
        )
        .into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Session);
    let pressure =
        SessionStorePressure::from_agent_error(error).ok_or("the failure carries no pressure")?;
    assert_eq!(pressure.admission(), SessionStoreAdmission::Ordinary);
    assert_eq!(pressure.threshold(), mark);
    assert!(pressure.subordinate());
    assert!(
        test_lock(&prepared).is_none(),
        "a provider call was prepared"
    );
    let settled = store.stats().await?.checkpoints();
    assert!(
        settled > mark && settled < ordinary_stop,
        "settlement passes the mark and stays under the root's stop: mark={mark} settled={settled} stop={ordinary_stop}"
    );
    let again = start_rejection(&child, "crossing:again").await?;
    assert_eq!(again.kind(), ControlErrorKind::StoragePressure);
    assert_eq!(
        store.stats().await?.checkpoints(),
        settled,
        "a refusal wrote state"
    );
    let child_locator = child.release().await?;
    assert_eq!(
        AgentSessions::inspect_recovery_sqlite(&database, child_locator)?,
        None
    );
    let reopened = open_released(
        &children,
        &workspace,
        "crossing:reopen-child",
        child_locator,
        b"child",
    )
    .await?;
    assert_eq!(
        store.stats().await?.checkpoints(),
        settled,
        "a reopen wrote state"
    );
    reopened.release().await?;

    let mut root_locator = root;
    for turn in ["first", "second"] {
        let root = open_released(
            &sessions,
            &workspace,
            &format!("crossing:open-root-{turn}"),
            root_locator,
            b"root",
        )
        .await?;
        let result = root.agent().prompt(turn).await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        root_locator = root.release().await?;
    }
    assert!(store.stats().await?.checkpoints() < ordinary_stop);
    drop(children);
    close_store(store, sessions).await
}

/// (c) A settlement refusal leaves a turn needing recovery; the open that
/// recovers it writes under the recovery class into the last of the reserve,
/// a repeat open writes nothing, and a store opened over its configured limit
/// still opens.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the refused settlement, reserve recovery and over-limit reopen are one store lifetime"
)]
async fn sqlite_recovery_writes_into_the_reserve_and_an_over_limit_store_opens() -> TestResult {
    // Sixteen is the smallest limit whose settlement threshold sits below it:
    // ordinary stops at 14, settlement at 15, recovery at 16.
    const LIMIT: usize = 16;
    // Seven retained checkpoints let the turn dispatch under the ordinary
    // threshold and refuse its terminal publication at the settlement one.
    const PAD: u64 = 7;
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("recovery-reserve.sqlite3");
    let (store, sessions) = open_store(&database, LIMIT)?;
    let mut padding = 0;
    while store.stats().await?.checkpoints() < PAD {
        padding += 1;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("capacity:pad-{padding}"))?,
                binding(&workspace, b"capacity-pad")?.0,
            ))
            .await?;
        session.release().await?;
    }
    let (runtime, prepared) = binding(&workspace, b"capacity-recover")?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("capacity:recover-main")?,
            runtime,
        ))
        .await?;
    let result = session
        .agent()
        .prompt("settle into the reserve")
        .await?
        .result()
        .await?;
    let TurnResultOutcome::RecoveryRequired(_) = result.outcome() else {
        return Err(format!("expected a settlement refusal: {:?}", result.outcome()).into());
    };
    assert!(test_lock(&prepared).is_some());
    let source = session.release().await?;
    let refused = store.stats().await?.checkpoints();
    assert_eq!(
        refused,
        StoreAdmission::Settlement.threshold(u64::try_from(LIMIT)?)
    );
    assert!(AgentSessions::inspect_recovery_sqlite(&database, source)?.is_some());

    // History retention never prunes under a Working head: the facade leaves
    // it for recovery, and the store refuses it typed.
    assert_eq!(
        sessions.prune_history(source).await?,
        PruneHistoryOutcome::RecoveryRequired
    );
    let lease = store
        .acquire(AcquireBranch::new(
            source.session_ref(),
            "00000000-0000-4000-8000-0000000000b2".parse()?,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        ))
        .await?;
    let head = store.load_branch(source.session_ref()).await?.head();
    assert_eq!(
        store
            .prune(PruneBranch::new(lease.clone(), head, BTreeSet::new(), 1))
            .await,
        Err(SessionStoreError::Invalid(
            StoreInvalidReason::PruneHeadNotQuiescent
        ))
    );
    store.release(lease).await?;

    let (runtime, replay) = binding(&workspace, b"capacity-recover")?;
    let session = match sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:recover-open")?,
            source,
            runtime,
        ))
        .await?
    {
        OpenSessionOutcome::Opened { session, .. } => session,
        OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
            turn.result().result().await?;
            session
        }
        OpenSessionOutcome::RecoveryRequired { recovery, .. } => {
            return Err(format!("recovery did not fit the reserve: {recovery:?}").into());
        }
    };
    assert!(test_lock(&replay).is_none(), "recovery replayed the model");
    let recovered = store.stats().await?.checkpoints();
    assert!(
        recovered > refused && recovered <= u64::try_from(LIMIT)?,
        "refused={refused} recovered={recovered}"
    );
    let refused_root = start_rejection(&session, "capacity:after-recovery").await?;
    assert_eq!(refused_root.kind(), ControlErrorKind::StoragePressure);
    let target = session.release().await?;
    assert_ne!(target, source);
    assert_eq!(
        AgentSessions::inspect_recovery_sqlite(&database, target)?,
        None
    );
    close_store(store, sessions).await?;

    // Lowering the limit below what is retained must not brick open; a
    // quiescent session opens and reopening writes nothing.
    let (store, sessions) = open_store(&database, LIMIT / 2)?;
    for attempt in ["capacity:over-limit-open", "capacity:over-limit-reopen"] {
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(attempt)?,
                target,
                binding(&workspace, b"capacity-recover")?.0,
            ))
            .await?
        else {
            return Err("recovered session did not reopen quiescently".into());
        };
        assert_eq!(session.release().await?, target);
        assert_eq!(store.stats().await?.checkpoints(), recovered);
    }
    // Retention is reclaim class: it runs over the absolute limit.
    let PruneHistoryOutcome::Pruned(report) = sessions.prune_history(target).await? else {
        return Err("a quiescent head over the limit was not pruned".into());
    };
    assert_eq!(report.locator(), target);
    assert_eq!(
        store.stats().await?.checkpoints(),
        recovered - report.pruned_checkpoints()
    );
    close_store(store, sessions).await
}

struct CountingModel {
    inner: Arc<CompletingModel>,
    starts: Arc<AtomicUsize>,
}

impl LanguageModel for CountingModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.inner.clone().revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let starts = Arc::clone(&self.starts);
        Ok(self
            .inner
            .clone()
            .prepare_call(plan, context)?
            .map_start(move |start| {
                starts.fetch_add(1, Ordering::AcqRel);
                start
            }))
    }
}

fn counting_compaction_binding(
    workspace: &Arc<WorkspacePolicy>,
    account: &[u8],
    starts: &Arc<AtomicUsize>,
) -> TestResult<SessionRuntimeBinding> {
    let descriptor = descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential", account])?,
        1,
        timestamp()?,
        None,
    )?;
    let scripted = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone())
            .with_context_observation(crate::test_model::context_observation()?)
            .with_content_sensitive_observation()
            .with_call_recovery(plakous_test_support::ScriptedRecoveryState::new()),
    );
    let model: Arc<dyn LanguageModel> = Arc::new(CountingModel {
        inner: Arc::new(CompletingModel { scripted }),
        starts: Arc::clone(starts),
    });
    let configured = crate::test_model::configured_compaction_model(
        model,
        descriptor,
        authority,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let builder = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::clone(workspace),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
        ReasoningPolicy::default(),
    ))
    .identity_sources(
        Arc::new(FixedClock::new(timestamp()?)),
        Arc::new(SystemIdGenerator),
    );
    Ok(SessionRuntimeBinding::new(builder))
}

/// A source session with two compactable turns, retained behind filler so
/// the reserve of the limit chosen next is several commits wide.
struct CompactionFixture {
    _directory: TempDir,
    workspace: Arc<WorkspacePolicy>,
    database: PathBuf,
    source: SessionLocator,
    used: u64,
}

async fn compaction_fixture() -> TestResult<CompactionFixture> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("compaction-pressure.sqlite3");
    let (store, sessions) = open_store(&database, ROOMY_CHECKPOINTS)?;
    fill_to(&sessions, &store, &workspace, 64).await?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("capacity:compact-source")?,
            binding(&workspace, b"capacity-compact")?.0,
        ))
        .await?;
    for input in ["older intent", "latest protected intent"] {
        let result = session.agent().prompt(input).await?.result().await?;
        assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    }
    let source = session.release().await?;
    let used = store.stats().await?.checkpoints();
    close_store(store, sessions).await?;
    Ok(CompactionFixture {
        _directory: directory,
        workspace,
        database,
        source,
        used,
    })
}

/// Compacts the fixture's source under a checkpoint limit whose ordinary
/// threshold is `ordinary_stop`, returning both attempts of one operation.
async fn compact_under_pressure(
    fixture: &CompactionFixture,
    ordinary_stop: u64,
) -> TestResult<(
    CompactSessionOutcome,
    CompactSessionOutcome,
    usize,
    u64,
    u64,
)> {
    let limit = limit_stopping_at(StoreAdmission::Ordinary, ordinary_stop)?;
    let (store, sessions) = open_store(&fixture.database, limit)?;
    // Compaction runs through the open session's driver.
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("capacity:open-compact-source")?,
            fixture.source,
            binding(&fixture.workspace, b"capacity-compact")?.0,
        ))
        .await?
    else {
        return Err("compaction source did not reopen quiescently".into());
    };
    assert_eq!(
        store.stats().await?.checkpoints(),
        fixture.used,
        "open wrote state"
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let compact = || -> TestResult<CompactSessionRequest> {
        Ok(CompactSessionRequest::new(
            SessionOperationId::new("capacity:compact")?,
            fixture.source,
            CompactReason::Manual,
            counting_compaction_binding(&fixture.workspace, b"capacity-compact", &starts)?,
        ))
    };
    let first = sessions.compact(compact()?).await?;
    let settled = store.stats().await?.checkpoints();
    let repeated = sessions.compact(compact()?).await?;
    assert_eq!(
        store.stats().await?.checkpoints(),
        settled,
        "a repeated compaction wrote state"
    );
    let _ = session.release().await;
    close_store(store, sessions).await?;
    Ok((
        first,
        repeated,
        starts.load(Ordering::Acquire),
        settled,
        StoreAdmission::Settlement.threshold(u64::try_from(limit)?),
    ))
}

/// (d) The paid compaction's prepare and its prepared recovery locator (the
/// first fence out of `Prepared`) are the last ordinary commits that fit; the
/// dispatch after the locator and the completion land in the settlement
/// reserve, so it publishes once and a retry never dispatches again.
#[tokio::test]
async fn sqlite_paid_compaction_completion_settles_past_the_ordinary_threshold() -> TestResult {
    let fixture = compaction_fixture().await?;
    let ordinary_stop = fixture.used + 2;
    let (first, repeated, starts, settled, settlement_stop) =
        compact_under_pressure(&fixture, ordinary_stop).await?;
    let CompactSessionOutcome::Compacted { target, .. } = first else {
        return Err(format!("paid compaction did not publish under settlement: {first:?}").into());
    };
    assert_ne!(target, fixture.source);
    assert!(matches!(
        repeated,
        CompactSessionOutcome::Compacted { target: repeated, .. } if repeated == target
    ));
    assert_eq!(starts, 1, "compaction dispatched other than once");
    assert!(
        settled > ordinary_stop && settled <= settlement_stop,
        "used={} settled={settled} stop={ordinary_stop}",
        fixture.used
    );
    Ok(())
}

/// An ordinary refusal of the prepared locator, the compaction's first fence,
/// is definite: nothing reached the provider, the compaction publishes a
/// failure under settlement, and a retry of the same operation replays it
/// rather than dispatching.
#[tokio::test]
async fn sqlite_refused_compaction_fence_fails_without_recovery_or_dispatch() -> TestResult {
    let fixture = compaction_fixture().await?;
    let ordinary_stop = fixture.used + 1;
    let (first, repeated, starts, settled, settlement_stop) =
        compact_under_pressure(&fixture, ordinary_stop).await?;
    let CompactSessionOutcome::Failed { .. } = first else {
        return Err(format!("refused fence did not publish a failure: {first:?}").into());
    };
    assert!(
        matches!(repeated, CompactSessionOutcome::Failed { .. }),
        "{repeated:?}"
    );
    assert_eq!(starts, 0, "a refused fence reached the provider");
    assert!(
        settled > ordinary_stop && settled <= settlement_stop,
        "used={} settled={settled} stop={ordinary_stop}",
        fixture.used
    );
    Ok(())
}

/// A deletion interrupted after its archive leaves the session archived and
/// its state retained. Archiving again cannot reach it, but
/// `delete_archived` finds the archived head and reclaims it; once the
/// tombstone exists the session counts as gone.
#[tokio::test]
async fn sqlite_an_archive_left_by_an_interrupted_delete_is_reclaimed_on_rerun() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let database = directory.path().join("interrupted-delete.sqlite3");
    let (store, sessions) = open_store(&database, ROOMY_CHECKPOINTS)?;
    let locator = *fill_to(&sessions, &store, &workspace, 12)
        .await?
        .last()
        .ok_or("no filler session")?;
    let retained = store.stats().await?;
    sessions.archive(locator).await?;

    let again = sessions
        .archive(locator)
        .await
        .expect_err("an archived session was archived again");
    assert_eq!(again.kind(), AgentSessionsErrorKind::NotFound);
    assert_eq!(store.stats().await?.checkpoints(), retained.checkpoints());
    assert_eq!(store.stats().await?.tombstones(), 0);

    let report = sessions
        .delete_archived(locator.session_id())
        .await?
        .ok_or("the archived session was not found")?;
    assert!(report.reclaimed_checkpoints() > 0);
    let reclaimed = store.stats().await?;
    assert!(reclaimed.checkpoints() < retained.checkpoints());
    assert_eq!(reclaimed.tombstones(), 1);
    assert!(
        sessions
            .delete_archived(locator.session_id())
            .await?
            .is_none()
    );
    close_store(store, sessions).await
}
