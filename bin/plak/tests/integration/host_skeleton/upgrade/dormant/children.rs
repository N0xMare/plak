//! Real child binding leaves an immutable allocation marker, not unfinished work.
use super::*;

#[test]
fn host_move_dormant_real_bound_child_preserves_the_allocation_and_native_identity() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let child_script = fixture.root.join("child.json");
    let release = fixture.root.join("child-release");
    std::fs::write(&child_script, serde_json::to_vec(&json!({"version":1,"steps":[{"stream":[{"gate":{"file":release}}],"output":{"text":"child done"}}]})).unwrap()).unwrap();
    let mut script = spawning_script();
    script["product_child_script"] = json!(child_script);
    script["steps"].as_array_mut().unwrap().remove(1);
    fixture.write_script(&script);
    let mut host = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("root not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "spawn one real bound child", ANSWER);
    let source = bound_tree(&fixture, &node.tree_id, false);
    let child = source
        .nodes()
        .values()
        .find(|child| child.parent_agent_id.is_some())
        .unwrap()
        .agent_id
        .clone();
    let old: plakous::sessions::SessionLocator = source
        .node(&child)
        .unwrap()
        .session_locator
        .as_ref()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        client.request(
            4,
            Op::Intent {
                attachment,
                intent: SessionIntent::InterruptTopologyNode {
                    agent_id: child.clone()
                },
                client_id: None,
                blobs: Vec::new()
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let source = bound_tree(&fixture, &node.tree_id, true);
    assert!(
        source
            .operations()
            .contains_key(&format!("child_session:{child}"))
    );
    let dispatched = crash::dispatches(&fixture);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(successor(&fixture, &session), session);
    assert_child_context(&fixture, &child, &source);
    assert_eq!(
        crash::dispatches(&fixture),
        dispatched,
        "moving a dormant tree must not replay its parent"
    );
    let (target, _) = plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
        &fixture
            .home()
            .join("state")
            .join(DESTINATION)
            .join("subagents.sqlite3"),
        &node.tree_id,
    )
    .unwrap()
    .unwrap();
    let target = target.rows();
    let next = target
        .node(&child)
        .unwrap()
        .session_locator
        .as_ref()
        .unwrap();
    let next_locator: plakous::sessions::SessionLocator = next.parse().unwrap();
    assert_eq!(next_locator.session_id(), old.session_id());
    assert_eq!(next_locator.branch_id(), old.branch_id());
    assert_ne!(next_locator.checkpoint_id(), old.checkpoint_id());
    assert_eq!(
        &target.operations()[&format!("child_session:{child}")].canonical_request,
        next
    );
    stop(&mut host);
}

fn assert_child_context(
    fixture: &Fixture,
    child: &str,
    source: &plakous_recursive_subagents::tree::ProductAgentTree,
) {
    let authority = receipts(fixture).remove(0);
    let path = fixture
        .home()
        .join("host/upgrades")
        .join(authority["id"].as_str().unwrap())
        .join(format!(
            "{}.json",
            authority["groups"][0]["id"].as_str().unwrap()
        ));
    let capsule: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let head = capsule["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["heads"].as_array().unwrap())
        .find(|head| head["context"]["agent"] == child)
        .unwrap();
    let configuration = &head["context"]["configuration"];
    let node = source.node(child).unwrap();
    assert_eq!(configuration["account"], node.account);
    assert_eq!(configuration["model"], node.tuple_id);
    assert_eq!(
        configuration["effort"],
        serde_json::to_value(&node.effort).unwrap()
    );
    assert!(configuration["target"].get("open_router").is_some());
    assert!(configuration["profile"].is_null());
}

fn bound_tree(
    fixture: &Fixture,
    id: &str,
    interrupted: bool,
) -> plakous_recursive_subagents::tree::ProductAgentTree {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    loop {
        let tree = plak::store::tree::load_tree(&paths, id).unwrap().unwrap();
        if let Some(child) = tree
            .nodes()
            .values()
            .find(|node| node.parent_agent_id.is_some())
            && child.session_locator.is_some()
            && child.fold_order.is_some()
            && (if interrupted {
                !child.active_turn && tree.nodes().values().all(|node| !node.active_turn)
            } else {
                child.active_turn
            })
        {
            return tree;
        }
        assert!(
            Instant::now() < deadline,
            "real child did not reach the requested boundary"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
