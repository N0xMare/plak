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
