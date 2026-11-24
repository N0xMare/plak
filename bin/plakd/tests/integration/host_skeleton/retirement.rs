use super::*;
use plakd::home::paths::PlakPaths;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionLocator,
    SessionOperationId,
};
use plakous_host_protocol::topology::WriterMode;
use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest};
use plakous_offline_crash_harness::{
    BoundaryController, protocol::CrashBoundary, text_runtime_binding,
};
use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore as _, vcs_fixtures};
use plakous_recursive_subagents_store::SqliteTreeStore;
use serde_json::Value;
use std::time::{Duration, Instant};

const CONFIG: &str = "[root]\nagent = \"lead\"\n[orchestration]\nsubagents = true\n";

fn seed_child(
    fixture: &Fixture,
    paths: &PlakPaths,
    workspace: &std::path::Path,
    node: &NodeAddress,
) -> (String, SessionLocator) {
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        fixture.root.join("retirement-evidence.jsonl"),
    )
    .unwrap();
    let binding = text_runtime_binding(workspace, controller, "fixture", "fixture").unwrap();
    let request = CreateSessionRequest::new(
        SessionOperationId::new("retirement:cli-fixture").unwrap(),
        binding,
    );
    let locator = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let CreateSessionOutcome::Created { session, .. } = sessions.create(request).await.unwrap()
        else {
            panic!("created session required");
        };
        session.release().await.unwrap()
    });
    let mut tree = plakd::store::tree::load_tree(paths, &node.tree_id)
        .unwrap()
        .unwrap_or_else(|| {
            ProductAgentTree::bootstrap_root_for_test(
                &node.tree_id,
                "root",
                "owner",
                workspace.to_owned(),
                "offline-loopback-v1",
                WriterMode::SingleLease,
            )
            .unwrap()
        });
    let parent = tree.root_agent_id.clone();
    let child = tree
        .spawn_agent(&parent, &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, locator.to_string())
        .unwrap();
    tree.submit_result(
        &child,
        &SubmitResultRequest {
            turn_token: tree.attempt(&child).unwrap().turn_token,
            output_json: r#"{"ok":true}"#.to_owned(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child).unwrap();
    tree.settle_for_test();
    let mail = tree.pending_result_mail(&parent).remove(0);
    tree.acknowledge_mail(&mail.message_id).unwrap();
    tree.attempt_mut_for_test(&child).ended_at = Some(1);
    plakd::store::tree::save_tree(paths, &mut tree).unwrap();
    (child, locator)
}

#[test]
fn a_cli_trace_receipt_unblocks_serving_host_retirement() {
    let Some(fixture) = fixture() else { return };
    let repo = vcs_fixtures::git_repo();
    let paths = PlakPaths::explicit(fixture.home()).for_workspace(repo.path());
    fixture.write_script(&json!({"version": 1, "tree": true, "durable_tree": true, "steps": []}));
    fixture.write_config(CONFIG);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(repo.path()))
    else {
        panic!("root not created");
    };
    stop(&mut host);
    let (child, locator) = seed_child(&fixture, &paths, repo.path(), &node);
    fixture.write_config(&format!(
        "{CONFIG}[runtime_store]\nretire_requires_archive_receipt = true\nretire_finished_children = true\n"
    ));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(2, &node);

    let address = format!("{session}.{child}");
    let artifact = fixture.root.join("retirement-trace.json");
    let captured = fixture
        .command()
        .current_dir(repo.path())
        .args(["session", "trace", &address, "--out"])
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let bytes = std::fs::read(&artifact).unwrap();
    let trace: Value = serde_json::from_slice(&bytes).unwrap();
    let source = &trace["source"];
    assert_eq!(source["head"], locator.to_string());
    let digest = plakous::model::sha256_hex(&[&bytes]);
    let archive_id = "fixture-bundle/retirement-trace.json";
    let recorded = fixture
        .command()
        .current_dir(repo.path())
        .args([
            "session",
            "trace-receipt",
            "record",
            &address,
            "--attempt",
            source["attempt_id"].as_str().unwrap(),
            "--head",
            source["head"].as_str().unwrap(),
            "--revision",
            &source["revision"].as_u64().unwrap().to_string(),
            "--sha256",
            &digest,
            "--archive",
            archive_id,
        ])
        .output()
        .unwrap();
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let tree = store.load(&node.tree_id).unwrap().unwrap();
        if !tree.rows().nodes().contains_key(&child) {
            let summary = &tree.rows().retired_children()[&child];
            let receipt = summary.archive.as_ref().unwrap();
            assert_eq!(receipt.id, archive_id);
            assert_eq!(receipt.sha256, digest);
            assert_eq!(summary.output.as_deref(), Some(r#"{"ok":true}"#));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "serving host did not collect receipted child"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        AgentSessions::inspect_settled_revision_sqlite(paths.runtime_database(), locator).is_err()
    );
    stop(&mut host);
}
