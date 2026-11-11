use super::ProductStateRoots;
use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};

#[tokio::test]
async fn an_unresolvable_child_refuses_embedded_daemon_inspection_with_a_remedy() {
    let home = tempfile::tempdir().unwrap();
    let roots = ProductStateRoots::with_doubles(home.path().into(), home.path().join("state"));
    let sessions = roots.runtime_sessions().unwrap();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        home.path().join("evidence.jsonl"),
    )
    .unwrap();
    let binding = plakous_offline_crash_harness::runtime_binding(home.path(), controller).unwrap();
    let plakous::sessions::CreateSessionOutcome::Created { session, .. } = sessions
        .create(plakous::sessions::CreateSessionRequest::new(
            plakous::sessions::SessionOperationId::new("create:inspection-child").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("created root")
    };
    let root = session.locator();
    session.agent().shutdown().await.unwrap();
    let missing = plakous::sessions::SessionLocator::new(
        uuid::Uuid::new_v4().try_into().unwrap(),
        root.branch_id(),
        root.checkpoint_id(),
    );

    for recorded in ["unparseable-child".to_owned(), missing.to_string()] {
        let tree_id = uuid::Uuid::new_v4().to_string();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &tree_id,
            "root",
            "owner",
            home.path().into(),
            "tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.bind_session_locator(&child.agent_id, &recorded)
            .unwrap();
        let revision = roots.tree_store().unwrap().save_in_custody(&tree).unwrap();
        tree.acknowledge_revision(revision);
        // A missing daemon has no ownership to inspect. The same tree must
        // fail closed once a daemon answers, even when it reports no jobs.
        roots.refuse_daemon_tree(root, &tree_id).await.unwrap();
        let daemon = inspection_daemon(home.path());
        let failure = roots.refuse_daemon_tree(root, &tree_id).await.unwrap_err();
        assert!(
            !failure.can_retry_open(),
            "invalid identity is not transient"
        );
        assert_eq!(failure.exit_code(), 7, "{}", failure.message());
        assert_eq!(failure.code(), Some("local_runner_inspection"));
        for expected in [
            &child.agent_id,
            &recorded,
            "open it through `plak host` (start `plak host`, then attach)",
            "plak runner ps",
            "only as a destructive last resort",
        ] {
            assert!(failure.message().contains(expected), "{failure}");
        }
        assert_eq!(
            roots
                .tree_store()
                .unwrap()
                .load(&tree_id)
                .unwrap()
                .unwrap()
                .rows()
                .nodes()[&child.agent_id]
                .session_locator
                .as_deref(),
            Some(recorded.as_str())
        );
        let root_failure = roots
            .refuse_daemon_tree(missing, &tree_id)
            .await
            .unwrap_err();
        assert_eq!(root_failure.code(), Some("local_runner_inspection"));
        assert!(root_failure.message().contains(&tree_id));
        assert!(root_failure.message().contains(&missing.to_string()));
        daemon.abort();
        let _ = daemon.await;
        std::fs::remove_file(inspection_socket(home.path())).unwrap();
    }
}

#[tokio::test]
async fn interrupted_inspection_is_retryable_but_a_missing_identity_is_not() {
    let home = tempfile::tempdir().unwrap();
    let roots = ProductStateRoots::with_doubles(home.path().into(), home.path().join("state"));
    let locator = plakous::sessions::SessionLocator::new(
        uuid::Uuid::new_v4().try_into().unwrap(),
        uuid::Uuid::new_v4().try_into().unwrap(),
        uuid::Uuid::new_v4().try_into().unwrap(),
    );
    let socket = inspection_socket(home.path());
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(home.path().join("host-id"), "inspection-home").unwrap();
    for silent in [true, false] {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            if silent {
                std::future::pending::<()>().await;
            }
        });
        let failure = roots.refuse_daemon_tree(locator, "root").await.unwrap_err();
        assert!(failure.can_retry_open(), "{failure}");
        server.abort();
        let _ = server.await;
        std::fs::remove_file(&socket).unwrap();
    }
    let daemon = inspection_daemon(home.path());
    let failure = roots.refuse_daemon_tree(locator, "root").await.unwrap_err();
    assert_eq!(failure.code(), Some("local_runner_inspection"));
    assert!(!failure.can_retry_open());
    daemon.abort();
    let _ = daemon.await;
}

#[tokio::test]
async fn inspection_lost_after_hello_before_lines_answer_is_retryable() {
    use plakous::runner::protocol::{HostFrame, RunnerFrame, frame};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let home = tempfile::tempdir().unwrap();
    let roots = ProductStateRoots::with_doubles(home.path().into(), home.path().join("state"));
    let service = inspection_service(home.path());
    let listener = tokio::net::UnixListener::bind(inspection_socket(home.path())).unwrap();
    let hello = service.hello().clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream
            .write_all(&frame::encode(&RunnerFrame::Hello(hello)).unwrap())
            .await
            .unwrap();
        let mut frames = frame::FrameReader::new();
        let mut buffer = [0; 4096];
        let mut bound = false;
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            frames.push(&buffer[..read]);
            while let Some(body) = frames.next_frame().unwrap() {
                match frame::decode::<HostFrame>(&body).unwrap() {
                    HostFrame::Bind(bind) => {
                        assert!(bind.inspect);
                        bound = true;
                    }
                    HostFrame::Lines { .. } => {
                        assert!(bound);
                        return;
                    }
                    _ => {}
                }
            }
        }
    });
    let locator = plakous::sessions::SessionLocator::new(
        uuid::Uuid::new_v4().try_into().unwrap(),
        uuid::Uuid::new_v4().try_into().unwrap(),
        uuid::Uuid::new_v4().try_into().unwrap(),
    );
    let failure = roots.refuse_daemon_tree(locator, "root").await.unwrap_err();
    assert!(failure.can_retry_open(), "{failure}");
    assert!(failure.message().contains("ReadInterrupted"), "{failure}");
    server.await.unwrap();
}

fn inspection_socket(home: &std::path::Path) -> std::path::PathBuf {
    home.join(format!(
        "host/runner/v{}/run/runner.sock",
        plakous::runner::protocol::PROTOCOL_VERSION
    ))
}

fn inspection_daemon(home: &std::path::Path) -> tokio::task::JoinHandle<()> {
    let service = inspection_service(home);
    let listener = tokio::net::UnixListener::bind(inspection_socket(home)).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            service.serve_connection(stream).await;
        }
    })
}

fn inspection_service(home: &std::path::Path) -> plakous::runner::RunnerService {
    use plakous::runner::protocol::VersionTriple;
    use plakous::runner::{RunnerIdentity, RunnerOptions, RunnerService};
    use plakous::tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
    };
    let socket = inspection_socket(home);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(home.join("host-id"), "inspection-home").unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(uuid::Uuid::new_v4().try_into().unwrap()),
        home,
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("inspection").unwrap(),
            version: VersionTriple::current("inspection", "debug"),
            boot_id: "inspection-boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            home_local: Some("inspection-home".into()),
            ..RunnerOptions::default()
        },
    )
    .unwrap()
}
