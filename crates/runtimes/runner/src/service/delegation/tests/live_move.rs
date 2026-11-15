use super::*;
use plakous_runner_protocol::retention as wire;

mod disconnect;

fn port_on_client(worker: &Worker, client: &crate::RunnerClient, root: &Path) -> RunnerSessions {
    let backend = Arc::new(
        plakous_tools::internal::runner::RunnerBackend::new(
            worker.service.hello().descriptor.clone(),
            Arc::new(client.clone()),
        )
        .unwrap(),
    );
    let environment = runner_environment(
        run_scope(),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
        backend.clone(),
    )
    .unwrap();
    let owner = plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a2".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        3,
    );
    RunnerSessions::new(&backend, &environment, owner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_abandoned_untransferred_witness_releases_headroom_idempotently() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(200)).await.unwrap();
    old.promote(process).await.unwrap();
    let receiver = Worker::new(root.path(), "second").await;
    let (client, serving) = client(&receiver).await;
    let lines = vec![owner(1).line().to_string()];
    client.owner_floors(&lines).await.unwrap();
    let other = port_on_client(&receiver, &client, root.path());
    assert!(other.start(command(), control(201)).await.is_err());
    let registry = &receiver.service.retention_context().unwrap().registry;
    assert_eq!(
        registry.all_entries().len(),
        0,
        "a read reserved durable transfer authority"
    );
    for _ in 0..2 {
        client.forget_owner_witnesses(&lines).await.unwrap();
    }
    client
        .forget_owner_witnesses(&["unknown".into()])
        .await
        .unwrap();
    assert_eq!(registry.occupancy(|_, _| true).values().sum::<usize>(), 0);
    assert_eq!(registry.all_entries().len(), 0);
    assert!(
        lock(&receiver.service.inner.tables)
            .owner("home", &lines[0])
            .is_none()
    );
    let started = other.start(command(), control(202)).await.unwrap();
    other
        .write_stdin(
            started,
            b"",
            JoinedProcessInputDisposition::Close,
            control(203),
        )
        .await
        .unwrap();
    assert_eq!(finish(&other, started).await, Vec::<u8>::new());
    assert_eq!(
        old.jobs().await.unwrap().len(),
        1,
        "forgetting killed an original process"
    );
    client.disconnect();
    serving.await.unwrap();
}

fn owner(fence: u64) -> plakous_tools::LineOwner {
    plakous_tools::LineOwner::new(
        "00000000-0000-0000-0000-0000000000a1".parse().unwrap(),
        1,
        "00000000-0000-0000-0000-0000000000b1".parse().unwrap(),
        fence,
    )
}

async fn client(worker: &Worker) -> (crate::RunnerClient, tokio::task::JoinHandle<()>) {
    client_instance(worker, "destination").await
}

async fn client_instance(
    worker: &Worker,
    instance: &str,
) -> (crate::RunnerClient, tokio::task::JoinHandle<()>) {
    let (host, runner) = tokio::io::duplex(64 * 1024);
    let service = worker.service.clone();
    let serving = tokio::spawn(async move { service.serve_connection(runner).await });
    let (client, _) = crate::RunnerClient::connect(
        host,
        Bind {
            host_id: "home".into(),
            host_instance: instance.into(),
            version: worker.service.hello().version.clone(),
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    (client, serving)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_two_live_home_instances_keep_independent_read_and_forget() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(205)).await.unwrap();
    old.promote(process).await.unwrap();
    let receiver = Worker::new(root.path(), "second").await;
    let (one, serving_one) = client_instance(&receiver, "one").await;
    let lines = vec![owner(1).line().to_string()];
    assert_eq!(one.owner_floors(&lines).await.unwrap()[0].retained.len(), 1);
    let (two, serving_two) = client_instance(&receiver, "two").await;
    assert_eq!(two.owner_floors(&lines).await.unwrap()[0].retained.len(), 1);
    assert_eq!(one.owner_floors(&lines).await.unwrap()[0].retained.len(), 1);
    one.forget_owner_witnesses(&lines).await.unwrap();
    let registry = &receiver.service.retention_context().unwrap().registry;
    assert_eq!(registry.occupancy(|_, _| true).values().sum::<usize>(), 1);
    one.disconnect();
    serving_one.await.unwrap();
    assert_eq!(two.owner_floors(&lines).await.unwrap()[0].retained.len(), 1);
    two.forget_owner_witnesses(&lines).await.unwrap();
    assert_eq!(registry.occupancy(|_, _| true).values().sum::<usize>(), 0);
    assert!(registry.all_entries().is_empty());
    two.disconnect();
    serving_two.await.unwrap();
    old.stop_jobs(plakous_tools::JobStopReason::User, Duration::ZERO)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_witnesses_are_fresh_and_disconnection_keeps_occupancy() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(72)).await.unwrap();
    old.promote(process).await.unwrap();
    let allocation = old.jobs().await.unwrap()[0].session_id;
    let next = Worker::new(root.path(), "second").await;
    let (client, serving) = client(&next).await;
    let source = owner(1);
    let desired = owner(2);
    let lines = vec![source.line().to_string()];
    let witnessed = client.owner_floors(&lines).await.unwrap();
    assert!(witnessed[0].owner.is_none());
    assert_eq!(
        witnessed[0].retained,
        [LineOwner {
            line: source.line().to_string(),
            generation: 1,
            branch: source.branch().to_string(),
            fence: 1,
        }]
    );
    assert!(
        lock(&next.service.inner.tables)
            .owner("home", &lines[0])
            .is_none()
    );
    assert!(
        next.service
            .retention_context()
            .unwrap()
            .registry
            .all_entries()
            .is_empty()
    );
    let other = port_on_client(&next, &client, root.path());
    let refused = other.start(command(), control(76)).await.unwrap_err();
    assert_eq!(
        refused.kind(),
        plakous_tools::ExecutionBackendErrorKind::ResourceLimit
    );
    assert!(
        matches!(client.transfer_owner_exact(Some(&source), &desired).await.unwrap(), crate::ExactOwnerTransfer::Accepted(stopped) if stopped.is_empty())
    );
    let fresh = client.owner_floors(&lines).await.unwrap();
    assert_eq!(fresh[0].retained, [fresh[0].owner.clone().unwrap()]);
    let rows = client.lines().await.unwrap();
    let (_, jobs) =
        plakous_tools::internal::runner::decode_jobs_snapshot(rows[0].jobs.clone()).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].session_id, allocation);
    assert_eq!(jobs[0].status, JobStatus::Running);
    first.serving.abort();
    assert!(client.owner_floors(&lines).await.is_err());
    assert_eq!(
        next.service
            .retention_context()
            .unwrap()
            .registry
            .occupancy(|_, _| true)
            .values()
            .sum::<usize>(),
        1
    );
    let (_, cached) = plakous_tools::internal::runner::decode_jobs_snapshot(
        client.lines().await.unwrap()[0].jobs.clone(),
    )
    .unwrap();
    assert_eq!(cached[0].session_id, jobs[0].session_id);
    assert_eq!(cached[0].status, JobStatus::Running);
    client.disconnect();
    serving.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_receiver_restart_recovers_both_sides_of_the_original_cas() {
    for (recorded, confirmed, late) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        pending_receiver(recorded, confirmed, late).await;
    }
}

async fn pending_receiver(recorded: bool, confirmed: bool, late: bool) {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(73)).await.unwrap();
    old.promote(process).await.unwrap();
    let allocation = old.jobs().await.unwrap()[0].session_id;
    let (stale, directory) = Worker::service(root.path(), "second");
    if recorded {
        lock(&stale.inner.tables).transfer(
            "home",
            "dead-receiver",
            "native-source".into(),
            None,
            LineOwner {
                line: owner(1).line().to_string(),
                generation: 1,
                branch: owner(1).branch().to_string(),
                fence: 1,
            },
        );
    }
    let registry = crate::retention::registry::Registry::open(&directory).unwrap();
    let own = wire::Origin {
        build: retention::id(crate::daemon::triple_key(&stale.hello().version)).unwrap(),
        instance: retention::id(stale.hello().instance.clone()).unwrap(),
        version: wire::VERSION,
    };
    let scope = scope("home", "dead-receiver").unwrap();
    stage_pending(&first, &registry, &scope, &own, confirmed, late).await;
    drop(registry);
    drop(stale);
    // Construction loads only the durable receipt, on either side of the CAS.
    let next = Worker::new(root.path(), "second").await;
    let (client, serving) = client(&next).await;
    let desired = owner(if late { 4 } else { 3 });
    assert!(
        matches!(client.transfer_owner_exact(Some(&owner(if recorded { 1 } else { 2 })), &desired).await.unwrap(), crate::ExactOwnerTransfer::Accepted(stopped) if stopped.is_empty())
    );
    let fresh = client
        .owner_floors(&[owner(1).line().to_string()])
        .await
        .unwrap();
    assert_eq!(fresh[0].owner.as_ref().unwrap().fence, desired.fence());
    assert_eq!(fresh[0].retained, [fresh[0].owner.clone().unwrap()]);
    let rows = client.lines().await.unwrap();
    let (_, jobs) =
        plakous_tools::internal::runner::decode_jobs_snapshot(rows[0].jobs.clone()).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].session_id, allocation);
    assert_eq!(jobs[0].status, JobStatus::Running);
    client.disconnect();
    serving.await.unwrap();
}

async fn stage_pending(
    first: &Worker,
    registry: &crate::retention::registry::Registry,
    scope: &wire::Bind,
    own: &wire::Origin,
    confirmed: bool,
    late: bool,
) {
    let inventory = registry.discover(scope, own).await.unwrap().remove(0);
    let intended = wire::Owner {
        fence: 2,
        ..inventory.owner.clone()
    };
    registry
        .intent(
            scope,
            inventory.owner.clone(),
            intended.clone(),
            inventory.clone(),
        )
        .unwrap();
    if late {
        registry
            .retry_intent(
                scope,
                &intended,
                inventory.owner.clone(),
                wire::Owner {
                    fence: 3,
                    ..intended.clone()
                },
                inventory.clone(),
            )
            .unwrap();
    }
    if confirmed || late {
        let original = first.service.retention_context().unwrap().origin.clone();
        let request = call(
            &original,
            "old-transfer",
            wire::Request::Transfer {
                from: inventory.owner.clone(),
                to: intended.clone(),
            },
        )
        .unwrap();
        assert!(matches!(
            first
                .service
                .retention_call(scope, request, &original)
                .await,
            Answer::Transferred { .. }
        ));
        if confirmed {
            registry
                .update(
                    scope,
                    &original,
                    &intended,
                    inventory.jobs,
                    inventory.next_number,
                )
                .unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_job_terminals_refuse_a_pending_delayed_cas_retry() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(206)).await.unwrap();
    old.promote(process).await.unwrap();
    old.write_stdin(
        process,
        b"original-terminal",
        JoinedProcessInputDisposition::Close,
        control(207),
    )
    .await
    .unwrap();
    assert_eq!(finish(&old, process).await, b"original-terminal");
    let receiver = Worker::new(root.path(), "second").await;
    let (client, serving) = client(&receiver).await;
    let context = receiver.service.retention_context().unwrap();
    let scope = scope("home", "destination").unwrap();
    stage_pending(
        &first,
        &context.registry,
        &scope,
        &context.origin,
        false,
        true,
    )
    .await;
    let desired = LineOwner {
        line: owner(3).line().to_string(),
        generation: 1,
        branch: owner(3).branch().to_string(),
        fence: 3,
    };
    lock(&receiver.service.inner.tables).transfer(
        "home",
        "destination",
        "pending-native".into(),
        None,
        desired.clone(),
    );
    let entry = context
        .registry
        .entries("home", Some(&desired.line))
        .remove(0);
    assert!(!entry.confirmed);
    assert_eq!(entry.to.fence, 3);
    assert!(entry.inventory.jobs[0].terminal);
    assert_eq!(entry.inventory.jobs[0].output, "original-terminal");
    assert_eq!(
        lock(&receiver.service.inner.tables)
            .owner("home", &desired.line)
            .unwrap()
            .0,
        desired
    );
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), client.job_terminals(&desired)).await,
            Ok(Err(_))
        ),
        "an unconfirmed own retry must not supply terminal authority"
    );
    assert_eq!(
        first
            .service
            .retention_context()
            .unwrap()
            .registry
            .all_entries()
            .len(),
        0
    );
    client.disconnect();
    serving.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_foreign_original_witness_never_creates_native_authority() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(74)).await.unwrap();
    old.promote(process).await.unwrap();
    let original = LineOwner {
        line: owner(1).line().to_string(),
        generation: 1,
        branch: owner(1).branch().to_string(),
        fence: 1,
    };
    let foreign = LineOwner {
        fence: 3,
        ..original.clone()
    };
    lock(&first.service.inner.tables).transfer(
        "home",
        "old-host",
        "foreign".into(),
        Some(&original),
        foreign.clone(),
    );
    let next = Worker::new(root.path(), "second").await;
    let (client, serving) = client(&next).await;
    let fresh = client
        .owner_floors(std::slice::from_ref(&original.line))
        .await
        .unwrap();
    assert_eq!(fresh[0].retained.as_slice(), std::slice::from_ref(&foreign));
    assert!(fresh[0].owner.is_none());
    assert!(
        matches!(client.transfer_owner_exact(Some(&owner(1)), &owner(4)).await.unwrap(), crate::ExactOwnerTransfer::WitnessMismatch(witness) if witness == foreign)
    );
    assert!(
        lock(&next.service.inner.tables)
            .owner("home", &original.line)
            .is_none()
    );
    client.disconnect();
    serving.await.unwrap();
}
