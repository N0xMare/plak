use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::{ConfiguredRunner, RunnerStatus};

fn registry(count: usize) -> RunnerRegistry {
    let registry = RunnerRegistry::default();
    for index in 0..count {
        let id = format!("runner-{index}");
        registry.configure(ConfiguredRunner {
            runner_id: id.clone(),
            names: vec![id],
        });
    }
    registry
}

async fn until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        predicate(),
        "scheduled probe did not reach its deterministic barrier"
    );
}

#[tokio::test(start_paused = true)]
async fn shared_refresh_is_four_wide_cached_and_not_owned_by_a_cancelled_reader() {
    let registry = registry(6);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    registry.probe_with(Arc::new({
        let (release, active, peak, calls) =
            (release.clone(), active.clone(), peak.clone(), calls.clone());
        move |registry, id| {
            let (release, active, peak, calls) =
                (release.clone(), active.clone(), peak.clone(), calls.clone());
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                registry.progress(&id, ProbePhase::Options);
                release.acquire().await.unwrap().forget();
                active.fetch_sub(1, Ordering::SeqCst);
                registry.blocked(&id, PlacementFailure::RunnerNotPaired, 1);
                Ok(())
            })
        }
    }));
    let first = tokio::spawn({
        let registry = registry.clone();
        async move { registry.refresh(None).await }
    });
    let second = tokio::spawn({
        let registry = registry.clone();
        async move { registry.refresh(None).await }
    });
    until(|| active.load(Ordering::SeqCst) == 4).await;
    first.abort();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    release.add_permits(10);
    assert_eq!(second.await.unwrap().unwrap().len(), 6);
    assert_eq!(peak.load(Ordering::SeqCst), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    registry.refresh(None).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    tokio::time::advance(Duration::from_secs(5)).await;
    registry.refresh(Some(&["runner-0".into()])).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 7);
}

#[tokio::test(start_paused = true)]
async fn queue_time_does_not_consume_budget_and_reconcile_outlives_the_picker() {
    let registry = registry(6);
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    registry.probe_with(Arc::new({
        let (release, started, completed) = (release.clone(), started.clone(), completed.clone());
        move |registry, id| {
            let (release, started, completed) =
                (release.clone(), started.clone(), completed.clone());
            Box::pin(async move {
                registry.progress(&id, ProbePhase::Reconcile);
                started.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                completed.fetch_add(1, Ordering::SeqCst);
                registry.blocked(&id, PlacementFailure::RunnerNotPaired, 2);
                Ok(())
            })
        }
    }));
    let request = tokio::spawn({
        let registry = registry.clone();
        async move { registry.refresh(None).await }
    });
    until(|| started.load(Ordering::SeqCst) == 4).await;
    tokio::time::advance(Duration::from_secs(25)).await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        4,
        "continuing reconcile retains its probe permit"
    );
    assert!(
        !request.is_finished(),
        "queued probes get their own budget after admission"
    );
    tokio::time::advance(Duration::from_secs(24)).await;
    assert!(!request.is_finished());
    release.notify_waiters();
    until(|| started.load(Ordering::SeqCst) == 6).await;
    assert_eq!(completed.load(Ordering::SeqCst), 4);
    tokio::time::advance(Duration::from_secs(24)).await;
    assert!(!request.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    let rows = request.await.unwrap().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(
                row.status,
                RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable {
                    phase: ProbePhase::Reconcile,
                    timed_out: true
                })
            ))
            .count(),
        2
    );
    assert_eq!(completed.load(Ordering::SeqCst), 4);
    release.notify_waiters();
    until(|| completed.load(Ordering::SeqCst) == 6).await;
    assert!(
        registry
            .refresh(Some(&["not-configured".into()]))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_and_concurrent_picker_refreshes_keep_the_original_same_instance_bind() {
    use crate::{
        ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService, RunnerServices,
        WorkspaceCatalog,
    };
    use plakous_runner_protocol::{Bind, VersionTriple};
    use plakous_tools::internal::runner::RunnerBackend;
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
    };
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("workspace");
    let protected = home.path().join("protected");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&protected).unwrap();
    let catalog =
        Arc::new(WorkspaceCatalog::machine(vec![root.clone()], &protected, false).unwrap());
    let version = VersionTriple::current("picker", "test");
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
            &root,
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("picker").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            workspace_catalog: Some(catalog.clone()),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let (host, runner) = tokio::io::duplex(65536);
    let serving = tokio::spawn(async move { service.serve_connection(runner).await });
    let (client, hello) = RunnerClient::connect(
        host,
        Bind {
            host_id: "host".into(),
            host_instance: "same-instance".into(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    let registry = RunnerRegistry::default();
    registry.configure(ConfiguredRunner {
        runner_id: "picker".into(),
        names: vec!["second".into(), "first".into()],
    });
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    registry
        .ready(
            "picker",
            client.clone(),
            RunnerServices::new(backend, tokio::runtime::Handle::current()),
            catalog.options(),
            catalog.discover(),
            1,
        )
        .unwrap();
    let instance = client.instance();
    let ids = ["picker".to_owned()];
    let (first, second) = tokio::join!(registry.refresh(None), registry.refresh(Some(&ids)));
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(registry.rows()[0].runner.names, ["first", "second"]);
    assert!(
        client.is_connected(),
        "a second bind must not evict the working client"
    );
    assert_eq!(client.instance(), instance);
    client.lines().await.unwrap();
    serving.abort();
}
