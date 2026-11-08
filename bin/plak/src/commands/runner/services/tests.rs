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
    let stops = Arc::new(AtomicUsize::new(0));
    let client = line_runner(&workspace, line, stops.clone()).await;
    let catalog = Arc::new(WorkspaceCatalog::local(&workspace, &[], paths.home()).unwrap());
    let registry = services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    registry.ready_local(catalog.clone(), 1).unwrap();
    registry.require_connection("local", true);
    registry.run_probes_on(tokio::runtime::Handle::current());
    registry.probe_with(Arc::new({
        let context = services.runner_probe_context();
        move |registry, id| {
            let (services, client, catalog) = (
                context.with_runner_registry(registry.clone()),
                client.clone(),
                catalog.clone(),
            );
            Box::pin(async move {
                reconcile_remote(&services, &id, &client).await?;
                registry.ready_local(catalog, 1)
            })
        }
    }));
    assert_eq!(
        registry.refresh(None).await.unwrap()[0].status,
        plakous::runner::RunnerStatus::Ready
    );
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    session.release().await.unwrap();
    registry.reconcile_when_available("local");
    assert_eq!(
        registry.refresh(None).await.unwrap()[0].status,
        plakous::runner::RunnerStatus::Ready
    );
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}
