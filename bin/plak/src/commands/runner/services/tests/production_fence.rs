use super::*;
use plakous::runner::protocol::Hello;
use plakous::runner::{PlacementFailure, ProbePhase, RunnerStatus};
use plakous::tools::ExecutionBackend as _;
use std::cell::RefCell;
use std::time::Duration;

mod stub;

tokio::task_local! {
    static REBIND_AFTER_CLEANUP: RefCell<Option<tokio::io::DuplexStream>>;
}

pub(in crate::commands::runner::services) async fn after_cleanup(client: &RunnerClient) {
    if let Ok(Some(stream)) = REBIND_AFTER_CLEANUP.try_with(|cut| cut.borrow_mut().take()) {
        client.reconnect(stream).await.unwrap();
        client.lines().await.unwrap();
        assert!(client.is_connected());
    }
}

#[tokio::test]
async fn ready_fence_production_bind_eof_schedules_one_redial() {
    production_cut(false, false).await;
}

#[tokio::test]
async fn ready_fence_production_bind_requires_capture_before_reconciliation() {
    production_cut(false, true).await;
}

#[tokio::test]
async fn production_bind_accepts_mixed_os_hello_after_reconciliation() {
    for os in ["macos", "linux"] {
        let mut fixture = stopped_runtime().await;
        fixture.session.take().unwrap().release().await.unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let cleanups = Arc::new(AtomicUsize::new(0));
        let (stream, hello) = stub::line_stream_for_os(
            &fixture.services,
            false,
            starts.clone(),
            cleanups.clone(),
            Some(os),
        );
        let (client, received) = RunnerClient::connect(
            stream,
            Bind {
                host_id: "fixture".into(),
                host_instance: "fixture".into(),
                version: hello.version.clone(),
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await
        .unwrap();
        assert_eq!(received.descriptor, hello.descriptor);
        stub::seed_cleanup(&fixture.services, &client, fixture.line).await;
        configure_remote(&fixture.services, &hello);
        let registry = fixture.services.runners();
        registry.configure(ConfiguredRunner {
            runner_id: "fixture".into(),
            names: vec!["fixture".into()],
        });
        registry.require_connection("fixture", true);
        let pass = Arc::new(Pass {
            client: client.clone(),
            hello,
            starts,
            cleanups,
            count: AtomicUsize::new(0),
            rebind_after_cleanup: false,
        });
        let retries = start_probes_with_dial(
            &fixture.services,
            fixture.catalog.clone(),
            None,
            &tokio::runtime::Handle::current(),
            pass.remote_dial(),
        );
        let rows = registry.refresh(None).await.unwrap();
        assert_eq!(rows[0].status, RunnerStatus::Ready);
        assert_eq!(pass.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(pass.count.load(Ordering::SeqCst), 1);
        assert_eq!(retries.lock().unwrap()["fixture"].delay, 0);
        assert!(!retries.lock().unwrap()["fixture"].scheduled);
        let runner = registry.services("fixture").unwrap().unwrap();
        assert_eq!(runner.backend().identity().operating_system(), os);
        client.disconnect();
    }
}

#[cfg(feature = "web")]
#[tokio::test]
async fn ready_fence_home_local_eof_schedules_one_redial() {
    production_cut(true, false).await;
}

#[cfg(feature = "web")]
#[tokio::test]
async fn ready_fence_home_local_requires_capture_before_reconciliation() {
    production_cut(true, true).await;
}

#[cfg(feature = "web")]
#[tokio::test]
async fn ready_fence_home_local_fresh_rebind_reopens_admission() {
    let mut fixture = stopped_runtime().await;
    fixture.session.take().unwrap().release().await.unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let cleanups = Arc::new(AtomicUsize::new(0));
    let (stream, hello) =
        stub::line_stream(&fixture.services, false, starts.clone(), cleanups.clone());
    let (client, _) = RunnerClient::connect_home_local(
        stream,
        Bind {
            host_id: "fixture".into(),
            host_instance: "fixture".into(),
            version: hello.version,
            inspect: false,
        },
        ClientWindows::default(),
        None,
    )
    .await
    .unwrap();
    stub::seed_cleanup(&fixture.services, &client, fixture.line).await;
    let registry = fixture.services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    host::finish_local_bind(&fixture.services, fixture.catalog.clone(), client.clone())
        .await
        .unwrap();
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert!(registry.services("local").unwrap().is_none());
    client.disconnect();
    assert!(
        registry.services("local").is_err(),
        "opens observe loss before the watcher"
    );
    let (stream, _) = stub::line_stream(&fixture.services, false, starts, cleanups);
    client.reconnect(stream).await.unwrap();
    host::finish_local_bind(&fixture.services, fixture.catalog, client.clone())
        .await
        .unwrap();
    assert_eq!(registry.rows()[0].status, RunnerStatus::Ready);
    assert!(registry.services("local").unwrap().is_none());
    client.disconnect();
}

async fn production_cut(local: bool, rebind_after_cleanup: bool) {
    let mut fixture = stopped_runtime().await;
    fixture.session.take().unwrap().release().await.unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let cleanups = Arc::new(AtomicUsize::new(0));
    let (stream, hello) =
        stub::line_stream(&fixture.services, true, starts.clone(), cleanups.clone());
    let bind = Bind {
        host_id: "fixture".into(),
        host_instance: "fixture".into(),
        version: hello.version.clone(),
        inspect: false,
    };
    let (client, _) = RunnerClient::connect(stream, bind, ClientWindows::default())
        .await
        .unwrap();
    stub::seed_cleanup(&fixture.services, &client, fixture.line).await;
    let pass = Arc::new(Pass {
        client: client.clone(),
        hello,
        starts,
        cleanups,
        count: AtomicUsize::new(0),
        rebind_after_cleanup,
    });
    let id = if local { "local" } else { "fixture" };
    let registry = fixture.services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: id.into(),
        names: vec![id.into()],
    });
    registry.require_connection(id, true);
    if !local {
        configure_remote(&fixture.services, &pass.hello);
    }
    // Keep the paused clock still while blocking catalog work finishes. The
    // existing redial test owns backoff advancement; here we observe scheduling.
    let clock_guard = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let mut progress = registry.progress_events();
    let retries = start_probes_with_dial(
        &fixture.services,
        fixture.catalog.clone(),
        local_dial(local, pass.clone(), fixture.catalog.clone()),
        &tokio::runtime::Handle::current(),
        pass.remote_dial(),
    );
    let rows = registry.refresh(None).await.unwrap();
    assert_eq!(
        rows[0].status,
        RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable {
            phase: ProbePhase::LinkLost,
            timed_out: false,
        })
    );
    while let Ok(event) = progress.try_recv() {
        assert!(
            event
                .row
                .is_none_or(|row| row.status != RunnerStatus::Ready),
            "failed bind published Ready before the watcher blocked it"
        );
    }
    assert!(registry.services(id).is_err());
    assert_eq!(
        pass.cleanups.load(Ordering::SeqCst),
        1,
        "the last cleanup answer must arrive"
    );
    assert_eq!(
        pass.starts.load(Ordering::SeqCst),
        1,
        "only the seeded call was admitted"
    );
    assert_eq!(client.is_connected(), rebind_after_cleanup);
    {
        let state = retries.lock().unwrap();
        let retry = state
            .get(id)
            .unwrap_or_else(|| panic!("production bind failure did not schedule redial"));
        assert!(retry.scheduled);
        assert_eq!(retry.delay, 1);
    }
    for _ in 0..3 {
        retry_remote(registry.clone(), id.into(), retries.clone());
    }
    assert_eq!(retries.lock().unwrap()[id].delay, 1);
    clock_guard.abort();
    assert_eq!(pass.count.load(Ordering::SeqCst), 1);
    tokio::time::resume();
    client.disconnect();
}

struct Pass {
    client: RunnerClient,
    hello: Hello,
    starts: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
    count: AtomicUsize,
    rebind_after_cleanup: bool,
}

impl Pass {
    fn prepare(&self, services: &Services) -> Option<tokio::io::DuplexStream> {
        assert_eq!(
            self.count.fetch_add(1, Ordering::SeqCst),
            0,
            "duplicate bind"
        );
        if self.rebind_after_cleanup {
            return Some(
                stub::line_stream(services, false, self.starts.clone(), self.cleanups.clone()).0,
            );
        }
        None
    }

    fn remote_dial(self: &Arc<Self>) -> Arc<RemoteDial> {
        let pass = self.clone();
        Arc::new(move |services, remote, handle, _| {
            let pass = pass.clone();
            Box::pin(async move {
                let rebind = pass.prepare(&services);
                let dialed = super::super::super::dial::Dialed {
                    client: pass.client.clone(),
                    hello: pass.hello.clone(),
                    _ssh: tokio::process::Command::new("sleep")
                        .arg("60")
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                        .unwrap(),
                };
                REBIND_AFTER_CLEANUP
                    .scope(
                        RefCell::new(rebind),
                        finish_remote_bind(
                            &services,
                            &remote,
                            &handle,
                            tokio::time::Instant::now() + Duration::from_secs(25),
                            dialed,
                        ),
                    )
                    .await
            })
        })
    }
}

#[cfg(feature = "web")]
fn local_dial(
    local: bool,
    pass: Arc<Pass>,
    catalog: Arc<WorkspaceCatalog>,
) -> Option<Arc<LocalDial>> {
    local.then(|| {
        Arc::new(move |services: Services, _, _| {
            let (pass, catalog) = (pass.clone(), catalog.clone());
            Box::pin(async move {
                let rebind = pass.prepare(&services);
                REBIND_AFTER_CLEANUP
                    .scope(
                        RefCell::new(rebind),
                        host::finish_local_bind(&services, catalog, pass.client.clone()),
                    )
                    .await
            }) as std::pin::Pin<Box<dyn Future<Output = _> + Send>>
        }) as Arc<LocalDial>
    })
}

#[cfg(not(feature = "web"))]
fn local_dial(local: bool, _: Arc<Pass>, _: Arc<WorkspaceCatalog>) -> Option<Arc<LocalDial>> {
    assert!(!local);
    None
}

fn configure_remote(services: &Services, hello: &Hello) {
    let paths = services.paths().unwrap();
    std::fs::write(
        paths.home().join("config.toml"),
        "[runners.fixture]\nssh='fixture'\ncommand='/bin/false'\n",
    )
    .unwrap();
    let directory = paths.home().join("runners/fixture");
    super::super::super::private_directory(&directory).unwrap();
    let pin = super::super::super::PairedRunner {
        runner_id: hello.runner_id.clone(),
        source_revision: hello.version.source_revision.clone(),
        flavor: hello.version.flavor.clone(),
        binary_id: None,
        machine_id: hello.machine_id.clone().unwrap(),
        roots: vec![paths.workspace().unwrap().to_str().unwrap().into()],
    };
    std::fs::write(
        directory.join("runner.json"),
        serde_json::to_vec(&pin).unwrap(),
    )
    .unwrap();
}
