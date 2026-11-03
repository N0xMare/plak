//! Agent-line hand-off through the host registry (spec 01 §5.1, §5.3, §5.7,
//! §8.5): a line-continuing carry or a head-advancing rebind takes the
//! source's running job, a carry is fenced like a rebind, a lease-only open
//! records terminals the head folds once, a new-line rebind keeps pending
//! exit notices, and a head advance without a `JobHost` refuses running jobs
//! on durable state.
#![allow(clippy::unwrap_used, reason = "offline line fixture")]

use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest};
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, CreateSessionOutcome, CreateSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, OpenedSession, RebindRefused, RebindSessionOutcome,
    RebindSessionRequest, SessionBackend, SessionOperationId,
};
use plakous::tools::{JobStatus, JobStopReason};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
use std::num::NonZeroUsize;
use std::path::Path;
use std::{sync::Arc, time::Duration};

struct Fixture {
    root: tempfile::TempDir,
    controller: BoundaryController,
    plan: Arc<plakous_offline_crash_harness::script::ScriptPlan>,
    sessions: Arc<AgentSessions>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("evidence.jsonl"),
        )
        .unwrap();
        let plan = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "process_tools": true,
                "steps": [
                    {"output": {"tool": {"name": "process_session_start", "arguments": {
                        "command": "read line", "workdir": ".", "login": false,
                        "tty": false, "keep_stdin_open": true, "yield_time_ms": 0,
                        "max_output_bytes": 8192
                    }}}, "finish": "tool_calls"},
                    {"stream": [{"text": "waiting"}, {"stall": 30000}]}
                ]
            }))
            .unwrap(),
        );
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        Self {
            root,
            controller,
            plan,
            sessions,
        }
    }

    fn binding(&self) -> plakous::sessions::SessionRuntimeBinding {
        scripted_runtime_binding(self.root.path(), self.controller.clone(), self.plan.clone())
            .unwrap()
    }

    /// The same scripted binding with another admitted window: a rebind
    /// onto it changes a bound configuration fact.
    fn rebound_binding(&self) -> plakous::sessions::SessionRuntimeBinding {
        let mut plan = (*self.plan).clone();
        plan.maximum_projected_input = 30_000;
        scripted_runtime_binding(self.root.path(), self.controller.clone(), Arc::new(plan)).unwrap()
    }

    async fn carry(
        &self,
        operation: &str,
        source: plakous::sessions::SessionLocator,
        binding: plakous::sessions::SessionRuntimeBinding,
    ) -> Result<OpenedSession, plakous::sessions::AgentSessionsError> {
        let CreateSessionOutcome::Created { session, .. } = self
            .sessions
            .create(
                CreateSessionRequest::new(SessionOperationId::new(operation).unwrap(), binding)
                    .continuing_line(source),
            )
            .await?
        else {
            panic!("carried session required")
        };
        Ok(session)
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    async fn create(
        &self,
        operation: &str,
        binding: plakous::sessions::SessionRuntimeBinding,
    ) -> OpenedSession {
        let CreateSessionOutcome::Created { session, .. } = self
            .sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(operation).unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        session
    }
}

/// Starts one promoted job with the scripted first round, then cancels the
/// turn so the job keeps running with no turn.
async fn start_job(fixture: &Fixture, session: &OpenedSession) -> u32 {
    let agent = session.agent().clone();
    let turn = agent.prompt("start").await.unwrap();
    let control = turn.control();
    tokio::time::timeout(Duration::from_secs(5), async {
        while agent.jobs().is_empty()
            || plakous_offline_crash_harness::evidence::read_evidence(
                fixture.path().join("evidence.jsonl"),
            )
            .unwrap()
            .iter()
            .filter(|record| {
                record.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
                < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.cancel().await.unwrap();
    turn.result().await.unwrap();
    assert_eq!(agent.jobs()[0].status, JobStatus::Running);
    agent.jobs()[0].number
}

async fn feed_facts(sessions: &AgentSessions, session: plakous::model::SessionId) -> Vec<FeedFact> {
    let FeedPage::Records { records, .. } = sessions
        .feed_page(FeedPageRequest {
            cursor: FeedCursor::start(session),
            max_records: NonZeroUsize::new(256).unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!("feed behind retention")
    };
    records
        .iter()
        .flat_map(|record| record.facts().to_vec())
        .collect()
}

#[tokio::test]
async fn a_line_carry_hands_the_running_job_to_the_next_head_and_its_exit_lands_once() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let source = fixture
        .create(
            "line-carry:source",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let line = source.line();
    let number = start_job(&fixture, &source).await;
    let source_locator = source.release_for_transfer().await.unwrap();

    let CreateSessionOutcome::Created {
        session: target, ..
    } = fixture
        .sessions
        .create(
            CreateSessionRequest::new(
                SessionOperationId::new("line-carry:target").unwrap(),
                fixture.binding().with_job_host(registry.clone()),
            )
            .continuing_line(source_locator),
        )
        .await
        .unwrap()
    else {
        panic!("carried session required")
    };
    assert_eq!(target.line(), line);
    let head = fixture.sessions.line_head(line).await.unwrap();
    assert_eq!(head.session_id(), target.locator().session_id());
    assert_eq!(head.generation(), 2);
    // The running job continues under the new head, with its number.
    let job = target.agent().jobs()[0].clone();
    assert_eq!(job.number, number);
    assert_eq!(job.status, JobStatus::Running);

    // The source is read-only history now: neither an open nor its
    // verification attaches it.
    let unverified = fixture
        .sessions
        .verify_open(
            source_locator,
            fixture.binding().with_job_host(registry.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(unverified.kind(), AgentSessionsErrorKind::NotLineHead);
    let refused = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-carry:reopen-source").unwrap(),
            source_locator,
            fixture.binding().with_job_host(registry.clone()),
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), AgentSessionsErrorKind::NotLineHead);

    // The exit lands in the new head's ledger exactly once.
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    assert!(target.agent().record_job_terminals().await.unwrap());
    assert!(!target.agent().record_job_terminals().await.unwrap());
    let target_locator = target.release().await.unwrap();
    let fresh = fixture
        .binding()
        .with_job_host(Arc::new(crate::jobs::JobRegistry::default()));
    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-carry:reopen-target").unwrap(),
            target_locator,
            fresh,
        ))
        .await
        .unwrap()
    else {
        panic!("reopened head required")
    };
    let jobs = session.agent().jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].number, number);
    assert_eq!(
        jobs[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    session.release().await.unwrap();
}

#[tokio::test]
async fn a_lease_only_holder_records_terminals_and_the_head_folds_them_once() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let session = fixture
        .create(
            "line-terminals:create",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let line = session.line();
    let number = start_job(&fixture, &session).await;
    let locator = session.release_for_transfer().await.unwrap();

    let operation = SessionOperationId::new("line-terminals:hold").unwrap();
    let held = fixture
        .sessions
        .open_lease_only(&operation, line)
        .await
        .unwrap();
    assert_eq!(held.locator(), locator);
    assert_eq!(held.generation(), 1);
    // The holder keeps every other owner off the head.
    let busy = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-terminals:busy").unwrap(),
            locator,
            fixture.binding().with_job_host(registry.clone()),
        ))
        .await
        .unwrap_err();
    assert_eq!(busy.kind(), AgentSessionsErrorKind::Ownership);
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    let views = registry.views();
    assert_eq!(held.record_job_terminals(&views).await.unwrap(), [number]);
    assert_eq!(held.record_job_terminals(&views).await.unwrap().len(), 0);
    held.release().await.unwrap();

    // A fresh process has no registry: the recorded terminal, not
    // `outcome_unknown`, answers, and the head's open folds it once.
    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-terminals:open").unwrap(),
            locator,
            fixture
                .binding()
                .with_job_host(Arc::new(crate::jobs::JobRegistry::default())),
        ))
        .await
        .unwrap()
    else {
        panic!("head open required")
    };
    assert_eq!(
        session.agent().jobs()[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    let session_id = session.locator().session_id();
    let facts = feed_facts(&fixture.sessions, session_id).await;
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact, FeedFact::LineJobTerminalRecorded { job, .. } if *job == number))
            .count(),
        1
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact, FeedFact::LineJobTerminalsConsumed { jobs, .. } if jobs == &[number]))
            .count(),
        1
    );
    session.release().await.unwrap();
}

#[tokio::test]
async fn a_rebind_with_running_jobs_and_no_job_host_is_refused() {
    let fixture = Fixture::new();
    let session = fixture
        .create("line-refusal:create", fixture.binding())
        .await;
    start_job(&fixture, &session).await;
    let runtime = fixture.binding();
    let target = runtime.target_binding();
    let outcome = fixture
        .sessions
        .rebind(RebindSessionRequest::new(
            SessionOperationId::new("line-refusal:rebind").unwrap(),
            session.locator(),
            target,
            runtime,
        ))
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        RebindSessionOutcome::Refused {
            reason: RebindRefused::RunningJobsWithoutJobHost { running: 1 },
            ..
        }
    ));
    assert_eq!(session.agent().jobs()[0].status, JobStatus::Running);
    session.release().await.unwrap();
}

/// A carry is fenced like a rebind: it never moves the head while another
/// owner, here a lease-only holder, holds it, nor while the head has an
/// unfinished turn.
#[tokio::test]
async fn a_carry_is_refused_while_the_head_is_held_or_mid_turn() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let session = fixture
        .create(
            "carry-fence:create",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let line = session.line();
    start_job(&fixture, &session).await;
    let locator = session.release_for_transfer().await.unwrap();

    let holder = fixture
        .sessions
        .open_lease_only(&SessionOperationId::new("carry-fence:hold").unwrap(), line)
        .await
        .unwrap();
    let busy = fixture
        .carry(
            "carry-fence:busy",
            locator,
            fixture.binding().with_job_host(registry.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(busy.kind(), AgentSessionsErrorKind::Ownership);
    assert_eq!(
        fixture.sessions.line_head(line).await.unwrap().session_id(),
        locator.session_id()
    );
    // The holder's recorded terminal moves with the head on the next carry.
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    let number = holder
        .record_job_terminals(&registry.views())
        .await
        .unwrap()[0];
    holder.release().await.unwrap();
    let carried = fixture
        .carry(
            "carry-fence:carry",
            locator,
            fixture.binding().with_job_host(registry.clone()),
        )
        .await
        .unwrap();
    let jobs = carried.agent().jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].number, number);
    assert_eq!(
        jobs[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    let facts = feed_facts(&fixture.sessions, carried.locator().session_id()).await;
    assert!(facts.iter().any(|fact| matches!(
        fact,
        FeedFact::LineJobTerminalsConsumed { jobs, .. } if jobs == &[number]
    )));
    let carried_locator = carried.locator();

    // A head whose turn is unfinished is not carried.
    let dispatched = || {
        plakous_offline_crash_harness::evidence::read_evidence(
            fixture.path().join("evidence.jsonl"),
        )
        .unwrap()
        .iter()
        .filter(|record| {
            record.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
        })
        .count()
    };
    let before = dispatched();
    let _turn = carried.agent().prompt("stall").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while dispatched() < before + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let working = carried
        .abandon_and_release(&[], Duration::from_millis(250))
        .await
        .unwrap();
    assert_ne!(working, carried_locator);
    let mid_turn = fixture
        .carry(
            "carry-fence:mid-turn",
            working,
            fixture.binding().with_job_host(registry.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(mid_turn.kind(), AgentSessionsErrorKind::NotQuiescent);
    let head = fixture.sessions.line_head(line).await.unwrap();
    assert_eq!(head.session_id(), working.session_id());
    assert_eq!(head.generation(), 2);
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
}

/// The default rebind hands the running job, through the shared `JobHost`,
/// to the line's next head; its exit lands there exactly once.
#[tokio::test]
async fn a_head_advancing_rebind_hands_a_running_job_to_the_next_head() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let source = fixture
        .create(
            "line-rebind:source",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let line = source.line();
    let number = start_job(&fixture, &source).await;
    let source_locator = source.release_for_transfer().await.unwrap();

    let runtime = fixture.rebound_binding().with_job_host(registry.clone());
    let target = runtime.target_binding();
    let RebindSessionOutcome::Rebound {
        target: target_locator,
        ..
    } = fixture
        .sessions
        .rebind(RebindSessionRequest::recompiling_target(
            SessionOperationId::new("line-rebind:rebind").unwrap(),
            source_locator,
            target,
            runtime,
        ))
        .await
        .unwrap()
    else {
        panic!("rebind required")
    };
    let head = fixture.sessions.line_head(line).await.unwrap();
    assert_eq!(head.branch_id(), target_locator.branch_id());
    assert_eq!(head.generation(), 2);

    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-rebind:open").unwrap(),
            target_locator,
            fixture.rebound_binding().with_job_host(registry.clone()),
        ))
        .await
        .unwrap()
    else {
        panic!("rebound head required")
    };
    let job = session.agent().jobs()[0].clone();
    assert_eq!(job.number, number);
    assert_eq!(job.status, JobStatus::Running);
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    assert!(session.agent().record_job_terminals().await.unwrap());
    assert!(!session.agent().record_job_terminals().await.unwrap());
    let released = session.release().await.unwrap();
    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-rebind:reopen").unwrap(),
            released,
            fixture
                .rebound_binding()
                .with_job_host(Arc::new(crate::jobs::JobRegistry::default())),
        ))
        .await
        .unwrap()
    else {
        panic!("reopened head required")
    };
    let jobs = session.agent().jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    session.release().await.unwrap();
    // The source never records the exit: it is read-only history.
    let source_jobs = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-rebind:source-open").unwrap(),
            source_locator,
            fixture.binding().with_job_host(registry.clone()),
        ))
        .await
        .unwrap_err();
    assert_eq!(source_jobs.kind(), AgentSessionsErrorKind::NotLineHead);
}

/// A new-line rebind (`/model`, a profile rebind) keeps the pending notice
/// of an exited job, never its running processes.
#[tokio::test]
async fn a_new_line_rebind_keeps_pending_exit_notices() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let source = fixture
        .create(
            "line-notice:source",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let number = start_job(&fixture, &source).await;
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    assert!(source.agent().record_job_terminals().await.unwrap());
    let source_locator = source.release().await.unwrap();

    let runtime = fixture.rebound_binding().with_job_host(registry.clone());
    let target = runtime.target_binding();
    let RebindSessionOutcome::Rebound {
        target: target_locator,
        ..
    } = fixture
        .sessions
        .rebind(
            RebindSessionRequest::recompiling_target(
                SessionOperationId::new("line-notice:rebind").unwrap(),
                source_locator,
                target,
                runtime,
            )
            .starting_line(),
        )
        .await
        .unwrap()
    else {
        panic!("rebind required")
    };
    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-notice:open").unwrap(),
            target_locator,
            fixture.rebound_binding().with_job_host(registry.clone()),
        ))
        .await
        .unwrap()
    else {
        panic!("rebound target required")
    };
    assert_ne!(
        session.line(),
        plakous::model::LineId::started_by(source_locator.branch_id())
    );
    let jobs = session.agent().jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].number, number);
    assert_eq!(
        jobs[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    session.release().await.unwrap();
}

/// Without a `JobHost` a transfer release has nothing to transfer to: it
/// stops and records the jobs, so a later head advance is not refused.
#[tokio::test]
async fn a_transfer_release_without_a_job_host_stops_and_records_its_jobs() {
    let fixture = Fixture::new();
    let source = fixture
        .create("line-private:create", fixture.binding())
        .await;
    let number = start_job(&fixture, &source).await;
    let locator = source.release_for_transfer().await.unwrap();
    let runtime = fixture.rebound_binding();
    let target = runtime.target_binding();
    let RebindSessionOutcome::Rebound {
        target: target_locator,
        ..
    } = fixture
        .sessions
        .rebind(RebindSessionRequest::recompiling_target(
            SessionOperationId::new("line-private:rebind").unwrap(),
            locator,
            target,
            runtime,
        ))
        .await
        .unwrap()
    else {
        panic!("rebind required")
    };
    let OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("line-private:open").unwrap(),
            target_locator,
            fixture.rebound_binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("rebound head required")
    };
    let jobs = session.agent().jobs();
    assert_eq!(jobs[0].number, number);
    assert_eq!(
        jobs[0].status,
        JobStatus::Stopped(JobStopReason::SessionExit)
    );
    session.release().await.unwrap();
}

/// A carry of a head with a running job onto a binding without a `JobHost`
/// is refused as `UnsettledProcesses`, and the head's lease it took is
/// released: a carry with the `JobHost` then succeeds.
#[tokio::test]
async fn a_carry_of_running_jobs_without_a_job_host_is_refused_and_releases_the_head() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let source = fixture
        .create(
            "carry-jobs:create",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let line = source.line();
    start_job(&fixture, &source).await;
    let locator = source.release_for_transfer().await.unwrap();
    let refused = fixture
        .carry("carry-jobs:private", locator, fixture.binding())
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), AgentSessionsErrorKind::UnsettledProcesses);
    assert_eq!(
        fixture.sessions.line_head(line).await.unwrap().generation(),
        1
    );
    let carried = fixture
        .carry(
            "carry-jobs:hosted",
            locator,
            fixture.binding().with_job_host(registry.clone()),
        )
        .await
        .unwrap();
    assert_eq!(carried.agent().jobs()[0].status, JobStatus::Running);
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    carried.release().await.unwrap();
}

/// An exact retry of a committed carry's operation returns the same session
/// at the same genesis; the line does not move again.
#[tokio::test]
async fn a_same_operation_carry_retry_returns_the_committed_successor() {
    let fixture = Fixture::new();
    let source = fixture
        .create("carry-retry:create", fixture.binding())
        .await;
    let line = source.line();
    let locator = source.release().await.unwrap();
    let first = fixture
        .carry("carry-retry:carry", locator, fixture.binding())
        .await
        .unwrap();
    let genesis = first.locator();
    first.release().await.unwrap();
    let retried = fixture
        .carry("carry-retry:carry", locator, fixture.binding())
        .await
        .unwrap();
    assert_eq!(retried.locator(), genesis);
    let head = fixture.sessions.line_head(line).await.unwrap();
    assert_eq!(
        (head.session_id(), head.branch_id(), head.generation()),
        (genesis.session_id(), genesis.branch_id(), 2)
    );
    assert_eq!(fixture.sessions.line_head_of(locator).await.unwrap(), head);
    retried.release().await.unwrap();
}

/// A Working head whose job stop a lease-only holder recorded answers that
/// recorded stop when it is opened, before any commit folds it.
#[tokio::test]
async fn a_working_head_answers_its_recorded_job_terminal_on_open() {
    let fixture = Fixture::new();
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let session = fixture
        .create(
            "working-terminal:create",
            fixture.binding().with_job_host(registry.clone()),
        )
        .await;
    let line = session.line();
    // The first round promotes the job; the second stalls mid-turn.
    let agent = session.agent().clone();
    let _turn = agent.prompt("start").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while agent.jobs().is_empty()
            || plakous_offline_crash_harness::evidence::read_evidence(
                fixture.path().join("evidence.jsonl"),
            )
            .unwrap()
            .iter()
            .filter(|record| {
                record.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
                < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let number = agent.jobs()[0].number;
    let working = session
        .abandon_and_release(&[], Duration::from_millis(250))
        .await
        .unwrap();
    registry.stop_all(Duration::from_millis(250)).await.unwrap();
    let holder = fixture
        .sessions
        .open_lease_only(
            &SessionOperationId::new("working-terminal:hold").unwrap(),
            line,
        )
        .await
        .unwrap();
    assert_eq!(holder.locator(), working);
    let recorded = holder
        .record_job_terminals(&registry.views())
        .await
        .unwrap();
    assert_eq!(recorded, [number]);
    holder.release().await.unwrap();
    let opened = fixture
        .sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("working-terminal:open").unwrap(),
            working,
            fixture
                .binding()
                .with_job_host(Arc::new(crate::jobs::JobRegistry::default())),
        ))
        .await
        .unwrap();
    let session = match opened {
        OpenSessionOutcome::Opened { session, .. }
        | OpenSessionOutcome::RestoredWorking { session, .. } => session,
        other => panic!("Working head did not open: {other:?}"),
    };
    let job = session
        .agent()
        .jobs()
        .into_iter()
        .find(|job| job.number == number)
        .unwrap();
    assert!(
        matches!(job.status, JobStatus::Stopped(_)),
        "{:?}",
        job.status
    );
    let _ = session.release().await;
}
