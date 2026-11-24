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
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub(super) mod production_fence;
mod ready_fence;

tokio::task_local! {
    static STOP_OPEN_HOLD: (LineId, Arc<tokio::sync::Notify>, Arc<tokio::sync::Semaphore>);
}

pub(super) async fn hold_stop_open<F: Future>(line: LineId, opening: F) -> F::Output {
    if let Ok((held_line, entered, release)) = STOP_OPEN_HOLD.try_with(Clone::clone)
        && held_line == line
    {
        entered.notify_one();
        let _permit = release.acquire().await.unwrap();
    }
    opening.await
}

async fn line_runner(
    workspace: &std::path::Path,
    line: LineId,
    stops: Arc<AtomicUsize>,
) -> (RunnerClient, serde_json::Value) {
    let (host, descriptor) = line_stream(workspace, line, stops);
    let client = RunnerClient::connect_home_local(
        host,
        Bind {
            host_id: "review".into(),
            host_instance: "instance".into(),
            version: VersionTriple::current("review", "test"),
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

fn line_stream(
    workspace: &std::path::Path,
    line: LineId,
    stops: Arc<AtomicUsize>,
) -> (tokio::io::DuplexStream, serde_json::Value) {
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
    (host, descriptor)
}

#[tokio::test]
async fn a_resident_stopped_line_reopens_ready_while_its_runtime_stays_open() {
    stopped_line_followup(false, false).await;
}

#[tokio::test]
async fn a_branch_hold_releases_recorded_stop_without_a_new_trigger() {
    stopped_line_followup(true, false).await;
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
    stopped_runtime_named("review:create").await
}

async fn stopped_runtime_named(operation: &str) -> StoppedLine {
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
            SessionOperationId::new(operation).unwrap(),
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
                client.connection_revision(),
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
                    let connection = client.connection_revision();
                    let pending = reconcile_remote(&services, &id, &client).await?;
                    let runner = registry.reconciliation_services(&id)?;
                    registry.ready(
                        &id,
                        connection,
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
        tokio::time::timeout(Duration::from_secs(20), async {
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

async fn stopped_line_followup(branch_hold: bool, check_ramp: bool) {
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
    if check_ramp {
        let mut progress = registry.progress_events();
        // At 1, 3, 8 and 23 seconds, join each attempt before advancing
        // again: the next backoff does not start until the store answers.
        assert_eq!(delay, 1);
        for (delay, next) in [(1, 2), (2, 5), (5, 15), (15, 15)] {
            advance_stop_delay(delay).await;
            let attempt = progress.recv().await.unwrap();
            assert_eq!(attempt.runner_id, "review");
            assert_eq!(attempt.phase, plakous::runner::ProbePhase::Reconcile);
            assert!(attempt.row.is_none());
            harness.assert_admission().await;
            assert_eq!(harness.stops.load(Ordering::SeqCst), 0);
            let retries = harness.retries.lock().unwrap();
            let retry = &retries["review"];
            assert_eq!(retry.pending_stops, [harness.fixture.line]);
            assert_eq!(retry.delay, next);
            assert!(retry.scheduled);
        }
        assert!(matches!(
            progress.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
    harness.release_holder().await;
    if check_ramp {
        let delay = harness.retries.lock().unwrap()["review"].delay;
        advance_stop_delay(delay).await;
        // Let the stop settle over real I/O without advancing its watchdog.
        tokio::time::resume();
    }
    harness.wait_retired().await;
    assert_eq!(harness.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.full_passes.load(Ordering::SeqCst),
        1,
        "a stop retry ran a runner-wide sweep"
    );
    assert!(harness.admitted.load(Ordering::SeqCst) > 0);
}

async fn advance_stop_delay(seconds: u64) {
    // Tokio can round the sleep's deadline up to the next millisecond.
    tokio::time::advance(Duration::from_secs(seconds) + Duration::from_millis(1)).await;
}

#[tokio::test(start_paused = true)]
async fn a_resident_held_for_twenty_seconds_keeps_ready_and_effect_admission() {
    // A live blocking task prevents Tokio from spending virtual time while
    // SQLite is still doing real I/O. The hang bound itself stays on wall time.
    let (done, release) = std::sync::mpsc::channel::<()>();
    let mut watchdog =
        tokio::task::spawn_blocking(move || release.recv_timeout(Duration::from_secs(30)));
    tokio::select! {
        () = stopped_line_followup(false, true) => {}
        result = &mut watchdog => panic!("stop retry stalled: {result:?}"),
    }
    drop(done);
    assert_eq!(
        watchdog.await.unwrap(),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    );
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
async fn a_stale_stop_attempt_preserves_a_new_pending_set_and_context() {
    let mut harness = StopHarness::new(true).await;
    let second = stopped_runtime_named("review:create-second").await;
    assert_ne!(harness.fixture.line, second.line);
    let registry = harness.fixture.services.runners().clone();
    let retries: Retries = Arc::new(std::sync::Mutex::new(BTreeMap::from([(
        "review".into(),
        Retry {
            delay: 15,
            scheduled: true,
            pending_stops: vec![harness.fixture.line],
            context: Some(StopContext {
                services: harness.fixture.services.runner_probe_context(),
                client: harness.client.clone(),
            }),
            generation: 1,
        },
    )])));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut attempt = Box::pin(STOP_OPEN_HOLD.scope(
        (harness.fixture.line, entered.clone(), release.clone()),
        retry_stops(registry.clone(), "review".into(), retries.clone()),
    ));
    // Capture the old retry, then hold its lease open across the replacement.
    // A branch hold refuses an open; it does not guarantee a Pending poll.
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            biased;
            () = &mut attempt => panic!("stop attempt completed before the lease-open hold"),
            () = entered.notified() => {}
        }
    })
    .await
    .unwrap();
    complete_reconciliation(
        &second.services.with_runner_registry(registry.clone()),
        "review",
        &harness.client,
        vec![harness.fixture.line, second.line],
        &retries,
    );
    harness.release_holder().await;
    release.add_permits(1);
    attempt.await;
    let state = retries.lock().unwrap();
    let retry = &state["review"];
    assert!(retry.pending_stops.contains(&second.line));
    assert_eq!(retry.pending_stops.len(), 2);
    assert_eq!(
        retry
            .context
            .as_ref()
            .unwrap()
            .services
            .paths()
            .unwrap()
            .home(),
        second.services.paths().unwrap().home()
    );
    assert!(retry.scheduled);
    assert!(registry.needs_connection("review"));
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
