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

#[test]
fn request_projects_its_deadline_while_the_owner_executor_is_not_polling() {
    let owner = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let reader = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let registry = registry(1);
    registry.run_probes_on(owner.handle().clone());
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    registry.probe_with(Arc::new({
        let (started, release) = (started.clone(), release.clone());
        move |registry, id| {
            let (started, release) = (started.clone(), release.clone());
            Box::pin(async move {
                registry.progress(&id, ProbePhase::Reconcile);
                started.fetch_add(1, Ordering::SeqCst);
                release.acquire().await.unwrap().forget();
                registry.blocked(&id, PlacementFailure::RunnerNotPaired, 1);
                Ok(())
            })
        }
    }));
    let task = reader.spawn({
        let registry = registry.clone();
        async move { registry.refresh(None).await }
    });
    reader.block_on(async { tokio::task::yield_now().await });
    owner.block_on(until(|| started.load(Ordering::SeqCst) == 1));
    let rows = reader.block_on(async {
        tokio::time::advance(Duration::from_secs(25)).await;
        task.await.unwrap().unwrap()
    });
    assert_eq!(
        rows[0].status,
        RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable {
            phase: ProbePhase::Reconcile,
            timed_out: true
        })
    );
    assert_eq!(
        registry.rows()[0].status,
        RunnerStatus::Unknown,
        "a request overwrote shared probe state"
    );
    release.add_permits(1);
    owner.block_on(until(|| {
        registry.rows()[0].status == RunnerStatus::Blocked(PlacementFailure::RunnerNotPaired)
    }));
}

#[tokio::test(start_paused = true)]
async fn stop_debt_invalidates_an_inflight_probe_before_readiness_can_reopen() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("product")).unwrap();
    std::fs::create_dir(home.path().join("workspace")).unwrap();
    let catalog = Arc::new(
        crate::WorkspaceCatalog::local(
            Some(&home.path().join("workspace")),
            &[],
            &home.path().join("product"),
        )
        .unwrap(),
    );
    let registry = RunnerRegistry::default();
    registry.configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    registry.run_probes_on(tokio::runtime::Handle::current());
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    registry.probe_with(Arc::new({
        let (calls, release) = (calls.clone(), release.clone());
        move |registry, _| {
            let (calls, release, catalog) = (calls.clone(), release.clone(), catalog.clone());
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                release.acquire().await.unwrap().forget();
                registry.ready_local(catalog, 1)
            })
        }
    }));
    let refresh = tokio::spawn({
        let registry = registry.clone();
        async move { registry.refresh(None).await }
    });
    until(|| calls.load(Ordering::SeqCst) == 1).await;
    registry.reconcile_when_available("local");
    release.add_permits(1);
    until(|| calls.load(Ordering::SeqCst) == 2).await;
    assert!(
        registry.services("local").is_err(),
        "the stale probe reopened admission"
    );
    for _ in 0..3 {
        registry.continue_reconciliation("local");
    }
    release.add_permits(1);
    until(|| registry.rows()[0].status == RunnerStatus::Ready).await;
    assert_eq!(
        refresh.await.unwrap().unwrap()[0].status,
        RunnerStatus::Ready
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    registry.continue_reconciliation("local");
    assert_eq!(registry.rows()[0].status, RunnerStatus::Ready);
    registry.reconcile_when_available("local");
    until(|| calls.load(Ordering::SeqCst) == 3).await;
    release.add_permits(1);
    until(|| registry.rows()[0].status == RunnerStatus::Ready).await;
}

#[test]
fn dropping_a_request_executor_cannot_kill_the_process_owned_reconcile() {
    let owner = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let request = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let registry = registry(1);
    registry.run_probes_on(owner.handle().clone());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (started, starting) = tokio::sync::oneshot::channel();
    let (completed, completing) = tokio::sync::oneshot::channel();
    let started = Arc::new(std::sync::Mutex::new(Some(started)));
    let completed = Arc::new(std::sync::Mutex::new(Some(completed)));
    registry.probe_with(Arc::new({
        let release = release.clone();
        move |registry, id| {
            let (started, completed, release) =
                (started.clone(), completed.clone(), release.clone());
            Box::pin(async move {
                registry.progress(&id, ProbePhase::Reconcile);
                started.lock().unwrap().take().unwrap().send(()).unwrap();
                release.acquire().await.unwrap().forget();
                registry.blocked(&id, PlacementFailure::RunnerNotPaired, 1);
                completed.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok(())
            })
        }
    }));
    request.block_on(async {
        let reader = tokio::spawn({
            let registry = registry.clone();
            async move { registry.refresh(None).await }
        });
        tokio::time::timeout(Duration::from_secs(5), starting)
            .await
            .unwrap()
            .unwrap();
        reader.abort();
        let _ = reader.await;
    });
    drop(request);
    release.add_permits(1);
    owner.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), completing)
            .await
            .unwrap()
            .unwrap();
        let rows = registry.refresh(None).await.unwrap();
        assert_eq!(
            rows[0].status,
            RunnerStatus::Blocked(PlacementFailure::RunnerNotPaired)
        );
    });
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

#[test]
fn a_stale_completion_cannot_clear_a_fresh_connection_requirement() {
    let registry = registry(1);
    registry
        .entries
        .write()
        .unwrap()
        .get_mut("runner-0")
        .unwrap()
        .row
        .status = RunnerStatus::Ready;
    registry.require_connection("runner-0", true);
    registry
        .entries
        .write()
        .unwrap()
        .get_mut("runner-0")
        .unwrap()
        .refresh
        .reconcile_again = true;
    registry.require_connection("runner-0", false);
    assert!(
        registry.needs_connection("runner-0"),
        "the Ready-to-blocked fence window lost its debt"
    );
    registry.reconcile_when_available("runner-0");
    registry.require_connection("runner-0", false);
    assert!(
        registry.needs_connection("runner-0"),
        "blocked reconciliation lost its debt"
    );
    registry
        .entries
        .write()
        .unwrap()
        .get_mut("runner-0")
        .unwrap()
        .row
        .status = RunnerStatus::Ready;
    registry.require_connection("runner-0", false);
    assert!(!registry.needs_connection("runner-0"));
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
    until(|| request.is_finished()).await;
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
            client.connection_revision(),
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
    assert_catalog_repair(&registry, &client).await;
    serving.abort();
}

async fn assert_catalog_repair(registry: &RunnerRegistry, client: &crate::RunnerClient) {
    let selected = registry.services("picker").unwrap().unwrap();
    let configured = Arc::new(std::sync::Mutex::new(vec![ConfiguredRunner {
        runner_id: "picker".into(),
        names: vec!["renamed".into()],
    }]));
    registry.configured_with(Arc::new({
        let configured = configured.clone();
        move || Ok(configured.lock().unwrap().clone())
    }));
    registry.services("picker").unwrap();
    assert_eq!(registry.rows()[0].runner.names, ["renamed"]);
    assert!(
        selected.is_available(),
        "alias rename preserves the pinned identity"
    );
    *configured.lock().unwrap() = vec![ConfiguredRunner {
        runner_id: "replacement".into(),
        names: vec!["renamed".into()],
    }];
    assert_eq!(
        registry.refresh(Some(&["picker".into()])).await,
        Err(PlacementFailure::RunnerNotConfigured {
            saved_id: "picker".into()
        })
    );
    assert_eq!(registry.rows()[0].runner.runner_id, "replacement");
    assert!(
        !selected.is_available(),
        "re-pair cannot silently retarget saved services"
    );
    assert!(
        client.is_connected(),
        "admitted calls retain their reconciliation client"
    );
}

mod hold {
    use super::*;
    use crate::{ClientWindows, RunnerClient, RunnerIdentity, RunnerService, RunnerServices};
    use plakous_runner_protocol::{Bind, Hello, VersionTriple};
    use plakous_tools::internal::runner::{RunnerBackend, RunnerLink, RunnerSessions};
    use plakous_tools::{
        ExecutionBackendError, ExecutionBackendErrorKind, ExecutionEnvironment,
        ExecutionEnvironmentConfig, ExecutionEnvironmentScope, JoinedProcessCommand,
        JoinedProcessStartRequest, JoinedProcessTerminalMode, LineOwner, OperationClass,
        OperationContext, RunnerId,
    };

    /// A live in-process runner `held` and the host's client of it.
    async fn linked(
        environment: &ExecutionEnvironment,
    ) -> (RunnerClient, Hello, tokio::task::JoinHandle<()>) {
        let version = VersionTriple::current("hold", "test");
        let service = RunnerService::new(
            environment.clone(),
            RunnerIdentity {
                runner_id: RunnerId::new("held").unwrap(),
                version: version.clone(),
                boot_id: "boot".into(),
                binary_id: None,
            },
        )
        .unwrap();
        let (host, runner) = tokio::io::duplex(65536);
        let serving = tokio::spawn(async move {
            service.serve_connection(runner).await;
        });
        let bind = Bind {
            host_id: "host".into(),
            host_instance: "hold".into(),
            version,
            inspect: false,
        };
        let (client, hello) = RunnerClient::connect(host, bind, ClientWindows::default())
            .await
            .unwrap();
        (client, hello, serving)
    }

    fn ready(registry: &RunnerRegistry, client: &RunnerClient, services: RunnerServices, now: u64) {
        let options = crate::WorkspaceOptions {
            roots: None,
            default_workspace: None,
            copies: false,
        };
        registry
            .ready(
                "held",
                client.connection_revision(),
                services,
                options,
                crate::WorkspaceDiscovery::default(),
                now,
            )
            .unwrap();
    }

    fn owner() -> LineOwner {
        LineOwner::new(
            "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
            1,
            "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
            1,
        )
    }

    /// A model tool call's process start through `link`, which would leave
    /// a marker in the workspace if it ran.
    fn start(
        link: Arc<dyn RunnerLink>,
        environment: &ExecutionEnvironment,
        call: u8,
    ) -> tokio::task::JoinHandle<Result<plakous_tools::JoinedProcessSessionId, ExecutionBackendError>>
    {
        let sessions = RunnerSessions::from_link(link, environment, owner());
        let request = JoinedProcessStartRequest::new(
            JoinedProcessCommand::literal(
                "/bin/sh",
                ["-c".to_owned(), format!("touch marker{call}")],
            )
            .unwrap(),
            ".",
            Vec::new(),
            false,
            JoinedProcessTerminalMode::Pipes,
        )
        .unwrap();
        let control = OperationContext::new(plakous_model::CancellationToken::new())
            .with_owner(owner())
            .with_class(OperationClass::EffectCall {
                call_id: format!("00000000-0000-0000-0000-0000000000{call:02x}")
                    .parse()
                    .unwrap(),
                attempt_id: "00000000-0000-0000-0000-0000000000c1".parse().unwrap(),
            });
        tokio::spawn(async move { sessions.start(request, control).await })
    }

    async fn refused(
        started: tokio::task::JoinHandle<
            Result<plakous_tools::JoinedProcessSessionId, ExecutionBackendError>,
        >,
    ) {
        let error = tokio::time::timeout(Duration::from_secs(10), started)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error.kind() == ExecutionBackendErrorKind::Io && error.proven_no_effect(),
            "{error:?}"
        );
    }

    /// An unreachable placement holds its nodes' model calls; a backend
    /// whose identity changed replaces the services and ends the old holds
    /// with a refusal rather than stranding them; a refused placement ends
    /// holds and refuses new calls at once. Nothing held is sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn placement_failures_hold_or_end_held_calls_by_kind() {
        let root = tempfile::tempdir().unwrap();
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
            root.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap();
        let (client, hello, serving) = linked(&environment).await;
        let registry = registry(0);
        registry.configure(ConfiguredRunner {
            runner_id: "held".into(),
            names: vec!["held".into()],
        });
        let services_on = |client: &RunnerClient, descriptor: serde_json::Value| {
            RunnerServices::new(
                Arc::new(RunnerBackend::new(descriptor, Arc::new(client.clone())).unwrap()),
                tokio::runtime::Handle::current(),
            )
        };
        let services = |descriptor: serde_json::Value| services_on(&client, descriptor);
        let first = services(hello.descriptor.clone());
        ready(&registry, &client, first.clone(), 1);
        registry.blocked(
            "held",
            PlacementFailure::RunnerUnreachable {
                phase: ProbePhase::LinkLost,
                timed_out: false,
            },
            2,
        );
        let held = start(first.backend().link(), &environment, 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!held.is_finished(), "an unreachable placement holds");

        // A redial binds a fresh backend allocation presenting the same
        // descriptor on the same client: the held services are kept and
        // released, so the held call is sent once.
        ready(&registry, &client, services(hello.descriptor.clone()), 3);
        tokio::time::timeout(Duration::from_secs(10), held)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // The started process runs on the runner: wait for its marker.
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.path().join("marker1").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(first.is_available());

        registry.blocked(
            "held",
            PlacementFailure::RunnerUnreachable {
                phase: ProbePhase::LinkLost,
                timed_out: false,
            },
            4,
        );
        let held = start(first.backend().link(), &environment, 2);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!held.is_finished());
        // Another client, as a fresh dial binds, replaces the services:
        // their held calls end refused rather than stranded.
        let (other, other_hello, other_serving) = linked(&environment).await;
        let second = services_on(&other, other_hello.descriptor.clone());
        ready(&registry, &other, second.clone(), 5);
        refused(held).await;
        assert!(second.is_available());

        registry.blocked("held", PlacementFailure::RunnerNotPaired, 6);
        refused(start(second.backend().link(), &environment, 3)).await;
        assert!(
            !root.path().join("marker2").exists(),
            "nothing held was sent"
        );
        assert!(!root.path().join("marker3").exists());
        other.disconnect();
        other_serving.abort();
        client.disconnect();
        serving.abort();
    }
}

#[test]
fn held_calls_reconciliation_requests_coalesce_on_a_short_backoff() {
    let start = Instant::now();
    let mut state = ProbeState::default();
    let at = |millis| start + Duration::from_millis(millis);
    assert!(state.reconcile_request_due(at(0)), "the first acts at once");
    assert!(
        !state.reconcile_request_due(at(500)),
        "coalesced within 1 s"
    );
    assert!(state.reconcile_request_due(at(1_000)));
    assert!(
        !state.reconcile_request_due(at(2_500)),
        "coalesced within 2 s"
    );
    assert!(state.reconcile_request_due(at(3_000)));
    assert!(
        !state.reconcile_request_due(at(7_000)),
        "coalesced within 5 s"
    );
    assert!(state.reconcile_request_due(at(8_000)));
    assert!(
        !state.reconcile_request_due(at(22_000)),
        "coalesced within 15 s"
    );
    assert!(state.reconcile_request_due(at(23_000)));
    assert!(
        state.reconcile_request_due(at(84_000)),
        "the backoff starts over"
    );
    assert!(!state.reconcile_request_due(at(84_500)));
}
