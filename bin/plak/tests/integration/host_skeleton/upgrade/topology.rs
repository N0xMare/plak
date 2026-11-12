//! Source child authority never becomes executable successor topology.

use super::*;
use plakous_host_protocol::topology::{NodeStatus, WriterMode};
use plakous_hosted::agent_tree_protocol::SpawnAgentRequest;
use plakous_recursive_subagents::tree::ProductAgentTree;

fn children(
    fixture: &Fixture,
    workspace: &std::path::Path,
    source: &str,
    status: NodeStatus,
) -> String {
    let template = std::fs::read_to_string(fixture.home().join("agents/lead.toml")).unwrap();
    for name in ["review", "explore", "implement", "orchestrate"] {
        write_private(
            &fixture.home().join(format!("agents/{name}.toml")),
            &template,
        );
    }
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        source,
        source,
        "lead",
        workspace.to_path_buf(),
        "offline-loopback-v1",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent(source, &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.set_node_status_for_test(&child.agent_id, status);
    plak::store::tree::save_tree(
        &plak::home::paths::PlakPaths::explicit(fixture.home()).for_workspace(workspace),
        &mut tree,
    )
    .unwrap();
    child.agent_id
}

#[test]
fn host_upgrade_refuses_live_or_sealing_children_before_drain() {
    for status in [NodeStatus::Running, NodeStatus::Sealing] {
        let Some(fixture) = fixture() else { return };
        let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
        let source = seed_in(&fixture, 1, repo.path());
        children(&fixture, repo.path(), &source, status);
        let mut host = start(&fixture);
        let mut client = Client::connect(&host);
        let binary = binary(&fixture);
        let refused = upgrade(&fixture, &binary);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("live_child"),
            "{status:?}: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert_eq!(client.request(1, Op::Ping), Outcome::Ok(Answer::Pong));
        assert!(!fixture.home().join("host/upgrades").exists());
        stop(&mut host);
    }
}

#[test]
fn host_upgrade_carries_dormant_children_only_as_inert_source_facts() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let repo = plakous_recursive_subagents::tree::vcs_fixtures::git_repo();
    let source = seed_in(&fixture, 2, repo.path());
    let child = children(&fixture, repo.path(), &source, NodeStatus::Interrupted);
    let mut host = start(&fixture);
    let binary = binary(&fixture);
    let upgraded = upgrade(&fixture, &binary);
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let next = successor(&fixture, &source);
    let exported = fixture.root.join("inert-children.json");
    let result = fixture
        .command()
        .current_dir(repo.path())
        .env("PLAK_TEST_STATE_KEY", DESTINATION)
        .args([
            "session",
            "export",
            &next,
            "--out",
            exported.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let carried: serde_json::Value =
        serde_json::from_slice(&std::fs::read(exported).unwrap()).unwrap();
    assert_eq!(carried["source_subagents"], 0);
    let receipt = receipts(&fixture).pop().unwrap();
    let source_document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .home()
                .join("host/upgrades")
                .join(receipt["id"].as_str().unwrap())
                .join(format!("{source}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        source_document["pending_work"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fact| { fact["kind"] == "source_child" && fact["operation"] == child })
    );
    let mut client =
        Client::connect_at(&fixture.home().join(format!("host/run/{DESTINATION}.sock")));
    assert!(matches!(
        client.request(
            3,
            Op::Attach {
                node: NodeAddress {
                    tree_id: source,
                    agent_id: child
                }
            }
        ),
        Outcome::Refused(_)
    ));
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut host);
}

#[test]
fn host_upgrade_refuses_held_root_continuation_without_resuming_it() {
    use plakous_hosted::session_store::SessionRecordStore as _;
    use plakous_hosted::sessions::projector::{RootInterrupt, RootLossIntent, RootStopAnchor};
    let Some(fixture) = fixture() else { return };
    let source = seed(&fixture, 3);
    let records = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    records
        .write_root_loss(
            &source,
            RootLossIntent::Interrupt(RootInterrupt {
                anchor: RootStopAnchor::Command {
                    command_id: format!("held:{source}"),
                },
                cause: RootInterruptCause::OwnerLost,
            }),
        )
        .unwrap();
    assert!(matches!(
        plakous_hosted::runtime::root_standing(&records, &source).unwrap(),
        plakous_hosted::runtime::RootStanding::Continuing { held: true, .. }
    ));
    let mut host = start(&fixture);
    let refused = upgrade(&fixture, &binary(&fixture));
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("held_work"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        Client::connect(&host).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    assert!(!fixture.root.join("evidence.jsonl").exists() || crash::dispatches(&fixture) == 0);
    stop(&mut host);
}
