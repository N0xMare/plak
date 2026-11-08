use super::*;
use plakous::runner::protocol::{Bind, HostFrame, LineStatus, RunnerFrame, VersionTriple, frame};
use plakous::runner::{
    ClientWindows, ConfiguredRunner, RunnerIdentity, RunnerOptions, RunnerService, WorkspaceCatalog,
};
use plakous::sessions::{CreateSessionOutcome, CreateSessionRequest, SessionOperationId};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

async fn line_runner(
    workspace: &std::path::Path,
    line: LineId,
    stops: Arc<AtomicUsize>,
) -> (RunnerClient, serde_json::Value) {
    let version = VersionTriple::current("review", "test");
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-4000-8000-000000000001".parse().unwrap()),
            workspace,
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("review").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions::default(),
    )
    .unwrap();
    let hello = service.hello().clone();
    let descriptor = hello.descriptor.clone();
    let (host, mut runner) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        runner
            .write_all(&frame::encode(&RunnerFrame::Hello(hello)).unwrap())
            .await
            .unwrap();
        let mut status = LineStatus {
            host_id: "review".into(),
            line: line.to_string(),
            owner: None,
            adopted: true,
            jobs: serde_json::json!({"instance":"review", "jobs":[]}),
            foreground: false,
        };
        let mut frames = frame::FrameReader::new();
        let mut bytes = [0; 8192];
        loop {
            let count = runner.read(&mut bytes).await.unwrap();
            if count == 0 {
                return;
            }
            frames.push(&bytes[..count]);
            while let Some(body) = frames.next_frame().unwrap() {
                let response = match frame::decode::<HostFrame>(&body).unwrap() {
                    HostFrame::Lines { request_id } => Some(RunnerFrame::Lines {
                        request_id,
                        lines: vec![status.clone()],
                    }),
                    HostFrame::TransferOwner(transfer) => {
                        status.owner = Some(transfer.to);
                        Some(RunnerFrame::TransferAccepted {
                            control_id: transfer.control_id,
                            stopped: Vec::new(),
                            unpromoted: Vec::new(),
                        })
                    }
                    HostFrame::Request(request) => {
                        assert_eq!(request.operation.verb, "jobs.stop");
                        stops.fetch_add(1, Ordering::SeqCst);
                        Some(RunnerFrame::Result {
                            request_id: request.request_id,
                            payload: serde_json::Value::Null,
                        })
                    }
                    HostFrame::Ping { nonce } => Some(RunnerFrame::Pong { nonce }),
                    _ => None,
                };
                if let Some(response) = response {
                    runner
                        .write_all(&frame::encode(&response).unwrap())
                        .await
                        .unwrap();
                }
            }
        }
    });
    let client = RunnerClient::connect_home_local(
        host,
        Bind {
            host_id: "review".into(),
            host_instance: "instance".into(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
        None,
    )
    .await
    .unwrap()
    .0;
    (client, descriptor)
}

#[tokio::test]
async fn a_resident_stopped_line_reopens_ready_while_its_runtime_stays_open() {
    stopped_line_followup(false, std::time::Duration::ZERO).await;
}

#[tokio::test]
async fn a_branch_hold_releases_recorded_stop_without_a_new_trigger() {
    stopped_line_followup(true, std::time::Duration::ZERO).await;
}

struct StoppedLine {
    _temporary: tempfile::TempDir,
    services: Services,
    sessions: Arc<AgentSessions>,
    session: Option<plakous::sessions::OpenedSession>,
    catalog: Arc<WorkspaceCatalog>,
    line: LineId,
    record_id: String,
}

async fn stopped_runtime() -> StoppedLine {
    use plakous_hosted::sessions::projector::{RootInterrupt, RootLossIntent, RootStopAnchor};
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, runtime_binding};
    let (temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let paths = paths.for_workspace(&workspace);
    paths.ensure_layout().unwrap();
    let services = Services::for_home(paths.clone());
    let sessions = services.state_roots().unwrap().runtime_sessions().unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        temporary.path().join("evidence"),
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("review:create").unwrap(),
            runtime_binding(&workspace, controller).unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("not created")
    };
    let locator = session.locator();
    let mut record = crate::store::sessions::SessionRecord::new(workspace.clone());
    record.bind_plakous_locator(locator).unwrap();
    let records = services.session_records().unwrap();
    records.save(&mut record).unwrap();
    records
        .write_root_loss(
            &record.id,
            RootLossIntent::Interrupt(RootInterrupt {
                anchor: RootStopAnchor::Command {
                    command_id: "review:stop".into(),
                },
                cause: RootInterruptCause::Stopped,
            }),
        )
        .unwrap();
    let line = sessions.line_head_of(locator).await.unwrap().line();
    let catalog = Arc::new(WorkspaceCatalog::local(Some(&workspace), &[], paths.home()).unwrap());
    StoppedLine {
        _temporary: temporary,
        services,
        sessions,
        session: Some(session),
        catalog,
        line,
        record_id: record.id,
    }
}

struct AdmissionEcho(Arc<AtomicUsize>);

fn admitted_echo_services(
    descriptor: serde_json::Value,
    admitted: Arc<AtomicUsize>,
) -> plakous::runner::RunnerServices {
    let backend =
        plakous::tools::runner::RunnerBackend::new(descriptor, Arc::new(AdmissionEcho(admitted)))
            .unwrap();
    plakous::runner::RunnerServices::new(Arc::new(backend), tokio::runtime::Handle::current())
}

impl plakous::tools::runner::RunnerLink for AdmissionEcho {
    fn call(
        &self,
        _: plakous::tools::runner::RunnerCall,
    ) -> plakous::tools::runner::RunnerCallFuture {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(serde_json::Value::Null) })
    }
}

struct StopHarness {
    fixture: StoppedLine,
    branch: Option<plakous::sessions::BranchHold>,
    client: RunnerClient,
    retries: Retries,
    stops: Arc<AtomicUsize>,
    full_passes: Arc<AtomicUsize>,
    admitted: Arc<AtomicUsize>,
}

impl StopHarness {
    async fn new(branch_hold: bool) -> Self {
        let mut fixture = stopped_runtime().await;
        let branch = if branch_hold {
            let session = fixture.session.take().unwrap();
            let locator = session.locator();
            session.release().await.unwrap();
            Some(
                fixture
                    .sessions
                    .hold_branch(
                        &SessionOperationId::new("review:branch-hold").unwrap(),
                        locator,
                    )
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let stops = Arc::new(AtomicUsize::new(0));
        let (client, descriptor) = line_runner(
            fixture.services.paths().unwrap().workspace().unwrap(),
            fixture.line,
            stops.clone(),
        )
        .await;
        let admitted = Arc::new(AtomicUsize::new(0));
        let runner = admitted_echo_services(descriptor, admitted.clone());
        let registry = fixture.services.runners();
        registry.configure(ConfiguredRunner {
            runner_id: "review".into(),
            names: vec!["review".into()],
        });
        registry
            .ready(
                "review",
                client.clone(),
                runner,
                fixture.catalog.options(),
                plakous::runner::WorkspaceDiscovery::default(),
                1,
            )
            .unwrap();
        registry.require_connection("review", true);
        registry.run_probes_on(tokio::runtime::Handle::current());
        let retries: Retries = Arc::default();
        let full_passes = Arc::new(AtomicUsize::new(0));
        registry.probe_with(Arc::new({
            let context = fixture.services.runner_probe_context();
            let (retries, calls, client, catalog) = (
                retries.clone(),
                full_passes.clone(),
                client.clone(),
                fixture.catalog.clone(),
            );
            move |registry, id| {
                let services = context.with_runner_registry(registry.clone());
                let (retries, calls, client, catalog) = (
                    retries.clone(),
                    calls.clone(),
                    client.clone(),
                    catalog.clone(),
                );
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let pending = reconcile_remote(&services, &id, &client).await?;
                    let runner = registry.reconciliation_services(&id)?;
                    registry.ready(
                        &id,
                        client.clone(),
                        runner,
                        catalog.options(),
                        plakous::runner::WorkspaceDiscovery::default(),
                        1,
                    )?;
                    complete_reconciliation(&services, &id, &client, pending, &retries);
                    Ok(())
                })
            }
        }));
        assert_eq!(
            registry.refresh(None).await.unwrap()[0].status,
            plakous::runner::RunnerStatus::Ready
        );
        Self {
            fixture,
            branch,
            client,
            retries,
            stops,
            full_passes,
            admitted,
        }
    }

    async fn assert_admission(&self) {
        let registry = self.fixture.services.runners();
        assert_eq!(
            registry.rows()[0].status,
            plakous::runner::RunnerStatus::Ready
        );
        registry
            .services("review")
            .unwrap()
            .unwrap()
            .backend()
            .link()
            .call(plakous::tools::runner::RunnerCall::unkeyed(
                plakous::tools::runner::RunnerVerb::VcsEffect,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
    }

    async fn release_holder(&mut self) {
        if let Some(branch) = self.branch.take() {
            branch.release().await.unwrap();
        }
        if let Some(session) = self.fixture.session.take() {
            session.release().await.unwrap();
        }
    }

    async fn wait_retired(&self) {
        let mut progress = self.fixture.services.runners().progress_events();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !self.retries.lock().unwrap()["review"]
                .pending_stops
                .is_empty()
            {
                progress.recv().await.unwrap();
                self.assert_admission().await;
            }
        })
        .await
        .unwrap();
        assert!(!self.retries.lock().unwrap()["review"].scheduled);
        assert_eq!(self.retries.lock().unwrap()["review"].delay, 0);
        assert!(!self.fixture.services.runners().needs_connection("review"));
    }
}

async fn stopped_line_followup(branch_hold: bool, held_for: std::time::Duration) {
    let mut harness = StopHarness::new(branch_hold).await;
    let registry = harness.fixture.services.runners();
    harness.assert_admission().await;
    assert_eq!(harness.stops.load(Ordering::SeqCst), 0);
    assert!(registry.needs_connection("review"));
    let delay = harness.retries.lock().unwrap()["review"].delay;
    assert!([1, 2, 5, 15].contains(&delay));
    for _ in 0..3 {
        complete_reconciliation(
            &harness.fixture.services,
            "review",
            &harness.client,
            vec![harness.fixture.line],
            &harness.retries,
        );
    }
    assert_eq!(harness.retries.lock().unwrap()["review"].delay, delay);
    let mut progress = registry.progress_events();
    let deadline = tokio::time::Instant::now() + held_for;
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut attempts = 0;
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            _ = poll.tick() => harness.assert_admission().await,
            _ = progress.recv() => {
                attempts += 1;
                harness.assert_admission().await;
            }
        }
    }
    if !held_for.is_zero() {
        assert!(
            attempts > 0 && attempts <= 3,
            "stop-only ramp spun or duplicated: {attempts}"
        );
        assert_eq!(harness.retries.lock().unwrap()["review"].delay, 15);
    }
    harness.release_holder().await;
    harness.wait_retired().await;
    assert_eq!(harness.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.full_passes.load(Ordering::SeqCst),
        1,
        "a stop retry ran a runner-wide sweep"
    );
    assert!(harness.admitted.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn a_resident_held_for_twenty_seconds_keeps_ready_and_effect_admission() {
    stopped_line_followup(false, std::time::Duration::from_secs(20)).await;
}

#[tokio::test]
async fn a_delayed_stop_revalidates_a_later_accepted_submission_under_the_head() {
    use plakous_hosted::sessions::projector::{
        Delivery, SessionViews, Submission, SubmissionKind, ViewWrites,
    };
    let mut harness = StopHarness::new(true).await;
    let store = harness.fixture.services.session_records().unwrap();
    let mut record = store.load(&harness.fixture.record_id).unwrap();
    let submission = Submission {
        command_id: "later-command".into(),
        kind: SubmissionKind::RootStart,
        queue_item_id: "later-item".into(),
        queue_item_revision: 1,
        target_turn_id: None,
        canonical_command: "later accepted turn".into(),
        submitted_at_ms: 1,
    };
    store.save_submission(&mut record, &submission).unwrap();
    let from = store
        .load_views(&record.id)
        .unwrap()
        .unwrap_or_else(SessionViews::empty);
    let mut to = from.clone();
    to.state
        .deliveries
        .insert(submission.command_id, Delivery::Accepted);
    store
        .commit_projection_views(&record.id, &ViewWrites::between(&from, &to), &to.state)
        .unwrap();
    harness.release_holder().await;
    harness.wait_retired().await;
    assert_eq!(
        harness.stops.load(Ordering::SeqCst),
        0,
        "the old stop dispatched after later acceptance"
    );
    assert_eq!(harness.full_passes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_settled_remote_stop_releases_its_deferred_connection_requirement() {
    let mut harness = StopHarness::new(true).await;
    harness.release_holder().await;
    harness.wait_retired().await;
}

#[tokio::test]
async fn a_serving_local_daemon_remains_required_after_stop_debt_retires() {
    let harness = StopHarness::new(false).await;
    let registry = harness.fixture.services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    registry
        .ready_local(harness.fixture.catalog.clone(), 1)
        .unwrap();
    complete_reconciliation(
        &harness.fixture.services,
        "local",
        &harness.client,
        Vec::new(),
        &harness.retries,
    );
    assert!(registry.needs_connection("local"));
    assert_eq!(
        harness.retries.lock().unwrap()["local"].pending_stops,
        [] as [LineId; 0]
    );
}

#[tokio::test]
async fn a_busy_stop_only_attempt_retains_debt_without_closing_admission() {
    let harness = StopHarness::new(true).await;
    let remaining = stop_only_followup(
        &harness.fixture.services,
        &harness.client,
        &[harness.fixture.line],
    )
    .await
    .unwrap();
    assert_eq!(remaining, [harness.fixture.line]);
    assert_eq!(harness.stops.load(Ordering::SeqCst), 0);
    harness.assert_admission().await;
}
