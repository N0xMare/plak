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
) -> RunnerClient {
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
    RunnerClient::connect_home_local(
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
    .0
}

#[tokio::test]
async fn a_resident_stopped_line_reopens_ready_while_its_runtime_stays_open() {
    stopped_line_followup(false).await;
}

#[tokio::test]
async fn a_branch_hold_releases_recorded_stop_without_a_new_trigger() {
    stopped_line_followup(true).await;
}

struct StoppedLine {
    _temporary: tempfile::TempDir,
    services: Services,
    sessions: Arc<AgentSessions>,
    session: plakous::sessions::OpenedSession,
    catalog: Arc<WorkspaceCatalog>,
    line: LineId,
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
        session,
        catalog,
        line,
    }
}

async fn stopped_line_followup(branch_hold: bool) {
    let StoppedLine {
        _temporary,
        services,
        sessions,
        session,
        catalog,
        line,
    } = stopped_runtime().await;
    let locator = session.locator();
    let mut resident = Some(session);
    let branch = if branch_hold {
        resident.take().unwrap().release().await.unwrap();
        Some(
            sessions
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
    let client = line_runner(
        services.paths().unwrap().workspace().unwrap(),
        line,
        stops.clone(),
    )
    .await;
    let registry = services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    registry.ready_local(catalog.clone(), 1).unwrap();
    registry.require_connection("local", true);
    registry.run_probes_on(tokio::runtime::Handle::current());
    let retries: Retries = Arc::default();
    let calls = Arc::new(AtomicUsize::new(0));
    registry.probe_with(Arc::new({
        let context = services.runner_probe_context();
        let (retries, calls) = (retries.clone(), calls.clone());
        move |registry, id| {
            let (services, client, catalog) = (
                context.with_runner_registry(registry.clone()),
                client.clone(),
                catalog.clone(),
            );
            let (retries, calls) = (retries.clone(), calls.clone());
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let pending = reconcile_remote(&services, &id, &client).await?;
                registry.ready_local(catalog, 1)?;
                complete_reconciliation(&registry, &id, pending, &retries);
                Ok(())
            })
        }
    }));
    assert_eq!(
        registry.refresh(None).await.unwrap()[0].status,
        plakous::runner::RunnerStatus::Ready
    );
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    assert!(registry.needs_connection("local"));
    let delay = retries.lock().unwrap()["local"].delay;
    let calls_before_release = calls.load(Ordering::SeqCst);
    assert!([1, 2, 5, 15].contains(&delay));
    for _ in 0..3 {
        complete_reconciliation(registry, "local", true, &retries);
    }
    assert_eq!(retries.lock().unwrap()["local"].delay, delay);
    assert_eq!(calls.load(Ordering::SeqCst), calls_before_release);
    assert_eq!(
        registry.rows()[0].status,
        plakous::runner::RunnerStatus::Ready
    );
    let mut progress = registry.progress_events();
    if let Some(branch) = branch {
        branch.release().await.unwrap();
    }
    if let Some(session) = resident {
        session.release().await.unwrap();
    }
    // Only the previously queued retry can settle this debt: no picker, link
    // change, explicit reconciliation or host restart is triggered here.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while stops.load(Ordering::SeqCst) == 0
            || registry.rows()[0].status != plakous::runner::RunnerStatus::Ready
        {
            progress.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(calls.load(Ordering::SeqCst) > calls_before_release);
    assert!(!registry.needs_connection("local"));
}
