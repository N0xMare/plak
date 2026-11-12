//! Host-only loss discovers runner pins; simultaneous state loss is unlocalized.

use super::*;
use plakous::runner::protocol::operator::TerminalCursor;
use plakous::runner::{ClientWindows, RunnerClient, RunnerService};
use std::time::{Duration, Instant};

fn listener(service: RunnerService, path: &Path) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = service.clone();
            tokio::spawn(async move {
                service.serve_connection(stream).await;
            });
        }
    })
}

#[tokio::test]
#[ignore = "host terminal crash subprocess fixture"]
async fn operator_host_crash_worker() {
    let home = std::env::var_os("PLAK_TEST_TERMINAL_HOME").unwrap();
    let workspace = PathBuf::from(std::env::var_os("PLAK_TEST_TERMINAL_WORKSPACE").unwrap());
    let endpoint = std::env::var_os("PLAK_TEST_TERMINAL_ENDPOINT").unwrap();
    let registry = TerminalRegistry::new(PlakPaths::explicit(home.into()), 7).unwrap();
    let stream = tokio::net::UnixStream::connect(PathBuf::from(endpoint))
        .await
        .unwrap();
    let (client, _) =
        RunnerClient::connect(stream, linked::bind(&registry), ClientWindows::default())
            .await
            .unwrap();
    registry.reconcile("local", &client).await.unwrap();
    let count = AttachmentCount::default();
    if std::env::var("PLAK_TEST_TERMINAL_CUT").unwrap() == "reserved" {
        TerminalRegistry::request(
            &registry.binding("local").unwrap(),
            registry.namespace(&context("operator").principal).unwrap(),
            OperatorOperation::Renew,
        )
        .await
        .unwrap();
        let mut reserved = pin(&registry, workspace);
        reserved.instance = client.instance();
        registry.reserve(reserved, count.hold().unwrap()).unwrap();
    } else {
        linked::open(&registry, &count, &context("operator"), &workspace).await;
    }
    std::fs::write(
        std::env::var_os("PLAK_TEST_TERMINAL_READY").unwrap(),
        b"ready",
    )
    .unwrap();
    std::future::pending::<()>().await;
}

struct HostProcess(std::process::Child);
impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn operator_host_loss_sigkill_cuts_retain_pins_and_first_bind_waits_for_proof() {
    for (cut, replacement) in [("reserved", false), ("running", false), ("running", true)] {
        loss_cut(cut, replacement).await;
    }
}

struct KilledHost {
    directory: tempfile::TempDir,
    paths: PlakPaths,
    workspace: PathBuf,
    endpoint: PathBuf,
    service: RunnerService,
    serving: tokio::task::JoinHandle<()>,
}

async fn kill_at(cut: &str) -> KilledHost {
    let (directory, initial, _) = fixture();
    let paths = initial.paths.clone();
    drop(initial);
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let endpoint = directory.path().join("runner.sock");
    let service = linked::service(&workspace);
    let serving = listener(service.clone(), &endpoint);
    let ready = directory.path().join("ready");
    let mut host = HostProcess(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal_registry::tests::loss::operator_host_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("PLAK_TEST_TERMINAL_HOME", paths.home())
            .env("PLAK_TEST_TERMINAL_WORKSPACE", &workspace)
            .env("PLAK_TEST_TERMINAL_ENDPOINT", &endpoint)
            .env("PLAK_TEST_TERMINAL_READY", &ready)
            .env("PLAK_TEST_TERMINAL_CUT", cut)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            host.0.try_wait().unwrap().is_none(),
            "host fixture exited before cut"
        );
        assert!(Instant::now() < until, "host fixture never reached cut");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        service.operator_terminals().unwrap().has_live_terminals(),
        cut == "running"
    );
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    KilledHost {
        directory,
        paths,
        workspace,
        endpoint,
        service,
        serving,
    }
}

async fn loss_cut(cut: &str, replacement: bool) {
    let killed = kill_at(cut).await;
    let registry = TerminalRegistry::new(killed.paths.clone(), 8).unwrap();
    let mut owner = context("operator");
    owner.host_generation = 8;
    assert!(
        registry.list(&owner).unwrap().is_empty(),
        "no host persistence"
    );
    assert!(!registry.paths.product_database().exists());
    assert!(
        registry.admit("local", &killed.workspace).await.is_err(),
        "no reclaim before first bind"
    );
    let replacement_service = linked::service(&killed.workspace);
    let replacement_endpoint = killed.directory.path().join("replacement.sock");
    let second = if replacement {
        Some(listener(replacement_service.clone(), &replacement_endpoint))
    } else {
        None
    };
    let stream = tokio::net::UnixStream::connect(if replacement {
        &replacement_endpoint
    } else {
        &killed.endpoint
    })
    .await
    .unwrap();
    let (client, _) =
        RunnerClient::connect(stream, linked::bind(&registry), ClientWindows::default())
            .await
            .unwrap();
    registry.reconcile("local", &client).await.unwrap();
    let forgotten = TerminalHandle {
        identity: "forgotten".into(),
        host_generation: 7,
        instance: killed.service.hello().instance.clone(),
    };
    assert!(matches!(
        registry
            .attach(
                &owner,
                &forgotten,
                TerminalCursor {
                    instance: id(forgotten.instance.clone()).unwrap(),
                    offset: 0
                }
            )
            .await,
        Err(TerminalError::Lost)
    ));
    if replacement {
        assert!(registry.list(&owner).unwrap().is_empty());
        assert!(
            !registry.blocks_idle_exit(),
            "no global first-use barrier after both tables were lost"
        );
        assert!(
            killed
                .service
                .operator_terminals()
                .unwrap()
                .has_live_terminals(),
            "unlocalized loss never kills or infers death of old processes"
        );
    }
    let until = Instant::now() + Duration::from_secs(5);
    while registry.blocks_idle_exit() {
        assert!(registry.admit("local", &killed.workspace).await.is_err());
        for old in registry.list(&owner).unwrap() {
            assert!(old.lost);
            assert!(matches!(
                registry
                    .attach(&owner, &old.handle, old.row.unwrap().oldest)
                    .await,
                Err(TerminalError::Lost)
            ));
        }
        registry.reconcile("local", &client).await.unwrap();
        assert!(Instant::now() < until, "first-bind cleanup never settled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!registry.blocks_idle_exit());
    assert!(registry.admit("local", &killed.workspace).await.is_ok());
    assert!(!registry.paths.product_database().exists());
    assert_eq!(client.lines().await.unwrap(), []);
    assert_eq!(registry.shutdown(Duration::from_secs(2)).await, 0);
    cleanup(killed, client, second).await;
}

async fn cleanup(
    killed: KilledHost,
    client: RunnerClient,
    second: Option<tokio::task::JoinHandle<()>>,
) {
    killed.service.operator_terminals().unwrap().close_all();
    let until = Instant::now() + Duration::from_secs(5);
    while killed
        .service
        .operator_terminals()
        .unwrap()
        .has_live_terminals()
    {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.disconnect();
    killed.serving.abort();
    if let Some(second) = second {
        second.abort();
    }
}
