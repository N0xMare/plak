use super::*;
use plakous_runner_protocol::retention as wire;

async fn retained() -> (
    tempfile::TempDir,
    Worker,
    Worker,
    Vec<Loopback>,
    RunnerSessions,
    JoinedProcessSessionId,
) {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (link, old) = first.port(root.path(), "same", 1).await;
    let process = old.start(command(), control(50)).await.unwrap();
    old.promote(process).await.unwrap();
    let next = Worker::new(root.path(), "second").await;
    let (new_link, port) = next.port(root.path(), "same", 2).await;
    assert_eq!(port.jobs().await.unwrap().len(), 1);
    (root, first, next, vec![link, new_link], port, process)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_c47_same_instance_native_lease_expiry_keeps_retention_authority() {
    let (_root, first, _next, link, port, process) = retained().await;
    link[0].sever();
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock(&first.service.inner.leases)
        .get(&("home".into(), "same".into()))
        .is_some_and(|lease| lease.connections != 0)
    {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    first
        .service
        .expire_leases(Instant::now() + Duration::from_secs(120));
    port.write_stdin(
        process,
        b"retained",
        JoinedProcessInputDisposition::Close,
        control(51),
    )
    .await
    .unwrap();
    assert_eq!(finish(&port, process).await, b"retained");
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.service.inner.foreground.pending() {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(
        !first.service.is_idle(),
        "unconsumed terminal must retain its worker"
    );
    port.acknowledge_jobs(vec![JoinedProcessSessions::job_number(process)])
        .await
        .unwrap();
    assert!(
        first.service.is_idle(),
        "consumption releases the drainer's idle hold"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_review_stale_and_foreign_owners_have_no_job_authority() {
    let (_root, first, next, _link, _port, _process) = retained().await;
    let entry = next
        .service
        .retention_context()
        .unwrap()
        .registry
        .entries("home", None)
        .into_iter()
        .find(|entry| !entry.inventory.jobs.is_empty())
        .unwrap();
    let origin = &entry.inventory.origin;
    let session = entry.inventory.jobs[0].session.clone();
    let correct = entry.to;
    let stale = wire::Owner {
        fence: correct.fence - 1,
        ..correct.clone()
    };
    let foreign = wire::Owner {
        branch: retention::id("foreign").unwrap(),
        ..correct.clone()
    };
    for (owner, mut scope) in [
        (stale, scope("home", "same").unwrap()),
        (foreign, scope("home", "same").unwrap()),
        (correct.clone(), scope("home", "foreign-instance").unwrap()),
        (correct, scope("foreign-host", "same").unwrap()),
    ] {
        scope.version = wire::VERSION;
        for request in [
            wire::Request::JobsSnapshot {
                owner: owner.clone(),
                after: 0,
            },
            wire::Request::Read {
                owner: owner.clone(),
                session: session.clone(),
                cursor: 0,
                max_bytes: 16,
            },
            wire::Request::Write {
                owner: owner.clone(),
                session: session.clone(),
                bytes: "78".into(),
                close: false,
            },
            wire::Request::Stop {
                owner: owner.clone(),
                session: Some(session.clone()),
                reason: wire::StopReason::User,
                grace_ms: 0,
            },
            wire::Request::Ack {
                owner: owner.clone(),
                sessions: vec![session.clone()],
                controls: Vec::new(),
            },
        ] {
            let mut call = call(origin, "stale", request).unwrap();
            if matches!(
                call.request,
                wire::Request::Write { .. } | wire::Request::Stop { .. }
            ) {
                call.mutation = Some(wire::Mutation {
                    key: wire::Key::Control {
                        control_id: retention::id("stale").unwrap(),
                    },
                    digest: retention::id("digest").unwrap(),
                    replay: false,
                    age_ms: None,
                });
            }
            assert_eq!(
                first.service.retention_call(&scope, call, origin).await,
                Answer::Refused {
                    reason: wire::Refusal::StaleOwner
                }
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_review_transfer_refuses_unsettled_foreground() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_link, old) = first.port(root.path(), "same", 1).await;
    let process = old.start(command(), control(52)).await.unwrap();
    let context = first.service.retention_context().unwrap();
    let from = lock(&first.service.inner.tables)
        .retention_owners("home")
        .into_iter()
        .next()
        .unwrap();
    let from = retention::owner(&from).unwrap();
    let request = wire::Request::Transfer {
        to: wire::Owner {
            fence: 2,
            ..from.clone()
        },
        from,
    };
    let call = call(&context.origin, "unsettled", request).unwrap();
    assert_eq!(
        first
            .service
            .retention_call(&scope("home", "next").unwrap(), call, &context.origin)
            .await,
        Answer::Refused {
            reason: wire::Refusal::UnsettledForeground
        }
    );
    old.write_stdin(
        process,
        b"still-owned",
        JoinedProcessInputDisposition::Close,
        control(53),
    )
    .await
    .unwrap();
    assert_eq!(finish(&old, process).await, b"still-owned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_review_lifetime_starts_before_first_terminal_observation() {
    let (_root, first, _next, _link, port, process) = retained().await;
    port.write_stdin(
        process,
        b"",
        JoinedProcessInputDisposition::Close,
        control(54),
    )
    .await
    .unwrap();
    finish(&port, process).await;
    assert!(
        !first
            .service
            .retention_keeps_alive(Instant::now() + Duration::from_secs(25 * 3600)),
        "the first inspection must not restart terminal retention"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_review_watch_stops_when_its_consumer_closes() {
    use tokio::io::AsyncBufReadExt as _;
    let (_root, _first, next, _link, port, process) = retained().await;
    port.write_stdin(
        process,
        b"",
        JoinedProcessInputDisposition::Close,
        control(58),
    )
    .await
    .unwrap();
    finish(&port, process).await;
    let owner = lock(&next.service.inner.tables)
        .retention_owners("home")
        .into_iter()
        .next()
        .unwrap();
    let (consumer, writer) = tokio::io::duplex(64 * 1024);
    let service = next.service.clone();
    let task = tokio::spawn(async move {
        service
            .relay_watch_pump("home", "same", &owner, writer)
            .await;
    });
    let mut consumer = tokio::io::BufReader::new(consumer);
    let mut initial = String::new();
    tokio::time::timeout(Duration::from_secs(5), consumer.read_line(&mut initial))
        .await
        .unwrap()
        .unwrap();
    assert!(initial.contains("exited"));
    // With a fixed terminal snapshot there is no later changed write to
    // accidentally notice closure; the pump must observe its reader directly.
    drop(consumer);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_review_forwarded_namespace_stops_charging_the_middle_worker() {
    let (root, _first, middle, _links, _port, process) = retained().await;
    let third = Worker::new(root.path(), "third").await;
    let (_link, latest) = third.port(root.path(), "third", 3).await;
    assert_eq!(latest.jobs().await.unwrap()[0].status, JobStatus::Running);
    let (_other_link, other) = middle
        .port_for_line(
            root.path(),
            "unrelated",
            1,
            "00000000-0000-0000-0000-0000000000a2",
        )
        .await;
    let local = other.start(command(), control(55)).await.unwrap();
    other
        .write_stdin(
            local,
            b"",
            JoinedProcessInputDisposition::Close,
            control(56),
        )
        .await
        .unwrap();
    finish(&other, local).await;
    latest
        .write_stdin(
            process,
            b"",
            JoinedProcessInputDisposition::Close,
            control(57),
        )
        .await
        .unwrap();
    finish(&latest, process).await;
}
