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
        let daemon = inspection_daemon(home.path()).await;
        let failure = roots.refuse_daemon_tree(root, &tree_id).await.unwrap_err();
        assert_eq!(failure.exit_code(), 7, "{}", failure.message());
        assert_eq!(failure.code(), Some("local_runner_inspection"));
        for expected in [
            &child.agent_id,
            &recorded,
            "repair this child's recorded session association",
            "plak runner ps",
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
        daemon.abort();
        let _ = daemon.await;
        std::fs::remove_file(inspection_socket(home.path())).unwrap();
    }
}

fn inspection_socket(home: &std::path::Path) -> std::path::PathBuf {
    home.join(format!(
        "host/runner/v{}/run/runner.sock",
        plakous::runner::protocol::PROTOCOL_VERSION
    ))
}

async fn inspection_daemon(home: &std::path::Path) -> tokio::task::JoinHandle<()> {
    use plakous::runner::protocol::VersionTriple;
    use plakous::runner::{RunnerIdentity, RunnerOptions, RunnerService};
    use plakous::tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
    };
    let socket = inspection_socket(home);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(home.join("host-id"), "inspection-home").unwrap();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(uuid::Uuid::new_v4().try_into().unwrap()),
        home,
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let service = RunnerService::with_options(
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
    .unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        service.serve_connection(stream).await;
    })
}
