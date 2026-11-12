//! Real runner links keep operator ownership separate from agent state.

use super::*;
use plakous::runner::protocol::operator::TerminalDimensions;
use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{ClientWindows, RunnerIdentity, RunnerOptions, RunnerService};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

pub(super) fn service(workspace: &Path) -> RunnerService {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        workspace,
        workspace,
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("terminal-fixture").unwrap(),
            version: VersionTriple::current("revision", "debug"),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            operator_workspace: Some(Arc::new(|cwd| {
                let mut launch = plakous::runner::operator::account_launch(
                    cwd.to_owned(),
                    "/usr/bin:/bin".into(),
                    None,
                )?;
                launch.shell = "/bin/sh".into();
                launch.home = cwd.to_owned();
                Ok(launch)
            })),
            ..RunnerOptions::default()
        },
    )
    .unwrap()
}

pub(super) fn bind(registry: &TerminalRegistry) -> Bind {
    Bind {
        host_id: registry.host_id.clone(),
        host_instance: registry.runner_generation.clone(),
        version: VersionTriple::current("revision", "debug"),
        inspect: false,
    }
}

pub(super) async fn connect(
    registry: &TerminalRegistry,
    service: &RunnerService,
) -> (RunnerClient, tokio::task::JoinHandle<()>) {
    let (host, runner) = tokio::io::duplex(64 * 1024);
    let service = service.clone();
    let serving = tokio::spawn(async move {
        service.serve_connection(runner).await;
    });
    let (client, _) = RunnerClient::connect(host, bind(registry), ClientWindows::default())
        .await
        .unwrap();
    (client, serving)
}

pub(super) async fn open(
    registry: &TerminalRegistry,
    count: &AttachmentCount,
    context: &HostRequestContext,
    workspace: &Path,
) -> TerminalView {
    registry
        .open_placement(
            context,
            pin(registry, workspace.to_owned()).key,
            TerminalDimensions::new(24, 80).unwrap(),
            control::Placement {
                runner: "local".into(),
                workspace: workspace.to_owned(),
            },
            count.hold().unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn operator_host_controller_principal_fences_disconnect_reattach_and_shutdown_join() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let service = service(&workspace);
    let (client, serving) = connect(&registry, &service).await;
    registry.reconcile("local", &client).await.unwrap();
    let owner = context("operator");
    let other = context("other");
    let view = open(&registry, &count, &owner, &workspace).await;
    let cursor = view.row.as_ref().unwrap().oldest.clone();
    assert_eq!(count.get(), 1);
    assert_eq!(registry.list(&other).unwrap().len(), 0);
    assert!(matches!(
        registry.close(&other, &view.handle, true).await,
        Err(TerminalError::Lost)
    ));
    assert!(matches!(
        registry
            .attach(&other, &view.handle, cursor.clone(), false)
            .await,
        Err(TerminalError::Lost)
    ));
    let controller = registry
        .attach(&owner, &view.handle, cursor.clone(), false)
        .await
        .unwrap();
    let mut second = owner.clone();
    second.connection_id = 2;
    assert!(matches!(
        registry
            .attach(&second, &view.handle, cursor.clone(), false)
            .await,
        Err(TerminalError::Busy)
    ));
    assert!(matches!(
        controller.input(&other, 0, b"must-not-run").await,
        Err(TerminalError::Lost)
    ));
    registry.disconnect(&other.principal, owner.connection_id, owner.host_generation);
    controller.credit(&owner, 65536).await.unwrap();
    registry.disconnect(&owner.principal, owner.connection_id, owner.host_generation);
    assert!(matches!(
        controller.input(&owner, 0, b"must-not-replay").await,
        Err(TerminalError::Lost)
    ));
    drop(controller);
    // Detach sends Close before the next Attach on the same client sender.
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut controller = loop {
        match registry
            .attach(&second, &view.handle, cursor.clone(), false)
            .await
        {
            Ok(controller) => break controller,
            Err(TerminalError::Refused(OperatorRefusal::TerminalBusy)) => {}
            Err(error) => panic!("reattach refused: {error:?}"),
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    controller.credit(&second, 65536).await.unwrap();
    controller
        .input(&second, 0, b"printf 'HOST-TERMINAL\\n'\n")
        .await
        .unwrap();
    let mut acknowledged = false;
    while !acknowledged {
        if let plakous::runner::protocol::operator::TerminalEvent::InputAccepted { next } =
            tokio::time::timeout(std::time::Duration::from_secs(2), controller.next(&second))
                .await
                .unwrap()
                .unwrap()
        {
            acknowledged = next > 0;
        }
    }
    assert_eq!(client.lines().await.unwrap(), []);
    assert_eq!(
        registry.store().unwrap().session_ids().unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        registry.shutdown(std::time::Duration::from_secs(4)).await,
        0
    );
    assert_eq!(count.get(), 0);
    assert!(!service.operator_terminals().unwrap().has_live_terminals());
    assert!(registry.admit_local("local", &workspace).is_ok());
    drop(controller);
    client.disconnect();
    serving.await.unwrap();
}

#[tokio::test]
async fn operator_host_same_generation_reconciliation_fences_old_stream_before_input() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let service = service(&workspace);
    let (client, serving) = connect(&registry, &service).await;
    registry.reconcile("local", &client).await.unwrap();
    let owner = context("operator");
    let view = open(&registry, &count, &owner, &workspace).await;
    let controller = registry
        .attach(&owner, &view.handle, view.row.unwrap().oldest, false)
        .await
        .unwrap();
    let (host, runner) = tokio::io::duplex(64 * 1024);
    let remote = service.clone();
    let next = tokio::spawn(async move {
        remote.serve_connection(runner).await;
    });
    client.reconnect(host).await.unwrap();
    assert!(matches!(
        controller.input(&owner, 0, b"no-replay").await,
        Err(TerminalError::Unavailable)
    ));
    registry.reconcile("local", &client).await.unwrap();
    assert!(matches!(
        controller.input(&owner, 0, b"no-replay").await,
        Err(TerminalError::Lost)
    ));
    assert_eq!(count.get(), 1);
    assert_eq!(
        registry.shutdown(std::time::Duration::from_secs(4)).await,
        0
    );
    client.disconnect();
    serving.await.unwrap();
    next.await.unwrap();
}

#[tokio::test]
async fn operator_host_runner_restart_retains_surviving_memory_pins_without_global_barrier() {
    let (directory, registry, count) = fixture();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let old = service(&workspace);
    let (client, serving) = connect(&registry, &old).await;
    registry.reconcile("local", &client).await.unwrap();
    let owner = context("operator");
    let view = open(&registry, &count, &owner, &workspace).await;
    let replacement = service(&workspace);
    let (host, runner) = tokio::io::duplex(64 * 1024);
    let peer = replacement.clone();
    let next = tokio::spawn(async move {
        peer.serve_connection(runner).await;
    });
    client.reconnect(host).await.unwrap();
    registry.reconcile("local", &client).await.unwrap();
    assert!(registry.list(&owner).unwrap()[0].lost);
    assert_eq!(count.get(), 1);
    assert!(registry.admit("local", &workspace).await.is_err());
    assert!(
        registry
            .admit("local", &directory.path().join("unrelated"))
            .await
            .is_ok()
    );
    assert!(matches!(
        registry.close(&context("other"), &view.handle, true).await,
        Err(TerminalError::Lost)
    ));
    registry.close(&owner, &view.handle, true).await.unwrap();
    assert_eq!(count.get(), 0);
    assert!(
        old.operator_terminals().unwrap().has_live_terminals(),
        "no kill or death inference on acknowledged loss"
    );
    old.operator_terminals().unwrap().close_all();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while old.operator_terminals().unwrap().has_live_terminals() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        registry.shutdown(std::time::Duration::from_secs(2)).await,
        0
    );
    client.disconnect();
    serving.await.unwrap();
    next.await.unwrap();
}
