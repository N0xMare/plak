//! Agent-line hand-off through the host registry (spec 01 §5.1, §5.3, §8.5):
//! a line-continuing carry takes the source's running job, a lease-only open
//! records terminals the head folds once, and a rebind without a `JobHost`
//! refuses running jobs.
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

    // The source is read-only history now.
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
